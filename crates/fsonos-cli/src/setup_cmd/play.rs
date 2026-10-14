//! `fsonos setup --test-play <room>`: proof the house plays, softly. A
//! favorite plays in the room's group at volume 10 for a few seconds. A
//! Spotify track comes first, since a household needs one for the DJ;
//! otherwise any track, then a station, then an album or playlist. Then
//! core's zone snapshot puts the group back exactly as it was: members,
//! levels, source and position, and transport. A group that had nothing
//! loaded is stopped again: Sonos has no way to unload a source, so the test
//! track stays loaded but stopped, and the step says so. This is the one
//! step that changes playback, and only when asked.

use fsonos_core::doctor::Status;
use fsonos_core::favorites::{self, Favorite, FavoriteKind};
use fsonos_core::snapshot::{self, SnapshotSource};
use fsonos_core::{HouseholdState, control, resolve_room};
use fsonos_proto::Transport;
use fsonos_types::TransportState;
use std::time::Duration;

use super::{Step, StepOutcome};

/// The level every room of the group plays the test at.
pub const TEST_VOLUME: u8 = 10;
/// How long the test plays before the group is put back.
pub const LISTEN: Duration = Duration::from_secs(5);

/// The favorite to test with: a Spotify track first, then any track, a
/// station, an album or playlist; never a shortcut with nothing to render.
#[must_use]
pub fn pick(favorites: &[Favorite]) -> Option<&Favorite> {
    let spotify = |f: &&Favorite| f.uri.as_deref().is_some_and(|u| u.contains("spotify"));
    let of = |kind: FavoriteKind| move |f: &&Favorite| f.kind == kind && f.uri.is_some();
    favorites
        .iter()
        .filter(of(FavoriteKind::Track))
        .find(spotify)
        .or_else(|| favorites.iter().find(of(FavoriteKind::Track)))
        .or_else(|| favorites.iter().find(of(FavoriteKind::Stream)))
        .or_else(|| favorites.iter().find(of(FavoriteKind::Container)))
}

fn outcome(status: Status, summary: impl Into<String>, remedy: Option<&str>) -> StepOutcome {
    let mut outcome = StepOutcome::new(Step::TestPlay, status, summary);
    outcome.remedies = remedy.map(str::to_owned).into_iter().collect();
    outcome
}

/// Play a favorite softly in `room`'s group for `listen`, then put the group
/// back.
pub fn test_play<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    room: &str,
    listen: Duration,
    now: i64,
) -> StepOutcome {
    let target = match resolve_room(households, room) {
        Ok(target) => target,
        Err(e) => {
            return outcome(
                Status::Fail,
                e.to_string(),
                Some("Name a room that fsonos rooms lists."),
            );
        }
    };
    let (room, coordinator) = (target.room.name.clone(), target.coordinator.id.clone());
    let favorites = match favorites::list(t, households, &coordinator) {
        Ok(found) => found,
        Err(e) => return outcome(Status::Fail, format!("the favorites: {e}"), None),
    };
    let Some(favorite) = pick(&favorites) else {
        return outcome(
            Status::Warn,
            format!("{room}'s household has no favorite to play"),
            Some(
                "In the Sonos app, add a Spotify track (or any station) to My Sonos, then run setup again.",
            ),
        );
    };
    let before = match snapshot::capture(t, households, &coordinator, now) {
        Ok(snap) => snap,
        Err(e) => {
            return outcome(
                Status::Fail,
                format!("could not record what {room}'s group is doing first: {e}"),
                None,
            );
        }
    };
    let mut trouble = Vec::new();
    for member in &before.members {
        if let Err(e) = control::set_volume(t, households, member, TEST_VOLUME) {
            trouble.push(format!("volume: {e}"));
        }
    }
    let played = favorites::play(t, households, &coordinator, favorite).map_err(|e| e.to_string());
    let state = played.as_ref().ok().and_then(|()| {
        std::thread::sleep(listen);
        control::playback(t, households, &coordinator)
            .ok()
            .map(|p| p.transport.state)
    });
    let restored = snapshot::restore(t, households, &before);
    match restored {
        Ok(report) if !report.skipped.is_empty() => trouble.extend(
            report
                .skipped
                .iter()
                .map(|(aspect, why)| format!("not restored: {aspect:?} ({why})")),
        ),
        Ok(_) => {}
        Err(e) => trouble.push(format!("not restored: {e}")),
    }
    let title = &favorite.title;
    let back = if matches!(before.source, SnapshotSource::Nothing) {
        "then stopped it again (the group had nothing loaded, so the test track stays loaded, stopped)"
    } else {
        "then put it back"
    };
    match (played, state) {
        (Err(why), _) => outcome(
            Status::Fail,
            format!("{title:?} did not start in {room}'s group: {why}"),
            Some("Check the Spotify steps above; then try another room."),
        ),
        (Ok(()), Some(TransportState::Playing)) if trouble.is_empty() => outcome(
            Status::Pass,
            format!(
                "played {title:?} in {room}'s group at volume {TEST_VOLUME} for {} s, {back}",
                listen.as_secs()
            ),
            None,
        ),
        (Ok(()), Some(TransportState::Playing)) => outcome(
            Status::Warn,
            format!(
                "played {title:?} in {room}'s group, but {}",
                trouble.join("; ")
            ),
            Some("Check the room in the Sonos app, and undo anything left behind."),
        ),
        (Ok(()), other) => outcome(
            Status::Fail,
            format!(
                "{title:?} was sent to {room}'s group but it is {}",
                other.map_or_else(
                    || "not answering".to_owned(),
                    |s| format!("{s:?}").to_lowercase()
                )
            ),
            Some("Check the Spotify steps above; then try another room."),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn favorite(id: &str, kind: FavoriteKind, uri: Option<&str>) -> Favorite {
        Favorite {
            id: id.to_owned(),
            title: id.to_owned(),
            kind,
            uri: uri.map(str::to_owned),
            metadata: String::new(),
            description: None,
            art_uri: None,
        }
    }

    #[test]
    fn a_spotify_track_is_picked_first_and_shortcuts_never() {
        let listed = vec![
            favorite("shortcut", FavoriteKind::Unplayable, None),
            favorite(
                "album",
                FavoriteKind::Container,
                Some("x-rincon-cpcontainer:album"),
            ),
            favorite(
                "station",
                FavoriteKind::Stream,
                Some("x-rincon-mp3radio://stream.example.invalid/a.mp3"),
            ),
            favorite(
                "local",
                FavoriteKind::Track,
                Some("x-file-cifs://nas.example.invalid/a.flac"),
            ),
            favorite(
                "spotify",
                FavoriteKind::Track,
                Some("x-sonos-spotify:spotify%3atrack%3aabc"),
            ),
        ];
        assert_eq!(pick(&listed).map(|f| f.id.as_str()), Some("spotify"));
        assert_eq!(pick(&listed[..4]).map(|f| f.id.as_str()), Some("local"));
        assert_eq!(pick(&listed[..3]).map(|f| f.id.as_str()), Some("station"));
        assert_eq!(pick(&listed[..2]).map(|f| f.id.as_str()), Some("album"));
        assert_eq!(pick(&listed[..1]), None);
    }
}
