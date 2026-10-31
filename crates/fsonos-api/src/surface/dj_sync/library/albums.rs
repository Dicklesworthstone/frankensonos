//! Album drill-down combines a complete catalog sequence with exact library metadata.
//! Catalog-only tracks remain outside the personal library and DJ candidates.

use super::{BrowseItem, item, source};
use fsonos_core::store::{CachedAlbum, LibraryEntry};
use std::collections::HashMap;

pub(super) fn items(
    entries: &[&LibraryEntry],
    album_uri: &str,
    album: &CachedAlbum,
) -> Vec<BrowseItem> {
    let known: HashMap<&str, &LibraryEntry> = entries
        .iter()
        .map(|entry| (entry.track.source_uri.as_str(), *entry))
        .collect();
    let title = entries
        .iter()
        .filter(|entry| entry.album_uri.as_deref() == Some(album_uri))
        .find_map(|entry| entry.track.album.as_deref());
    let mut rows = album
        .tracks
        .iter()
        .filter(|track| {
            source(&track.source_uri, "track") && track.disc_number > 0 && track.track_number > 0
        })
        .map(|track| {
            let saved = known.get(track.source_uri.as_str());
            let mut row = item(
                "track",
                &track.source_uri,
                &track.title,
                saved
                    .and_then(|entry| entry.track.artist.as_deref())
                    .unwrap_or(""),
            );
            row.album = title.map(str::to_owned);
            row.album_uri = Some(album_uri.to_owned());
            row.disc_number = Some(track.disc_number);
            row.track_number = Some(track.track_number);
            row.duration_secs = track.duration_secs;
            row.explicit = saved.is_some_and(|entry| entry.explicit);
            row.explicit_known = saved.is_some();
            row
        })
        .collect::<Vec<_>>();
    rows.sort_by_key(|row| (row.disc_number, row.track_number));
    rows
}

#[cfg(test)]
mod tests;
