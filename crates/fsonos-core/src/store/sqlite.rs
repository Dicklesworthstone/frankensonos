//! The fsqlite-backed [`Store`].
//!
//! Uses fsqlite's `AsyncConnection`: a `Send` handle over a dedicated
//! large-stack worker that owns the `!Send` engine connection, whose `*_sync`
//! methods fit the synchronous [`Store`] trait (see AGENTS.md, "Dependency
//! Recipe"). Writes that touch several rows run in one short transaction;
//! callers never hold a transaction across network I/O because none escapes a
//! method call.

use super::{
    Action, ActionFilter, AlbumTrack, ArtistGenresBatch, AuthEntry, CachedAlbum, CachedPlayer,
    CachedPlaylist, DjSession, Feedback, FeedbackKey, LibraryEntry, LibraryOrigin, LoggedAction,
    PlayName, PlayRecord, SpotifyArtistDiscoveryFence, SpotifyLibraryOwner,
    SpotifyPlaylistPublication, SpotifyPlaylistPublicationFence, SpotifyPlaylistPublicationItem,
    Store, StoreError, StoredScene, StoredSchedule,
};
use fsonos_proto::didl::SpotifyRenderParams;
use fsonos_types::{Generation, Player, PlayerId, Track, ZoneGroup};
use fsqlite::{AsyncConnection, FrankenError, Row, SqliteValue};
use std::fmt::Display;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
    Migration {
        version: 9,
        name: "albums whose track lists couldn't be read, and when to try again",
        sql: "
        CREATE TABLE album_misses (album_uri TEXT PRIMARY KEY, retry_at INTEGER NOT NULL);
    ",
    },
    Migration {
        version: 10,
        name: "the playlists in the owner's Spotify list, by name, for search",
        sql: "
        CREATE TABLE spotify_playlists (
            playlist_uri TEXT PRIMARY KEY, name TEXT NOT NULL, position INTEGER NOT NULL);
    ",
    },
    Migration {
        version: 11,
        name: "what the owner played on Spotify lately, so the DJ counts it as heard",
        sql: "
        CREATE TABLE spotify_heard (source_uri TEXT PRIMARY KEY, played_at INTEGER NOT NULL);
    ",
    },
    Migration {
        version: 12,
        name: "the genres a piece of feedback carries, so the DJ nudges them (plan §12.9)",
        sql: "
        ALTER TABLE feedback ADD COLUMN genres TEXT;
    ",
    },
    Migration {
        version: 13,
        name: "what the players called what they played, so the history can name it",
        sql: "
        CREATE TABLE play_names (
            source_uri TEXT PRIMARY KEY, title TEXT NOT NULL, artist TEXT,
            named_at INTEGER NOT NULL);
    ",
    },
    Migration {
        version: 14,
        name: "ordered Spotify playlist observations with explicit read coverage",
        sql: "
        CREATE TABLE spotify_playlist_contents (
            playlist_uri TEXT PRIMARY KEY, observation TEXT NOT NULL);
    ",
    },
    Migration {
        version: 15,
        name: "bounded Spotify playlist scans resume after their last attempted ID",
        sql: "
        CREATE TABLE spotify_playlist_scans (
            owner_id TEXT PRIMARY KEY, playlist_id TEXT NOT NULL);
    ",
    },
    Migration {
        version: 16,
        name: "snapshot-bound Spotify playlist reads continue across syncs",
        sql: "
        CREATE TABLE spotify_playlist_progress (
            owner_id TEXT NOT NULL, playlist_uri TEXT NOT NULL, progress TEXT NOT NULL,
            PRIMARY KEY (owner_id, playlist_uri));
    ",
    },
    Migration {
        version: 17,
        name: "the verified owner of the Spotify library and personal taste cache",
        sql: "
        CREATE TABLE spotify_library_owner (
            singleton INTEGER PRIMARY KEY, owner_id TEXT NOT NULL,
            generation INTEGER NOT NULL);
    ",
    },
    Migration {
        version: 18,
        name: "explicit-content metadata for expanded album movements",
        sql: "
        ALTER TABLE album_tracks ADD COLUMN explicit INTEGER;
    ",
    },
    Migration {
        version: 19,
        name: "bounded Spotify playlist directories resume across restarts",
        sql: "
        CREATE TABLE spotify_playlist_directories (
            owner_id TEXT PRIMARY KEY, generation INTEGER NOT NULL,
            progress TEXT NOT NULL);
    ",
    },
    Migration {
        version: 20,
        name: "lead artist identity for resumable Spotify genre enrichment",
        sql: "
        ALTER TABLE spotify_library ADD COLUMN artist_id TEXT;
    ",
    },
    Migration {
        version: 21,
        name: "essential Spotify library reads resume before account adoption",
        sql: "
        CREATE TABLE spotify_library_progress (
            incoming_id TEXT PRIMARY KEY, base_owner_id TEXT,
            base_generation INTEGER, progress TEXT NOT NULL);
    ",
    },
    Migration {
        version: 22,
        name: "bounded followed-artist directories resume across restarts",
        sql: "
        CREATE TABLE spotify_followed_directories (
            owner_id TEXT PRIMARY KEY, generation INTEGER NOT NULL,
            progress TEXT NOT NULL);
    ",
    },
    Migration {
        version: 23,
        name: "fair artist discovery advances with accepted library publications",
        sql: "
        CREATE TABLE spotify_artist_discovery (
            owner_id TEXT PRIMARY KEY, generation INTEGER NOT NULL,
            progress TEXT NOT NULL);
    ",
    },
    Migration {
        version: 24,
        name: "monotonic public catalog revisions for reusable library browsing",
        sql: "
        CREATE TABLE library_catalog_clock (
            singleton INTEGER PRIMARY KEY, revision INTEGER);
        INSERT INTO library_catalog_clock (singleton, revision) VALUES (1, 0);
    ",
    },
];

/// How `spotify_library.genres` and `artist_genres.genres` join tags.
const GENRE_SEPARATOR: &str = "; ";

fn split_genres(joined: Option<String>) -> Vec<String> {
    joined.map_or_default(|g| {
        g.split(GENRE_SEPARATOR)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .collect()
    })
}

/// Read bounded artist chunks while isolating optional cache failures.
fn artist_genres_batch(
    artist_ids: &[&str],
    mut query: impl FnMut(&str, &[SqliteValue]) -> Result<Vec<Row>, StoreError>,
) -> ArtistGenresBatch {
    let mut ids = artist_ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    let mut out = ArtistGenresBatch::default();
    for chunk in ids.chunks(400) {
        let placeholders = (1..=chunk.len())
            .map(|index| format!("?{index}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT artist_id, genres, fetched_at FROM artist_genres \
             WHERE artist_id IN ({placeholders})"
        );
        let params = chunk.iter().map(|&id| id.into()).collect::<Vec<_>>();
        // Keep the identity decode fallible for the whole chunk: without
        // it we cannot safely assign either tags or a failure to a row.
        let rows = query(&sql, &params).and_then(|rows| {
            rows.into_iter()
                .map(|row| Ok((text(&row, 0)?, row)))
                .collect::<Result<Vec<_>, StoreError>>()
        });
        match rows {
            Ok(rows) => {
                for (id, row) in rows {
                    let cached =
                        opt_text(&row, 1).and_then(|tags| Ok((split_genres(tags), int(&row, 2)?)));
                    match cached {
                        Ok(cached) => {
                            out.cached.insert(id, cached);
                        }
                        Err(error) => {
                            out.failed.insert(id, error.to_string());
                        }
                    }
                }
            }
            Err(error) => {
                let message = error.to_string();
                out.failed
                    .extend(chunk.iter().map(|&id| (id.to_owned(), message.clone())));
            }
        }
    }
    out
}

/// The durable store: one fsqlite database file in the daemon's data dir.
#[derive(Debug)]
pub struct SqliteStore {
    conn: AsyncConnection,
    /// The database's WAL file ([`Self::wrote`]); `None` for an in-memory
    /// database, which has none.
    wal: Option<PathBuf>,
    /// Plays recorded through this handle, for [`PRUNE_EVERY`].
    plays: u32,
    /// This store in [`HELD`] while it is open (a file, not `:memory:`).
    held: Option<Held>,
}

/// The database files this process has open as a [`SqliteStore`], each with
/// its schema versions ([`SqliteStore::held`]). A second connection to a file
/// the process already holds is one fsqlite cannot close: it keeps the
/// descriptors while any connection holds a lock on the file (closing one
/// would drop the process' locks). So a check that only asks whether the
/// store opens asks here first.
static HELD: Mutex<Vec<(PathBuf, Vec<i64>)>> = Mutex::new(Vec::new());

/// A store's entry in [`HELD`], gone when the store closes or drops.
#[derive(Debug)]
struct Held(PathBuf);

impl Drop for Held {
    fn drop(&mut self) {
        let mut held = HELD.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(i) = held.iter().position(|(path, _)| *path == self.0) {
            held.swap_remove(i);
        }
    }
}

/// `path` as [`HELD`] keys it: canonical when it exists.
fn held_key(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// How far back plays and feedback are kept, before the newest of each: a
/// year. Feedback halves every 30 days, so a year-old signal weighs 1/4096,
/// and the DJ reads plays by count (its newest 2,000), never by age. The
/// rest is pruned when a store opens and after every [`PRUNE_EVERY`]th
/// play, so a daemon that runs for months keeps a bounded store.
pub const LISTENING_RETENTION_SECS: i64 = 365 * 86_400;

/// Plays a store records between two prunes.
const PRUNE_EVERY: u32 = 512;

/// The WAL size past which the store folds the WAL into the database and
/// truncates it. fsqlite's automatic checkpoints never shrink the WAL file,
/// so a daemon's months of small writes grew it without bound (about 1 MB an
/// hour on a live house). It is the file's size that counts, whoever wrote
/// it: a daemon keeps several stores open (one per surface, and the
/// remembered players), and a count of each one's own writes left a 16 MB
/// WAL a restart inherited in place for hours.
const WAL_LIMIT: u64 = 1 << 20;

fn backend(e: impl Display) -> StoreError {
    StoreError::Backend(e.to_string())
}

/// An engine error, noted first when it is a storage fault (a full disk,
/// corruption, I/O) that no retry fixes ([`super::faults`]).
fn engine(e: FrankenError) -> StoreError {
    super::faults::note(&e);
    backend(e)
}

/// Attempts at an operation that meets a transient conflict (busy, snapshot
/// conflict, lock) before the conflict is reported. Other connections share
/// the database file: the daemon's own (known players, DJ sync) and other
/// fsonos processes (`fsonos mcp`, CLI direct mode).
const BUSY_ATTEMPTS: u32 = 8;

/// Run `op` again after a transient conflict, backing off from 5 ms and
/// doubling (with jitter, so two waiting connections do not collide again),
/// 0.64 to 0.95 s in all. Other errors return at once.
fn retrying<T>(mut op: impl FnMut() -> Result<T, FrankenError>) -> Result<T, FrankenError> {
    let mut attempt = 0;
    loop {
        match op() {
            Err(e) if e.is_transient() && attempt + 1 < BUSY_ATTEMPTS => {
                let base = 5_u64 << attempt;
                let jitter = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| u64::from(d.subsec_nanos()))
                    % (base / 2 + 1);
                std::thread::sleep(Duration::from_millis(base + jitter));
                attempt += 1;
            }
            done => return done,
        }
    }
}

impl SqliteStore {
    /// Open (creating if needed) the database at `path` and bring its schema
    /// up to date.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        Self::open_at(&path.to_string_lossy())
    }

    /// The schema versions of the store at `path` when this process already
    /// has it open (the daemon, `fsonos mcp`); `None` when it does not.
    #[must_use]
    pub fn held(path: &Path) -> Option<Vec<i64>> {
        let key = held_key(path);
        let held = HELD.lock().unwrap_or_else(PoisonError::into_inner);
        held.iter()
            .find(|(path, _)| *path == key)
            .map(|(_, versions)| versions.clone())
    }

    /// A private, non-persistent database (tests, `--no-store` runs).
    pub fn open_in_memory() -> Result<Self, StoreError> {
        Self::open_at(":memory:")
    }

    fn open_at(path: &str) -> Result<Self, StoreError> {
        Self::open_migrated(path, MIGRATIONS)
    }

    fn open_migrated(path: &str, migrations: &[Migration]) -> Result<Self, StoreError> {
        let mut store = Self {
            conn: retrying(|| AsyncConnection::open_sync(path)).map_err(engine)?,
            wal: (path != ":memory:").then(|| PathBuf::from(format!("{path}-wal"))),
            plays: 0,
            held: None,
        };
        store.migrate(migrations)?;
        if path != ":memory:" {
            let key = held_key(Path::new(path));
            let versions = store.schema_versions()?;
            HELD.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((key.clone(), versions));
            store.held = Some(Held(key));
        }
        if let Err(e) = store.prune_listening() {
            tracing::warn!("old plays and feedback not pruned: {e}");
        }
        // A WAL an earlier process left large is trimmed now, not after this
        // store's own writes.
        store.wrote();
        Ok(store)
    }

    /// Versions of the migrations applied to this database, ascending.
    pub fn schema_versions(&self) -> Result<Vec<i64>, StoreError> {
        retrying(|| {
            self.conn
                .query_sync("SELECT version FROM schema_migrations ORDER BY version")
        })
        .map_err(engine)?
        .iter()
        .map(|row| int(row, 0))
        .collect()
    }

    /// Close the database, checkpointing its WAL. Dropping a store without
    /// closing is also safe: committed writes stay in the WAL and the next
    /// open recovers them.
    pub fn close(mut self) -> Result<(), StoreError> {
        self.conn.close_sync().map_err(|e| {
            // Close hands its error over shared.
            super::faults::note(&e);
            backend(e)
        })
    }

    fn migrate(&self, migrations: &[Migration]) -> Result<(), StoreError> {
        retrying(|| {
            self.conn.execute_sync(
                "CREATE TABLE IF NOT EXISTS schema_migrations (\
                 version INTEGER PRIMARY KEY, name TEXT NOT NULL, applied_at INTEGER NOT NULL)",
            )
        })
        .map_err(engine)?;
        let applied = self.schema_versions()?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
        for m in migrations.iter().filter(|m| !applied.contains(&m.version)) {
            self.apply(m, now)?;
        }
        Ok(())
    }

    /// Apply `m` unless another connection has since: two processes opening
    /// a new database at once both find it unmigrated. The check shares the
    /// schema change's transaction, and a change that fails because the
    /// other one won is not an error.
    fn apply(&self, m: &Migration, now: i64) -> Result<(), StoreError> {
        let applied = |c: &AsyncConnection| {
            c.query_with_params_sync(
                "SELECT 1 FROM schema_migrations WHERE version = ?1",
                &[m.version.into()],
            )
            .map(|rows| !rows.is_empty())
        };
        let changed = self.in_transaction(|c| {
            if applied(c)? {
                return Ok(());
            }
            c.execute_batch_sync(m.sql)?;
            c.execute_with_params_sync(
                "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
                &[m.version.into(), m.name.into(), now.into()],
            )
            .map(drop)
        });
        match changed {
            Err(_) if retrying(|| applied(&self.conn)).unwrap_or(false) => Ok(()),
            other => {
                other.map_err(|e| backend(format!("migration {} ({}): {e}", m.version, m.name)))
            }
        }
    }

    /// Run `f` in one transaction, rolling back if it fails; a transient
    /// conflict runs it again in a new one (see [`retrying`]).
    fn in_transaction<T>(
        &self,
        mut f: impl FnMut(&AsyncConnection) -> Result<T, FrankenError>,
    ) -> Result<T, StoreError> {
        retrying(|| {
            self.conn.begin_transaction_sync()?;
            match f(&self.conn)
                .and_then(|value| self.conn.commit_transaction_sync().map(|()| value))
            {
                Ok(value) => Ok(value),
                Err(e) => {
                    // The original error is the one worth reporting; a failed
                    // commit leaves nothing open for the next attempt either.
                    let _ = self.conn.rollback_transaction_sync();
                    Err(e)
                }
            }
        })
        .map_err(engine)
        .inspect(|_| self.wrote())
    }

    /// Forget plays, their names and feedback more than
    /// [`LISTENING_RETENTION_SECS`] older than the newest of each: measured
    /// from the newest record, not the clock, so a store kept under a
    /// simulated clock keeps what it was told. How many rows went.
    fn prune_listening(&self) -> Result<usize, StoreError> {
        let mut gone = 0;
        for (table, column) in [
            ("play_history", "played_at"),
            ("play_names", "named_at"),
            ("feedback", "at"),
        ] {
            let newest = self.query(&format!("SELECT MAX({column}) FROM {table}"), &[])?;
            let Some(newest) = newest.first().map(|r| opt_int(r, 0)).transpose()?.flatten() else {
                continue;
            };
            let cutoff = newest.saturating_sub(LISTENING_RETENTION_SECS);
            gone += self.in_transaction(|c| {
                c.execute_with_params_sync(
                    &format!("DELETE FROM {table} WHERE {column} < ?1"),
                    &[cutoff.into()],
                )
            })?;
        }
        Ok(gone)
    }

    /// After a write, and at open: once the WAL has grown past
    /// [`WAL_LIMIT`], fold it into the database and truncate it. A
    /// checkpoint that another connection holds up (a transaction open in
    /// `fsonos mcp`, say) answers busy, changes nothing, and is tried again
    /// after the next write.
    fn wrote(&self) {
        let Some(wal) = &self.wal else {
            return;
        };
        if std::fs::metadata(wal).map_or(0, |m| m.len()) <= WAL_LIMIT {
            return;
        }
        match self.conn.query_sync("PRAGMA wal_checkpoint(TRUNCATE)") {
            Ok(rows)
                if rows
                    .first()
                    .is_some_and(|r| int(r, 0).is_ok_and(|busy| busy != 0)) =>
            {
                tracing::debug!("WAL checkpoint deferred: another connection is busy");
            }
            Ok(_) => {}
            Err(e) => {
                super::faults::note(&e);
                tracing::warn!("WAL checkpoint failed: {e}");
            }
        }
    }

    fn query(&self, sql: &str, params: &[SqliteValue]) -> Result<Vec<Row>, StoreError> {
        retrying(|| self.conn.query_with_params_sync(sql, params)).map_err(engine)
    }

    fn execute(&self, sql: &str, params: &[SqliteValue]) -> Result<(), StoreError> {
        retrying(|| self.conn.execute_with_params_sync(sql, params))
            .map(drop)
            .map_err(engine)
            .inspect(|()| self.wrote())
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

/// Called only inside the transaction that changed a public catalog projection.
/// NULL is sticky: exhausting the counter disables reuse without failing writes.
fn bump_library_catalog_revision(connection: &AsyncConnection) -> Result<(), FrankenError> {
    connection
        .execute_sync(
            "UPDATE library_catalog_clock SET revision = \
             CASE WHEN revision < 9223372036854775807 THEN revision + 1 ELSE NULL END \
             WHERE singleton = 1",
        )
        .map(drop)
}

fn library_rows(entries: &[LibraryEntry]) -> Vec<Vec<SqliteValue>> {
    entries
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
                opt_value(e.artist_id.as_deref()),
            ]
        })
        .collect()
}

fn write_library(
    connection: &AsyncConnection,
    rows: &[Vec<SqliteValue>],
) -> Result<(), FrankenError> {
    if rows.is_empty() {
        return Ok(());
    }
    connection
        .execute_many_with_params_in_transaction_sync(
            "INSERT OR REPLACE INTO spotify_library \
             (source_uri, title, artist, album, duration_secs, is_classical, added, \
              album_uri, album_artists, origin, disc_number, track_number, work_key, \
              genres, release_year, explicit, candidate, taste_weight, artist_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, \
              ?16, ?17, ?18, ?19)",
            rows,
        )
        .map(drop)
}

fn playlist_rows(playlists: &[(String, String)]) -> Vec<Vec<SqliteValue>> {
    (0_i64..)
        .zip(playlists)
        .map(|(at, (uri, name))| vec![uri.as_str().into(), name.as_str().into(), at.into()])
        .collect()
}

fn heard_rows(plays: &[(String, i64)]) -> Vec<Vec<SqliteValue>> {
    super::latest_plays(plays)
        .iter()
        .map(|(uri, at)| vec![uri.as_str().into(), (*at).into()])
        .collect()
}

struct SpotifyLibraryProjection {
    library: Vec<Vec<SqliteValue>>,
    directory: Option<Vec<Vec<SqliteValue>>>,
    observations: Vec<Vec<SqliteValue>>,
    listening: Option<Vec<Vec<SqliteValue>>>,
}

fn prepare_spotify_library(
    entries: &[LibraryEntry],
    playlists: Option<&[(String, String)]>,
    contents: &[CachedPlaylist],
    heard: Option<&[(String, i64)]>,
) -> Result<SpotifyLibraryProjection, StoreError> {
    let observations = contents
        .iter()
        .map(|contents| {
            contents.validate()?;
            let encoded = serde_json::to_string(contents).map_err(backend)?;
            Ok(vec![contents.playlist_uri.as_str().into(), encoded.into()])
        })
        .collect::<Result<Vec<Vec<SqliteValue>>, StoreError>>()?;
    Ok(SpotifyLibraryProjection {
        library: library_rows(entries),
        directory: playlists.map(playlist_rows),
        observations,
        listening: heard.map(heard_rows),
    })
}

fn write_spotify_library(
    connection: &AsyncConnection,
    projection: &SpotifyLibraryProjection,
) -> Result<bool, FrankenError> {
    write_library(connection, &projection.library)?;
    let mut changed = !projection.library.is_empty() || !projection.observations.is_empty();
    if let Some(rows) = &projection.directory {
        changed |= connection.execute_sync("DELETE FROM spotify_playlists")? > 0;
        changed |= !rows.is_empty();
        if !rows.is_empty() {
            connection.execute_many_with_params_in_transaction_sync(
                "INSERT OR IGNORE INTO spotify_playlists (playlist_uri, name, position) \
                 VALUES (?1, ?2, ?3)",
                rows,
            )?;
        }
    }
    if !projection.observations.is_empty() {
        connection.execute_many_with_params_in_transaction_sync(
            "INSERT OR REPLACE INTO spotify_playlist_contents (playlist_uri, observation) \
             VALUES (?1, ?2)",
            &projection.observations,
        )?;
    }
    if let Some(rows) = &projection.listening {
        connection.execute_sync("DELETE FROM spotify_heard")?;
        if !rows.is_empty() {
            connection.execute_many_with_params_in_transaction_sync(
                "INSERT OR REPLACE INTO spotify_heard (source_uri, played_at) VALUES (?1, ?2)",
                rows,
            )?;
        }
    }
    Ok(changed)
}

fn read_artist_discovery(
    connection: &AsyncConnection,
    owner: &SpotifyLibraryOwner,
) -> Result<Option<String>, FrankenError> {
    let generation = i64::try_from(owner.generation)
        .map_err(|error| FrankenError::Internal(error.to_string()))?;
    connection
        .query_with_params_sync(
            "SELECT progress FROM spotify_artist_discovery WHERE owner_id = ?1 AND generation = ?2",
            &[owner.owner_id.as_str().into(), generation.into()],
        )?
        .first()
        .map(|row| text(row, 0).map_err(|error| FrankenError::Internal(error.to_string())))
        .transpose()
}

fn write_artist_discovery(
    connection: &AsyncConnection,
    owner: &SpotifyLibraryOwner,
    progress: &str,
) -> Result<(), FrankenError> {
    let generation = i64::try_from(owner.generation)
        .map_err(|error| FrankenError::Internal(error.to_string()))?;
    connection
        .execute_with_params_sync(
            "INSERT OR REPLACE INTO spotify_artist_discovery (owner_id, generation, progress) \
         VALUES (?1, ?2, ?3)",
            &[
                owner.owner_id.as_str().into(),
                generation.into(),
                progress.into(),
            ],
        )
        .map(drop)
}

fn read_playlist_progress(
    connection: &AsyncConnection,
    owner: &SpotifyLibraryOwner,
    playlist_uri: &str,
) -> Result<Option<String>, FrankenError> {
    connection
        .query_with_params_sync(
            "SELECT progress FROM spotify_playlist_progress WHERE owner_id = ?1 AND playlist_uri = ?2",
            &[owner.owner_id.as_str().into(), playlist_uri.into()],
        )?
        .first()
        .map(|row| text(row, 0).map_err(|error| FrankenError::Internal(error.to_string())))
        .transpose()
}

fn playlist_evidence_matches(
    connection: &AsyncConnection,
    owner: &SpotifyLibraryOwner,
    expected_directory: Option<&str>,
    observations: &[SpotifyPlaylistPublicationItem],
) -> Result<bool, FrankenError> {
    if read_spotify_owner(connection)?.as_ref() != Some(owner) {
        return Ok(false);
    }
    let generation = i64::try_from(owner.generation)
        .map_err(|error| FrankenError::Internal(error.to_string()))?;
    let rows = connection.query_with_params_sync(
        "SELECT progress FROM spotify_playlist_directories WHERE owner_id = ?1 AND generation = ?2",
        &[owner.owner_id.as_str().into(), generation.into()],
    )?;
    let directory = rows
        .first()
        .map(|row| text(row, 0).map_err(|error| FrankenError::Internal(error.to_string())))
        .transpose()?;
    if directory.as_deref() != expected_directory {
        return Ok(false);
    }
    for observation in observations {
        if read_playlist_progress(connection, owner, &observation.contents.playlist_uri)?.as_deref()
            != Some(observation.expected_progress.as_str())
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn write_playlist_evidence(
    connection: &AsyncConnection,
    owner: &SpotifyLibraryOwner,
    directory: Option<&str>,
    observations: &[SpotifyPlaylistPublicationItem],
) -> Result<(), FrankenError> {
    for observation in observations {
        connection.execute_with_params_sync(
            "UPDATE spotify_playlist_progress SET progress = ?3 WHERE owner_id = ?1 AND playlist_uri = ?2",
            &[
                owner.owner_id.as_str().into(),
                observation.contents.playlist_uri.as_str().into(),
                observation.applied_progress.as_str().into(),
            ],
        )?;
    }
    if let Some(directory) = directory {
        let generation = i64::try_from(owner.generation)
            .map_err(|error| FrankenError::Internal(error.to_string()))?;
        connection.execute_with_params_sync(
            "INSERT OR REPLACE INTO spotify_playlist_directories (owner_id, generation, progress) VALUES (?1, ?2, ?3)",
            &[owner.owner_id.as_str().into(), generation.into(), directory.into()],
        )?;
    }
    Ok(())
}

/// Read only affected URIs, inside the write transaction. Existing active rows
/// are never reconstructed from a caller's older snapshot. Chunking bounds SQL
/// parameters and the temporary row buffers even for long playlists.
fn merge_playlist_library(
    connection: &AsyncConnection,
    entries: &[LibraryEntry],
) -> Result<bool, FrankenError> {
    let mut changed = false;
    for chunk in entries.chunks(400) {
        let placeholders = (1..=chunk.len())
            .map(|index| format!("?{index}"))
            .collect::<Vec<_>>()
            .join(",");
        let values = chunk
            .iter()
            .map(|entry| entry.track.source_uri.as_str().into())
            .collect::<Vec<SqliteValue>>();
        let rows = connection.query_with_params_sync(
            &format!("SELECT source_uri, candidate, is_classical, taste_weight FROM spotify_library WHERE source_uri IN ({placeholders})"),
            &values,
        )?;
        let current = rows
            .iter()
            .map(|row| {
                Ok((
                    text(row, 0)?,
                    (
                        opt_int(row, 1)?.unwrap_or(int(row, 2)?) != 0,
                        opt_int(row, 3)?
                            .map(|weight| u32_of(weight, "taste_weight"))
                            .transpose()?,
                    ),
                ))
            })
            .collect::<Result<std::collections::BTreeMap<_, _>, StoreError>>()
            .map_err(|error| FrankenError::Internal(error.to_string()))?;
        let mut inserted = Vec::new();
        let mut weights = Vec::new();
        for entry in chunk {
            if let Some((true, weight)) = current.get(&entry.track.source_uri) {
                if let Some(stronger) = super::stronger_playlist_weight(*weight, entry.taste_weight)
                {
                    weights.push(vec![
                        entry.track.source_uri.as_str().into(),
                        i64::from(stronger).into(),
                    ]);
                }
            } else {
                inserted.extend(library_rows(std::slice::from_ref(entry)));
            }
        }
        changed |= !inserted.is_empty() || !weights.is_empty();
        write_library(connection, &inserted)?;
        if !weights.is_empty() {
            connection.execute_many_with_params_in_transaction_sync(
                "UPDATE spotify_library SET taste_weight = ?2 WHERE source_uri = ?1",
                &weights,
            )?;
        }
    }
    Ok(changed)
}

fn write_playlist_observations(
    connection: &AsyncConnection,
    observations: &[SpotifyPlaylistPublicationItem],
) -> Result<bool, FrankenError> {
    let mut changed = false;
    for observation in observations {
        let incoming = &observation.contents;
        if !incoming.complete {
            let rows = connection.query_with_params_sync(
                "SELECT observation FROM spotify_playlist_contents WHERE playlist_uri = ?1",
                &[incoming.playlist_uri.as_str().into()],
            )?;
            if let Some(row) = rows.first() {
                let encoded =
                    text(row, 0).map_err(|error| FrankenError::Internal(error.to_string()))?;
                let current: CachedPlaylist = serde_json::from_str(&encoded)
                    .map_err(|error| FrankenError::Internal(error.to_string()))?;
                current
                    .validate()
                    .map_err(|error| FrankenError::Internal(error.to_string()))?;
                if current.complete {
                    continue;
                }
            }
        }
        let encoded = serde_json::to_string(incoming)
            .map_err(|error| FrankenError::Internal(error.to_string()))?;
        connection.execute_with_params_sync(
            "INSERT OR REPLACE INTO spotify_playlist_contents (playlist_uri, observation) VALUES (?1, ?2)",
            &[incoming.playlist_uri.as_str().into(), encoded.into()],
        )?;
        changed = true;
    }
    Ok(changed)
}

fn read_spotify_owner(
    connection: &AsyncConnection,
) -> Result<Option<SpotifyLibraryOwner>, FrankenError> {
    let rows = connection.query_with_params_sync(
        "SELECT owner_id, generation FROM spotify_library_owner WHERE singleton = 1",
        &[],
    )?;
    rows.first()
        .map(|row| {
            let owner_id = text(row, 0)?;
            super::validate_spotify_owner(&owner_id)?;
            let generation = u64::try_from(int(row, 1)?)
                .ok()
                .filter(|value| *value > 0)
                .ok_or_else(|| backend("invalid Spotify library ownership generation"))?;
            Ok(SpotifyLibraryOwner {
                owner_id,
                generation,
            })
        })
        .transpose()
        .map_err(|error: StoreError| FrankenError::Internal(error.to_string()))
}

/// The caller checks the public receipt in the same transaction.
fn read_spotify_library_progress(
    connection: &AsyncConnection,
    incoming_id: &str,
    base: Option<&SpotifyLibraryOwner>,
) -> Result<Option<String>, FrankenError> {
    let generation = base
        .map(|owner| i64::try_from(owner.generation))
        .transpose()
        .map_err(|error| FrankenError::Internal(error.to_string()))?;
    let rows = connection.query_with_params_sync(
        "SELECT base_owner_id, base_generation, progress FROM spotify_library_progress \
         WHERE incoming_id = ?1",
        &[incoming_id.into()],
    )?;
    rows.first()
        .map(|row| {
            if opt_text(row, 0)?.as_deref() != base.map(|owner| owner.owner_id.as_str())
                || opt_int(row, 1)? != generation
            {
                return Ok(None);
            }
            let progress = text(row, 2)?;
            super::validate_spotify_library_progress(incoming_id, None, Some(&progress))?;
            Ok(Some(progress))
        })
        .transpose()
        .map(Option::flatten)
        .map_err(|error: StoreError| FrankenError::Internal(error.to_string()))
}

fn write_spotify_library_progress(
    connection: &AsyncConnection,
    incoming_id: &str,
    base: Option<&SpotifyLibraryOwner>,
    progress: &str,
) -> Result<(), FrankenError> {
    let generation = base
        .map(|owner| i64::try_from(owner.generation))
        .transpose()
        .map_err(|error| FrankenError::Internal(error.to_string()))?;
    connection
        .execute_with_params_sync(
            "INSERT OR REPLACE INTO spotify_library_progress \
             (incoming_id, base_owner_id, base_generation, progress) VALUES (?1, ?2, ?3, ?4)",
            &[
                incoming_id.into(),
                opt_value(base.map(|owner| owner.owner_id.as_str())),
                opt_value(generation),
                progress.into(),
            ],
        )
        .map(drop)
}

fn adopt_spotify_owner(
    connection: &AsyncConnection,
    owner_id: &str,
) -> Result<(SpotifyLibraryOwner, bool), FrankenError> {
    let previous = read_spotify_owner(connection)?;
    if let Some(current) = &previous
        && current.owner_id == owner_id
    {
        return Ok((current.clone(), false));
    }
    let generation = previous
        .as_ref()
        .map_or(0, |current| current.generation)
        .checked_add(1)
        .ok_or_else(|| {
            FrankenError::Internal("Spotify library ownership generation overflow".into())
        })?;
    let stored_generation = i64::try_from(generation).map_err(|_| {
        FrankenError::Internal("Spotify library ownership generation overflow".into())
    })?;
    connection.execute_sync("UPDATE spotify_library SET candidate = 0, taste_weight = NULL")?;
    connection.execute_sync("DELETE FROM spotify_playlists")?;
    connection.execute_sync("DELETE FROM spotify_playlist_contents")?;
    connection.execute_sync("DELETE FROM spotify_heard")?;
    // Returning to an account cannot treat discarded public contents
    // as still published merely because its private proof survived.
    connection.execute_with_params_sync(
        "DELETE FROM spotify_playlist_progress WHERE owner_id = ?1",
        &[owner_id.into()],
    )?;
    connection.execute_with_params_sync(
        "DELETE FROM spotify_playlist_directories WHERE owner_id = ?1",
        &[owner_id.into()],
    )?;
    connection.execute_with_params_sync(
        "DELETE FROM spotify_followed_directories WHERE owner_id = ?1",
        &[owner_id.into()],
    )?;
    connection.execute_with_params_sync(
        "DELETE FROM spotify_artist_discovery WHERE owner_id = ?1",
        &[owner_id.into()],
    )?;
    connection.execute_sync("DELETE FROM spotify_library_progress")?;
    connection.execute_with_params_sync(
        "INSERT OR REPLACE INTO spotify_library_owner (singleton, owner_id, generation) \
         VALUES (1, ?1, ?2)",
        &[owner_id.into(), stored_generation.into()],
    )?;
    bump_library_catalog_revision(connection)?;
    Ok((
        SpotifyLibraryOwner {
            owner_id: owner_id.to_owned(),
            generation,
        },
        true,
    ))
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
        )?;
        self.plays = self.plays.wrapping_add(1);
        if self.plays.is_multiple_of(PRUNE_EVERY)
            && let Err(e) = self.prune_listening()
        {
            tracing::warn!("old plays and feedback not pruned: {e}");
        }
        Ok(())
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

    fn name_play(&mut self, name: &PlayName, at: i64) -> Result<(), StoreError> {
        self.execute(
            "INSERT OR REPLACE INTO play_names (source_uri, title, artist, named_at) \
             VALUES (?1, ?2, ?3, ?4)",
            &[
                name.source_uri.as_str().into(),
                name.title.as_str().into(),
                opt_value(name.artist.as_deref()),
                at.into(),
            ],
        )
    }

    fn play_names(&self, uris: &[&str]) -> Result<Vec<PlayName>, StoreError> {
        let mut names = Vec::new();
        for uri in uris {
            for r in self.query(
                "SELECT title, artist FROM play_names WHERE source_uri = ?1",
                &[(*uri).into()],
            )? {
                names.push(PlayName {
                    source_uri: (*uri).to_string(),
                    title: text(&r, 0)?,
                    artist: opt_text(&r, 1)?,
                });
            }
        }
        Ok(names)
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
        let rows = library_rows(entries);
        if rows.is_empty() {
            return Ok(());
        }
        self.in_transaction(|connection| {
            write_library(connection, &rows)?;
            bump_library_catalog_revision(connection)
        })
    }

    fn library(&self) -> Result<Vec<LibraryEntry>, StoreError> {
        self.query(
            "SELECT source_uri, title, artist, album, duration_secs, is_classical, added, \
             album_uri, album_artists, origin, disc_number, track_number, work_key, \
             genres, release_year, explicit, candidate, taste_weight, artist_id \
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
                artist_id: opt_text(r, 18)?,
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

    fn library_catalog_revision(&self) -> Result<Option<u64>, StoreError> {
        let rows = self.query(
            "SELECT revision FROM library_catalog_clock WHERE singleton = 1",
            &[],
        )?;
        let Some(revision) = rows
            .first()
            .map(|row| opt_int(row, 0))
            .transpose()?
            .flatten()
        else {
            return Ok(None);
        };
        u64::try_from(revision)
            .map(Some)
            .map_err(|_| backend("the library catalog revision is negative"))
    }

    fn spotify_library_owner(&self) -> Result<Option<SpotifyLibraryOwner>, StoreError> {
        retrying(|| read_spotify_owner(&self.conn)).map_err(engine)
    }

    fn spotify_artist_discovery(
        &self,
        owner: &SpotifyLibraryOwner,
    ) -> Result<Option<String>, StoreError> {
        super::validate_spotify_artist_discovery(owner, None, None)?;
        let progress = self.in_transaction(|connection| {
            if read_spotify_owner(connection)?.as_ref() != Some(owner) {
                return Ok(None);
            }
            read_artist_discovery(connection, owner)
        })?;
        super::validate_spotify_artist_discovery(owner, None, progress.as_deref())?;
        Ok(progress)
    }

    fn spotify_library_progress(
        &self,
        incoming_id: &str,
        base: Option<&SpotifyLibraryOwner>,
    ) -> Result<Option<String>, StoreError> {
        super::validate_spotify_owner(incoming_id)?;
        self.in_transaction(|connection| {
            if read_spotify_owner(connection)?.as_ref() != base {
                return Ok(None);
            }
            read_spotify_library_progress(connection, incoming_id, base)
        })
    }

    fn save_spotify_library_progress(
        &mut self,
        incoming_id: &str,
        base: Option<&SpotifyLibraryOwner>,
        expected: Option<&str>,
        progress: &str,
    ) -> Result<bool, StoreError> {
        super::validate_spotify_library_progress(incoming_id, expected, Some(progress))?;
        self.in_transaction(|connection| {
            if read_spotify_owner(connection)?.as_ref() != base
                || read_spotify_library_progress(connection, incoming_id, base)?.as_deref()
                    != expected
            {
                return Ok(false);
            }
            write_spotify_library_progress(connection, incoming_id, base, progress)?;
            Ok(true)
        })
    }

    fn adopt_spotify_library_owner(
        &mut self,
        owner_id: &str,
    ) -> Result<(SpotifyLibraryOwner, bool), StoreError> {
        super::validate_spotify_owner(owner_id)?;
        self.in_transaction(|connection| adopt_spotify_owner(connection, owner_id))
    }

    fn adopt_spotify_library_from_progress(
        &mut self,
        incoming_id: &str,
        base: Option<&SpotifyLibraryOwner>,
        expected: &str,
    ) -> Result<Option<(SpotifyLibraryOwner, bool)>, StoreError> {
        super::validate_spotify_library_progress(incoming_id, Some(expected), None)?;
        self.in_transaction(|connection| {
            if read_spotify_owner(connection)?.as_ref() != base
                || read_spotify_library_progress(connection, incoming_id, base)?.as_deref()
                    != Some(expected)
            {
                return Ok(None);
            }
            let (owner, changed) = adopt_spotify_owner(connection, incoming_id)?;
            write_spotify_library_progress(connection, incoming_id, Some(&owner), expected)?;
            Ok(Some((owner, changed)))
        })
    }

    fn publish_spotify_library(
        &mut self,
        owner: &SpotifyLibraryOwner,
        entries: &[LibraryEntry],
        playlists: Option<&[(String, String)]>,
        contents: &[CachedPlaylist],
        heard: Option<&[(String, i64)]>,
    ) -> Result<bool, StoreError> {
        let projection = prepare_spotify_library(entries, playlists, contents, heard)?;
        self.in_transaction(|connection| {
            if read_spotify_owner(connection)?.as_ref() != Some(owner) {
                return Ok(false);
            }
            if write_spotify_library(connection, &projection)? {
                bump_library_catalog_revision(connection)?;
            }
            Ok(true)
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn publish_spotify_library_from_progress(
        &mut self,
        owner: &SpotifyLibraryOwner,
        expected: &str,
        entries: &[LibraryEntry],
        playlists: Option<&[(String, String)]>,
        contents: &[CachedPlaylist],
        heard: Option<&[(String, i64)]>,
        playlist_fence: Option<&SpotifyPlaylistPublicationFence>,
        artist_fence: Option<&SpotifyArtistDiscoveryFence>,
    ) -> Result<bool, StoreError> {
        super::validate_spotify_library_progress(&owner.owner_id, Some(expected), None)?;
        if let Some(fence) = artist_fence {
            super::validate_spotify_artist_discovery(
                owner,
                fence.expected.as_deref(),
                Some(&fence.progress),
            )?;
        }
        if let Some(fence) = playlist_fence {
            super::validate_spotify_playlist_evidence(
                owner,
                fence.expected_directory.as_deref(),
                fence.directory.as_deref(),
                &fence.observations,
            )?;
        }
        let projection = prepare_spotify_library(entries, playlists, contents, heard)?;
        self.in_transaction(|connection| {
            if read_spotify_owner(connection)?.as_ref() != Some(owner)
                || read_spotify_library_progress(connection, &owner.owner_id, Some(owner))?
                    .as_deref()
                    != Some(expected)
            {
                return Ok(false);
            }
            if let Some(fence) = artist_fence
                && read_artist_discovery(connection, owner)?.as_deref() != fence.expected.as_deref()
            {
                return Ok(false);
            }
            if let Some(fence) = playlist_fence
                && !playlist_evidence_matches(
                    connection,
                    owner,
                    fence.expected_directory.as_deref(),
                    &fence.observations,
                )?
            {
                return Ok(false);
            }
            let changed = write_spotify_library(connection, &projection)?;
            if let Some(fence) = playlist_fence {
                write_playlist_evidence(
                    connection,
                    owner,
                    fence.directory.as_deref(),
                    &fence.observations,
                )?;
            }
            if let Some(fence) = artist_fence {
                write_artist_discovery(connection, owner, &fence.progress)?;
            }
            connection.execute_with_params_sync(
                "DELETE FROM spotify_library_progress WHERE incoming_id = ?1",
                &[owner.owner_id.as_str().into()],
            )?;
            if changed {
                bump_library_catalog_revision(connection)?;
            }
            Ok(true)
        })
    }

    fn publish_spotify_playlist_continuation(
        &mut self,
        owner: &SpotifyLibraryOwner,
        publication: &SpotifyPlaylistPublication,
    ) -> Result<bool, StoreError> {
        super::validate_spotify_playlist_publication(owner, publication)?;
        let projection = prepare_spotify_library(&[], publication.playlists.as_deref(), &[], None)?;
        self.in_transaction(|connection| {
            if !playlist_evidence_matches(
                connection,
                owner,
                publication.expected_directory.as_deref(),
                &publication.observations,
            )? {
                return Ok(false);
            }
            let mut changed = merge_playlist_library(connection, &publication.entries)?;
            changed |= write_spotify_library(connection, &projection)?;
            changed |= write_playlist_observations(connection, &publication.observations)?;
            write_playlist_evidence(
                connection,
                owner,
                publication.directory.as_deref(),
                &publication.observations,
            )?;
            if changed {
                bump_library_catalog_revision(connection)?;
            }
            Ok(true)
        })
    }

    fn acknowledge_spotify_playlist(
        &mut self,
        owner: &SpotifyLibraryOwner,
        playlist_uri: &str,
        expected: &str,
        applied: &str,
    ) -> Result<bool, StoreError> {
        if expected.len() > super::MAX_PLAYLIST_BYTES || applied.len() > super::MAX_PLAYLIST_BYTES {
            return Err(backend("playlist progress is too large"));
        }
        self.in_transaction(|connection| {
            if read_spotify_owner(connection)?.as_ref() != Some(owner) {
                return Ok(false);
            }
            connection
                .execute_with_params_sync(
                    "UPDATE spotify_playlist_progress SET progress = ?3 \
                     WHERE owner_id = ?1 AND playlist_uri = ?2 AND progress = ?4",
                    &[
                        owner.owner_id.as_str().into(),
                        playlist_uri.into(),
                        applied.into(),
                        expected.into(),
                    ],
                )
                .map(|changed| changed == 1)
        })
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
             (at, work_key, composer_key, performer, signal, artist_key, album_key, genres) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            &[
                feedback.at.into(),
                opt_value(feedback.work_key.as_deref()),
                opt_value(feedback.composer_key.as_deref()),
                opt_value(feedback.performer.as_deref()),
                feedback.signal.into(),
                opt_value(feedback.artist_key.as_deref()),
                opt_value(feedback.album_key.as_deref()),
                opt_value(
                    (!feedback.genres.is_empty()).then(|| feedback.genres.join(GENRE_SEPARATOR)),
                ),
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
                "SELECT at, work_key, composer_key, performer, signal, artist_key, album_key, genres \
                 FROM feedback \
                 WHERE work_key = ?1 AND at >= ?2 AND at < ?3 ORDER BY at, id",
                k,
            ),
            FeedbackKey::Composer(k) => (
                "SELECT at, work_key, composer_key, performer, signal, artist_key, album_key, genres \
                 FROM feedback \
                 WHERE composer_key = ?1 AND at >= ?2 AND at < ?3 ORDER BY at, id",
                k,
            ),
            FeedbackKey::Performer(k) => (
                "SELECT at, work_key, composer_key, performer, signal, artist_key, album_key, genres \
                 FROM feedback \
                 WHERE performer = ?1 AND at >= ?2 AND at < ?3 ORDER BY at, id",
                k,
            ),
            FeedbackKey::Artist(k) => (
                "SELECT at, work_key, composer_key, performer, signal, artist_key, album_key, genres \
                 FROM feedback \
                 WHERE artist_key = ?1 AND at >= ?2 AND at < ?3 ORDER BY at, id",
                k,
            ),
            FeedbackKey::Album(k) => (
                "SELECT at, work_key, composer_key, performer, signal, artist_key, album_key, genres \
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
            "SELECT at, work_key, composer_key, performer, signal, artist_key, album_key, genres \
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
                    opt_value(t.explicit.map(i64::from)),
                ]
            })
            .collect();
        self.in_transaction(|c| {
            let removed = c.execute_with_params_sync(
                "DELETE FROM album_tracks WHERE album_uri = ?1",
                &[album_uri.into()],
            )?;
            if rows.is_empty() {
                if removed > 0 {
                    bump_library_catalog_revision(c)?;
                }
                return Ok(());
            }
            c.execute_many_with_params_in_transaction_sync(
                "INSERT OR REPLACE INTO album_tracks \
                 (album_uri, disc_number, track_number, source_uri, title, duration_secs, \
                  fetched_at, explicit) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                &rows,
            )?;
            bump_library_catalog_revision(c)
        })
    }

    fn album_tracks(&self, album_uri: &str) -> Result<Option<CachedAlbum>, StoreError> {
        let rows = self.query(
            "SELECT disc_number, track_number, source_uri, title, duration_secs, fetched_at, explicit \
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
                    explicit: opt_int(r, 6)?.map(|value| value != 0),
                })
            })
            .collect::<Result<_, StoreError>>()?;
        Ok(Some(CachedAlbum { tracks, fetched_at }))
    }

    fn album_tracks_many(
        &self,
        album_uris: &[&str],
    ) -> Result<Vec<(String, CachedAlbum)>, StoreError> {
        // SQLite caps bound variables, so read in chunks and group the rows by
        // album: one query per chunk rather than one per partly-held work.
        let mut out: Vec<(String, CachedAlbum)> = Vec::new();
        for chunk in album_uris.chunks(400) {
            if chunk.is_empty() {
                continue;
            }
            let placeholders = (1..=chunk.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "SELECT album_uri, disc_number, track_number, source_uri, title, \
                 duration_secs, fetched_at, explicit FROM album_tracks \
                 WHERE album_uri IN ({placeholders}) \
                 ORDER BY album_uri, disc_number, track_number"
            );
            let params: Vec<SqliteValue> = chunk.iter().map(|&uri| uri.into()).collect();
            for row in self.query(&sql, &params)? {
                let uri = text(&row, 0)?;
                let track = AlbumTrack {
                    disc_number: u32_of(int(&row, 1)?, "disc_number")?,
                    track_number: u32_of(int(&row, 2)?, "track_number")?,
                    source_uri: text(&row, 3)?,
                    title: text(&row, 4)?,
                    duration_secs: opt_int(&row, 5)?
                        .map(|d| u32_of(d, "duration_secs"))
                        .transpose()?,
                    explicit: opt_int(&row, 7)?.map(|value| value != 0),
                };
                let fetched_at = int(&row, 6)?;
                match out.last_mut() {
                    Some((last, album)) if *last == uri => album.tracks.push(track),
                    _ => out.push((
                        uri,
                        CachedAlbum {
                            tracks: vec![track],
                            fetched_at,
                        },
                    )),
                }
            }
        }
        Ok(out)
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

    fn artist_genres_many(&self, artist_ids: &[&str]) -> ArtistGenresBatch {
        artist_genres_batch(artist_ids, |sql, params| self.query(sql, params))
    }

    fn save_album_misses(&mut self, misses: &[(String, i64)]) -> Result<(), StoreError> {
        let rows: Vec<Vec<SqliteValue>> = misses
            .iter()
            .map(|(uri, at)| vec![uri.as_str().into(), (*at).into()])
            .collect();
        self.in_transaction(|c| {
            c.execute_sync("DELETE FROM album_misses")?;
            if rows.is_empty() {
                return Ok(());
            }
            c.execute_many_with_params_in_transaction_sync(
                "INSERT OR REPLACE INTO album_misses (album_uri, retry_at) VALUES (?1, ?2)",
                &rows,
            )
            .map(drop)
        })
    }

    fn save_playlists(&mut self, playlists: &[(String, String)]) -> Result<(), StoreError> {
        let rows = playlist_rows(playlists);
        self.in_transaction(|c| {
            let removed = c.execute_sync("DELETE FROM spotify_playlists")?;
            if rows.is_empty() {
                if removed > 0 {
                    bump_library_catalog_revision(c)?;
                }
                return Ok(());
            }
            c.execute_many_with_params_in_transaction_sync(
                "INSERT OR IGNORE INTO spotify_playlists (playlist_uri, name, position) \
                 VALUES (?1, ?2, ?3)",
                &rows,
            )?;
            bump_library_catalog_revision(c)
        })
    }

    fn playlists(&self) -> Result<Vec<(String, String)>, StoreError> {
        self.query(
            "SELECT playlist_uri, name FROM spotify_playlists ORDER BY position",
            &[],
        )?
        .iter()
        .map(|r| Ok((text(r, 0)?, text(r, 1)?)))
        .collect()
    }

    fn save_playlist_contents(&mut self, contents: &CachedPlaylist) -> Result<(), StoreError> {
        contents.validate()?;
        let observation = serde_json::to_string(contents).map_err(backend)?;
        self.in_transaction(|connection| {
            connection.execute_with_params_sync(
                "INSERT OR REPLACE INTO spotify_playlist_contents (playlist_uri, observation) \
                 VALUES (?1, ?2)",
                &[
                    contents.playlist_uri.as_str().into(),
                    observation.as_str().into(),
                ],
            )?;
            bump_library_catalog_revision(connection)
        })
    }

    fn playlist_contents(&self, playlist_uri: &str) -> Result<Option<CachedPlaylist>, StoreError> {
        self.query(
            "SELECT observation FROM spotify_playlist_contents WHERE playlist_uri = ?1",
            &[playlist_uri.into()],
        )?
        .first()
        .map(|row| {
            let contents: CachedPlaylist = serde_json::from_str(&text(row, 0)?).map_err(backend)?;
            contents.validate()?;
            if contents.playlist_uri != playlist_uri {
                return Err(backend("cached playlist identity differs from its key"));
            }
            Ok(contents)
        })
        .transpose()
    }

    fn spotify_playlist_directory(
        &self,
        owner: &SpotifyLibraryOwner,
    ) -> Result<Option<String>, StoreError> {
        let generation = i64::try_from(owner.generation).map_err(backend)?;
        self.query(
            "SELECT directory.progress FROM spotify_playlist_directories AS directory \
             JOIN spotify_library_owner AS current \
             ON current.owner_id = directory.owner_id AND current.generation = directory.generation \
             WHERE current.singleton = 1 AND directory.owner_id = ?1 AND directory.generation = ?2",
            &[owner.owner_id.as_str().into(), generation.into()],
        )?
        .first()
        .map(|row| {
            let progress = text(row, 0)?;
            if progress.len() > super::MAX_PLAYLIST_BYTES {
                return Err(backend("playlist directory is too large"));
            }
            Ok(progress)
        })
        .transpose()
    }

    fn save_spotify_playlist_directory(
        &mut self,
        owner: &SpotifyLibraryOwner,
        expected: Option<&str>,
        progress: &str,
        active: Option<&[String]>,
    ) -> Result<bool, StoreError> {
        if progress.len() > super::MAX_PLAYLIST_BYTES
            || expected.is_some_and(|value| value.len() > super::MAX_PLAYLIST_BYTES)
        {
            return Err(backend("playlist directory is too large"));
        }
        let generation = i64::try_from(owner.generation).map_err(backend)?;
        let keep = active.map(|uris| {
            uris.iter()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>()
        });
        self.in_transaction(|connection| {
            if read_spotify_owner(connection)?.as_ref() != Some(owner) {
                return Ok(false);
            }
            let current = connection.query_with_params_sync(
                "SELECT progress FROM spotify_playlist_directories WHERE owner_id = ?1 AND generation = ?2",
                &[owner.owner_id.as_str().into(), generation.into()],
            )?;
            let current = current
                .first()
                .map(|row| text(row, 0).map_err(|error| FrankenError::Internal(error.to_string())))
                .transpose()?;
            if current.as_deref() != expected {
                return Ok(false);
            }
            connection.execute_with_params_sync(
                "INSERT OR REPLACE INTO spotify_playlist_directories (owner_id, generation, progress) \
                 VALUES (?1, ?2, ?3)",
                &[owner.owner_id.as_str().into(), generation.into(), progress.into()],
            )?;
            if let Some(keep) = &keep {
                let rows = connection.query_with_params_sync(
                    "SELECT playlist_uri FROM spotify_playlist_progress WHERE owner_id = ?1",
                    &[owner.owner_id.as_str().into()],
                )?;
                for row in rows {
                    let uri = text(&row, 0)
                        .map_err(|error| FrankenError::Internal(error.to_string()))?;
                    if !keep.contains(uri.as_str()) {
                        connection.execute_with_params_sync(
                            "DELETE FROM spotify_playlist_progress WHERE owner_id = ?1 AND playlist_uri = ?2",
                            &[owner.owner_id.as_str().into(), uri.into()],
                        )?;
                    }
                }
            }
            Ok(true)
        })
    }

    fn spotify_followed_directory(
        &self,
        owner: &SpotifyLibraryOwner,
    ) -> Result<Option<String>, StoreError> {
        super::validate_spotify_followed_directory(owner, None, None)?;
        let generation = i64::try_from(owner.generation).map_err(backend)?;
        self.query(
            "SELECT directory.progress FROM spotify_followed_directories AS directory \
             JOIN spotify_library_owner AS current \
             ON current.owner_id = directory.owner_id AND current.generation = directory.generation \
             WHERE current.singleton = 1 AND directory.owner_id = ?1 AND directory.generation = ?2",
            &[owner.owner_id.as_str().into(), generation.into()],
        )?
        .first()
        .map(|row| {
            let progress = text(row, 0)?;
            super::validate_spotify_followed_directory(owner, None, Some(&progress))?;
            Ok(progress)
        })
        .transpose()
    }

    fn save_spotify_followed_directory(
        &mut self,
        owner: &SpotifyLibraryOwner,
        expected: Option<&str>,
        progress: &str,
    ) -> Result<bool, StoreError> {
        super::validate_spotify_followed_directory(owner, expected, Some(progress))?;
        let generation = i64::try_from(owner.generation).map_err(backend)?;
        self.in_transaction(|connection| {
            if read_spotify_owner(connection)?.as_ref() != Some(owner) {
                return Ok(false);
            }
            let current = connection.query_with_params_sync(
                "SELECT progress FROM spotify_followed_directories \
                 WHERE owner_id = ?1 AND generation = ?2",
                &[owner.owner_id.as_str().into(), generation.into()],
            )?;
            let current = current
                .first()
                .map(|row| text(row, 0).map_err(|error| FrankenError::Internal(error.to_string())))
                .transpose()?;
            if current.as_deref() != expected {
                return Ok(false);
            }
            connection.execute_with_params_sync(
                "INSERT OR REPLACE INTO spotify_followed_directories \
                 (owner_id, generation, progress) VALUES (?1, ?2, ?3)",
                &[
                    owner.owner_id.as_str().into(),
                    generation.into(),
                    progress.into(),
                ],
            )?;
            Ok(true)
        })
    }

    fn playlist_scan_cursor(&self, owner_id: &str) -> Result<Option<String>, StoreError> {
        self.query(
            "SELECT playlist_id FROM spotify_playlist_scans WHERE owner_id = ?1",
            &[owner_id.into()],
        )?
        .first()
        .map(|row| text(row, 0))
        .transpose()
    }

    fn save_playlist_scan_cursor(
        &mut self,
        owner_id: &str,
        playlist_id: &str,
    ) -> Result<(), StoreError> {
        self.execute(
            "INSERT OR REPLACE INTO spotify_playlist_scans (owner_id, playlist_id) \
             VALUES (?1, ?2)",
            &[owner_id.into(), playlist_id.into()],
        )
    }

    fn playlist_progress(
        &self,
        owner_id: &str,
        playlist_uri: &str,
    ) -> Result<Option<String>, StoreError> {
        self.query(
            "SELECT progress FROM spotify_playlist_progress WHERE owner_id = ?1 AND playlist_uri = ?2",
            &[owner_id.into(), playlist_uri.into()],
        )?
        .first()
        .map(|row| {
            let progress = text(row, 0)?;
            if progress.len() > super::MAX_PLAYLIST_BYTES {
                return Err(backend("playlist progress is too large"));
            }
            Ok(progress)
        })
        .transpose()
    }

    fn save_playlist_progress(
        &mut self,
        owner_id: &str,
        playlist_uri: &str,
        progress: &str,
    ) -> Result<(), StoreError> {
        if progress.len() > super::MAX_PLAYLIST_BYTES {
            return Err(backend("playlist progress is too large"));
        }
        self.execute(
            "INSERT OR REPLACE INTO spotify_playlist_progress (owner_id, playlist_uri, progress) \
             VALUES (?1, ?2, ?3)",
            &[owner_id.into(), playlist_uri.into(), progress.into()],
        )
    }

    fn save_spotify_playlist_progress(
        &mut self,
        owner: &SpotifyLibraryOwner,
        playlist_uri: &str,
        expected: Option<&str>,
        progress: &str,
    ) -> Result<bool, StoreError> {
        super::validate_spotify_playlist_progress(owner, playlist_uri, expected, progress)?;
        self.in_transaction(|connection| {
            if read_spotify_owner(connection)?.as_ref() != Some(owner)
                || read_playlist_progress(connection, owner, playlist_uri)?.as_deref() != expected
            {
                return Ok(false);
            }
            connection.execute_with_params_sync(
                "INSERT OR REPLACE INTO spotify_playlist_progress (owner_id, playlist_uri, progress) VALUES (?1, ?2, ?3)",
                &[owner.owner_id.as_str().into(), playlist_uri.into(), progress.into()],
            )?;
            Ok(true)
        })
    }

    fn retain_playlist_progress(
        &mut self,
        owner_id: &str,
        playlist_uris: &[String],
    ) -> Result<(), StoreError> {
        let keep: std::collections::BTreeSet<_> = playlist_uris.iter().cloned().collect();
        let gone = self
            .query(
                "SELECT playlist_uri FROM spotify_playlist_progress WHERE owner_id = ?1",
                &[owner_id.into()],
            )?
            .iter()
            .map(|row| text(row, 0))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|uri| !keep.contains(uri))
            .collect::<Vec<_>>();
        if gone.is_empty() {
            return Ok(());
        }
        self.in_transaction(|connection| {
            for uri in &gone {
                connection.execute_with_params_sync(
                    "DELETE FROM spotify_playlist_progress WHERE owner_id = ?1 AND playlist_uri = ?2",
                    &[owner_id.into(), uri.as_str().into()],
                )?;
            }
            Ok(())
        })
    }

    fn save_spotify_heard(&mut self, plays: &[(String, i64)]) -> Result<(), StoreError> {
        let rows = heard_rows(plays);
        self.in_transaction(|c| {
            c.execute_sync("DELETE FROM spotify_heard")?;
            if rows.is_empty() {
                return Ok(());
            }
            c.execute_many_with_params_in_transaction_sync(
                "INSERT OR REPLACE INTO spotify_heard (source_uri, played_at) VALUES (?1, ?2)",
                &rows,
            )
            .map(drop)
        })
    }

    fn spotify_heard(&self) -> Result<Vec<(String, i64)>, StoreError> {
        self.query(
            "SELECT source_uri, played_at FROM spotify_heard ORDER BY played_at, source_uri",
            &[],
        )?
        .iter()
        .map(|r| Ok((text(r, 0)?, int(r, 1)?)))
        .collect()
    }

    fn album_misses(&self) -> Result<Vec<(String, i64)>, StoreError> {
        self.query(
            "SELECT album_uri, retry_at FROM album_misses ORDER BY album_uri",
            &[],
        )?
        .iter()
        .map(|r| Ok((text(r, 0)?, int(r, 1)?)))
        .collect()
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

    fn put_schedule(&mut self, schedule: &StoredSchedule) -> Result<bool, StoreError> {
        self.in_transaction(|c| {
            let taken = c.query_with_params_sync(
                "SELECT 1 FROM schedules WHERE id = ?1",
                &[schedule.id.into()],
            )?;
            if !taken.is_empty() {
                return Ok(false);
            }
            c.execute_with_params_sync(
                "INSERT INTO schedules (id, spec, action, creator, enabled, created, last_fired) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                &[
                    schedule.id.into(),
                    schedule.spec.as_str().into(),
                    schedule.action.as_str().into(),
                    schedule.creator.as_str().into(),
                    i64::from(schedule.enabled).into(),
                    schedule.created.into(),
                    opt_value(schedule.last_fired),
                ],
            )?;
            Ok(true)
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
        genres: split_genres(opt_text(r, 7)?),
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
    use std::sync::{Arc, Barrier};

    fn catalog_entry(name: &str) -> LibraryEntry {
        LibraryEntry {
            track: Track {
                source_uri: format!("spotify:track:{name}"),
                title: name.into(),
                artist: None,
                album: None,
                uri: None,
                duration_secs: None,
            },
            ..LibraryEntry::default()
        }
    }

    #[test]
    fn catalog_clock_migration_preserves_existing_rows_and_starts_a_new_revision() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("catalog-clock-upgrade.db");
        let old = SqliteStore::open_migrated(&file.to_string_lossy(), &MIGRATIONS[..23]).unwrap();
        old.execute(
            "INSERT INTO spotify_library (source_uri, title, is_classical, added) \
             VALUES (?1, 'Existing', 0, 10)",
            &["spotify:track:Existing".into()],
        )
        .unwrap();
        old.execute(
            "INSERT INTO spotify_playlists (playlist_uri, name, position) \
             VALUES (?1, 'Existing list', 0)",
            &["spotify:playlist:Existing".into()],
        )
        .unwrap();
        old.execute(
            "INSERT INTO album_tracks \
             (album_uri, disc_number, track_number, source_uri, title, fetched_at) \
             VALUES (?1, 1, 1, ?2, 'Existing album track', 11)",
            &[
                "spotify:album:Existing".into(),
                "spotify:track:Existing".into(),
            ],
        )
        .unwrap();
        let library = old.library().unwrap();
        let names = old.playlists().unwrap();
        let album = old.album_tracks("spotify:album:Existing").unwrap();
        old.close().unwrap();

        let mut store = SqliteStore::open(&file).unwrap();
        assert_eq!(store.schema_versions().unwrap().last(), Some(&24));
        assert_eq!(store.library_catalog_revision().unwrap(), Some(0));
        assert_eq!(store.library().unwrap(), library);
        assert_eq!(store.playlists().unwrap(), names);
        assert_eq!(store.album_tracks("spotify:album:Existing").unwrap(), album);
        store
            .upsert_library(&[catalog_entry("AfterUpgrade")])
            .unwrap();
        assert_eq!(store.library_catalog_revision().unwrap(), Some(1));
        store.close().unwrap();
    }

    #[test]
    fn failed_publication_preserves_the_clock_rows_and_completed_proof() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let (owner, _) = store.adopt_spotify_library_owner("catalog-owner").unwrap();
        store.upsert_library(&[catalog_entry("Existing")]).unwrap();
        store
            .save_playlists(&[("spotify:playlist:Existing".into(), "Existing".into())])
            .unwrap();
        assert!(
            store
                .save_spotify_library_progress("catalog-owner", Some(&owner), None, "ready")
                .unwrap()
        );
        let revision = store.library_catalog_revision().unwrap();
        let library = store.library().unwrap();
        let names = store.playlists().unwrap();
        // This fails after the library and directory writes inside publication.
        store.execute("DROP TABLE spotify_heard", &[]).unwrap();
        assert!(
            store
                .publish_spotify_library_from_progress(
                    &owner,
                    "ready",
                    &[catalog_entry("MustRollBack")],
                    Some(&[]),
                    &[],
                    Some(&[]),
                    None,
                    None,
                )
                .is_err()
        );
        assert_eq!(store.library_catalog_revision().unwrap(), revision);
        assert_eq!(store.library().unwrap(), library);
        assert_eq!(store.playlists().unwrap(), names);
        assert_eq!(
            store
                .spotify_library_progress("catalog-owner", Some(&owner))
                .unwrap()
                .as_deref(),
            Some("ready")
        );
    }

    #[test]
    fn a_rollback_after_increment_restores_the_previous_catalog_token() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        store.upsert_library(&[catalog_entry("Existing")]).unwrap();
        let revision = store.library_catalog_revision().unwrap();
        let library = store.library().unwrap();
        let replacement = library_rows(&[catalog_entry("MustRollBack")]);
        let result = store.in_transaction(|connection| {
            write_library(connection, &replacement)?;
            bump_library_catalog_revision(connection)?;
            Err::<(), _>(FrankenError::Internal("failure after the increment".into()))
        });
        assert!(result.is_err());
        assert_eq!(store.library_catalog_revision().unwrap(), revision);
        assert_eq!(store.library().unwrap(), library);
    }

    #[test]
    fn catalog_clock_exhaustion_is_durable_and_does_not_block_publication() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("catalog-clock-exhausted.db");
        let mut store = SqliteStore::open(&file).unwrap();
        store
            .execute(
                "UPDATE library_catalog_clock SET revision = ?1 WHERE singleton = 1",
                &[(i64::MAX - 1).into()],
            )
            .unwrap();
        store.upsert_library(&[catalog_entry("Last")]).unwrap();
        assert_eq!(
            store.library_catalog_revision().unwrap(),
            Some(i64::MAX.unsigned_abs())
        );
        store.upsert_library(&[catalog_entry("Overflow")]).unwrap();
        assert_eq!(store.library_catalog_revision().unwrap(), None);
        store.close().unwrap();

        let mut reopened = SqliteStore::open(&file).unwrap();
        assert_eq!(reopened.library_catalog_revision().unwrap(), None);
        let (owner, _) = reopened
            .adopt_spotify_library_owner("still-writable")
            .unwrap();
        assert!(
            reopened
                .publish_spotify_library(
                    &owner,
                    &[catalog_entry("StillWritable")],
                    None,
                    &[],
                    None,
                )
                .unwrap()
        );
        assert_eq!(reopened.library_catalog_revision().unwrap(), None);
        assert_eq!(reopened.library().unwrap().len(), 3);
        reopened.close().unwrap();
    }

    #[test]
    fn artist_cache_batches_are_bounded_and_isolate_failed_rows_and_chunks() {
        let store = SqliteStore::open_in_memory().unwrap();
        let ids = (0..805)
            .map(|id| format!("Artist{id:04}"))
            .collect::<Vec<_>>();
        let rows = ids
            .iter()
            .map(|id| vec![id.as_str().into(), "jazz".into(), 1_000i64.into()])
            .collect::<Vec<Vec<SqliteValue>>>();
        store
            .in_transaction(|connection| {
                connection.execute_many_with_params_in_transaction_sync(
                "INSERT INTO artist_genres (artist_id, genres, fetched_at) VALUES (?1, ?2, ?3)",
                &rows,
            ).map(drop)
            })
            .unwrap();
        store
            .execute(
                "UPDATE artist_genres SET fetched_at = ?1 WHERE artist_id = ?2",
                &["invalid-timestamp".into(), ids[401].as_str().into()],
            )
            .unwrap();
        let mut requested = ids.iter().map(String::as_str).collect::<Vec<_>>();
        requested.extend([ids[0].as_str(), "Unknown'Artist"]);
        let mut queries = 0;
        let batch = artist_genres_batch(&requested, |sql, params| {
            queries += 1;
            assert!(params.len() <= 400);
            store.query(sql, params)
        });
        assert_eq!(queries, 3);
        assert_eq!(batch.cached.len(), 804);
        assert_eq!(batch.failed.len(), 1);
        assert!(batch.failed.contains_key(&ids[401]));
        assert!(!batch.cached.contains_key("Unknown'Artist"));
        assert_eq!(batch.cached[&ids[402]], (vec!["jazz".to_owned()], 1_000));

        let mut queries = 0;
        let failed_chunk = artist_genres_batch(&requested, |sql, params| {
            queries += 1;
            if queries == 2 {
                Err(StoreError::Backend("chunk unavailable".into()))
            } else {
                store.query(sql, params)
            }
        });
        assert_eq!(
            queries, 3,
            "a failed batch must not fan out into single reads"
        );
        assert_eq!(failed_chunk.failed.len(), 400);
        assert_eq!(failed_chunk.cached.len(), 405);
        assert!(failed_chunk.cached.contains_key(&ids[804]));
        let empty = artist_genres_batch(&[], |_, _| panic!("empty batches need no query"));
        assert!(empty.cached.is_empty() && empty.failed.is_empty());
    }

    #[test]
    fn artist_rotation_migration_preserves_followed_directory_and_library_read() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("artist-discovery-upgrade.db");
        let mut old =
            SqliteStore::open_migrated(&file.to_string_lossy(), &MIGRATIONS[..22]).unwrap();
        old.execute(
            "INSERT INTO spotify_library_owner (singleton, owner_id, generation) VALUES (1, ?1, 7)",
            &["existing-owner".into()],
        )
        .unwrap();
        let owner = old.spotify_library_owner().unwrap().unwrap();
        assert!(
            old.save_spotify_followed_directory(&owner, None, "followed proof")
                .unwrap()
        );
        assert!(
            old.save_spotify_library_progress(
                "existing-owner",
                Some(&owner),
                None,
                "complete saved read",
            )
            .unwrap()
        );
        old.close().unwrap();

        let mut store = SqliteStore::open(&file).unwrap();
        assert_eq!(store.schema_versions().unwrap().last(), Some(&24));
        assert_eq!(store.spotify_library_owner().unwrap(), Some(owner.clone()));
        assert_eq!(
            store.spotify_followed_directory(&owner).unwrap().as_deref(),
            Some("followed proof")
        );
        assert_eq!(store.spotify_artist_discovery(&owner).unwrap(), None);
        let cursor = SpotifyArtistDiscoveryFence {
            expected: None,
            progress: "artist050".into(),
        };
        assert!(
            store
                .publish_spotify_library_from_progress(
                    &owner,
                    "complete saved read",
                    &[],
                    None,
                    &[],
                    None,
                    None,
                    Some(&cursor),
                )
                .unwrap()
        );
        store.close().unwrap();

        let reopened = SqliteStore::open(&file).unwrap();
        assert_eq!(
            reopened
                .spotify_artist_discovery(&owner)
                .unwrap()
                .as_deref(),
            Some("artist050")
        );
        assert_eq!(
            reopened
                .spotify_followed_directory(&owner)
                .unwrap()
                .as_deref(),
            Some("followed proof")
        );
        assert_eq!(
            reopened
                .spotify_library_progress("existing-owner", Some(&owner))
                .unwrap(),
            None
        );
    }

    #[test]
    fn followed_directory_migration_preserves_existing_personal_checkpoints() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("followed-directory-upgrade.db");
        let mut old =
            SqliteStore::open_migrated(&file.to_string_lossy(), &MIGRATIONS[..21]).unwrap();
        old.execute(
            "INSERT INTO spotify_library_owner (singleton, owner_id, generation) VALUES (1, ?1, 7)",
            &["existing-owner".into()],
        )
        .unwrap();
        old.execute(
            "INSERT INTO spotify_library \
             (source_uri, title, is_classical, added, candidate, taste_weight, artist_id) \
             VALUES (?1, ?2, 0, 10, 1, 1080, ?3)",
            &[
                "spotify:track:Existing".into(),
                "Existing".into(),
                "CatalogArtist".into(),
            ],
        )
        .unwrap();
        let owner = old.spotify_library_owner().unwrap().unwrap();
        let library = old.library().unwrap();
        assert!(
            old.save_spotify_playlist_directory(&owner, None, "playlist prefix", None)
                .unwrap()
        );
        assert!(
            old.save_spotify_library_progress(
                "existing-owner",
                Some(&owner),
                None,
                "essential prefix"
            )
            .unwrap()
        );
        old.close().unwrap();

        let mut store = SqliteStore::open(&file).unwrap();
        assert_eq!(store.schema_versions().unwrap().last(), Some(&24));
        assert_eq!(store.spotify_library_owner().unwrap(), Some(owner.clone()));
        assert_eq!(store.library().unwrap(), library);
        assert_eq!(
            store.spotify_playlist_directory(&owner).unwrap().as_deref(),
            Some("playlist prefix")
        );
        assert_eq!(
            store
                .spotify_library_progress("existing-owner", Some(&owner))
                .unwrap()
                .as_deref(),
            Some("essential prefix")
        );
        assert_eq!(store.spotify_followed_directory(&owner).unwrap(), None);
        assert!(
            store
                .save_spotify_followed_directory(&owner, None, "followed prefix")
                .unwrap()
        );
        store.close().unwrap();

        let reopened = SqliteStore::open(&file).unwrap();
        assert_eq!(
            reopened
                .spotify_followed_directory(&owner)
                .unwrap()
                .as_deref(),
            Some("followed prefix")
        );
        assert_eq!(reopened.spotify_library_owner().unwrap(), Some(owner));
        assert_eq!(reopened.library().unwrap(), library);
        reopened.close().unwrap();
    }

    #[test]
    fn essential_library_progress_migration_preserves_an_adopted_cache() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("library-progress-upgrade.db");
        let old = SqliteStore::open_migrated(&file.to_string_lossy(), &MIGRATIONS[..20]).unwrap();
        old.execute(
            "INSERT INTO spotify_library_owner (singleton, owner_id, generation) VALUES (1, ?1, 7)",
            &["existing-owner".into()],
        )
        .unwrap();
        old.execute(
            "INSERT INTO spotify_library \
             (source_uri, title, is_classical, added, candidate, taste_weight, artist_id) \
             VALUES (?1, ?2, 0, 10, 1, 1200, ?3)",
            &[
                "spotify:track:Existing".into(),
                "Existing".into(),
                "CatalogArtist".into(),
            ],
        )
        .unwrap();
        let owner = old.spotify_library_owner().unwrap().unwrap();
        let library = old.library().unwrap();
        old.close().unwrap();

        let mut store = SqliteStore::open(&file).unwrap();
        assert!(store.schema_versions().unwrap().contains(&21));
        assert_eq!(store.spotify_library_owner().unwrap(), Some(owner.clone()));
        assert_eq!(store.library().unwrap(), library);
        assert_eq!(
            store
                .spotify_library_progress("incoming-owner", Some(&owner))
                .unwrap(),
            None
        );
        assert!(
            store
                .save_spotify_library_progress("incoming-owner", Some(&owner), None, "prefix")
                .unwrap()
        );
        assert_eq!(store.spotify_library_owner().unwrap(), Some(owner.clone()));
        assert_eq!(store.library().unwrap(), library);
        store.close().unwrap();

        let reopened = SqliteStore::open(&file).unwrap();
        assert_eq!(
            reopened
                .spotify_library_progress("incoming-owner", Some(&owner))
                .unwrap()
                .as_deref(),
            Some("prefix")
        );
        assert_eq!(reopened.spotify_library_owner().unwrap(), Some(owner));
        assert_eq!(reopened.library().unwrap(), library);
        reopened.close().unwrap();
    }

    #[test]
    fn a_migration_another_connection_applied_is_not_applied_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fsonos.db").to_string_lossy().into_owned();
        let first = SqliteStore::open_migrated(&path, MIGRATIONS).unwrap();
        // A second connection that read the versions before the first
        // migrated: it reaches migration 1 although it is applied.
        let second = SqliteStore {
            conn: AsyncConnection::open_sync(path.as_str()).unwrap(),
            wal: None,
            plays: 0,
            held: None,
        };
        second.apply(&MIGRATIONS[0], 0).unwrap();
        assert_eq!(
            second.schema_versions().unwrap(),
            first.schema_versions().unwrap()
        );
        // A migration that fails for a reason of its own is still an error.
        let broken = Migration {
            version: 99,
            name: "broken",
            sql: "CREATE TABLE players (id TEXT)",
        };
        let error = second.apply(&broken, 0).unwrap_err();
        assert!(error.to_string().contains("migration 99"), "{error}");
        assert!(!second.schema_versions().unwrap().contains(&99));
    }

    #[test]
    fn concurrent_first_opens_of_one_database_all_succeed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fsonos.db");
        let start = Arc::new(Barrier::new(4));
        let opens: Vec<_> = (0..4)
            .map(|_| {
                let (path, start) = (path.clone(), Arc::clone(&start));
                std::thread::spawn(move || {
                    start.wait();
                    SqliteStore::open(&path).map(|store| store.schema_versions())
                })
            })
            .collect();
        let all: Vec<i64> = MIGRATIONS.iter().map(|m| m.version).collect();
        for open in opens {
            let versions = open.join().unwrap().unwrap().unwrap();
            assert_eq!(versions, all);
        }
    }

    #[test]
    fn transient_conflicts_are_retried_a_bounded_number_of_times() {
        let mut calls = 0;
        let busy = retrying(|| -> Result<(), FrankenError> {
            calls += 1;
            Err(FrankenError::Busy)
        });
        assert!(matches!(busy, Err(FrankenError::Busy)));
        assert_eq!(calls, BUSY_ATTEMPTS);

        let mut calls = 0;
        let recovered = retrying(|| {
            calls += 1;
            if calls < 3 {
                Err(FrankenError::BusySnapshot {
                    conflicting_pages: "1".into(),
                })
            } else {
                Ok(calls)
            }
        });
        assert_eq!(recovered.unwrap(), 3);

        let mut calls = 0;
        let fatal = retrying(|| -> Result<(), FrankenError> {
            calls += 1;
            Err(FrankenError::Internal("not transient".into()))
        });
        assert!(fatal.is_err());
        assert_eq!(calls, 1, "a non-transient error is not retried");
    }

    #[test]
    fn legacy_album_content_flags_stay_unknown_until_refreshed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join("legacy-albums.db")
            .to_string_lossy()
            .into_owned();
        let album_uri = "spotify:album:LegacyCatalog";
        let old = SqliteStore::open_migrated(&path, &MIGRATIONS[..17]).unwrap();
        old.execute(
            "INSERT INTO album_tracks \
             (album_uri, disc_number, track_number, source_uri, title, duration_secs, fetched_at) \
             VALUES (?1, 1, 1, ?2, 'I. Allegro', 180, 10)",
            &[album_uri.into(), "spotify:track:LegacyMovement".into()],
        )
        .unwrap();
        old.close().unwrap();

        let mut store = SqliteStore::open(Path::new(&path)).unwrap();
        let mut album = store.album_tracks(album_uri).unwrap().unwrap();
        assert_eq!(album.tracks[0].explicit, None);
        assert_eq!(album.fetched_at, 10);
        assert_eq!(
            store.album_tracks_many(&[album_uri]).unwrap(),
            [(album_uri.to_owned(), album.clone())]
        );
        album.tracks[0].explicit = Some(true);
        store
            .save_album_tracks(album_uri, &album.tracks, 20)
            .unwrap();
        store.close().unwrap();

        let store = SqliteStore::open(Path::new(&path)).unwrap();
        let refreshed = store.album_tracks(album_uri).unwrap().unwrap();
        assert_eq!(refreshed.tracks[0].explicit, Some(true));
        assert_eq!(refreshed.fetched_at, 20);
        store.close().unwrap();
    }

    #[test]
    fn v19_library_migrates_without_inventing_artist_identity() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("artist-identity.db");
        let legacy =
            SqliteStore::open_migrated(&path.to_string_lossy(), &MIGRATIONS[..19]).unwrap();
        legacy
            .execute(
                "INSERT INTO spotify_library \
                 (source_uri, title, artist, album, is_classical, added, origin, \
                  genres, explicit, candidate, taste_weight, work_key) \
                 VALUES (?1, 'Shared artist name', 'Catalog Artist', 'The Album', \
                  0, 9, 'both', 'jazz; cool jazz', 1, 1, 1230, 'catalog artist|song')",
                &["spotify:track:LegacyArtistIdentity".into()],
            )
            .unwrap();
        legacy.close().unwrap();

        let mut store = SqliteStore::open(&path).unwrap();
        assert_eq!(store.schema_versions().unwrap().last(), Some(&24));
        let mut entry = store.library().unwrap().pop().unwrap();
        assert_eq!(entry.artist_id, None, "a name does not prove an artist ID");
        assert_eq!(entry.origin, LibraryOrigin::Both);
        assert_eq!(entry.added, 9);
        assert_eq!(entry.genres, ["jazz", "cool jazz"]);
        assert!(entry.explicit);
        assert_eq!(entry.candidate, Some(true));
        assert_eq!(entry.taste_weight, Some(1230));
        assert_eq!(entry.work_key.as_deref(), Some("catalog artist|song"));
        entry.artist_id = Some("CatalogArtistIdentity".into());
        store.upsert_library(std::slice::from_ref(&entry)).unwrap();
        store.close().unwrap();

        let reopened = SqliteStore::open(&path).unwrap();
        assert_eq!(reopened.library().unwrap(), [entry]);
        reopened.close().unwrap();
    }

    #[test]
    fn v1_database_upgrades_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fsonos.db").to_string_lossy().into_owned();

        // A database created before migration 2, with a library row in it.
        let v1 = SqliteStore::open_migrated(&path, &MIGRATIONS[..1]).unwrap();
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
        assert_eq!(
            store.schema_versions().unwrap(),
            [
                1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
                24
            ]
        );
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
        let v2 = SqliteStore::open_migrated(&path, &MIGRATIONS[..2]).unwrap();
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
        assert_eq!(
            store.schema_versions().unwrap(),
            [
                1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
                24
            ]
        );
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
        let v6 = SqliteStore::open_migrated(&path, &MIGRATIONS[..6]).unwrap();
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
        assert_eq!(
            store.schema_versions().unwrap(),
            [
                1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
                24
            ]
        );
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
