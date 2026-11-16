//! One native CSV edit addresses original saved-playlist occurrences and
//! their final positions. The playing queue is a separate copy.

use super::{
    Args, Fault, INVALID_ARGS, Out, QueueItem, STALE_UPDATE_ID, State, TRANSITION_NOT_AVAILABLE,
};
use std::collections::HashSet;

impl State {
    pub(super) fn reorder_saved_tracks(
        &mut self,
        player: usize,
        args: &Args<'_>,
    ) -> Result<Out, Fault> {
        self.require_coordinator(player)?;
        if args.num::<u32>("InstanceID")? != 0 {
            return Err(INVALID_ARGS);
        }
        let id = args
            .get("ObjectID")?
            .strip_prefix("SQ:")
            .filter(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
            .and_then(|id| id.parse::<u32>().ok())
            .ok_or(INVALID_ARGS)?;
        let revision = args.num::<u32>("UpdateID")?;
        let tracks = numbers(args.get("TrackList")?)?;
        let destinations = args.get("NewPositionList")?;
        let positions = if destinations.is_empty() {
            None
        } else {
            Some(numbers(destinations)?)
        };
        let house = &mut self.households[self.players[player].household];
        if revision != 0 && revision != house.saved_playlists_update_id {
            return Err(STALE_UPDATE_ID);
        }
        let saved = house
            .saved_playlists
            .iter_mut()
            .find(|saved| saved.id == id)
            .ok_or(TRANSITION_NOT_AVAILABLE)?;
        let before = i64::try_from(saved.tracks.len()).map_err(|_| INVALID_ARGS)?;
        let edited = apply(&saved.tracks, &tracks, positions.as_deref())?;
        let after = i64::try_from(edited.len()).map_err(|_| INVALID_ARGS)?;
        saved.tracks = edited;
        house.saved_playlists_update_id = house.saved_playlists_update_id.wrapping_add(1);
        Ok(vec![
            ("QueueLengthChange", (after - before).to_string()),
            ("NewQueueLength", after.to_string()),
            ("NewUpdateID", house.saved_playlists_update_id.to_string()),
        ])
    }
}

fn numbers(value: &str) -> Result<Vec<usize>, Fault> {
    if value.is_empty() {
        return Err(INVALID_ARGS);
    }
    value
        .split(',')
        .map(|number| number.parse().map_err(|_| INVALID_ARGS))
        .collect()
}

fn apply(
    original: &[QueueItem],
    tracks: &[usize],
    positions: Option<&[usize]>,
) -> Result<Vec<QueueItem>, Fault> {
    let selected: HashSet<_> = tracks.iter().copied().collect();
    if tracks.is_empty()
        || selected.len() != tracks.len()
        || tracks.iter().any(|&track| track >= original.len())
    {
        return Err(INVALID_ARGS);
    }
    let Some(positions) = positions else {
        return Ok(original
            .iter()
            .enumerate()
            .filter(|(index, _)| !selected.contains(index))
            .map(|(_, item)| item.clone())
            .collect());
    };
    if positions.len() != tracks.len()
        || positions.iter().any(|&position| position >= original.len())
        || positions.iter().collect::<HashSet<_>>().len() != positions.len()
    {
        return Err(INVALID_ARGS);
    }
    let mut ordered = vec![None; original.len()];
    for (&track, &position) in tracks.iter().zip(positions) {
        ordered[position] = Some(original[track].clone());
    }
    let mut remaining = original
        .iter()
        .enumerate()
        .filter(|(index, _)| !selected.contains(index))
        .map(|(_, item)| item.clone());
    for place in &mut ordered {
        if place.is_none() {
            *place = remaining.next();
        }
    }
    Ok(ordered.into_iter().map(Option::unwrap).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracks() -> Vec<QueueItem> {
        (0..5)
            .map(|index| QueueItem {
                uri: format!("test:{index}"),
                metadata: String::new(),
                title: index.to_string(),
                creator: None,
                album: None,
                duration_ms: 180_000,
            })
            .collect()
    }

    fn order(items: &[QueueItem]) -> Vec<&str> {
        items.iter().map(|item| item.title.as_str()).collect()
    }

    #[test]
    fn documented_csv_reverse_addresses_original_occurrences() {
        let original = tracks();
        let result = apply(&original[..4], &[3, 2, 1, 0], Some(&[0, 1, 2, 3])).unwrap();
        assert_eq!(order(&result), ["3", "2", "1", "0"]);
    }

    #[test]
    fn blocks_move_in_both_directions_and_removal_preserves_unselected_order() {
        let original = tracks();
        assert_eq!(
            order(&apply(&original, &[2, 1], Some(&[4, 3])).unwrap()),
            ["0", "3", "4", "1", "2"]
        );
        assert_eq!(
            order(&apply(&original, &[4, 3], Some(&[2, 1])).unwrap()),
            ["0", "3", "4", "1", "2"]
        );
        assert_eq!(
            order(&apply(&original, &[3, 2], None).unwrap()),
            ["0", "1", "4"]
        );
        assert!(apply(&original, &[0, 1, 2, 3, 4], None).unwrap().is_empty());
    }

    #[test]
    fn invalid_native_selection_has_no_partial_result() {
        for (selection, positions) in [
            (vec![], None),
            (vec![5], None),
            (vec![1, 1], None),
            (vec![2, 1], Some(vec![0])),
            (vec![2, 1], Some(vec![0, 0])),
            (vec![2, 1], Some(vec![0, 5])),
        ] {
            assert!(apply(&tracks(), &selection, positions.as_deref()).is_err());
        }
    }
}
