//! Fair artist sampling across accepted full refreshes. The cursor is only
//! scheduling advice; its exact receipt is consumed with library publication.

use super::{Artist, MAX_ARTIST_READS};
use crate::SpotifyError;
use crate::client::random_state;
use fsonos_core::store::SpotifyArtistDiscoveryFence;
use serde::{Deserialize, Serialize};

const VERSION: u8 = 1;
const MAX_PROGRESS_BYTES: usize = 1_024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Progress {
    version: u8,
    last_artist_id: Option<String>,
    publication: String,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

/// An invalid scheduling hint may be replaced by a successful full refresh.
/// The caller reports that recovery and retains all previous taste candidates.
pub(super) fn cursor(raw: Option<&str>) -> Result<Option<String>, SpotifyError> {
    let Some(raw) = raw else { return Ok(None) };
    let invalid = || SpotifyError::Decode("invalid artist discovery cursor".into());
    if raw.len() > MAX_PROGRESS_BYTES {
        return Err(invalid());
    }
    let progress: Progress = serde_json::from_str(raw).map_err(|_| invalid())?;
    if progress.version != VERSION
        || progress.publication.is_empty()
        || progress.publication.len() > 128
        || progress.publication.chars().any(char::is_control)
        || progress
            .last_artist_id
            .as_deref()
            .is_some_and(|id| !valid_id(id))
    {
        return Err(invalid());
    }
    Ok(progress.last_artist_id)
}

/// Sort by stable identity, resume strictly after the accepted cursor and wrap.
/// Missing or newly covered cursor artists need no special case. New names or
/// response ordering cannot reset the rotation or hide identically named artists.
pub(super) fn select<'a>(
    mut artists: Vec<&'a Artist>,
    cursor: Option<&str>,
) -> (Vec<&'a Artist>, Vec<&'a Artist>) {
    artists.retain(|artist| artist.id.as_deref().is_some_and(valid_id));
    artists.sort_by(|left, right| left.id.cmp(&right.id));
    artists.dedup_by(|left, right| left.id == right.id);
    if let Some(cursor) = cursor {
        let start =
            artists.partition_point(|artist| artist.id.as_deref().is_some_and(|id| id <= cursor));
        artists.rotate_left(start);
    }
    let later = artists.split_off(artists.len().min(MAX_ARTIST_READS));
    (artists, later)
}

/// A fresh nonce changes the receipt even when no artist was attempted, or a
/// complete traversal returns to the same cursor. The store commits it only if
/// the original receipt and the full saved-library proof still match exactly.
pub(crate) fn prepare_publication(
    expected: Option<&str>,
    cursor: Option<&str>,
) -> Result<SpotifyArtistDiscoveryFence, SpotifyError> {
    if cursor.is_some_and(|id| !valid_id(id)) {
        return Err(SpotifyError::Decode(
            "invalid artist discovery publication".into(),
        ));
    }
    let progress = serde_json::to_string(&Progress {
        version: VERSION,
        last_artist_id: cursor.map(str::to_owned),
        publication: random_state()?,
    })
    .map_err(|error| SpotifyError::Decode(error.to_string()))?;
    Ok(SpotifyArtistDiscoveryFence {
        expected: expected.map(str::to_owned),
        progress,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artists(count: usize) -> Vec<Artist> {
        (0..count)
            .map(|number| Artist {
                id: Some(format!("Artist{number:04}")),
                name: "A shared artist name".into(),
                genres: Vec::new(),
            })
            .collect()
    }

    #[test]
    fn accepted_receipts_continue_on_the_same_day_and_wrap() {
        let all = artists(120);
        let mut accepted = None;
        let mut seen = std::collections::HashSet::new();
        for expected in ["Artist0000", "Artist0050", "Artist0100"] {
            let cursor = cursor(accepted.as_deref()).unwrap();
            let (selected, later) = select(all.iter().rev().collect(), cursor.as_deref());
            assert_eq!((selected.len(), later.len()), (MAX_ARTIST_READS, 70));
            assert_eq!(selected[0].id.as_deref(), Some(expected));
            seen.extend(selected.iter().filter_map(|artist| artist.id.as_deref()));
            let fence =
                prepare_publication(accepted.as_deref(), selected.last().unwrap().id.as_deref())
                    .unwrap();
            assert_eq!(fence.expected, accepted);
            accepted = Some(fence.progress);
        }
        assert_eq!(seen.len(), all.len());
        assert_eq!(
            cursor(accepted.as_deref()).unwrap().as_deref(),
            Some("Artist0029")
        );
    }

    #[test]
    fn removed_cursor_and_duplicate_or_invalid_ids_do_not_reset_rotation() {
        let all = artists(6);
        let invalid = Artist {
            id: Some("not/an/id".into()),
            name: "invalid".into(),
            genres: Vec::new(),
        };
        let (selected, later) = select(
            vec![&all[5], &all[0], &all[2], &all[2], &invalid],
            Some("Artist0003"),
        );
        assert!(later.is_empty());
        assert_eq!(
            selected
                .iter()
                .filter_map(|artist| artist.id.as_deref())
                .collect::<Vec<_>>(),
            ["Artist0005", "Artist0000", "Artist0002"]
        );
    }

    #[test]
    fn each_publication_changes_its_receipt_without_inventing_attempts() {
        let first = prepare_publication(None, Some("Artist0009")).unwrap();
        let second = prepare_publication(Some(&first.progress), Some("Artist0009")).unwrap();
        assert_eq!(
            cursor(Some(&second.progress)).unwrap().as_deref(),
            Some("Artist0009")
        );
        assert_ne!(first.progress, second.progress);
        assert_eq!(second.expected.as_deref(), Some(first.progress.as_str()));
    }

    #[test]
    fn malformed_or_unbounded_cursor_is_not_used_for_selection() {
        for raw in [
            "{}".to_owned(),
            "x".repeat(MAX_PROGRESS_BYTES + 1),
            r#"{"version":1,"last_artist_id":"a/b","publication":"valid"}"#.to_owned(),
        ] {
            assert!(cursor(Some(&raw)).is_err());
        }
        assert_eq!(cursor(None).unwrap(), None);
    }
}
