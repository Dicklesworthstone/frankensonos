//! Pure state transition for the native AVTransport queue move. Kept apart
//! from HTTP framing; the server still applies faults, logging and GENA.

use crate::model::{Fault, INVALID_ARGS, Out, SONOS_FAILURE, State, Transport};

pub(super) fn reorder(
    state: &mut State,
    player: usize,
    args: &[(String, String)],
) -> Result<Out, Fault> {
    let number = |name: &str| -> Result<u32, Fault> {
        args.iter()
            .find(|(key, _)| key == name)
            .ok_or(INVALID_ARGS)?
            .1
            .trim()
            .parse()
            .map_err(|_| INVALID_ARGS)
    };
    if state.players[player].coordinator != player {
        return Err(SONOS_FAILURE);
    }
    if number("InstanceID")? != 0 {
        return Err(INVALID_ARGS);
    }
    apply(
        &mut state.players[player].transport,
        number("StartingIndex")?,
        number("NumberOfTracks")?,
        number("InsertBefore")?,
        number("UpdateID")?,
    )
}

fn apply(
    transport: &mut Transport,
    first: u32,
    count: u32,
    before: u32,
    revision: u32,
) -> Result<Out, Fault> {
    let start =
        usize::try_from(first.checked_sub(1).ok_or(INVALID_ARGS)?).map_err(|_| INVALID_ARGS)?;
    let count = usize::try_from(count).map_err(|_| INVALID_ARGS)?;
    let end = start.checked_add(count).ok_or(INVALID_ARGS)?;
    let destination =
        usize::try_from(before.checked_sub(1).ok_or(INVALID_ARGS)?).map_err(|_| INVALID_ARGS)?;
    if count == 0
        || end > transport.queue.len()
        || destination > transport.queue.len()
        || (revision != 0 && revision != transport.queue_update_id)
    {
        return Err(INVALID_ARGS);
    }
    if (start..=end).contains(&destination) {
        return Ok(Vec::new());
    }
    let current = transport.track.checked_sub(1);
    if destination < start {
        transport.queue[destination..end].rotate_right(count);
        if let Some(current) = current {
            let index = if (start..end).contains(&current) {
                destination + current - start
            } else if (destination..start).contains(&current) {
                current + count
            } else {
                current
            };
            transport.track = index + 1;
        }
    } else {
        transport.queue[start..destination].rotate_left(count);
        if let Some(current) = current {
            let index = if (start..end).contains(&current) {
                destination - count + current - start
            } else if (end..destination).contains(&current) {
                current - count
            } else {
                current
            };
            transport.track = index + 1;
        }
    }
    // Keep the current item and its clock: reordering is not seeking.
    transport.queue_update_id = transport.queue_update_id.wrapping_add(1);
    Ok(Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{QueueItem, Source, TransportState};

    fn queue(len: usize, current: usize) -> Transport {
        let mut t = Transport::new();
        t.queue = (0..len)
            .map(|i| QueueItem {
                uri: format!("test:{i}"),
                metadata: String::new(),
                title: i.to_string(),
                creator: None,
                album: None,
                duration_ms: 180_000,
            })
            .collect();
        t.source = Source::Queue;
        t.track = current;
        t.state = TransportState::Playing;
        t.position_ms = 12_000;
        t.playing_since = Some(100);
        t.play_mode = "SHUFFLE".into();
        t.queue_update_id = 7;
        t
    }

    #[test]
    fn every_small_move_keeps_order_and_the_exact_playing_slot() {
        for len in 1..=7 {
            for start in 0..len {
                for count in 1..=len - start {
                    for destination in 0..=len {
                        let mut expected: Vec<_> = (0..len).collect();
                        let noop = (start..=start + count).contains(&destination);
                        if !noop {
                            let moved: Vec<_> = expected.drain(start..start + count).collect();
                            let at = if destination < start {
                                destination
                            } else {
                                destination - count
                            };
                            expected.splice(at..at, moved);
                        }
                        for current in 0..=len {
                            let mut t = queue(len, current);
                            apply(
                                &mut t,
                                u32::try_from(start + 1).unwrap(),
                                u32::try_from(count).unwrap(),
                                u32::try_from(destination + 1).unwrap(),
                                7,
                            )
                            .unwrap();
                            let actual: Vec<_> = t
                                .queue
                                .iter()
                                .map(|q| q.title.parse::<usize>().unwrap())
                                .collect();
                            assert_eq!(actual, expected);
                            let playing = if current == 0 {
                                0
                            } else {
                                expected.iter().position(|&id| id == current - 1).unwrap() + 1
                            };
                            assert_eq!(t.track, playing);
                            assert_eq!(t.position_ms, 12_000);
                            assert_eq!(t.playing_since, Some(100));
                            assert_eq!(t.play_mode, "SHUFFLE");
                            assert_eq!(t.state, TransportState::Playing);
                            assert_eq!(t.queue_update_id, if noop { 7 } else { 8 });
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn invalid_or_stale_moves_do_not_change_any_queue_state() {
        for (first, count, before, revision) in [
            (0, 1, 2, 7),
            (1, 0, 2, 7),
            (1, 1, 0, 7),
            (4, 1, 1, 7),
            (3, 2, 1, 7),
            (1, 1, 5, 7),
            (1, 1, 3, 6),
            (u32::MAX, u32::MAX, 1, 7),
        ] {
            let mut t = queue(3, 2);
            assert!(apply(&mut t, first, count, before, revision).is_err());
            assert_eq!(t.queue[0].uri, "test:0");
            assert_eq!(t.track, 2);
            assert_eq!(t.queue_update_id, 7);
        }
    }

    #[test]
    fn revision_wrap_and_native_wildcard_do_not_reset_playback() {
        let mut t = queue(3, 1);
        t.queue_update_id = u32::MAX;
        apply(&mut t, 1, 1, 4, u32::MAX).unwrap();
        assert_eq!(t.queue_update_id, 0);
        assert_eq!(t.track, 3);
        apply(&mut t, 3, 1, 1, 0).unwrap();
        assert_eq!(t.track, 1);
        assert_eq!(t.queue_update_id, 1);
    }
}
