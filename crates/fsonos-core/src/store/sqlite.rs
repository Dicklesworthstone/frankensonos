//! The fsqlite-backed [`Store`].
//!
//! Uses fsqlite's `AsyncConnection`: a `Send` handle over a dedicated
//! large-stack worker that owns the `!Send` engine connection, whose `*_sync`
//! methods fit the synchronous [`Store`] trait (see AGENTS.md, "Dependency
//! Recipe"). Writes that touch several rows run in one short transaction;
//! callers never hold a transaction across network I/O because none escapes a
//! method call.

use super::{
    Action, ActionFilter, AlbumTrack, AuthEntry, CachedAlbum, CachedPlayer, DjSession, Feedback,
    FeedbackKey, LibraryEntry, LibraryOrigin, LoggedAction, PlayRecord, Store, StoreError,
    StoredScene, StoredSchedule,
};
use fsonos_proto::didl::SpotifyRenderParams;
use fsonos_types::{Generation, Player, PlayerId, Track, ZoneGroup};
use fsqlite::{AsyncConnection, FrankenError, Row, SqliteValue};
use std::fmt::Display;
use std::ops::Range;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// One schema step. Append new steps to [`MIGRATIONS`]; never edit or
/// reorder applied ones (their `version` is recorded in each database).
struct Migration {
    version: i64,
    name: &'static str,
    sql: &'static str,
}

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "initial schema (plan §7)",
        sql: "
        CREATE TABLE players (
            id TEXT PRIMARY KEY, household TEXT NOT NULL, room TEXT NOT NULL,
            ip TEXT NOT NULL, model TEXT NOT NULL, generation TEXT NOT NULL,
            last_seen INTEGER NOT NULL);
        CREATE TABLE zone_groups (
            id INTEGER PRIMARY KEY, household TEXT NOT NULL,
            coordinator TEXT NOT NULL, member TEXT NOT NULL, updated INTEGER NOT NULL);
        CREATE TABLE spotify_library (
            source_uri TEXT PRIMARY KEY, title TEXT NOT NULL, artist TEXT,
            album TEXT, duration_secs INTEGER, is_classical INTEGER NOT NULL,
            added INTEGER NOT NULL);
        CREATE TABLE play_history (
            id INTEGER PRIMARY KEY, zone TEXT NOT NULL, source_uri TEXT NOT NULL,
            played_at INTEGER NOT NULL);
        CREATE INDEX play_history_by_zone ON play_history (zone, id);
        CREATE TABLE render_params (
            household TEXT PRIMARY KEY, sid INTEGER NOT NULL, flags INTEGER NOT NULL,
            sn INTEGER NOT NULL, cdudn TEXT NOT NULL, item_id_prefix TEXT NOT NULL,
            learned_at INTEGER NOT NULL);
        CREATE TABLE auth (
            service TEXT PRIMARY KEY, refresh_token TEXT NOT NULL,
            expires INTEGER NOT NULL);
    ",
    },
    Migration {
        version: 2,
        name: "library album/work fields for the DJ (plan §12.1)",
        sql: "
        ALTER TABLE spotify_library ADD COLUMN album_uri TEXT;
        ALTER TABLE spotify_library ADD COLUMN album_artists TEXT;
        ALTER TABLE spotify_library ADD COLUMN origin TEXT NOT NULL DEFAULT 'saved_album';
        ALTER TABLE spotify_library ADD COLUMN disc_number INTEGER;
        ALTER TABLE spotify_library ADD COLUMN track_number INTEGER;
    ",
    },
    Migration {
        version: 3,
        name: "DJ sessions, feedback, album track cache, library work_key (plan §7)",
        sql: "
        ALTER TABLE spotify_library ADD COLUMN work_key TEXT;
        CREATE TABLE dj_sessions (
            coordinator TEXT PRIMARY KEY, mood TEXT, constraints TEXT,
            expires INTEGER NOT NULL);
        CREATE TABLE feedback (
            id INTEGER PRIMARY KEY, at INTEGER NOT NULL, work_key TEXT,
            composer_key TEXT, performer TEXT, signal INTEGER NOT NULL);
        CREATE INDEX feedback_by_work ON feedback (work_key, at);
        CREATE INDEX feedback_by_composer ON feedback (composer_key, at);
        CREATE INDEX feedback_by_performer ON feedback (performer, at);
        CREATE TABLE album_tracks (
            album_uri TEXT NOT NULL, disc_number INTEGER NOT NULL,
            track_number INTEGER NOT NULL, source_uri TEXT NOT NULL,
            title TEXT NOT NULL, duration_secs INTEGER, fetched_at INTEGER NOT NULL,
            PRIMARY KEY (album_uri, disc_number, track_number));
    ",
    },
    Migration {
        version: 4,
        name: "action log for policy enforcement and undo (plan §7)",
        sql: "
        CREATE TABLE actions (
            id INTEGER PRIMARY KEY, at INTEGER NOT NULL, client TEXT NOT NULL,
            surface TEXT NOT NULL, intent TEXT NOT NULL, decision TEXT NOT NULL,
            result TEXT NOT NULL, before_state TEXT, undo_of INTEGER);
        CREATE INDEX actions_by_client ON actions (client, id);
        CREATE INDEX actions_by_time ON actions (at);
    ",
    },
    Migration {
        version: 5,
        name: "scenes: named house states (plan §7)",
        sql: "
        CREATE TABLE scenes (
            name TEXT PRIMARY KEY, spec TEXT NOT NULL, updated INTEGER NOT NULL);
    ",
    },
    Migration {
        version: 6,
        name: "schedules, run as their creator (plan §7)",
        sql: "
        CREATE TABLE schedules (
            id INTEGER PRIMARY KEY, spec TEXT NOT NULL, action TEXT NOT NULL,
            creator TEXT NOT NULL, enabled INTEGER NOT NULL, created INTEGER NOT NULL,
            last_fired INTEGER);
    ",
    },
    Migration {
        version: 7,
        name: "genre-agnostic DJ: library genres, year, explicit, candidacy, taste; \
               feedback artist and album keys (plan §12.1)",
        sql: "
        ALTER TABLE spotify_library ADD COLUMN genres TEXT;
        ALTER TABLE spotify_library ADD COLUMN release_year INTEGER;
        ALTER TABLE spotify_library ADD COLUMN explicit INTEGER NOT NULL DEFAULT 0;
        ALTER TABLE spotify_library ADD COLUMN candidate INTEGER;
        ALTER TABLE spotify_library ADD COLUMN taste_weight INTEGER;
        ALTER TABLE feedback ADD COLUMN artist_key TEXT;
        ALTER TABLE feedback ADD COLUMN album_key TEXT;
        CREATE INDEX feedback_by_artist ON feedback (artist_key, at);
        CREATE INDEX feedback_by_album ON feedback (album_key, at);
    ",
    },
    Migration {
        version: 8,
        name: "artist genre cache for the genre-agnostic DJ (plan §12.1)",
        sql: "
        CREATE TABLE artist_genres (
            artist_id TEXT PRIMARY KEY, genres TEXT, fetched_at INTEGER NOT NULL);
    ",
    },
];

/// How `spotify_library.genres` and `artist_genres.genres` join tags.
const GENRE_SEPARATOR: &str = "; ";

fn split_genres(joined: Option<String>) -> Vec<String> {
    joined
        .map(|g| {
            g.split(GENRE_SEPARATOR)
                .filter(|t| !t.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The durable store: one fsqlite database file in the daemon's data dir.
#[derive(Debug)]
pub struct SqliteStore {
    conn: AsyncConnection,
}

fn backend(e: impl Display) -> StoreError {
    StoreError::Backend(e.to_string())
}

impl SqliteStore {
    /// Open (creating if needed) the database at `path` and bring its schema
    /// up to date.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        Self::open_at(path.to_string_lossy().into_owned())
    }

    /// A private, non-persistent database (tests, `--no-store` runs).
    pub fn open_in_memory() -> Result<Self, StoreError> {
        Self::open_at(":memory:".to_string())
    }

    fn open_at(path: String) -> Result<Self, StoreError> {
        Self::open_migrated(path, MIGRATIONS)
    }

    fn open_migrated(path: String, migrations: &[Migration]) -> Result<Self, StoreError> {
        let store = Self {
            conn: AsyncConnection::open_sync(path).map_err(backend)?,
        };
        store.migrate(migrations)?;
        Ok(store)
    }

    /// Versions of the migrations applied to this database, ascending.
    pub fn schema_versions(&self) -> Result<Vec<i64>, StoreError> {
        self.conn
            .query_sync("SELECT version FROM schema_migrations ORDER BY version")
            .map_err(backend)?
            .iter()
            .map(|row| int(row, 0))
            .collect()
    }

    /// Close the database, checkpointing its WAL. Dropping a store without
    /// closing is also safe: committed writes stay in the WAL and the next
    /// open recovers them.
    pub fn close(mut self) -> Result<(), StoreError> {
        self.conn.close_sync().map_err(backend)
    }

    fn migrate(&self, migrations: &[Migration]) -> Result<(), StoreError> {
        self.conn
            .execute_sync(
                "CREATE TABLE IF NOT EXISTS schema_migrations (\
                 version INTEGER PRIMARY KEY, name TEXT NOT NULL, applied_at INTEGER NOT NULL)",
            )
            .map_err(backend)?;
        let applied = self.schema_versions()?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
        for m in migrations.iter().filter(|m| !applied.contains(&m.version)) {
            self.in_transaction(|c| {
                c.execute_batch_sync(m.sql)?;
                c.execute_with_params_sync(
                    "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
                    &[m.version.into(), m.name.into(), now.into()],
                )
                .map(drop)
            })
            .map_err(|e| backend(format!("migration {} ({}): {e}", m.version, m.name)))?;
        }
        Ok(())
    }

    /// Run `f` in one transaction, rolling back if it fails.
    fn in_transaction<T>(
        &self,
        f: impl FnOnce(&AsyncConnection) -> Result<T, FrankenError>,
    ) -> Result<T, StoreError> {
        self.conn.begin_transaction_sync().map_err(backend)?;
        match f(&self.conn) {
            Ok(value) => {
                self.conn.commit_transaction_sync().map_err(backend)?;
                Ok(value)
            }
            Err(e) => {
                // The original error is the one worth reporting.
                let _ = self.conn.rollback_transaction_sync();
                Err(backend(e))
            }
        }
    }

    fn query(&self, sql: &str, params: &[SqliteValue]) -> Result<Vec<Row>, StoreError> {
        self.conn
            .query_with_params_sync(sql, params)
            .map_err(backend)
    }

    fn execute(&self, sql: &str, params: &[SqliteValue]) -> Result<(), StoreError> {
        self.conn
            .execute_with_params_sync(sql, params)
            .map(drop)
            .map_err(backend)
    }
}

fn column(row: &Row, i: usize) -> Result<&SqliteValue, StoreError> {
    row.get(i)
        .ok_or_else(|| backend(format!("row has no column {i}")))
}

fn int(row: &Row, i: usize) -> Result<i64, StoreError> {
    column(row, i)?
        .as_integer()
        .ok_or_else(|| backend(format!("column {i} is not an INTEGER")))
}

fn opt_int(row: &Row, i: usize) -> Result<Option<i64>, StoreError> {
    match column(row, i)? {
        SqliteValue::Null => Ok(None),
        v => v
            .as_integer()
            .map(Some)
            .ok_or_else(|| backend(format!("column {i} is not an INTEGER"))),
    }
}

fn text(row: &Row, i: usize) -> Result<String, StoreError> {
    column(row, i)?
        .as_text()
        .map(str::to_string)
        .ok_or_else(|| backend(format!("column {i} is not TEXT")))
}

fn opt_text(row: &Row, i: usize) -> Result<Option<String>, StoreError> {
    match column(row, i)? {
        SqliteValue::Null => Ok(None),
        _ => text(row, i).map(Some),
    }
}

fn u32_of(v: i64, what: &str) -> Result<u32, StoreError> {
    u32::try_from(v).map_err(|_| backend(format!("{what} out of range: {v}")))
}

fn opt_value<T: Into<SqliteValue>>(v: Option<T>) -> SqliteValue {
    v.map_or(SqliteValue::Null, Into::into)
}

fn limit_value(limit: usize) -> SqliteValue {
    i64::try_from(limit).unwrap_or(i64::MAX).into()
}

fn generation_text(g: Generation) -> &'static str {
    match g {
        Generation::S1 => "S1",
        Generation::S2 => "S2",
    }
}

impl Store for SqliteStore {
    fn record_play(
        &mut self,
        zone: &str,
        source_uri: &str,
        played_at: i64,
    ) -> Result<(), StoreError> {
        self.execute(
            "INSERT INTO play_history (zone, source_uri, played_at) VALUES (?1, ?2, ?3)",
            &[zone.into(), source_uri.into(), played_at.into()],
        )
    }

    fn recent_plays(
        &self,
        zone: Option<&str>,
        limit: usize,
    ) -> Result<Vec<PlayRecord>, StoreError> {
        let rows = match zone {
            Some(z) => self.query(
                "SELECT zone, source_uri, played_at FROM play_history \
                 WHERE zone = ?1 ORDER BY id DESC LIMIT ?2",
                &[z.into(), limit_value(limit)],
            )?,
            None => self.query(
                "SELECT zone, source_uri, played_at FROM play_history \
                 ORDER BY id DESC LIMIT ?1",
                &[limit_value(limit)],
            )?,
        };
        let mut plays = rows
            .iter()
            .map(|r| {
                Ok(PlayRecord {
                    zone: text(r, 0)?,
                    source_uri: text(r, 1)?,
                    played_at: int(r, 2)?,
                })
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        plays.reverse();
        Ok(plays)
    }

    fn save_players(
        &mut self,
        household: &str,
        players: &[Player],
        seen_at: i64,
    ) -> Result<(), StoreError> {
        let rows: Vec<Vec<SqliteValue>> = players
            .iter()
            .map(|p| {
                vec![
                    p.id.0.as_str().into(),
                    household.into(),
                    p.room_name.as_str().into(),
                    p.ip.to_string().into(),
                    p.model.as_str().into(),
                    generation_text(p.generation).into(),
                    seen_at.into(),
                ]
            })
            .collect();
        self.in_transaction(|c| {
            c.execute_many_with_params_in_transaction_sync(
                "INSERT OR REPLACE INTO players \
                 (id, household, room, ip, model, generation, last_seen) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                &rows,
            )
            .map(drop)
        })
    }

    fn cached_players(&self) -> Result<Vec<CachedPlayer>, StoreError> {
        self.query(
            "SELECT household, id, room, ip, model, generation, last_seen \
             FROM players ORDER BY household, id",
            &[],
        )?
        .iter()
        .map(|r| {
            let ip = text(r, 3)?;
            let generation = match text(r, 5)?.as_str() {
                "S1" => Generation::S1,
                "S2" => Generation::S2,
                other => return Err(backend(format!("unknown generation {other:?}"))),
            };
            Ok(CachedPlayer {
                household: text(r, 0)?,
                player: Player {
                    id: PlayerId(text(r, 1)?),
                    room_name: text(r, 2)?,
                    ip: ip.parse().map_err(|_| backend(format!("bad ip {ip:?}")))?,
                    model: text(r, 4)?,
                    generation,
                },
                last_seen: int(r, 6)?,
            })
        })
        .collect()
    }

    fn save_groups(
        &mut self,
        household: &str,
        groups: &[ZoneGroup],
        updated: i64,
    ) -> Result<(), StoreError> {
        let edges: Vec<Vec<SqliteValue>> = groups
            .iter()
            .flat_map(|g| {
                g.members.iter().map(|m| {
                    vec![
                        household.into(),
                        g.coordinator.0.as_str().into(),
                        m.0.as_str().into(),
                        updated.into(),
                    ]
                })
            })
            .collect();
        self.in_transaction(|c| {
            c.execute_with_params_sync(
                "DELETE FROM zone_groups WHERE household = ?1",
                &[household.into()],
            )?;
            if edges.is_empty() {
                return Ok(());
            }
            c.execute_many_with_params_in_transaction_sync(
                "INSERT INTO zone_groups (household, coordinator, member, updated) \
                 VALUES (?1, ?2, ?3, ?4)",
                &edges,
            )
            .map(drop)
        })
    }

    fn cached_groups(&self, household: &str) -> Result<Vec<ZoneGroup>, StoreError> {
        let mut groups: Vec<ZoneGroup> = Vec::new();
        for r in self.query(
            "SELECT coordinator, member FROM zone_groups WHERE household = ?1 ORDER BY id",
            &[household.into()],
        )? {
            let coordinator = PlayerId(text(&r, 0)?);
            let member = PlayerId(text(&r, 1)?);
            match groups.last_mut() {
                Some(g) if g.coordinator == coordinator => g.members.push(member),
                _ => groups.push(ZoneGroup {
                    coordinator,
                    members: vec![member],
                }),
            }
        }
        Ok(groups)
    }

    fn upsert_library(&mut self, entries: &[LibraryEntry]) -> Result<(), StoreError> {
        let rows: Vec<Vec<SqliteValue>> = entries
            .iter()
            .map(|e| {
                vec![
                    e.track.source_uri.as_str().into(),
                    e.track.title.as_str().into(),
                    opt_value(e.track.artist.as_deref()),
                    opt_value(e.track.album.as_deref()),
                    opt_value(e.track.duration_secs.map(i64::from)),
                    i64::from(e.is_classical).into(),
                    e.added.into(),
                    opt_value(e.album_uri.as_deref()),
                    opt_value(e.album_artists.as_deref()),
                    e.origin.as_str().into(),
                    opt_value(e.disc_number.map(i64::from)),
                    opt_value(e.track_number.map(i64::from)),
                    opt_value(e.work_key.as_deref()),
                    opt_value((!e.genres.is_empty()).then(|| e.genres.join(GENRE_SEPARATOR))),
                    opt_value(e.release_year.map(i64::from)),
                    i64::from(e.explicit).into(),
                    opt_value(e.candidate.map(i64::from)),
                    opt_value(e.taste_weight.map(i64::from)),
                ]
            })
            .collect();
        if rows.is_empty() {
            return Ok(());
        }
        self.in_transaction(|c| {
            c.execute_many_with_params_in_transaction_sync(
                "INSERT OR REPLACE INTO spotify_library \
                 (source_uri, title, artist, album, duration_secs, is_classical, added, \
                  album_uri, album_artists, origin, disc_number, track_number, work_key, \
                  genres, release_year, explicit, candidate, taste_weight) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, \
                  ?16, ?17, ?18)",
                &rows,
            )
            .map(drop)
        })
    }

    fn library(&self) -> Result<Vec<LibraryEntry>, StoreError> {
        self.query(
            "SELECT source_uri, title, artist, album, duration_secs, is_classical, added, \
             album_uri, album_artists, origin, disc_number, track_number, work_key, \
             genres, release_year, explicit, candidate, taste_weight \
             FROM spotify_library ORDER BY added, source_uri",
            &[],
        )?
        .iter()
        .map(|r| {
            let origin = text(r, 9)?;
            let small = |i: usize, what: &str| opt_int(r, i)?.map(|v| u32_of(v, what)).transpose();
            Ok(LibraryEntry {
                album_uri: opt_text(r, 7)?,
                album_artists: opt_text(r, 8)?,
                origin: LibraryOrigin::parse(&origin)
                    .ok_or_else(|| backend(format!("unknown library origin {origin:?}")))?,
                disc_number: small(10, "disc_number")?,
                track_number: small(11, "track_number")?,
                work_key: opt_text(r, 12)?,
                genres: split_genres(opt_text(r, 13)?),
                release_year: opt_int(r, 14)?
                    .map(|y| {
                        u16::try_from(y)
                            .map_err(|_| backend(format!("release_year out of range: {y}")))
                    })
                    .transpose()?,
                explicit: int(r, 15)? != 0,
                candidate: opt_int(r, 16)?.map(|c| c != 0),
                taste_weight: small(17, "taste_weight")?,
                track: Track {
                    source_uri: text(r, 0)?,
                    title: text(r, 1)?,
                    artist: opt_text(r, 2)?,
                    album: opt_text(r, 3)?,
                    duration_secs: opt_int(r, 4)?
                        .map(|d| u32_of(d, "duration_secs"))
                        .transpose()?,
                    uri: None,
                },
                is_classical: int(r, 5)? != 0,
                added: int(r, 6)?,
            })
        })
        .collect()
    }

    fn save_render_params(
        &mut self,
        household: &str,
        params: &SpotifyRenderParams,
        learned_at: i64,
    ) -> Result<(), StoreError> {
        self.execute(
            "INSERT OR REPLACE INTO render_params \
             (household, sid, flags, sn, cdudn, item_id_prefix, learned_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            &[
                household.into(),
                i64::from(params.sid).into(),
                i64::from(params.flags).into(),
                i64::from(params.sn).into(),
                params.cdudn.as_str().into(),
                params.item_id_prefix.as_str().into(),
                learned_at.into(),
            ],
        )
    }

    fn render_params(
        &self,
        household: &str,
    ) -> Result<Option<(SpotifyRenderParams, i64)>, StoreError> {
        let rows = self.query(
            "SELECT sid, flags, sn, cdudn, item_id_prefix, learned_at \
             FROM render_params WHERE household = ?1",
            &[household.into()],
        )?;
        rows.first()
            .map(|r| {
                Ok((
                    SpotifyRenderParams {
                        sid: u32_of(int(r, 0)?, "sid")?,
                        flags: u32_of(int(r, 1)?, "flags")?,
                        sn: u32_of(int(r, 2)?, "sn")?,
                        cdudn: text(r, 3)?,
                        item_id_prefix: text(r, 4)?,
                    },
                    int(r, 5)?,
                ))
            })
            .transpose()
    }

    fn save_auth(
        &mut self,
        service: &str,
        refresh_token: &str,
        expires: i64,
    ) -> Result<(), StoreError> {
        self.execute(
            "INSERT OR REPLACE INTO auth (service, refresh_token, expires) VALUES (?1, ?2, ?3)",
            &[service.into(), refresh_token.into(), expires.into()],
        )
    }

    fn auth(&self, service: &str) -> Result<Option<AuthEntry>, StoreError> {
        let rows = self.query(
            "SELECT refresh_token, expires FROM auth WHERE service = ?1",
            &[service.into()],
        )?;
        rows.first()
            .map(|r| {
                Ok(AuthEntry {
                    refresh_token: text(r, 0)?,
                    expires: int(r, 1)?,
                })
            })
            .transpose()
    }

    fn save_dj_session(&mut self, session: &DjSession) -> Result<(), StoreError> {
        self.execute(
            "INSERT OR REPLACE INTO dj_sessions (coordinator, mood, constraints, expires) \
             VALUES (?1, ?2, ?3, ?4)",
            &[
                session.coordinator.as_str().into(),
                opt_value(session.mood.as_deref()),
                opt_value(session.constraints.as_deref()),
                session.expires.into(),
            ],
        )
    }

    fn dj_session(&self, coordinator: &str) -> Result<Option<DjSession>, StoreError> {
        self.query(
            "SELECT coordinator, mood, constraints, expires FROM dj_sessions \
             WHERE coordinator = ?1",
            &[coordinator.into()],
        )?
        .first()
        .map(dj_session_row)
        .transpose()
    }

    fn dj_sessions(&self) -> Result<Vec<DjSession>, StoreError> {
        self.query(
            "SELECT coordinator, mood, constraints, expires FROM dj_sessions \
             ORDER BY coordinator",
            &[],
        )?
        .iter()
        .map(dj_session_row)
        .collect()
    }

    fn delete_dj_session(&mut self, coordinator: &str) -> Result<(), StoreError> {
        self.execute(
            "DELETE FROM dj_sessions WHERE coordinator = ?1",
            &[coordinator.into()],
        )
    }

    fn record_feedback(&mut self, feedback: &Feedback) -> Result<(), StoreError> {
        self.execute(
            "INSERT INTO feedback \
             (at, work_key, composer_key, performer, signal, artist_key, album_key) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            &[
                feedback.at.into(),
                opt_value(feedback.work_key.as_deref()),
                opt_value(feedback.composer_key.as_deref()),
                opt_value(feedback.performer.as_deref()),
                feedback.signal.into(),
                opt_value(feedback.artist_key.as_deref()),
                opt_value(feedback.album_key.as_deref()),
            ],
        )
    }

    fn feedback(
        &self,
        key: FeedbackKey<'_>,
        window: Range<i64>,
    ) -> Result<Vec<Feedback>, StoreError> {
        // The column names are fixed here, never caller text.
        let (sql, key) = match key {
            FeedbackKey::Work(k) => (
                "SELECT at, work_key, composer_key, performer, signal, artist_key, album_key \
                 FROM feedback \
                 WHERE work_key = ?1 AND at >= ?2 AND at < ?3 ORDER BY at, id",
                k,
            ),
            FeedbackKey::Composer(k) => (
                "SELECT at, work_key, composer_key, performer, signal, artist_key, album_key \
                 FROM feedback \
                 WHERE composer_key = ?1 AND at >= ?2 AND at < ?3 ORDER BY at, id",
                k,
            ),
            FeedbackKey::Performer(k) => (
                "SELECT at, work_key, composer_key, performer, signal, artist_key, album_key \
                 FROM feedback \
                 WHERE performer = ?1 AND at >= ?2 AND at < ?3 ORDER BY at, id",
                k,
            ),
            FeedbackKey::Artist(k) => (
                "SELECT at, work_key, composer_key, performer, signal, artist_key, album_key \
                 FROM feedback \
                 WHERE artist_key = ?1 AND at >= ?2 AND at < ?3 ORDER BY at, id",
                k,
            ),
            FeedbackKey::Album(k) => (
                "SELECT at, work_key, composer_key, performer, signal, artist_key, album_key \
                 FROM feedback \
                 WHERE album_key = ?1 AND at >= ?2 AND at < ?3 ORDER BY at, id",
                k,
            ),
        };
        self.query(sql, &[key.into(), window.start.into(), window.end.into()])?
            .iter()
            .map(feedback_row)
            .collect()
    }

    fn feedback_between(&self, window: Range<i64>) -> Result<Vec<Feedback>, StoreError> {
        self.query(
            "SELECT at, work_key, composer_key, performer, signal, artist_key, album_key \
                 FROM feedback \
             WHERE at >= ?1 AND at < ?2 ORDER BY at, id",
            &[window.start.into(), window.end.into()],
        )?
        .iter()
        .map(feedback_row)
        .collect()
    }

    fn save_album_tracks(
        &mut self,
        album_uri: &str,
        tracks: &[AlbumTrack],
        fetched_at: i64,
    ) -> Result<(), StoreError> {
        let rows: Vec<Vec<SqliteValue>> = tracks
            .iter()
            .map(|t| {
                vec![
                    album_uri.into(),
                    i64::from(t.disc_number).into(),
                    i64::from(t.track_number).into(),
                    t.source_uri.as_str().into(),
                    t.title.as_str().into(),
                    opt_value(t.duration_secs.map(i64::from)),
                    fetched_at.into(),
                ]
            })
            .collect();
        self.in_transaction(|c| {
            c.execute_with_params_sync(
                "DELETE FROM album_tracks WHERE album_uri = ?1",
                &[album_uri.into()],
            )?;
            if rows.is_empty() {
                return Ok(());
            }
            c.execute_many_with_params_in_transaction_sync(
                "INSERT OR REPLACE INTO album_tracks \
                 (album_uri, disc_number, track_number, source_uri, title, duration_secs, \
                  fetched_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                &rows,
            )
            .map(drop)
        })
    }

    fn album_tracks(&self, album_uri: &str) -> Result<Option<CachedAlbum>, StoreError> {
        let rows = self.query(
            "SELECT disc_number, track_number, source_uri, title, duration_secs, fetched_at \
             FROM album_tracks WHERE album_uri = ?1 ORDER BY disc_number, track_number",
            &[album_uri.into()],
        )?;
        let Some(first) = rows.first() else {
            return Ok(None);
        };
        let fetched_at = int(first, 5)?;
        let tracks = rows
            .iter()
            .map(|r| {
                Ok(AlbumTrack {
                    disc_number: u32_of(int(r, 0)?, "disc_number")?,
                    track_number: u32_of(int(r, 1)?, "track_number")?,
                    source_uri: text(r, 2)?,
                    title: text(r, 3)?,
                    duration_secs: opt_int(r, 4)?
                        .map(|d| u32_of(d, "duration_secs"))
                        .transpose()?,
                })
            })
            .collect::<Result<_, StoreError>>()?;
        Ok(Some(CachedAlbum { tracks, fetched_at }))
    }

    fn save_artist_genres(
        &mut self,
        artist_id: &str,
        genres: &[String],
        fetched_at: i64,
    ) -> Result<(), StoreError> {
        self.execute(
            "INSERT OR REPLACE INTO artist_genres (artist_id, genres, fetched_at) \
             VALUES (?1, ?2, ?3)",
            &[
                artist_id.into(),
                opt_value((!genres.is_empty()).then(|| genres.join(GENRE_SEPARATOR))),
                fetched_at.into(),
            ],
        )
    }

    fn artist_genres(&self, artist_id: &str) -> Result<Option<(Vec<String>, i64)>, StoreError> {
        let rows = self.query(
            "SELECT genres, fetched_at FROM artist_genres WHERE artist_id = ?1",
            &[artist_id.into()],
        )?;
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        Ok(Some((split_genres(opt_text(row, 0)?), int(row, 1)?)))
    }

    fn record_action(&mut self, action: &Action) -> Result<i64, StoreError> {
        self.in_transaction(|c| {
            c.execute_with_params_sync(
                "INSERT INTO actions \
                 (at, client, surface, intent, decision, result, before_state, undo_of) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                &[
                    action.at.into(),
                    action.client.as_str().into(),
                    action.surface.as_str().into(),
                    action.intent.as_str().into(),
                    action.decision.as_str().into(),
                    action.result.as_str().into(),
                    opt_value(action.before_state.as_deref()),
                    opt_value(action.undo_of),
                ],
            )?;
            c.last_insert_rowid_sync()
        })
    }

    fn recent_actions(&self, filter: &ActionFilter) -> Result<Vec<LoggedAction>, StoreError> {
        let limit = if filter.limit == 0 {
            i64::MAX
        } else {
            i64::try_from(filter.limit).unwrap_or(i64::MAX)
        };
        let rows = match &filter.client {
            Some(client) => self.query(
                &format!(
                    "SELECT {ACTION_COLUMNS} FROM actions WHERE client = ?1 AND at >= ?2 \
                     ORDER BY id DESC LIMIT ?3"
                ),
                &[
                    client.as_str().into(),
                    filter.since.unwrap_or(i64::MIN).into(),
                    limit.into(),
                ],
            )?,
            None => self.query(
                &format!(
                    "SELECT {ACTION_COLUMNS} FROM actions WHERE at >= ?1 \
                     ORDER BY id DESC LIMIT ?2"
                ),
                &[filter.since.unwrap_or(i64::MIN).into(), limit.into()],
            )?,
        };
        rows.iter().map(action_row).collect()
    }

    fn last_undoable_action(
        &self,
        client: Option<&str>,
    ) -> Result<Option<LoggedAction>, StoreError> {
        let undone: Vec<i64> = self
            .query("SELECT undo_of FROM actions WHERE undo_of IS NOT NULL", &[])?
            .iter()
            .map(|r| int(r, 0))
            .collect::<Result<_, _>>()?;
        let candidates = match client {
            Some(c) => self.query(
                &format!(
                    "SELECT {ACTION_COLUMNS} FROM actions WHERE before_state IS NOT NULL \
                     AND undo_of IS NULL AND client = ?1 ORDER BY id DESC"
                ),
                &[c.into()],
            )?,
            None => self.query(
                &format!(
                    "SELECT {ACTION_COLUMNS} FROM actions WHERE before_state IS NOT NULL \
                     AND undo_of IS NULL ORDER BY id DESC"
                ),
                &[],
            )?,
        };
        for r in &candidates {
            let action = action_row(r)?;
            if !undone.contains(&action.id) {
                return Ok(Some(action));
            }
        }
        Ok(None)
    }

    fn prune_actions(&mut self, keep: usize, older_than: i64) -> Result<usize, StoreError> {
        // The newest id that falls outside the `keep` newest, if any.
        let cutoff = self
            .query(
                "SELECT id FROM actions ORDER BY id DESC LIMIT 1 OFFSET ?1",
                &[limit_value(keep)],
            )?
            .first()
            .map(|r| int(r, 0))
            .transpose()?;
        self.in_transaction(|c| {
            let mut gone = c.execute_with_params_sync(
                "DELETE FROM actions WHERE at < ?1",
                &[older_than.into()],
            )?;
            if let Some(id) = cutoff {
                gone +=
                    c.execute_with_params_sync("DELETE FROM actions WHERE id <= ?1", &[id.into()])?;
            }
            Ok(gone)
        })
    }

    fn save_scene(&mut self, scene: &StoredScene) -> Result<(), StoreError> {
        self.execute(
            "INSERT OR REPLACE INTO scenes (name, spec, updated) VALUES (?1, ?2, ?3)",
            &[
                scene.name.as_str().into(),
                scene.spec.as_str().into(),
                scene.updated.into(),
            ],
        )
    }

    fn scene(&self, name: &str) -> Result<Option<StoredScene>, StoreError> {
        self.query(
            "SELECT name, spec, updated FROM scenes WHERE name = ?1",
            &[name.into()],
        )?
        .first()
        .map(scene_row)
        .transpose()
    }

    fn scenes(&self) -> Result<Vec<StoredScene>, StoreError> {
        self.query("SELECT name, spec, updated FROM scenes ORDER BY name", &[])?
            .iter()
            .map(scene_row)
            .collect()
    }

    fn delete_scene(&mut self, name: &str) -> Result<bool, StoreError> {
        let gone = self.in_transaction(|c| {
            c.execute_with_params_sync("DELETE FROM scenes WHERE name = ?1", &[name.into()])
        })?;
        Ok(gone > 0)
    }

    fn add_schedule(&mut self, schedule: &StoredSchedule) -> Result<i64, StoreError> {
        self.in_transaction(|c| {
            c.execute_with_params_sync(
                "INSERT INTO schedules (spec, action, creator, enabled, created, last_fired) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                &[
                    schedule.spec.as_str().into(),
                    schedule.action.as_str().into(),
                    schedule.creator.as_str().into(),
                    i64::from(schedule.enabled).into(),
                    schedule.created.into(),
                    opt_value(schedule.last_fired),
                ],
            )?;
            c.last_insert_rowid_sync()
        })
    }

    fn schedules(&self) -> Result<Vec<StoredSchedule>, StoreError> {
        self.query(
            "SELECT id, spec, action, creator, enabled, created, last_fired FROM schedules \
             ORDER BY id",
            &[],
        )?
        .iter()
        .map(schedule_row)
        .collect()
    }

    fn set_schedule_enabled(&mut self, id: i64, enabled: bool) -> Result<bool, StoreError> {
        let changed = self.in_transaction(|c| {
            c.execute_with_params_sync(
                "UPDATE schedules SET enabled = ?2 WHERE id = ?1",
                &[id.into(), i64::from(enabled).into()],
            )
        })?;
        Ok(changed > 0)
    }

    fn mark_schedule_fired(&mut self, id: i64, at: i64) -> Result<bool, StoreError> {
        let claimed = self.in_transaction(|c| {
            c.execute_with_params_sync(
                "UPDATE schedules SET last_fired = ?2 \
                 WHERE id = ?1 AND (last_fired IS NULL OR last_fired < ?2)",
                &[id.into(), at.into()],
            )
        })?;
        Ok(claimed > 0)
    }

    fn delete_schedule(&mut self, id: i64) -> Result<bool, StoreError> {
        let gone = self.in_transaction(|c| {
            c.execute_with_params_sync("DELETE FROM schedules WHERE id = ?1", &[id.into()])
        })?;
        Ok(gone > 0)
    }
}

fn schedule_row(r: &Row) -> Result<StoredSchedule, StoreError> {
    Ok(StoredSchedule {
        id: int(r, 0)?,
        spec: text(r, 1)?,
        action: text(r, 2)?,
        creator: text(r, 3)?,
        enabled: int(r, 4)? != 0,
        created: int(r, 5)?,
        last_fired: opt_int(r, 6)?,
    })
}

fn scene_row(r: &Row) -> Result<StoredScene, StoreError> {
    Ok(StoredScene {
        name: text(r, 0)?,
        spec: text(r, 1)?,
        updated: int(r, 2)?,
    })
}

const ACTION_COLUMNS: &str =
    "id, at, client, surface, intent, decision, result, before_state, undo_of";

fn action_row(r: &Row) -> Result<LoggedAction, StoreError> {
    Ok(LoggedAction {
        id: int(r, 0)?,
        action: Action {
            at: int(r, 1)?,
            client: text(r, 2)?,
            surface: text(r, 3)?,
            intent: text(r, 4)?,
            decision: text(r, 5)?,
            result: text(r, 6)?,
            before_state: opt_text(r, 7)?,
            undo_of: opt_int(r, 8)?,
        },
    })
}

fn feedback_row(r: &Row) -> Result<Feedback, StoreError> {
    Ok(Feedback {
        at: int(r, 0)?,
        work_key: opt_text(r, 1)?,
        composer_key: opt_text(r, 2)?,
        performer: opt_text(r, 3)?,
        signal: int(r, 4)?,
        artist_key: opt_text(r, 5)?,
        album_key: opt_text(r, 6)?,
    })
}

fn dj_session_row(r: &Row) -> Result<DjSession, StoreError> {
    Ok(DjSession {
        coordinator: text(r, 0)?,
        mood: opt_text(r, 1)?,
        constraints: opt_text(r, 2)?,
        expires: int(r, 3)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v1_database_upgrades_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fsonos.db").to_string_lossy().into_owned();

        // A database created before migration 2, with a library row in it.
        let v1 = SqliteStore::open_migrated(path.clone(), &MIGRATIONS[..1]).unwrap();
        assert_eq!(v1.schema_versions().unwrap(), [1]);
        v1.execute(
            "INSERT INTO spotify_library \
             (source_uri, title, artist, album, duration_secs, is_classical, added) \
             VALUES (?1, ?2, NULL, NULL, NULL, 1, 7)",
            &["spotify:track:old".into(), "Old".into()],
        )
        .unwrap();
        v1.close().unwrap();

        let store = SqliteStore::open(Path::new(&path)).unwrap();
        assert_eq!(store.schema_versions().unwrap(), [1, 2, 3, 4, 5, 6, 7, 8]);
        let lib = store.library().unwrap();
        assert_eq!(lib.len(), 1);
        assert_eq!(lib[0].track.source_uri, "spotify:track:old");
        assert_eq!(lib[0].origin, LibraryOrigin::SavedAlbum);
        assert_eq!(
            (
                lib[0].album_uri.as_deref(),
                lib[0].disc_number,
                lib[0].track_number
            ),
            (None, None, None)
        );
        store.close().unwrap();
    }

    #[test]
    fn v2_database_upgrades_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fsonos.db").to_string_lossy().into_owned();

        // A database created before migration 3, with a v2 library row.
        let v2 = SqliteStore::open_migrated(path.clone(), &MIGRATIONS[..2]).unwrap();
        assert_eq!(v2.schema_versions().unwrap(), [1, 2]);
        v2.execute(
            "INSERT INTO spotify_library \
             (source_uri, title, is_classical, added, album_uri, origin, disc_number, \
              track_number) VALUES (?1, ?2, 1, 7, ?3, 'liked_track', 1, 2)",
            &[
                "spotify:track:m2".into(),
                "II. Allegro".into(),
                "spotify:album:a".into(),
            ],
        )
        .unwrap();
        v2.close().unwrap();

        let mut store = SqliteStore::open(Path::new(&path)).unwrap();
        assert_eq!(store.schema_versions().unwrap(), [1, 2, 3, 4, 5, 6, 7, 8]);
        let lib = store.library().unwrap();
        assert_eq!(lib.len(), 1);
        assert_eq!(lib[0].origin, LibraryOrigin::LikedTrack);
        assert_eq!(
            (lib[0].track_number, lib[0].work_key.as_deref()),
            (Some(2), None)
        );
        // The new tables are usable at once.
        store
            .save_dj_session(&DjSession {
                coordinator: "RINCON_A".into(),
                mood: None,
                constraints: None,
                expires: 9,
            })
            .unwrap();
        assert_eq!(store.dj_sessions().unwrap().len(), 1);
        store.close().unwrap();
    }

    #[test]
    fn v6_database_upgrades_to_genre_agnostic_candidacy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fsonos.db").to_string_lossy().into_owned();

        // Before migration 7: a classical candidate, a song that wasn't a
        // candidate then, and a feedback row.
        let v6 = SqliteStore::open_migrated(path.clone(), &MIGRATIONS[..6]).unwrap();
        v6.execute(
            "INSERT INTO spotify_library (source_uri, title, is_classical, added, work_key) \
             VALUES (?1, ?2, 1, 7, ?3)",
            &[
                "spotify:track:aria".into(),
                "Aria".into(),
                "johann sebastian bach|aria".into(),
            ],
        )
        .unwrap();
        v6.execute(
            "INSERT INTO spotify_library (source_uri, title, is_classical, added) \
             VALUES (?1, ?2, 0, 8)",
            &["spotify:track:song".into(), "Blue Hours".into()],
        )
        .unwrap();
        v6.execute(
            "INSERT INTO feedback (at, work_key, composer_key, performer, signal) \
             VALUES (5, 'johann sebastian bach|aria', 'johann sebastian bach', NULL, 3)",
            &[],
        )
        .unwrap();
        v6.close().unwrap();

        let mut store = SqliteStore::open(Path::new(&path)).unwrap();
        assert_eq!(store.schema_versions().unwrap(), [1, 2, 3, 4, 5, 6, 7, 8]);
        let lib = store.library().unwrap();
        assert_eq!(lib.len(), 2, "no row lost");
        // Legacy rows keep their candidacy: is_classical decides.
        assert_eq!((lib[0].candidate, lib[0].is_candidate()), (None, true));
        assert_eq!((lib[1].candidate, lib[1].is_candidate()), (None, false));
        assert!(lib.iter().all(|e| e.genres.is_empty()
            && e.release_year.is_none()
            && !e.explicit
            && e.taste_weight.is_none()));
        let old = store.feedback_between(0..10).unwrap();
        assert_eq!(old.len(), 1);
        assert_eq!(
            old[0].composer_key.as_deref(),
            Some("johann sebastian bach")
        );
        assert_eq!(
            (old[0].artist_key.as_deref(), old[0].album_key.as_deref()),
            (None, None)
        );

        // A sync rewrites the song as a candidate, with the new columns.
        let song = LibraryEntry {
            track: Track {
                title: "Blue Hours".into(),
                artist: Some("Nina Marsh Quartet".into()),
                album: Some("Blue Hours".into()),
                source_uri: "spotify:track:song".into(),
                uri: None,
                duration_secs: Some(200),
            },
            added: 8,
            work_key: Some("nina marsh quartet|blue hours".into()),
            genres: vec!["jazz".into(), "cool jazz".into()],
            release_year: Some(1962),
            explicit: true,
            candidate: Some(true),
            taste_weight: Some(1300),
            ..LibraryEntry::default()
        };
        store.upsert_library(std::slice::from_ref(&song)).unwrap();
        let lib = store.library().unwrap();
        assert_eq!(lib[1], song);
        assert!(lib[1].is_candidate() && !lib[1].is_classical);
        store
            .record_feedback(&Feedback {
                at: 6,
                artist_key: Some("nina marsh quartet".into()),
                album_key: Some("spotify:album:blue".into()),
                signal: 3,
                ..Feedback::default()
            })
            .unwrap();
        assert_eq!(
            store
                .feedback(FeedbackKey::Artist("nina marsh quartet"), 0..10)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(store.feedback_between(0..10).unwrap().len(), 2);
        store.close().unwrap();
    }

    #[test]
    fn migration_versions_are_unique_and_ascending() {
        let versions: Vec<i64> = MIGRATIONS.iter().map(|m| m.version).collect();
        assert!(versions.windows(2).all(|w| w[0] < w[1]), "{versions:?}");
        assert_eq!(versions[0], 1);
    }
}
