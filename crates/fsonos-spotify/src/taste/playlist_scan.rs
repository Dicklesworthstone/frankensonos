//! Resume ordered reads only while Spotify's playlist version stays unchanged.
//! Private checkpoints never replace a complete published cache with a prefix.

use super::continuation::Readiness;
use super::paging::Observation;
use super::{MAX_PLAYLIST_REQUESTS, PlaylistObservation, Reader, valid_taste_track};
use crate::client::{PAGE_LIMIT, PlaylistItem, SimplifiedPlaylist};
use crate::library::{LibraryItem, Origin};
use fsonos_core::store::{
    CachedPlaylist, MAX_PLAYLIST_BYTES, MAX_PLAYLIST_ENTRIES, PlaylistTrack, SpotifyLibraryOwner,
    Store, StoreError,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Deserialize)]
struct Snapshot {
    id: String,
    #[serde(rename = "snapshot_id")]
    revision: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct Occurrence {
    position: u32,
    item: LibraryItem,
}

#[derive(Clone, Serialize, Deserialize)]
struct Progress {
    version: u8,
    owner_id: String,
    playlist_uri: String,
    snapshot_id: String,
    source_total: Option<u32>,
    examined: u32,
    complete: bool,
    #[serde(default)]
    applied: bool,
    #[serde(default)]
    fetched_at: Option<i64>,
    /// Next metadata window within an already complete, unchanged snapshot.
    #[serde(default)]
    refresh_offset: u32,
    /// A metadata bound is terminal for this snapshot, until Spotify changes it.
    #[serde(default)]
    limited: bool,
    tracks: Vec<Occurrence>,
}

fn version(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 1_024 && !value.chars().any(char::is_control)
}

impl Progress {
    fn new(owner: &str, uri: &str, snapshot: &str) -> Self {
        Self {
            version: 1,
            owner_id: owner.into(),
            playlist_uri: uri.into(),
            snapshot_id: snapshot.into(),
            source_total: None,
            examined: 0,
            complete: false,
            applied: false,
            fetched_at: None,
            refresh_offset: 0,
            limited: false,
            tracks: Vec::new(),
        }
    }

    fn valid(&self, owner: &str, uri: &str) -> bool {
        if self.version != 1
            || self.owner_id != owner
            || self.playlist_uri != uri
            || !version(&self.snapshot_id)
            || (self.limited && self.complete)
            || self.examined > MAX_PLAYLIST_ENTRIES
            || (self.refresh_offset != 0
                && (!self.complete || self.refresh_offset >= self.examined))
            || self.source_total.is_some_and(|total| {
                self.examined > total || (self.complete && self.examined != total)
            })
        {
            return false;
        }
        let mut previous = 0;
        self.tracks.iter().all(|track| {
            let valid = track.position > previous
                && track.position <= self.examined
                && track.item.origin == Origin::Taste
                && valid_taste_track(&track.item);
            previous = track.position;
            valid
        })
    }

    fn append(&mut self, entries: Observation<serde_json::Value>) {
        for (position, entry) in (self.examined + 1..).zip(entries.items) {
            if entry.get("is_local").and_then(serde_json::Value::as_bool) == Some(true) {
                continue;
            }
            if let Some(item) = serde_json::from_value::<PlaylistItem>(entry)
                .ok()
                .and_then(|entry| entry.library_item(Origin::Taste))
                .filter(valid_taste_track)
            {
                self.tracks.push(Occurrence { position, item });
            }
        }
        self.examined = entries.examined;
        self.source_total = entries.source_total;
        self.complete = entries.complete;
    }

    fn observation(&self, now: i64) -> CachedPlaylist {
        let tracks = self
            .tracks
            .iter()
            .map(|occurrence| {
                let item = &occurrence.item;
                let track = item.to_track();
                PlaylistTrack {
                    position: occurrence.position,
                    source_uri: track.source_uri,
                    title: track.title,
                    artist: track.artist,
                    album: track.album,
                    album_uri: item.album_uri.clone(),
                    disc_number: item.disc_number,
                    track_number: item.track_number,
                    duration_secs: item.duration_secs,
                    explicit: item.explicit,
                }
            })
            .collect();
        CachedPlaylist::new(
            self.playlist_uri.clone(),
            now,
            self.source_total,
            self.examined,
            self.complete,
            tracks,
        )
    }
}

fn load_progress<S: Store + ?Sized>(
    store: &S,
    owner: &str,
    uri: &str,
) -> Result<Option<Progress>, StoreError> {
    let stored = store.playlist_progress(owner, uri)?;
    match stored
        .as_deref()
        .map(serde_json::from_str::<Progress>)
        .transpose()
    {
        Ok(previous)
            if previous
                .as_ref()
                .is_none_or(|state| state.valid(owner, uri)) =>
        {
            Ok(previous)
        }
        _ => Err(StoreError::Backend(
            "stored playlist progress is invalid".into(),
        )),
    }
}

/// A complete/capped unchanged snapshot is not work for a continuation pass.
/// Without a directory version, completed observations wait for the next full
/// refresh; pending windows still perform their normal preflight version check.
pub(super) fn readiness<S: Store + ?Sized>(
    store: &S,
    owner: &str,
    list: &SimplifiedPlaylist,
) -> Result<Readiness, StoreError> {
    let uri = format!("spotify:playlist:{}", list.id);
    let Some(progress) = load_progress(store, owner, &uri)? else {
        return Ok(Readiness::Pending);
    };
    if list
        .snapshot_id
        .as_ref()
        .is_some_and(|snapshot| *snapshot != progress.snapshot_id)
    {
        return Ok(Readiness::Pending);
    }
    Ok(if progress.limited {
        Readiness::Limited
    } else if !progress.applied {
        // Snapshot proof can precede a failed public-cache/library write.
        Readiness::Pending
    } else if progress.complete {
        Readiness::Complete
    } else if progress.examined >= MAX_PLAYLIST_ENTRIES {
        Readiness::Limited
    } else {
        Readiness::Pending
    })
}

/// Cache projection is a separate durable step from the snapshot proof. Only
/// acknowledge the exact observation that was just published and merged.
/// Returns whether this acknowledgement completes or caps the scan.
pub(crate) fn mark_applied<S: Store + ?Sized>(
    store: &mut S,
    owner: &SpotifyLibraryOwner,
    contents: &CachedPlaylist,
) -> Result<(bool, bool), StoreError> {
    let Some(expected) = store.playlist_progress(&owner.owner_id, &contents.playlist_uri)? else {
        return Ok((false, false));
    };
    let mut progress: Progress =
        serde_json::from_str(&expected).map_err(|error| StoreError::Backend(error.to_string()))?;
    if !progress.valid(&owner.owner_id, &contents.playlist_uri) {
        return Err(StoreError::Backend(
            "stored playlist progress is invalid".into(),
        ));
    }
    if progress.applied
        || progress.limited
        || progress.observation(contents.fetched_at) != *contents
    {
        return Ok((false, false));
    }
    progress.applied = true;
    let encoded =
        serde_json::to_string(&progress).map_err(|error| StoreError::Backend(error.to_string()))?;
    if !store.acknowledge_spotify_playlist(owner, &contents.playlist_uri, &expected, &encoded)? {
        return Ok((false, false));
    }
    Ok((
        progress.complete,
        !progress.complete && progress.examined >= MAX_PLAYLIST_ENTRIES,
    ))
}

impl Reader<'_> {
    fn playlist_failed(&mut self, detail: impl std::fmt::Display) {
        self.read
            .failed
            .push(("playlist items".into(), detail.to_string()));
    }

    async fn playlist_snapshot(&mut self, id: &str) -> Option<String> {
        let url = self.session.endpoints().playlist_snapshot(id);
        let snapshot = self.get::<Snapshot>(&url, "playlist snapshot").await?;
        if snapshot.id != id || !version(&snapshot.revision) {
            self.playlist_failed("playlist snapshot has a different identity or no version");
            return None;
        }
        Some(snapshot.revision)
    }

    async fn playlist_window<S: Store + ?Sized>(
        &mut self,
        store: &mut S,
        owner: &str,
        list: &SimplifiedPlaylist,
        now: i64,
        pending_only: bool,
    ) -> Option<(Progress, bool)> {
        let uri = format!("spotify:playlist:{}", list.id);
        let previous = match load_progress(store, owner, &uri) {
            Ok(previous) => previous,
            Err(error) => {
                self.playlist_failed(error);
                return None;
            }
        };
        let (snapshot, pages) = match &list.snapshot_id {
            Some(snapshot) if version(snapshot) => (snapshot.clone(), MAX_PLAYLIST_REQUESTS - 1),
            Some(_) => {
                self.playlist_failed("playlist directory has an invalid version");
                return None;
            }
            None => (
                self.playlist_snapshot(&list.id).await?,
                MAX_PLAYLIST_REQUESTS - 2,
            ),
        };
        if pending_only
            && let Some(ready) = previous.as_ref().filter(|state| {
                !state.applied
                    && !state.limited
                    && (state.complete || state.examined >= MAX_PLAYLIST_ENTRIES)
                    && state.snapshot_id == snapshot
            })
        {
            // A prior projection write failed. Reuse its proved local contents,
            // not another Spotify read or a fabricated new fetched-at time.
            return Some((ready.clone(), false));
        }
        let previous = match previous {
            Some(progress)
                if progress.complete && !progress.limited && progress.snapshot_id == snapshot =>
            {
                // Membership is proved for this version. Refresh metadata inside
                // that whole proof instead of turning a long complete list into
                // a prefix and preventing authoritative library reconciliation.
                return self
                    .refresh_snapshot(store, progress, &list.id, pages, now)
                    .await;
            }
            previous => previous,
        };
        // An edit discards private progress, never the last complete public cache.
        let mut progress = previous
            .filter(|state| !state.complete && state.snapshot_id == snapshot)
            .unwrap_or_else(|| Progress::new(owner, &uri, &snapshot));
        if progress.limited {
            self.playlist_failed("playlist snapshot reached its 16 MiB metadata limit");
            return None;
        }
        let first = progress.examined;
        let url = self.session.endpoints().playlist_items(&list.id, first);
        let entries = self
            .observe_pages_from::<serde_json::Value>(
                &url,
                "playlist items",
                pages,
                Some(PAGE_LIMIT as usize),
                (first, progress.source_total),
                Some(MAX_PLAYLIST_ENTRIES),
            )
            .await;
        if entries.examined == first && !entries.complete && first == 0 {
            return None;
        }
        let failed = entries.failed;
        progress.append(entries);
        progress.applied = false;
        progress.fetched_at = Some(now);
        self.accept_window(store, progress, failed, first).await
    }

    async fn refresh_snapshot<S: Store + ?Sized>(
        &mut self,
        store: &mut S,
        mut progress: Progress,
        id: &str,
        pages: usize,
        now: i64,
    ) -> Option<(Progress, bool)> {
        let first = progress.refresh_offset;
        let url = self.session.endpoints().playlist_items(id, first);
        let entries = self
            .observe_window_from::<serde_json::Value>(
                &url,
                "playlist items",
                pages,
                Some(PAGE_LIMIT as usize),
                (first, progress.source_total),
                Some(MAX_PLAYLIST_ENTRIES),
            )
            .await;
        if entries.failed {
            return None;
        }
        if entries.examined > progress.examined
            || (entries.complete && entries.examined != progress.examined)
            || (!entries.complete && entries.examined >= progress.examined)
            || entries
                .source_total
                .is_some_and(|total| total != progress.examined)
        {
            self.playlist_failed("unchanged playlist snapshot has different source coverage");
            return None;
        }
        if self.playlist_snapshot(id).await? != progress.snapshot_id {
            self.playlist_failed("playlist changed during its metadata refresh");
            return None;
        }
        let mut window = Progress::new(
            &progress.owner_id,
            &progress.playlist_uri,
            &progress.snapshot_id,
        );
        window.examined = first;
        window.append(entries);
        // Raw positions, including null/unavailable entries, define the replaced
        // interval. A track disappearing within it must not survive from cache.
        progress
            .tracks
            .retain(|entry| entry.position <= first || entry.position > window.examined);
        progress.tracks.extend(window.tracks);
        progress.tracks.sort_by_key(|entry| entry.position);
        progress.refresh_offset = if window.complete { 0 } else { window.examined };
        progress.source_total = window.source_total;
        progress.fetched_at = Some(now);
        progress.applied = false;
        let encoded = match serde_json::to_string(&progress) {
            Ok(encoded) if encoded.len() <= MAX_PLAYLIST_BYTES => encoded,
            _ => {
                self.playlist_failed("playlist metadata refresh exceeds its 16 MiB limit");
                return None;
            }
        };
        if let Err(error) =
            store.save_playlist_progress(&progress.owner_id, &progress.playlist_uri, &encoded)
        {
            self.playlist_failed(error);
            return None;
        }
        self.read.continuation.advanced = self
            .read
            .continuation
            .advanced
            .saturating_add(window.examined.saturating_sub(first));
        Some((progress, false))
    }

    /// Unvisited full-sync lists may contribute their whole proved contents
    /// only when today's directory explicitly confirms that same version.
    pub(super) fn reuse_playlist<S: Store + ?Sized>(
        &mut self,
        store: &S,
        owner: &str,
        list: &SimplifiedPlaylist,
        named: &mut HashMap<String, Vec<String>>,
    ) -> Option<(Vec<LibraryItem>, PlaylistObservation)> {
        let snapshot = list
            .snapshot_id
            .as_deref()
            .filter(|snapshot| version(snapshot))?;
        let uri = format!("spotify:playlist:{}", list.id);
        let progress = match load_progress(store, owner, &uri) {
            Ok(progress) => progress?,
            Err(error) => {
                self.playlist_failed(error);
                return None;
            }
        };
        if !progress.complete || progress.limited || progress.snapshot_id != snapshot {
            return None;
        }
        let (tracks, observation) = project(progress, list, named, false);
        self.read.playlist_tracks += tracks.len();
        Some((tracks, observation))
    }

    async fn accept_window<S: Store + ?Sized>(
        &mut self,
        store: &mut S,
        mut progress: Progress,
        failed: bool,
        first: u32,
    ) -> Option<(Progress, bool)> {
        let id = progress.playlist_uri.strip_prefix("spotify:playlist:")?;
        let encoded = serde_json::to_string(&progress).ok();
        let too_large = encoded
            .as_ref()
            .is_none_or(|encoded| encoded.len() > MAX_PLAYLIST_BYTES);
        // A halt leaves the old checkpoint intact: without this final read,
        // even a successful page prefix cannot be joined to another window.
        if self.playlist_snapshot(id).await? != progress.snapshot_id {
            self.playlist_failed(
                "playlist changed while reading; its next turn restarts from the beginning",
            );
            return None;
        }
        if too_large {
            // Keep the published last-good contents, and a small terminal
            // marker so background passes do not repeat this oversized window.
            progress.limited = true;
            progress.complete = false;
            progress.tracks.clear();
            match serde_json::to_string(&progress) {
                Ok(marker) => {
                    if let Err(error) = store.save_playlist_progress(
                        &progress.owner_id,
                        &progress.playlist_uri,
                        &marker,
                    ) {
                        self.playlist_failed(error);
                    }
                }
                Err(error) => self.playlist_failed(error),
            }
            self.playlist_failed("playlist progress exceeds its 16 MiB metadata limit");
            return None;
        }
        // A malformed window can still supply a useful first observation,
        // but it must not move the durable restart point past its failure.
        if !failed {
            if let Err(error) = store.save_playlist_progress(
                &progress.owner_id,
                &progress.playlist_uri,
                encoded.as_deref().unwrap_or(""),
            ) {
                self.playlist_failed(error);
                return None;
            }
            self.read.continuation.advanced = self
                .read
                .continuation
                .advanced
                .saturating_add(progress.examined.saturating_sub(first));
        }
        if !progress.complete {
            self.playlist_failed(format!(
                "snapshot checked through source position {}{}; {}",
                progress.examined,
                progress
                    .source_total
                    .map_or_else(String::new, |total| format!(" of {total}")),
                if progress.examined == MAX_PLAYLIST_ENTRIES {
                    "10,000-entry cache limit reached"
                } else {
                    "continuing on the playlist's next sync turn"
                },
            ));
        }
        Some((progress, failed))
    }

    pub(super) async fn read_playlist<S: Store + ?Sized>(
        &mut self,
        store: &mut S,
        owner: &str,
        list: &SimplifiedPlaylist,
        named: &mut HashMap<String, Vec<String>>,
        now: i64,
        pending_only: bool,
    ) -> (Vec<LibraryItem>, Option<PlaylistObservation>) {
        self.read.playlists += 1;
        let Some((progress, failed)) = self
            .playlist_window(store, owner, list, now, pending_only)
            .await
        else {
            return (Vec::new(), None);
        };
        let (tracks, observation) = project(progress, list, named, failed);
        self.read.playlist_tracks += tracks.len();
        (tracks, Some(observation))
    }
}

fn project(
    progress: Progress,
    list: &SimplifiedPlaylist,
    named: &mut HashMap<String, Vec<String>>,
    failed: bool,
) -> (Vec<LibraryItem>, PlaylistObservation) {
    let observation = PlaylistObservation {
        contents: progress.observation(progress.fetched_at.unwrap_or(0)),
        failed,
    };
    let stated = crate::genre_guess::from_name(&list.name);
    let tracks = progress
        .tracks
        .into_iter()
        .map(|entry| {
            let item = entry.item;
            if !stated.is_empty() {
                let genres = named.entry(item.source_uri.clone()).or_default();
                for genre in &stated {
                    if !genres.contains(genre) {
                        genres.push(genre.clone());
                    }
                }
            }
            item
        })
        .collect::<Vec<_>>();
    (tracks, observation)
}
