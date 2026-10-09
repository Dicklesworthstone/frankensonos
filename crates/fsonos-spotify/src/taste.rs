//! The owner's taste beyond the saved library.
//!
//! With the read-only taste scopes granted ([`TASTE_SCOPES`]), a sync also
//! reads the artists the owner follows, their own playlists, their top
//! artists and tracks, and what they played recently. Tracks from their
//! playlists, top tracks and recent plays join the DJ's pool (as
//! [`Origin::Taste`] unless saved as well), and every signal weighs the
//! tracks it names: a top track most, then a followed or top artist's
//! tracks, then their playlists' and recent plays' (`taste_pm`, the
//! `Factor::Taste` of a pick). These are the account's own signals, the
//! weakest rung of the DJ's precedence: learned feedback, the owner's
//! preferences and steering all outrank them (see `crate::prefs`). Followed
//! and top artists come with their genres, which go to the artist genre cache
//! (`crate::genres`). The playlists in the owner's list (their own and the
//! ones they follow) are kept by name, so a search can play one.
//!
//! A grant without the taste scopes reads none of this and fails nothing:
//! the DJ's taste is the library alone, and [`TasteRead::missing_scopes`]
//! says what a new sign-in would add (the doctor warns). A run that stops
//! early (rate limited past the waits, a server error, signed out) keeps
//! what it read; the sync then keeps the taste-only tracks it cached
//! before, rather than retiring what it didn't get to read.

use std::collections::{HashMap, HashSet};

use asupersync::Cx;
use fsonos_core::store::Store;

use crate::SpotifyError;
use crate::client::{
    Artist, CursorPage, FollowedArtists, FullTrack, Me, Paging, PlayHistory, PlaylistItem,
    SimplifiedPlaylist, TASTE_SCOPES, decode,
};
use crate::expand::halts;
use crate::library::{LibraryItem, Origin};
use crate::session::Session;

/// A top track's weight (per mille).
pub const TOP_TRACK_PM: u32 = 1300;
/// The weight of a track by an artist the owner follows or plays most.
pub const ARTIST_PM: u32 = 1150;
/// The weight of a track on one of the owner's own playlists.
pub const PLAYLIST_PM: u32 = 1100;
/// The weight of a recently played track.
pub const RECENT_PM: u32 = 1100;
/// No track weighs more than this from account signals alone.
pub const MAX_TASTE_PM: u32 = 1800;
/// Bounds on one sync's reads, so a daily sync stays cheap: pages of a
/// list (followed artists, playlists), playlists read, and pages per
/// playlist (its first 200 tracks speak for it).
const MAX_PAGES: usize = 20;
const MAX_PLAYLISTS: usize = 25;
const MAX_PLAYLIST_PAGES: usize = 4;

/// What one taste read did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TasteRead {
    /// Taste scopes the grant lacks; their signals were not read.
    pub missing_scopes: Vec<&'static str>,
    pub followed_artists: usize,
    pub top_artists: usize,
    pub top_tracks: usize,
    pub recent_tracks: usize,
    /// The owner's own (or collaborative) playlists read, and their tracks.
    pub playlists: usize,
    pub playlist_tracks: usize,
    /// Reads that failed, and why (the others still count).
    pub failed: Vec<(String, String)>,
    /// Why the run stopped early, if it did.
    pub halted: Option<String>,
}

impl TasteRead {
    /// Whether some granted signal went unread (a failure or a halt): the
    /// sync then keeps the taste-only tracks it cached before.
    #[must_use]
    pub fn incomplete(&self) -> bool {
        self.halted.is_some() || !self.failed.is_empty()
    }
}

/// What the signals say: the tracks they add, and the weight of each track
/// and lead artist they name.
#[derive(Debug, Clone, Default)]
pub struct Taste {
    pub tracks: Vec<LibraryItem>,
    /// The playlists in the owner's list (`spotify:playlist:<id>`, name), in
    /// its order, when the whole list was read; `None` when it wasn't (no
    /// grant, or a failed read), so the cached ones stay.
    pub playlists: Option<Vec<(String, String)>>,
    track_pm: HashMap<String, u32>,
    artist_pm: HashMap<String, u32>,
}

impl Taste {
    fn track(&mut self, item: LibraryItem, pm: u32) {
        lift(&mut self.track_pm, &item.source_uri, pm);
        self.tracks.push(item);
    }

    fn artist(&mut self, artist: &Artist, pm: u32) {
        if let Some(id) = &artist.id {
            lift(&mut self.artist_pm, id, pm);
        }
    }

    /// Weigh `items` by the signals: each gets the product of its own and its
    /// lead artist's weights, at most [`MAX_TASTE_PM`] (`None` when no
    /// signal names it).
    pub fn weigh(&self, items: &mut [LibraryItem]) {
        for item in items {
            let own = self.track_pm.get(&item.source_uri).copied();
            let artist = item
                .artist_id
                .as_ref()
                .and_then(|id| self.artist_pm.get(id))
                .copied();
            if own.is_none() && artist.is_none() {
                continue;
            }
            let pm = own.unwrap_or(1000) * artist.unwrap_or(1000) / 1000;
            item.taste_pm = Some(pm.min(MAX_TASTE_PM));
        }
    }
}

/// Raise a key's weight by `pm` (signals multiply), up to [`MAX_TASTE_PM`].
fn lift(weights: &mut HashMap<String, u32>, key: &str, pm: u32) {
    let w = weights.entry(key.to_owned()).or_insert(1000);
    *w = (*w * pm / 1000).min(MAX_TASTE_PM);
}

/// Read the owner's taste signals the grant allows. Artists' genres are
/// cached in `store` (read at `now`). Never fails: a missing scope skips its
/// signal, a failed read is reported and the rest carry on.
pub async fn read_taste<S: Store + ?Sized>(
    session: &mut Session,
    cx: &Cx,
    store: &mut S,
    now: i64,
) -> (Taste, TasteRead) {
    let mut taste = Taste::default();
    let mut read = TasteRead {
        missing_scopes: session.missing_taste_scopes(),
        ..TasteRead::default()
    };
    let granted =
        |scope: &str| TASTE_SCOPES.contains(&scope) && !read.missing_scopes.contains(&scope);
    let (follow, top, recent, playlists) = (
        granted("user-follow-read"),
        granted("user-top-read"),
        granted("user-read-recently-played"),
        granted("playlist-read-private"),
    );
    let mut reader = Reader {
        session,
        cx,
        read: &mut read,
    };
    if follow {
        for artist in reader.followed().await {
            remember_genres(store, &artist, now, reader.read);
            taste.artist(&artist, ARTIST_PM);
            reader.read.followed_artists += 1;
        }
    }
    if top {
        let url = reader.session.endpoints().top("artists");
        for artist in reader.page::<Artist>(&url, "top artists", MAX_PAGES).await {
            remember_genres(store, &artist, now, reader.read);
            taste.artist(&artist, ARTIST_PM);
            reader.read.top_artists += 1;
        }
        let url = reader.session.endpoints().top("tracks");
        for track in reader
            .page::<FullTrack>(&url, "top tracks", MAX_PAGES)
            .await
        {
            if let Some(item) = track.library_item(Origin::Taste, None) {
                taste.track(item, TOP_TRACK_PM);
                reader.read.top_tracks += 1;
            }
        }
    }
    if recent {
        let url = reader.session.endpoints().recently_played();
        if let Some(page) = reader
            .get::<CursorPage<PlayHistory>>(&url, "recently played")
            .await
        {
            // A track played over and over counts once: recent rotation is a
            // nudge, not a top track.
            let mut heard = HashSet::new();
            for track in page.items.iter().filter_map(|h| h.track.as_ref()) {
                if let Some(item) = track.library_item(Origin::Taste, None)
                    && heard.insert(item.source_uri.clone())
                {
                    taste.track(item, RECENT_PM);
                    reader.read.recent_tracks += 1;
                }
            }
        }
    }
    if playlists {
        let (tracks, listed) = reader.playlist_tracks().await;
        taste.playlists = listed;
        for item in tracks {
            taste.track(item, PLAYLIST_PM);
        }
    }
    (taste, read)
}

/// Cache an artist's genres from a followed or top artist object, so the
/// genre read needn't ask for them.
fn remember_genres<S: Store + ?Sized>(
    store: &mut S,
    artist: &Artist,
    now: i64,
    read: &mut TasteRead,
) {
    // An artist listed without genres may still have them: leave it to the
    // genre read rather than cache "none".
    let Some(id) = artist.id.as_ref().filter(|_| !artist.genres.is_empty()) else {
        return;
    };
    if let Err(e) = store.save_artist_genres(id, &artist.genres, now) {
        read.failed
            .push((format!("artist {id}"), SpotifyError::from(e).to_string()));
    }
}

/// The taste read's requests: a halt stops every later one.
struct Reader<'a> {
    session: &'a mut Session,
    cx: &'a Cx,
    read: &'a mut TasteRead,
}

impl Reader<'_> {
    /// GET and decode one response, recording a failure or a halt.
    async fn get<T: serde::de::DeserializeOwned>(&mut self, url: &str, what: &str) -> Option<T> {
        if self.read.halted.is_some() {
            return None;
        }
        let body = match self.session.get_brief(self.cx, url).await {
            Ok(body) => body,
            Err(e) if halts(&e) => {
                self.read.halted = Some(format!("{what}: {e}"));
                return None;
            }
            Err(e) => {
                self.read.failed.push((what.to_owned(), e.to_string()));
                return None;
            }
        };
        match decode(&body, what) {
            Ok(value) => Some(value),
            Err(e) => {
                self.read.failed.push((what.to_owned(), e.to_string()));
                None
            }
        }
    }

    /// Every item of an offset-paged list, following `next` for at most
    /// `pages` pages.
    async fn page<T: serde::de::DeserializeOwned>(
        &mut self,
        url: &str,
        what: &str,
        pages: usize,
    ) -> Vec<T> {
        let mut items = Vec::new();
        let mut next = Some(url.to_owned());
        for _ in 0..pages {
            let Some(url) = next.take() else {
                break;
            };
            let Some(page) = self.get::<Paging<T>>(&url, what).await else {
                break;
            };
            items.extend(page.items);
            next = page.next;
        }
        items
    }

    /// The followed artists, following the cursor (bounded).
    async fn followed(&mut self) -> Vec<Artist> {
        let mut artists = Vec::new();
        let mut next = Some(self.session.endpoints().followed_artists());
        for _ in 0..MAX_PAGES {
            let Some(url) = next.take() else {
                break;
            };
            let Some(page) = self.get::<FollowedArtists>(&url, "followed artists").await else {
                break;
            };
            artists.extend(page.artists.items);
            next = page.artists.next;
        }
        artists
    }

    /// The tracks of the owner's own and collaborative playlists (others'
    /// playlists' items aren't readable since February 2026), and every
    /// playlist in their list when the whole list was read.
    async fn playlist_tracks(&mut self) -> (Vec<LibraryItem>, Option<Vec<(String, String)>>) {
        let Some(me) = self
            .get::<Me>(&self.session.endpoints().me(), "the user")
            .await
        else {
            return (Vec::new(), None);
        };
        let url = self.session.endpoints().my_playlists(0);
        let failed = self.read.failed.len();
        let lists: Vec<SimplifiedPlaylist> = self.page(&url, "playlists", MAX_PAGES).await;
        let listed = (self.read.failed.len() == failed && self.read.halted.is_none()).then(|| {
            lists
                .iter()
                .filter(|l| !l.id.is_empty())
                .map(|l| (format!("spotify:playlist:{}", l.id), l.name.clone()))
                .collect()
        });
        let mut tracks = Vec::new();
        for list in lists
            .iter()
            .filter(|l| l.readable_by(&me.id))
            .take(MAX_PLAYLISTS)
        {
            let url = self.session.endpoints().playlist_items(&list.id, 0);
            let entries: Vec<PlaylistItem> =
                self.page(&url, "playlist items", MAX_PLAYLIST_PAGES).await;
            self.read.playlists += 1;
            for entry in &entries {
                if let Some(item) = entry.library_item(Origin::Taste) {
                    self.read.playlist_tracks += 1;
                    tracks.push(item);
                }
            }
        }
        (tracks, listed)
    }
}

#[cfg(test)]
mod tests {
    //! Against the fake Spotify on loopback (`crate::fake_spotify`).

    use std::path::PathBuf;
    use std::sync::Arc;

    use asupersync::http::Client;
    use fsonos_core::store::{LibraryOrigin, MemStore};

    use super::*;
    use crate::cache::{pool_from_store, sync_library};
    use crate::client::{CachedToken, SCOPE, TokenCache, requested_scope};
    use crate::dj::{DjConfig, Factor, PickContext, Rng, WorkPool, pick_next};
    use crate::fake_spotify::{Fake, FakeSpotify, config, runtime, scratch_dir};

    const NOW: i64 = 1_790_035_200;

    /// A fake Spotify and a token cache granting `scope`.
    fn signed_in(name: &str, scope: &str) -> (FakeSpotify, TokenCache, PathBuf) {
        let spotify = FakeSpotify::start();
        let dir = scratch_dir(name);
        let cache = TokenCache::in_data_dir(&dir);
        cache
            .store(&CachedToken {
                access_token: "access-1".into(),
                refresh_token: "refresh-1".into(),
                expires_at: i64::MAX / 2,
                scope: scope.into(),
            })
            .unwrap();
        {
            let mut fake = spotify.state.lock().unwrap();
            "access-1".clone_into(&mut fake.access);
            "refresh-1".clone_into(&mut fake.refresh);
        }
        (spotify, cache, dir)
    }

    /// The taste requests the fake saw (log lines are `Method uri`).
    fn taste_gets(fake: &Fake) -> Vec<&str> {
        const PATHS: [&str; 5] = [
            "/v1/me/following",
            "/v1/me/top/",
            "/v1/me/player/",
            "/v1/me/playlists",
            "/v1/playlists/",
        ];
        fake.log
            .iter()
            .filter_map(|l| l.split_whitespace().nth(1))
            .filter(|uri| *uri == "/v1/me" || PATHS.iter().any(|p| uri.starts_with(p)))
            .collect()
    }

    fn by_uri(items: &[LibraryItem], n: u32) -> &LibraryItem {
        let uri = format!("spotify:track:FakeTrack{n:013}");
        items.iter().find(|i| i.source_uri == uri).unwrap()
    }

    #[test]
    fn with_the_taste_scopes_every_signal_is_read_and_weighed() {
        let (spotify, cache, dir) = signed_in("taste-all", &requested_scope());
        let endpoints = spotify.endpoints();
        let (taste, read, store) = runtime().block_on(async move {
            let cx = Cx::current().expect("ambient Cx");
            let http = Client::default_for_runtime(&cx);
            let mut session = Session::open(config(), cache, http)
                .unwrap()
                .with_endpoints(endpoints);
            let mut store = MemStore::default();
            let (taste, read) = read_taste(&mut session, &cx, &mut store, NOW).await;
            (taste, read, store)
        });
        let fake = spotify.stop();

        assert!(read.missing_scopes.is_empty(), "{read:?}");
        assert_eq!(
            (
                read.followed_artists,
                read.top_artists,
                read.top_tracks,
                read.recent_tracks
            ),
            (1, 1, 1, 2),
            "{read:?}"
        );
        // Their own playlist is read (the episode on it skipped); a friend's
        // isn't asked for.
        assert_eq!((read.playlists, read.playlist_tracks), (1, 1));
        assert!(!read.incomplete(), "{read:?}");
        let gets = taste_gets(&fake);
        assert!(gets.contains(&"/v1/me"), "{gets:?}");
        assert!(
            !gets.iter().any(|g| g.contains("FakePlaylist000000002")),
            "{gets:?}"
        );
        assert!(taste.tracks.iter().all(|t| t.origin == Origin::Taste));
        // Every playlist in their list is kept by name, a friend's too.
        assert_eq!(
            taste.playlists,
            Some(vec![
                (
                    "spotify:playlist:FakePlaylist000000001".into(),
                    "Mine".into()
                ),
                (
                    "spotify:playlist:FakePlaylist000000002".into(),
                    "A Friend's".into()
                ),
            ])
        );
        // Followed and top artists' genres go to the artist genre cache.
        assert_eq!(
            store.artist_genres("FakeArtist000000000007").unwrap(),
            Some((vec!["cool jazz".to_owned()], NOW))
        );

        // The weights: a top track by a followed artist most, then tracks by
        // a top artist, then recent plays and playlist tracks.
        let mut items = taste.tracks.clone();
        let chopin = LibraryItem {
            source_uri: "spotify:track:FakeChopin00000000001".into(),
            artist_id: Some("FakeArtist000000000003".into()),
            ..LibraryItem::default()
        };
        let other = LibraryItem {
            source_uri: "spotify:track:FakeOther000000000001".into(),
            ..LibraryItem::default()
        };
        items.extend([chopin, other]);
        taste.weigh(&mut items);
        assert_eq!(
            by_uri(&items, 101).taste_pm,
            Some(TOP_TRACK_PM * ARTIST_PM / 1000)
        );
        assert_eq!(
            by_uri(&items, 102).taste_pm,
            Some(RECENT_PM),
            "played twice lately, counted once"
        );
        assert_eq!(by_uri(&items, 103).taste_pm, Some(PLAYLIST_PM));
        assert_eq!(items[items.len() - 2].taste_pm, Some(ARTIST_PM));
        assert_eq!(items[items.len() - 1].taste_pm, None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn without_the_taste_scopes_the_library_is_the_taste() {
        let (spotify, cache, dir) = signed_in("taste-none", SCOPE);
        let endpoints = spotify.endpoints();
        let (taste, read) = runtime().block_on(async move {
            let cx = Cx::current().expect("ambient Cx");
            let http = Client::default_for_runtime(&cx);
            let mut session = Session::open(config(), cache, http)
                .unwrap()
                .with_endpoints(endpoints);
            read_taste(&mut session, &cx, &mut MemStore::default(), NOW).await
        });
        let fake = spotify.stop();

        assert_eq!(
            read.missing_scopes, TASTE_SCOPES,
            "a warning, not a failure"
        );
        assert!(taste.tracks.is_empty() && !read.incomplete());
        assert_eq!(taste.playlists, None, "no playlists read");
        assert!(taste_gets(&fake).is_empty(), "nothing was asked for");
        assert_eq!(
            CachedToken {
                access_token: String::new(),
                refresh_token: String::new(),
                expires_at: 0,
                scope: SCOPE.into(),
            }
            .missing_taste_scopes(),
            TASTE_SCOPES
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_sync_adds_weighs_and_keeps_the_taste_tracks() {
        let (spotify, cache, dir) = signed_in("taste-sync", &requested_scope());
        let endpoints = spotify.endpoints();
        let state = Arc::clone(&spotify.state);
        let (first, second, pool, rows, playlists) = runtime().block_on(async move {
            let cx = Cx::current().expect("ambient Cx");
            let http = Client::default_for_runtime(&cx);
            let mut session = Session::open(config(), cache, http)
                .unwrap()
                .with_endpoints(endpoints);
            let mut store = MemStore::default();
            let first = sync_library(&mut session, &cx, &mut store).await.unwrap();
            // The taste reads fail next time: what they added stays.
            state.lock().unwrap().taste_down = true;
            let second = sync_library(&mut session, &cx, &mut store).await.unwrap();
            (
                first,
                second,
                pool_from_store(&store).unwrap(),
                store.library().unwrap(),
                store.playlists().unwrap(),
            )
        });
        spotify.stop();

        // The first sync's playlists stay through the failed second read.
        assert_eq!(
            playlists,
            [
                (
                    "spotify:playlist:FakePlaylist000000001".to_owned(),
                    "Mine".to_owned()
                ),
                (
                    "spotify:playlist:FakePlaylist000000002".to_owned(),
                    "A Friend's".to_owned()
                ),
            ]
        );
        // The library's 6 tracks, plus the top track, a recent play and a
        // playlist track (the recently played aria is already saved).
        assert_eq!((first.tracks, first.candidates, first.retired), (9, 9, 0));
        assert_eq!(
            second.retired, 0,
            "an unfinished taste read retires nothing"
        );
        let top = pool.get("spotify:track:FakeTrack0000000000101").unwrap();
        assert_eq!(top.taste_pm, TOP_TRACK_PM * ARTIST_PM / 1000);
        assert_eq!(top.origin, Origin::Taste);
        let aria = rows
            .iter()
            .find(|r| r.track.source_uri == "spotify:track:FakeTrack0000000000001")
            .unwrap();
        assert_eq!(
            aria.origin,
            LibraryOrigin::SavedAlbum,
            "a saved sighting outranks a recent play"
        );
        assert_eq!(aria.taste_weight, Some(RECENT_PM));

        // The DJ weighs it, and says so.
        let works = WorkPool::new(&pool);
        let only = crate::steer::Steer {
            mood: None,
            constraints: crate::steer::DjConstraints {
                include_keywords: vec!["take the long way".into()],
                ..crate::steer::DjConstraints::default()
            },
        };
        let narrow = DjConfig {
            min_steered_works: 1,
            ..DjConfig::default()
        };
        let ctx = PickContext {
            steer: Some(&only),
            ..PickContext::default()
        };
        let pick = pick_next(&works, &ctx, &narrow, &mut Rng::new(1)).unwrap();
        assert!(pick.reason.has(Factor::Taste), "{:?}", pick.reason);
        assert!(
            pick.reason.summary.contains("in your Spotify listening"),
            "{}",
            pick.reason.summary
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
