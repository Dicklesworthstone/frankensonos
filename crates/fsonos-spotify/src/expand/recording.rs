//! Choose one recording from an album without dropping movement sections or
//! joining alternate performances. Album positions disambiguate repeated URIs;
//! an unknown position is never evidence that two tracks are the same.

use std::collections::HashSet;

use fsonos_core::store::AlbumTrack;

use crate::classical::{ClassicalTrack, normalize, split_title};
use crate::works::{Completeness, Work, build, movement_number};

/// Rebuild from the album's positions rather than appending the missing
/// tracks to held movements whose coordinates may be absent or stale.
pub(super) fn expand(work: &Work, album: &[AlbumTrack]) -> Option<Work> {
    let template = work.movements.first()?;
    let tracks = select(work, album)?;
    let mut positions = HashSet::new();
    for track in &tracks {
        if track.disc_number == 0
            || track.track_number == 0
            || !positions.insert((track.disc_number, track.track_number))
        {
            return None;
        }
    }
    let mut originals: Vec<Option<&ClassicalTrack>> = vec![None; tracks.len()];
    for held in &work.movements {
        let matched = anchor(held, &tracks)?;
        let index = tracks
            .iter()
            .position(|track| std::ptr::eq(*track, matched))?;
        if originals[index].replace(held).is_some() {
            // Two held aliases for one slot cannot silently discard either
            // track's URI, explicit flag or preference metadata.
            return None;
        }
    }
    let movements: Vec<ClassicalTrack> = tracks
        .iter()
        .zip(originals)
        .map(|(track, original)| match original {
            Some(held) => {
                let mut restored = held.clone();
                restored.disc_number = Some(track.disc_number);
                restored.track_number = Some(track.track_number);
                restored.track.duration_secs = restored.track.duration_secs.or(track.duration_secs);
                restored
            }
            None => super::filled_in(template, track),
        })
        .collect();
    let ordered: Vec<&ClassicalTrack> = movements.iter().collect();
    let mut full = build(&ordered);
    if full.completeness == Completeness::Unknown {
        // All-liked recordings can be confirmed without inventing an album
        // save. A visible numbering gap still remains Partial.
        full.completeness = Completeness::Complete;
    }
    (full != *work).then_some(full)
}

/// Every held movement must identify the same recording. Keep the original
/// work unchanged when its identity is missing, ambiguous or contradictory.
fn select<'a>(work: &Work, album: &'a [AlbumTrack]) -> Option<Vec<&'a AlbumTrack>> {
    let title = normalize(&work.title);
    let mut ordered: Vec<&AlbumTrack> = album.iter().collect();
    ordered.sort_by_key(|t| (t.disc_number, t.track_number));
    let mut runs: Vec<Vec<&AlbumTrack>> = Vec::new();
    let mut seen = HashSet::new();
    let mut last = None;
    for track in ordered {
        let parts = split_title(&track.title);
        if normalize(parts.work) != title {
            continue;
        }
        let movement = normalize(parts.movement.unwrap_or(&track.title));
        let number = parts.movement.and_then(movement_number);
        // Requiem III.1, III.2, III.3 are sections, not three recordings.
        // A strict decrease, or a repeated full movement, starts a new take.
        let restarts = matches!((last, number), (Some(prev), Some(n)) if n < prev);
        if runs.is_empty() || restarts || seen.contains(&movement) {
            runs.push(Vec::new());
            seen.clear();
            last = None;
        }
        runs.last_mut()?.push(track);
        seen.insert(movement);
        last = number.or(last);
    }
    let tracks: Vec<&AlbumTrack> = runs.iter().flatten().copied().collect();
    let mut selected = None;
    for held in &work.movements {
        let anchor = anchor(held, &tracks)?;
        let index = runs
            .iter()
            .position(|run| run.iter().any(|track| std::ptr::eq(*track, anchor)))?;
        if selected.is_some_and(|previous| previous != index) {
            return None;
        }
        selected = Some(index);
    }
    runs.into_iter().nth(selected?)
}

/// Prefer a unique URI match; a relinked URI can match a known album position.
/// When a URI occurs more than once, the position must resolve that ambiguity.
fn anchor<'a>(held: &ClassicalTrack, tracks: &[&'a AlbumTrack]) -> Option<&'a AlbumTrack> {
    let mut matching = tracks.iter().copied().filter(|track| {
        !held.track.source_uri.is_empty() && track.source_uri == held.track.source_uri
    });
    let first = matching.next();
    if first.is_some() && matching.next().is_none() {
        return first;
    }
    let disc = held.disc_number.filter(|&number| number > 0)?;
    let number = held.track_number.filter(|&number| number > 0)?;
    let held_part = normalize(
        split_title(&held.track.title)
            .movement
            .unwrap_or(&held.track.title),
    );
    let mut positioned = tracks.iter().copied().filter(|track| {
        (track.disc_number, track.track_number) == (disc, number)
            && if first.is_some() {
                track.source_uri == held.track.source_uri
            } else {
                let parts = split_title(&track.title);
                normalize(parts.movement.unwrap_or(&track.title)) == held_part
            }
    });
    let first = positioned.next()?;
    positioned.next().is_none().then_some(first)
}
