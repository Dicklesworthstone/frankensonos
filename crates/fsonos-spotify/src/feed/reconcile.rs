//! Prove queue occurrences across the complete retained DJ sequence.
//! Repeated URIs identify music, not ownership. Only a movement occupying the
//! same position in every maximum ordered matching remains a DJ claim.

use std::collections::{HashMap, HashSet};

use super::{FeedSnapshot, QueuedWork};

impl FeedSnapshot {
    /// Confirm an inherited queue without moving claims onto another copy of
    /// the same music. Every waiting claim and the selected retained work must
    /// remain at its exact, uniquely proved positions. The selected occurrence
    /// may be in `played`: a listener can rewind before a coordinator changes.
    /// Source, group and boot observations still belong to the caller.
    #[must_use]
    pub fn owns_inherited_occurrence(
        &self,
        queue: &[Option<String>],
        position: u32,
        uri: &str,
    ) -> bool {
        if position == 0 {
            return false;
        }
        let works: Vec<_> = self.played.iter().chain(&self.queued).collect();
        let Some(current) = works.iter().position(|work| {
            work.movements.iter().any(|movement| {
                movement.added && movement.position == Some(position) && movement.uri == uri
            })
        }) else {
            return false;
        };
        let Some(proved) = positions(&self.played, &self.queued, queue) else {
            return false;
        };
        let mut occupied = HashSet::new();
        works.iter().enumerate().all(|(index, work)| {
            if index < self.played.len() && index != current {
                return true;
            }
            work.movements
                .iter()
                .zip(&proved[index])
                .all(|(movement, found)| {
                    let Some(expected) = movement.position else {
                        return true;
                    };
                    movement.added && occupied.insert(expected) && *found == Some(expected)
                })
        })
    }
}

// Normal queues use the indexed complete-sequence path. Owner edits that
// require partial matching may use at most 16 MiB for the forward scores;
// reverse scores use two rows. Even the protocol's 100,000-item browse limit
// cannot force an unbounded quadratic allocation or reconciliation pause.
const MAX_CELLS: usize = 4_000_000;

struct Claim<'a> {
    work: usize,
    movement: usize,
    uri: &'a str,
    occurrences: &'a [usize],
}

struct Matches {
    positions: Vec<Option<usize>>,
    // An occurrence is possible in an optimal matching, but not mandatory.
    ambiguous: Vec<bool>,
}

/// Keep uniquely proved occurrences, including legitimate repeated DJ works.
/// A work with an ambiguous movement is retired as a whole: retaining only
/// its later movements could make Skip start halfway through an owner's copy.
/// Definitely absent movements may remain missing within an otherwise owned
/// work. `None` means the bounded partial proof was too large to establish any
/// ownership; the caller must retire all claims without changing the music.
pub(super) fn positions(
    played: &[QueuedWork],
    queued: &[QueuedWork],
    queue: &[Option<String>],
) -> Option<Vec<Vec<Option<u32>>>> {
    let works: Vec<_> = played.iter().chain(queued).collect();
    let mut result: Vec<_> = works
        .iter()
        .map(|work| vec![None; work.movements.len()])
        .collect();
    let mut occurrences: HashMap<&str, Vec<usize>> = HashMap::new();
    for (position, uri) in queue.iter().enumerate() {
        if let Some(uri) = uri {
            occurrences.entry(uri).or_default().push(position);
        }
    }
    let mut claims = Vec::new();
    for (work, expected) in works.iter().enumerate() {
        for (movement, expected) in expected.movements.iter().enumerate() {
            // A retired or unadded movement cannot reclaim a later copy.
            let Some(old) = expected.position.filter(|&old| old > 0) else {
                continue;
            };
            if !expected.added {
                continue;
            }
            let Some(found) = occurrences.get(expected.uri.as_str()) else {
                continue;
            };
            // Played claims can shift earlier after cleanup, never migrate
            // onto a later identical occurrence belonging to the listener.
            let end = if work < played.len() {
                found.partition_point(|&at| at < usize::try_from(old).unwrap_or(usize::MAX))
            } else {
                found.len()
            };
            if end > 0 {
                claims.push(Claim {
                    work,
                    movement,
                    uri: &expected.uri,
                    occurrences: &found[..end],
                });
            }
        }
    }
    if claims.is_empty() {
        return Some(result);
    }
    let matches = complete(&claims, queue.len()).or_else(|| partial(&claims, queue))?;
    let mut ambiguous = vec![false; works.len()];
    for (index, claim) in claims.iter().enumerate() {
        result[claim.work][claim.movement] =
            matches.positions[index].and_then(|position| u32::try_from(position + 1).ok());
        ambiguous[claim.work] |= matches.ambiguous[index];
    }
    for (positions, ambiguous) in result.iter_mut().zip(ambiguous) {
        if ambiguous {
            positions.fill(None);
        }
    }
    Some(result)
}

/// When all surviving claims fit, greedy embeddings give the exact earliest
/// and latest position of each claim. Agreement then proves uniqueness. This
/// is not sound for a partial embedding, which needs the exact solver below.
fn complete(claims: &[Claim<'_>], length: usize) -> Option<Matches> {
    let mut earliest = Vec::with_capacity(claims.len());
    let mut cursor = 0;
    for claim in claims {
        let next = claim.occurrences.partition_point(|&at| at < cursor);
        let &position = claim.occurrences.get(next)?;
        earliest.push(position);
        cursor = position + 1;
    }
    let mut positions = vec![None; claims.len()];
    let mut ambiguous = vec![false; claims.len()];
    cursor = length;
    for (index, claim) in claims.iter().enumerate().rev() {
        let end = claim.occurrences.partition_point(|&at| at < cursor);
        let &position = claim.occurrences.get(end.checked_sub(1)?)?;
        if earliest[index] == position {
            positions[index] = Some(position);
        } else {
            ambiguous[index] = true;
        }
        cursor = position;
    }
    Some(Matches {
        positions,
        ambiguous,
    })
}

/// Longest common subsequence with the played-position bounds. A matching
/// pair belongs to some optimal alignment exactly when prefix + 1 + suffix
/// reaches its maximum length. Its rank is prefix + 1. Each optimal alignment
/// has one pair at each rank, so a rank with only one possible pair proves
/// that pair appears in every optimum, even when some claims are missing.
fn partial(claims: &[Claim<'_>], queue: &[Option<String>]) -> Option<Matches> {
    let needed: HashSet<_> = claims.iter().map(|claim| claim.uri).collect();
    let actual: Vec<_> = queue
        .iter()
        .enumerate()
        .filter_map(|(position, uri)| {
            let uri = uri.as_deref()?;
            needed.contains(uri).then_some((position, uri))
        })
        .collect();
    let width = actual.len().checked_add(1)?;
    let cells = claims.len().checked_add(1)?.checked_mul(width)?;
    if cells > MAX_CELLS {
        return None;
    }
    let matches = |claim: &Claim<'_>, position: usize, uri: &str| {
        claim.uri == uri
            && claim
                .occurrences
                .last()
                .is_some_and(|&last| position <= last)
    };
    let mut forward = vec![0_u32; cells];
    for (i, claim) in claims.iter().enumerate() {
        for (j, &(position, uri)) in actual.iter().enumerate() {
            let mut score = forward[i * width + j + 1].max(forward[(i + 1) * width + j]);
            if matches(claim, position, uri) {
                score = score.max(forward[i * width + j] + 1);
            }
            forward[(i + 1) * width + j + 1] = score;
        }
    }
    let length = usize::try_from(forward[cells - 1]).ok()?;
    let mut ranks = vec![None; length];
    let mut multiple = vec![false; length];
    let mut possible = vec![false; claims.len()];
    let mut after = vec![0_u32; width];
    let mut current = vec![0_u32; width];
    for (i, claim) in claims.iter().enumerate().rev() {
        current[actual.len()] = 0;
        for (j, &(position, uri)) in actual.iter().enumerate().rev() {
            current[j] = after[j].max(current[j + 1]);
            if matches(claim, position, uri) {
                current[j] = current[j].max(1 + after[j + 1]);
                let before = forward[i * width + j];
                if before + 1 + after[j + 1] == forward[cells - 1] {
                    possible[i] = true;
                    let rank = usize::try_from(before).ok()?;
                    if ranks[rank].is_some() {
                        multiple[rank] = true;
                    } else {
                        ranks[rank] = Some((i, position));
                    }
                }
            }
        }
        std::mem::swap(&mut after, &mut current);
    }
    let mut positions = vec![None; claims.len()];
    for (pair, multiple) in ranks.into_iter().zip(multiple) {
        if !multiple && let Some((claim, position)) = pair {
            positions[claim] = Some(position);
        }
    }
    let ambiguous = possible
        .into_iter()
        .zip(&positions)
        .map(|(possible, position)| possible && position.is_none())
        .collect();
    Some(Matches {
        positions,
        ambiguous,
    })
}

#[cfg(test)]
mod tests;
