//! Bounded, monotonic observations of listening on Spotify devices.

use std::collections::BTreeMap;

/// Retain the latest observation of at most this many distinct tracks.
/// This is repeat-avoidance evidence, not a complete listening archive.
pub const MAX_SPOTIFY_HEARD_TRACKS: usize = 10_000;

/// Each track's latest play, oldest first (by time, then URI).
pub(super) fn latest_plays(plays: &[(String, i64)]) -> Vec<(String, i64)> {
    let mut latest: BTreeMap<&str, i64> = BTreeMap::new();
    for (uri, at) in plays {
        let kept = latest.entry(uri.as_str()).or_insert(*at);
        *kept = (*kept).max(*at);
    }
    let mut plays: Vec<_> = latest
        .into_iter()
        .map(|(uri, at)| (uri.to_owned(), at))
        .collect();
    plays.sort_by(|a, b| (a.1, &a.0).cmp(&(b.1, &b.0)));
    plays
}

/// An empty or delayed observation cannot erase more recent evidence.
pub(super) fn merge_plays(
    current: &[(String, i64)],
    incoming: &[(String, i64)],
) -> Vec<(String, i64)> {
    let mut plays = Vec::with_capacity(current.len().saturating_add(incoming.len()));
    plays.extend_from_slice(current);
    plays.extend_from_slice(incoming);
    let mut merged = latest_plays(&plays);
    let discard = merged.len().saturating_sub(MAX_SPOTIFY_HEARD_TRACKS);
    merged.drain(..discard);
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    fn play(uri: &str, at: i64) -> (String, i64) {
        (uri.into(), at)
    }

    #[test]
    fn delayed_full_reads_preserve_newer_listening_and_other_tracks() {
        let current = vec![play("spotify:track:A", 300), play("spotify:track:B", 200)];
        let incoming = vec![play("spotify:track:A", 100), play("spotify:track:C", 400)];
        assert_eq!(
            merge_plays(&current, &incoming),
            vec![
                play("spotify:track:B", 200),
                play("spotify:track:A", 300),
                play("spotify:track:C", 400)
            ]
        );
        assert_eq!(merge_plays(&current, &[]), latest_plays(&current));
    }

    #[test]
    fn repeated_observations_are_idempotent_and_order_independent() {
        let a = vec![play("A", 1), play("A", 3), play("B", 2)];
        let b = vec![play("B", 4), play("C", 4)];
        let merged = merge_plays(&a, &b);
        assert_eq!(merged, merge_plays(&b, &a));
        assert_eq!(merged, merge_plays(&merged, &a));
        assert_eq!(merged, merge_plays(&merged, &b));
    }

    #[test]
    fn retention_keeps_the_latest_distinct_tracks_with_stable_ties() {
        let current: Vec<_> = (0..MAX_SPOTIFY_HEARD_TRACKS)
            .map(|i| (format!("track{i:05}"), i64::try_from(i).unwrap()))
            .collect();
        let merged = merge_plays(&current, &[play("new", 20_000), play("track00000", 30_000)]);
        assert_eq!(merged.len(), MAX_SPOTIFY_HEARD_TRACKS);
        assert!(!merged.iter().any(|(uri, _)| uri == "track00001"));
        assert_eq!(merged.last(), Some(&play("track00000", 30_000)));
        assert_eq!(merged, merge_plays(&merged, &[play("old", 0)]));
    }
}
