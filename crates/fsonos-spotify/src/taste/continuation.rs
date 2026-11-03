//! Playlist-only passes finish bounded reads without rereading saved music.

use super::{MAX_PAGES, MAX_PLAYLISTS, PlaylistObservation, Reader, Taste, TasteRead};
use crate::client::{Me, SimplifiedPlaylist};
use crate::library::LibraryItem;
use crate::session::Session;
use asupersync::Cx;
use fsonos_core::store::Store;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Durable scheduling evidence. Counts describe the current account's listed
/// playlists; no Spotify requests or speaker actions are implied by reading it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistContinuation {
    pub owner: Option<String>,
    /// Unread or unfinished snapshots, plus one when the directory is incomplete.
    pub pending: usize,
    /// Snapshots at the entry or metadata bound; they do not schedule more reads.
    pub limited: usize,
    /// Raw positions newly accepted during this pass, for no-progress backoff.
    pub advanced: u32,
    /// Remaining server cooldown, rounded up; never shorten it on restart.
    pub retry_after_secs: u64,
    /// A changed signed-in account requires a full sync before merging more music.
    pub requires_refresh: bool,
}

#[derive(Clone, Copy)]
pub(super) enum Scan<'a> {
    Refresh,
    Pending(&'a str),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Readiness {
    Pending,
    Complete,
    Limited,
}

/// The same playlist reader, without liked tracks, saved albums, top items,
/// recent listening, followed artists, or genre endpoint requests.
pub(crate) async fn read_pending_playlists<S: Store + ?Sized>(
    session: &mut Session,
    cx: &Cx,
    store: &mut S,
    owner: &str,
    now: i64,
) -> (Taste, TasteRead) {
    let mut taste = Taste::default();
    let mut read = TasteRead {
        missing_scopes: session.missing_taste_scopes(),
        ..TasteRead::default()
    };
    if !read.missing_scopes.contains(&"playlist-read-private") {
        // Preserve retry readiness when even the owner or directory read fails.
        read.continuation.owner = Some(owner.to_owned());
        read.continuation.pending = 1;
        let mut reader = Reader {
            session,
            cx,
            read: &mut read,
        };
        let (tracks, listed) = reader
            .playlist_tracks(
                store,
                &mut taste.named,
                &mut taste.playlist_contents,
                now,
                Scan::Pending(owner),
            )
            .await;
        taste.playlists = listed;
        for item in tracks {
            taste.track(item, super::PLAYLIST_PM);
        }
    }
    (taste, read)
}

impl Reader<'_> {
    /// Keep only authoritative directory replacements. A partial directory can
    /// yield work, but cannot prune checkpoints or claim that intake finished.
    pub(super) async fn playlist_tracks<S: Store + ?Sized>(
        &mut self,
        store: &mut S,
        named: &mut HashMap<String, Vec<String>>,
        observations: &mut Vec<PlaylistObservation>,
        now: i64,
        scan: Scan<'_>,
    ) -> (Vec<LibraryItem>, Option<Vec<(String, String)>>) {
        let Some(me) = self
            .get::<Me>(&self.session.endpoints().me(), "the user")
            .await
        else {
            return (Vec::new(), None);
        };
        if me.id.trim().is_empty() || me.id.len() > 1_024 || me.id.chars().any(char::is_control) {
            self.read
                .failed
                .push(("the user".into(), "invalid Spotify owner identity".into()));
            return (Vec::new(), None);
        }
        self.read.continuation.owner = Some(me.id.clone());
        self.read.continuation.pending = 1;
        if matches!(scan, Scan::Pending(owner) if owner != me.id) {
            self.read.continuation.requires_refresh = true;
            self.read.continuation.pending = 0;
            self.read.failed.push((
                "playlist continuation".into(),
                "the Spotify account changed; a full library refresh is required".into(),
            ));
            return (Vec::new(), None);
        }
        let url = self.session.endpoints().my_playlists(0);
        let failed = self.read.failed.len();
        let lists: Vec<SimplifiedPlaylist> = self.page(&url, "playlists", MAX_PAGES).await;
        let listed = (self.read.failed.len() == failed && self.read.halted.is_none()).then(|| {
            lists
                .iter()
                .filter(|list| !list.id.is_empty())
                .map(|list| (format!("spotify:playlist:{}", list.id), list.name.clone()))
                .collect()
        });
        let mut readable: Vec<_> = lists
            .iter()
            .filter(|list| !list.id.is_empty() && list.readable_by(&me.id))
            .collect();
        readable.sort_by(|a, b| a.id.cmp(&b.id));
        readable.dedup_by(|a, b| a.id == b.id);
        if listed.is_some() {
            let active = readable
                .iter()
                .map(|list| format!("spotify:playlist:{}", list.id))
                .collect::<Vec<_>>();
            if let Err(error) = store.retain_playlist_progress(&me.id, &active) {
                self.read
                    .failed
                    .push(("playlist progress".into(), error.to_string()));
                return (Vec::new(), listed);
            }
        }
        self.read.continuation.pending = usize::from(listed.is_none());
        let tracks = self
            .scan_playlists(store, &me.id, readable, named, observations, now, scan)
            .await;
        (tracks, listed)
    }

    fn readiness<S: Store + ?Sized>(
        &mut self,
        store: &S,
        owner: &str,
        list: &SimplifiedPlaylist,
    ) -> Readiness {
        match super::playlist_scan::readiness(store, owner, list) {
            Ok(state) => state,
            Err(error) => {
                self.read
                    .failed
                    .push(("playlist progress".into(), error.to_string()));
                Readiness::Pending
            }
        }
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "one bounded scan shares its reader, store and output projections"
    )]
    async fn scan_playlists<S: Store + ?Sized>(
        &mut self,
        store: &mut S,
        owner: &str,
        mut readable: Vec<&SimplifiedPlaylist>,
        named: &mut HashMap<String, Vec<String>>,
        observations: &mut Vec<PlaylistObservation>,
        now: i64,
        scan: Scan<'_>,
    ) -> Vec<LibraryItem> {
        let mut tracks = Vec::new();
        for list in &readable {
            match self.readiness(store, owner, list) {
                Readiness::Pending => self.read.continuation.pending += 1,
                Readiness::Limited => self.read.continuation.limited += 1,
                Readiness::Complete => {}
            }
        }
        if readable.is_empty() || self.read.halted.is_some() {
            return tracks;
        }
        let cursor = match store.playlist_scan_cursor(owner) {
            Ok(cursor) => cursor,
            Err(error) => {
                self.read
                    .failed
                    .push(("playlist scan position".into(), error.to_string()));
                return tracks;
            }
        };
        let start = readable
            .partition_point(|list| cursor.as_ref().is_some_and(|last| list.id <= *last))
            % readable.len();
        readable.rotate_left(start);
        let mut attempted = 0;
        for list in &readable {
            if self.read.halted.is_some() {
                break;
            }
            let before = self.readiness(store, owner, list);
            if matches!(scan, Scan::Pending(_)) && before != Readiness::Pending {
                continue;
            }
            if attempted == MAX_PLAYLISTS {
                self.read.failed.push((
                    "playlist items".into(),
                    "taste playlist budget reached before every readable playlist was visited"
                        .into(),
                ));
                break;
            }
            // Advance before I/O: a failed playlist yields to later IDs even
            // across restarts. Skipped complete/capped lists spend no request.
            if let Err(error) = store.save_playlist_scan_cursor(owner, &list.id) {
                self.read
                    .failed
                    .push(("playlist scan position".into(), error.to_string()));
                break;
            }
            attempted += 1;
            let (items, observation) = self
                .read_playlist(
                    store,
                    owner,
                    list,
                    named,
                    now,
                    matches!(scan, Scan::Pending(_)),
                )
                .await;
            tracks.extend(items);
            observations.extend(observation);
            let after = self.readiness(store, owner, list);
            self.read.continuation.pending = self
                .read
                .continuation
                .pending
                .saturating_sub(usize::from(before == Readiness::Pending))
                .saturating_add(usize::from(after == Readiness::Pending));
            self.read.continuation.limited = self
                .read
                .continuation
                .limited
                .saturating_sub(usize::from(before == Readiness::Limited))
                .saturating_add(usize::from(after == Readiness::Limited));
        }
        tracks
    }
}
