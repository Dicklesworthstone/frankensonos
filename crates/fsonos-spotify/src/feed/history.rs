//! Merge recent Spotify listening without rescanning every local play for
//! every retained Spotify track. Keep the original observation order: an
//! accepted Spotify row can suppress a later one, but a skipped row cannot.

use std::collections::{BTreeSet, HashMap};

use crate::dj::PlayRecord;

// Spotify notes a play when it ends; a classical movement can run long.
const SAME_PLAY_SECS: u64 = 60 * 60;

pub(super) fn merge_spotify(history: &mut Vec<PlayRecord>, heard: Vec<(String, i64)>) {
    if heard.is_empty() {
        return;
    }
    let keep: Vec<bool> = {
        // Borrow identities until admission finishes, then move accepted rows
        // into history. The index never needs to clone the track strings.
        let mut times: HashMap<&str, BTreeSet<i64>> = HashMap::new();
        for play in history.iter() {
            if let Some(at) = play.played_at {
                times.entry(&play.source_uri).or_default().insert(at);
            }
        }
        heard
            .iter()
            .map(|(uri, at)| {
                let known = times.entry(uri).or_default();
                let from = at.saturating_sub_unsigned(SAME_PLAY_SECS);
                let through = at.saturating_add_unsigned(SAME_PLAY_SECS);
                if known.range(from..=through).next().is_some() {
                    false
                } else {
                    known.insert(*at);
                    true
                }
            })
            .collect()
    };
    for ((uri, at), keep) in heard.into_iter().zip(keep) {
        if keep {
            history.push(PlayRecord::at(uri, at));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The original sequential definition is an independent equivalence oracle.
    fn reference(history: &mut Vec<PlayRecord>, heard: Vec<(String, i64)>) {
        for (uri, at) in heard {
            if !history.iter().any(|play| {
                play.source_uri == uri
                    && play
                        .played_at
                        .is_some_and(|time| time.abs_diff(at) <= SAME_PLAY_SECS)
            }) {
                history.push(PlayRecord::at(uri, at));
            }
        }
    }

    fn heard(uri: &str, times: &[i64]) -> Vec<(String, i64)> {
        times.iter().map(|&at| (uri.to_owned(), at)).collect()
    }

    #[test]
    fn an_hour_is_inclusive_in_both_directions() {
        for at in [i64::MIN, -10_000, 0, 10_000, i64::MAX] {
            for difference in [-3_601_i64, -3_600, -1, 0, 1, 3_600, 3_601] {
                let Some(other) = at.checked_add(difference) else {
                    continue;
                };
                let original = PlayRecord::at("track", at);
                let mut history = vec![original.clone()];
                merge_spotify(&mut history, heard("track", &[other]));
                let expected = if difference.unsigned_abs() <= SAME_PLAY_SECS {
                    vec![original]
                } else {
                    vec![original, PlayRecord::at("track", other)]
                };
                assert_eq!(history, expected, "at={at}, difference={difference}");
            }
        }
        let mut history = vec![PlayRecord::at("track", i64::MIN)];
        merge_spotify(&mut history, heard("track", &[i64::MAX]));
        assert_eq!(history.len(), 2, "opposite timestamp limits are distinct");
    }

    #[test]
    fn only_accepted_observations_suppress_later_rows() {
        let mut history = Vec::new();
        let input = heard("track", &[0, 3_599, 7_198, -3_600, -3_601]);
        merge_spotify(&mut history, input);
        assert_eq!(
            history,
            [
                PlayRecord::at("track", 0),
                PlayRecord::at("track", 7_198),
                PlayRecord::at("track", -3_601),
            ],
            "the rejected 3599 observation must not suppress 7198; preserve input order"
        );
    }

    #[test]
    fn local_rows_unknown_times_and_distinct_tracks_keep_their_meaning() {
        let original = vec![
            PlayRecord::at("other", 10),
            PlayRecord::new("unknown"),
            PlayRecord::at("track", 20),
            PlayRecord::at("track", 20),
            PlayRecord::at("track", -10_000),
        ];
        let mut history = original.clone();
        merge_spotify(
            &mut history,
            vec![
                ("unknown".into(), 20),
                ("track".into(), 20),
                ("new".into(), 20),
                ("track".into(), -6_400),
                ("track".into(), -6_399),
            ],
        );
        assert_eq!(&history[..original.len()], original.as_slice());
        assert_eq!(
            &history[original.len()..],
            [
                PlayRecord::at("unknown", 20),
                PlayRecord::at("new", 20),
                PlayRecord::at("track", -6_399),
            ]
        );
    }

    #[test]
    fn indexed_merge_matches_sequential_admission_over_varied_histories() {
        let clocks = [
            i64::MIN,
            i64::MIN + 3_600,
            i64::MIN + 3_601,
            -7_200,
            -3_601,
            -3_600,
            -1,
            0,
            1,
            3_600,
            3_601,
            7_200,
            i64::MAX - 3_601,
            i64::MAX - 3_600,
            i64::MAX,
        ];
        for seed in 0..256 {
            let mut rng = crate::dj::Rng::new(seed);
            let mut history: Vec<_> = (0..rng.below(65))
                .map(|_| PlayRecord {
                    source_uri: format!("track-{}", rng.below(7)),
                    played_at: (rng.below(4) != 0).then(|| clocks[rng.below(clocks.len())]),
                })
                .collect();
            let heard: Vec<_> = (0..rng.below(129))
                .map(|_| {
                    (
                        format!("track-{}", rng.below(11)),
                        clocks[rng.below(clocks.len())],
                    )
                })
                .collect();
            let mut expected = history.clone();
            reference(&mut expected, heard.clone());
            merge_spotify(&mut history, heard);
            assert_eq!(history, expected, "seed={seed}");
        }
    }

    #[test]
    fn the_full_recent_listening_window_merges_with_many_coordinator_histories() {
        // Five retained coordinator histories, then the maximum 10,000-track
        // Spotify observation. No timing assertion: exact results and order
        // remain the gate even on a heavily loaded build host.
        let mut history: Vec<_> = (0..10_000)
            .map(|index| PlayRecord::at(format!("track-{index}"), index * 10))
            .collect();
        let heard: Vec<_> = (0..10_000)
            .map(|index| {
                let offset = if index % 2 == 0 { 3_600 } else { 3_601 };
                (format!("track-{index}"), index * 10 + offset)
            })
            .collect();
        merge_spotify(&mut history, heard);
        assert_eq!(history.len(), 15_000);
        for (index, play) in history[..10_000].iter().enumerate() {
            let index = i64::try_from(index).unwrap();
            assert_eq!(*play, PlayRecord::at(format!("track-{index}"), index * 10));
        }
        for (index, play) in history[10_000..].iter().enumerate() {
            let index = i64::try_from(index).unwrap() * 2 + 1;
            assert_eq!(
                *play,
                PlayRecord::at(format!("track-{index}"), index * 10 + 3_601)
            );
        }
    }
}
