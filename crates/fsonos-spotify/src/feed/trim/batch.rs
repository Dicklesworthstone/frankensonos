//! Coalesce only adjacent, complete work claims, never the gaps between them.

use super::{QueuedWork, range};
use fsonos_proto::content::MAX_BROWSE_OBJECTS;
use std::ops::Range;

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Batch {
    pub works: Range<usize>,
    pub first: u32,
    pub count: u32,
}

/// Physical queue order must agree with claim order. A corrupt/overlapping
/// claim list cannot authorize positional cleanup. Incomplete works form a
/// barrier, as do unowned queue entries between otherwise eligible works.
pub(super) fn plan(works: &[QueuedWork], before: u32) -> Vec<Batch> {
    let mut batches: Vec<Batch> = Vec::new();
    let mut previous_end = 0;
    for (index, work) in works.iter().enumerate() {
        let Some((first, count)) = range(work, before) else {
            continue;
        };
        if first < previous_end {
            return Vec::new();
        }
        // range's last position is below `before`, so its exclusive end fits.
        previous_end = first + count;
        if count > MAX_BROWSE_OBJECTS {
            continue;
        }
        if let Some(last) = batches.last_mut()
            && last.works.end == index
            && last.first.checked_add(last.count) == Some(first)
            && let Some(combined) = last.count.checked_add(count)
            && combined <= MAX_BROWSE_OBJECTS
        {
            last.count = combined;
            last.works.end = index + 1;
            continue;
        }
        batches.push(Batch {
            works: index..index + 1,
            first,
            count,
        });
    }
    batches
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dj::PickReason;
    use crate::feed::Movement;

    fn work(first: u32, count: u32) -> QueuedWork {
        QueuedWork {
            title: "Synthetic work".into(),
            composer: "Synthetic artist".into(),
            work_key: first.to_string(),
            movements: (0..count)
                .map(|offset| Movement {
                    uri: format!("spotify:track:{offset:022}"),
                    title: String::new(),
                    position: first.checked_add(offset),
                    added: true,
                })
                .collect(),
            reason: PickReason {
                factors: Vec::new(),
                relaxed: Vec::new(),
                summary: String::new(),
            },
        }
    }

    #[test]
    fn adjacent_complete_works_share_one_range_without_copying_movements() {
        assert_eq!(
            plan(&[work(2, 2), work(4, 3), work(7, 1)], 10),
            [Batch {
                works: 0..3,
                first: 2,
                count: 6,
            }]
        );
    }

    #[test]
    fn owner_gaps_and_incomplete_works_are_never_bridged() {
        assert_eq!(
            plan(&[work(2, 2), work(5, 1)], 10),
            [
                Batch {
                    works: 0..1,
                    first: 2,
                    count: 2,
                },
                Batch {
                    works: 1..2,
                    first: 5,
                    count: 1,
                },
            ]
        );
        let mut incomplete = work(4, 1);
        incomplete.movements[0].added = false;
        assert_eq!(
            plan(&[work(2, 2), incomplete, work(5, 1)], 10),
            [
                Batch {
                    works: 0..1,
                    first: 2,
                    count: 2,
                },
                Batch {
                    works: 2..3,
                    first: 5,
                    count: 1,
                },
            ]
        );
    }

    #[test]
    fn a_work_crossing_the_retained_boundary_is_kept_whole() {
        assert_eq!(
            plan(&[work(1, 2), work(3, 3)], 5),
            [Batch {
                works: 0..1,
                first: 1,
                count: 2,
            }]
        );
        assert!(plan(&[work(1, 2)], 2).is_empty());
    }

    #[test]
    fn overlapping_or_reordered_claims_cannot_authorize_cleanup() {
        for works in [
            vec![work(1, 3), work(3, 2)],
            vec![work(5, 1), work(1, 2)],
            vec![work(1, 1), work(1, 1)],
        ] {
            assert!(plan(&works, 10).is_empty());
        }
    }

    #[test]
    fn missing_positions_and_overflowing_work_ranges_are_not_selected() {
        for mut work in [work(0, 1), work(u32::MAX, 2), work(1, 0)] {
            assert!(plan(std::slice::from_ref(&work), u32::MAX).is_empty());
            if let Some(movement) = work.movements.first_mut() {
                movement.position = None;
                assert!(plan(&[work], u32::MAX).is_empty());
            }
        }
    }

    #[test]
    fn many_single_track_songs_need_one_batch_not_one_batch_per_song() {
        let works: Vec<_> = (1..=512).map(|first| work(first, 1)).collect();
        assert_eq!(
            plan(&works, 514),
            [Batch {
                works: 0..512,
                first: 1,
                count: 512,
            }]
        );
    }
}
