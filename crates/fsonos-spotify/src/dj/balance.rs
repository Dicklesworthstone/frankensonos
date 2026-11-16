//! Catalogue balance follows distinct music. A work's strongest admitted
//! manual preference tier determines its share; the original taste scores
//! still choose between all its intact recordings. Stable first-seen order
//! keeps seeded draws reproducible; indexes avoid quadratic catalogue scans.

use std::collections::{HashMap, HashSet};

use super::{Rng, WorkPool, as_u64, draw};

pub(super) fn composer_counts<'p>(
    pool: &'p WorkPool,
    permitted: &[usize],
) -> HashMap<&'p str, usize> {
    let mut seen = HashSet::new();
    let mut counts = HashMap::new();
    for &index in permitted {
        let work = &pool.works[index];
        let composer = work.composer_key();
        if seen.insert((composer, work.work_key.as_str())) {
            *counts.entry(composer).or_default() += 1;
        }
    }
    counts
}

struct RecordingGroup {
    first: usize,
    count: usize,
    preference_pm: u64,
    preferred_count: usize,
    total: u128,
}

impl RecordingGroup {
    /// Average only the strongest admitted manual tier: neutral editions must
    /// not dilute a favor, pin, or that preference's feedback shield. The
    /// wider accumulator cannot overflow for any addressable score slice, and
    /// rounding up keeps a positive score reachable at integer precision.
    fn weight(&self) -> u64 {
        u64::try_from(
            self.total
                .div_ceil(u128::from(as_u64(self.preferred_count))),
        )
        .expect("a mean cannot exceed its largest u64 input")
    }
}

pub(super) struct RecordingLottery {
    groups: Vec<RecordingGroup>,
    group_of: Vec<usize>,
}

impl RecordingLottery {
    pub(super) fn new(pool: &WorkPool, candidates: &[usize], scores: &[u64]) -> Self {
        debug_assert_eq!(candidates.len(), scores.len());
        let mut index = HashMap::new();
        let mut groups: Vec<RecordingGroup> = Vec::new();
        let mut group_of = Vec::with_capacity(candidates.len());
        for (candidate, (&work, &weight)) in candidates.iter().zip(scores).enumerate() {
            let group = *index
                .entry(pool.works[work].work_key.as_str())
                .or_insert_with(|| {
                    groups.push(RecordingGroup {
                        first: candidate,
                        count: 0,
                        preference_pm: 0,
                        preferred_count: 0,
                        total: 0,
                    });
                    groups.len() - 1
                });
            let preference_pm = pool.verdicts[work].weight_pm();
            let recording_group = &mut groups[group];
            recording_group.count += 1;
            if preference_pm > recording_group.preference_pm {
                recording_group.preference_pm = preference_pm;
                recording_group.preferred_count = 0;
                recording_group.total = 0;
            }
            if preference_pm == recording_group.preference_pm {
                recording_group.preferred_count += 1;
                recording_group.total += u128::from(weight);
            }
            group_of.push(group);
        }
        Self { groups, group_of }
    }

    /// Returns a candidate index and the number of eligible recordings of its
    /// work. One edition per work uses the same scores and RNG draw as before.
    pub(super) fn draw(&self, scores: &[u64], rng: &mut Rng) -> (usize, usize) {
        let work_scores: Vec<_> = self.groups.iter().map(RecordingGroup::weight).collect();
        let selected = draw(&work_scores, rng);
        let group = &self.groups[selected];
        if group.count == 1 {
            return (group.first, 1);
        }
        // Only the selected work allocates a recording list. Keeping a flat
        // group index avoids one heap allocation per song in a large library.
        let recordings: Vec<_> = self
            .group_of
            .iter()
            .enumerate()
            .filter_map(|(candidate, &group)| (group == selected).then_some(candidate))
            .collect();
        let recording_scores: Vec<_> = recordings.iter().map(|&index| scores[index]).collect();
        (recordings[draw(&recording_scores, rng)], group.count)
    }
}

#[cfg(test)]
mod tests;
