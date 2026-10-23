//! Resume ordered reads only while Spotify's playlist version stays unchanged.
//! Private checkpoints never replace a complete published cache with a prefix.

use super::paging::Observation;
use super::{MAX_PLAYLIST_REQUESTS, PlaylistObservation, Reader, valid_playlist_track};
use crate::client::{PAGE_LIMIT, PlaylistItem, SimplifiedPlaylist};
use crate::library::{LibraryItem, Origin};
use fsonos_core::store::{
    CachedPlaylist, MAX_PLAYLIST_BYTES, MAX_PLAYLIST_ENTRIES, PlaylistTrack, Store, StoreError,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Deserialize)]
struct Snapshot {
    id: String,
    #[serde(rename = "snapshot_id")]
    revision: String,
}

#[derive(Serialize, Deserialize)]
struct Occurrence {
    position: u32,
    item: LibraryItem,
}

#[derive(Serialize, Deserialize)]
struct Progress {
    version: u8,
    owner_id: String,
    playlist_uri: String,
    snapshot_id: String,
    source_total: Option<u32>,
    examined: u32,
    complete: bool,
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
            tracks: Vec::new(),
        }
    }

    fn valid(&self, owner: &str, uri: &str) -> bool {
        if self.version != 1
            || self.owner_id != owner
            || self.playlist_uri != uri
            || !version(&self.snapshot_id)
            || self.examined > MAX_PLAYLIST_ENTRIES
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
                && valid_playlist_track(&track.item);
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
                .filter(valid_playlist_track)
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
        // Completed observations are refreshed on the next turn. An edit
        // discards private progress, never the last complete published cache.
        let mut progress = previous
            .filter(|state| !state.complete && state.snapshot_id == snapshot)
            .unwrap_or_else(|| Progress::new(owner, &uri, &snapshot));
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
        let encoded = match serde_json::to_string(&progress) {
            Ok(encoded) if encoded.len() <= MAX_PLAYLIST_BYTES => encoded,
            _ => {
                self.playlist_failed("playlist progress exceeds its 16 MiB metadata limit");
                return None;
            }
        };
        // A halt leaves the old checkpoint intact: without this final read,
        // even a successful page prefix cannot be joined to another window.
        if self.playlist_snapshot(&list.id).await? != snapshot {
            self.playlist_failed(
                "playlist changed while reading; its next turn restarts from the beginning",
            );
            return None;
        }
        // A malformed window can still supply a useful first observation,
        // but it must not move the durable restart point past its failure.
        if !failed && let Err(error) = store.save_playlist_progress(owner, &uri, &encoded) {
            self.playlist_failed(error);
            return None;
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
    ) -> (Vec<LibraryItem>, Option<PlaylistObservation>) {
        self.read.playlists += 1;
        let Some((progress, failed)) = self.playlist_window(store, owner, list).await else {
            return (Vec::new(), None);
        };
        let observation = PlaylistObservation {
            contents: progress.observation(now),
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
        self.read.playlist_tracks += tracks.len();
        (tracks, Some(observation))
    }
}
