use super::super::{Movement, PickReason, QueuedWork};
use super::positions;

fn work(uris: &[&str], first: u32) -> QueuedWork {
    QueuedWork {
        title: "Synthetic work".into(),
        composer: "Synthetic artist".into(),
        work_key: uris.join("/"),
        movements: uris
            .iter()
            .enumerate()
            .map(|(index, uri)| Movement {
                uri: (*uri).into(),
                title: "Synthetic movement".into(),
                position: Some(first + u32::try_from(index).unwrap()),
                added: true,
            })
            .collect(),
        reason: PickReason {
            factors: Vec::new(),
            relaxed: Vec::new(),
            summary: "Synthetic selection".into(),
        },
    }
}

fn queue(uris: &[&str]) -> Vec<Option<String>> {
    uris.iter().map(|uri| Some((*uri).into())).collect()
}

#[test]
fn an_intact_owner_copy_cannot_outvote_the_djs_split_work() {
    assert_eq!(
        positions(
            &[],
            &[work(&["a", "b"], 1), work(&["c"], 3)],
            &queue(&["a", "owner", "b", "a", "b", "c"]),
        ),
        Some(vec![vec![None, None], vec![Some(6)]]),
    );
}

#[test]
fn partial_repeated_claims_need_more_than_agreement_of_two_greedy_passes() {
    let queued: Vec<_> = ["u", "a", "b", "a", "v"]
        .iter()
        .enumerate()
        .map(|(index, uri)| work(&[uri], u32::try_from(index + 1).unwrap()))
        .collect();
    assert_eq!(
        positions(&[], &queued, &queue(&["u", "a", "a", "b", "v"])),
        Some(vec![
            vec![Some(1)],
            vec![None],
            vec![None],
            vec![None],
            vec![Some(5)],
        ]),
    );
}

#[test]
fn a_missing_or_retired_movement_does_not_retire_unique_neighbors() {
    let mut owned = work(&["a", "b", "c", "d"], 1);
    owned.movements[1].position = None;
    owned.movements[3].added = false;
    assert_eq!(
        positions(&[], &[owned], &queue(&["a", "b", "owner", "c", "d"])),
        Some(vec![vec![Some(1), None, Some(4), None]]),
    );
    assert_eq!(
        positions(
            &[],
            &[work(&["a", "b", "c"], 1)],
            &queue(&["a", "c"]),
        ),
        Some(vec![vec![Some(1), None, Some(2)]]),
    );
}

#[test]
fn the_bound_only_limits_partial_matching() {
    let queued = vec![work(&vec!["a"; 2_500], 1)];
    let actual = queue(&vec!["a"; 2_500]);
    let complete = positions(&[], &queued, &actual).unwrap();
    assert!(complete[0].iter().all(Option::is_some));
    assert!(positions(&[], &queued, &actual[..2_499]).is_none());
}

fn sequences(length: usize) -> Vec<Vec<&'static str>> {
    (0..1 << length)
        .map(|bits| {
            (0..length)
                .map(|index| if (bits >> index) & 1 == 0 { "a" } else { "b" })
                .collect()
        })
        .collect()
}

// Independently enumerate every ordered partial alignment, including omitted
// claims, then intersect all alignments with the largest number of matches.
// This catches optional claims that forward/reverse greedy passes both place.
fn oracle(
    played: &[QueuedWork],
    queued: &[QueuedWork],
    actual: &[&str],
) -> Vec<Vec<Option<u32>>> {
    let works: Vec<_> = played.iter().chain(queued).collect();
    let claims: Vec<_> = works
        .iter()
        .enumerate()
        .flat_map(|(work, value)| {
            value.movements.iter().map(move |movement| {
                let bound = if work < played.len() {
                    movement.position.unwrap_or(0)
                } else {
                    u32::MAX
                };
                (movement, bound)
            })
        })
        .collect();
    fn enumerate(
        claims: &[(&Movement, u32)],
        actual: &[&str],
        from: usize,
        prefix: &mut Vec<Option<u32>>,
        best: &mut Vec<Vec<Option<u32>>>,
        length: &mut usize,
    ) {
        let Some((&(movement, bound), remaining)) = claims.split_first() else {
            let matches = prefix.iter().filter(|position| position.is_some()).count();
            if matches > *length {
                best.clear();
                *length = matches;
            }
            if matches == *length {
                best.push(prefix.clone());
            }
            return;
        };
        prefix.push(None);
        enumerate(remaining, actual, from, prefix, best, length);
        prefix.pop();
        if !movement.added || movement.position.is_none_or(|position| position == 0) {
            return;
        }
        for (at, uri) in actual.iter().enumerate().skip(from) {
            let position = u32::try_from(at + 1).unwrap();
            if *uri == movement.uri && position <= bound {
                prefix.push(Some(position));
                enumerate(remaining, actual, at + 1, prefix, best, length);
                prefix.pop();
            }
        }
    }
    let mut best = Vec::new();
    enumerate(&claims, actual, 0, &mut Vec::new(), &mut best, &mut 0);
    let mut offset = 0;
    works
        .iter()
        .map(|work| {
            let mut ambiguous = false;
            let found = (offset..offset + work.movements.len())
                .map(|claim| {
                    let mandatory = best[0][claim]
                        .filter(|position| best.iter().all(|row| row[claim] == Some(*position)));
                    ambiguous |= mandatory.is_none() && best.iter().any(|row| row[claim].is_some());
                    mandatory
                })
                .collect::<Vec<_>>();
            offset += work.movements.len();
            if ambiguous {
                vec![None; work.movements.len()]
            } else {
                found
            }
        })
        .collect()
}

#[test]
fn exhaustive_partial_alignments_match_the_independent_oracle() {
    for count in 0..=4 {
        for expected in sequences(count) {
            for group in [1, 2] {
                let works: Vec<_> = expected
                    .chunks(group)
                    .enumerate()
                    .map(|(index, uris)| work(uris, u32::try_from(index * group + 1).unwrap()))
                    .collect();
                for played in 0..=works.len() {
                    let (played, queued) = works.split_at(played);
                    for length in 0..=5 {
                        for actual in sequences(length) {
                            assert_eq!(
                                positions(played, queued, &queue(&actual)),
                                Some(oracle(played, queued, &actual)),
                                "expected={expected:?}, actual={actual:?}, group={group}, played={}",
                                played.len(),
                            );
                        }
                    }
                }
            }
        }
    }
}
