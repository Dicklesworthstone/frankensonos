//! Source preparation without an audible Play/Pause cycle. Playing and
//! loading share renderer account selection and the same bounded recovery.

use super::{
    addr, is_render_800, pause_at, play_uri, queue_start, source_start, spotify_source,
    spotify_track_source,
};
use crate::{CoreError, HouseholdState};
use fsonos_proto::content;
use fsonos_proto::control as soap;
use fsonos_proto::didl::SpotifyContainer;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::{PlayerId, TransportState};
use std::net::IpAddr;

fn coordinator_at<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    coordinator: &PlayerId,
) -> Result<(), CoreError> {
    if get_zone_group_state(t, host)?
        .groups
        .iter()
        .any(|group| group.coordinator == *coordinator)
    {
        Ok(())
    } else {
        Err(ProtoError::Malformed("the source target no longer leads its group".into()).into())
    }
}

/// Pause (or stop an unpausable stream) and confirm the group is inactive.
/// An uncertain reply is observed, not repeated. Unknown state never permits
/// subsequent queue mutation. This may leave playback paused on later failure.
pub fn pause_confirmed<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
) -> Result<(), CoreError> {
    let host = addr(households, coordinator)?;
    coordinator_at(t, host, coordinator)?;
    match soap::get_transport_info(t, host)?.state {
        TransportState::Paused | TransportState::Stopped => return Ok(()),
        TransportState::Playing | TransportState::Transitioning => {}
        _ => {
            return Err(
                ProtoError::Malformed("playback state is unknown before loading".into()).into(),
            );
        }
    }
    let written = pause_at(t, host);
    if let Err(error) = &written
        && !matches!(
            error,
            CoreError::Proto(ProtoError::Network { .. } | ProtoError::Malformed(_))
        )
    {
        return written;
    }
    let confirmed = (|| -> Result<(), CoreError> {
        coordinator_at(t, host, coordinator)?;
        if !matches!(
            soap::get_transport_info(t, host)?.state,
            TransportState::Paused | TransportState::Stopped
        ) {
            return Err(ProtoError::Malformed(
                "playback did not become inactive before loading".into(),
            )
            .into());
        }
        Ok(())
    })();
    // Keep the original uncertain write failure if its effect cannot be
    // proved; a successful observation does not require another pause.
    match (written, confirmed) {
        (_, Ok(())) => Ok(()),
        (Err(error), _) | (Ok(()), Err(error)) => Err(error),
    }
}

/// Load a renderer-ready source without Play, leaving it ready for resume.
/// Volumes, mutes and queue contents are not changed. Identity and inactive
/// transport are read back; success never means merely an acknowledged write.
pub fn load_uri<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    uri: &str,
    metadata: &str,
) -> Result<(), CoreError> {
    if uri.trim().is_empty() {
        return Err(ProtoError::Malformed("cannot load an empty source URI".into()).into());
    }
    if uri.starts_with("x-rincon:") {
        return Err(ProtoError::Malformed(
            "group-join URIs cannot be loaded silently; use scene group membership".into(),
        )
        .into());
    }
    let host = addr(households, coordinator)?;
    coordinator_at(t, host, coordinator)?;
    Ok(source_start::load(t, host, coordinator, uri, metadata)?)
}

/// Select an existing queue slot, without Play or a mode change.
pub fn load_queue_from<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    position: u32,
) -> Result<(), CoreError> {
    load_queue(t, households, coordinator, position, false)
}

/// Select an existing queue slot without Play, with shuffle/repeat disabled.
/// The mode changes only after queue selection, as in an ordered DJ start.
pub fn load_queue_from_in_order<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    position: u32,
) -> Result<(), CoreError> {
    load_queue(t, households, coordinator, position, true)
}

fn load_queue<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    position: u32,
    in_order: bool,
) -> Result<(), CoreError> {
    if position == 0 {
        return Err(ProtoError::Malformed("queue positions start at one".into()).into());
    }
    let host = addr(households, coordinator)?;
    coordinator_at(t, host, coordinator)?;
    Ok(queue_start::load(t, host, coordinator, position, in_order)?)
}

/// Load a Spotify track through the household's own account, without Play.
pub fn load_spotify_track<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    spotify_uri: &str,
    title: &str,
) -> Result<(), CoreError> {
    spotify_track(t, households, coordinator, spotify_uri, title, false)
}

pub(super) fn spotify_track<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    spotify_uri: &str,
    title: &str,
    autoplay: bool,
) -> Result<(), CoreError> {
    let source = || spotify_track_source(t, households, coordinator, spotify_uri, title);
    let render = |(uri, metadata): &(String, String)| {
        if autoplay {
            play_uri(t, households, coordinator, uri, metadata)
        } else {
            load_uri(t, households, coordinator, uri, metadata)
        }
    };
    let initial = source()?.ok_or(CoreError::NoSpotifyFavorite)?;
    match render(&initial) {
        Err(error) if is_render_800(&error) => {
            let refreshed = source()?.ok_or(CoreError::NoSpotifyFavorite)?;
            match render(&refreshed) {
                Err(error) if is_render_800(&error) => Err(CoreError::RenderParamsStale),
                other => other,
            }
        }
        other => other,
    }
}

/// Stage, verify and select a Spotify album or playlist without Play.
/// The old queue is paused before appending; failure is not a rollback.
pub fn load_spotify_container<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    kind: SpotifyContainer,
    spotify_uri: &str,
    title: &str,
) -> Result<(), CoreError> {
    spotify_container(t, households, coordinator, kind, spotify_uri, title, false)
}

pub(super) fn spotify_container<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    kind: SpotifyContainer,
    spotify_uri: &str,
    title: &str,
    autoplay: bool,
) -> Result<(), CoreError> {
    let host = addr(households, coordinator)?;
    let source = |rejected: Option<&(String, String)>| -> Result<(String, String), CoreError> {
        let favorites = content::browse_all(t, host, "FV:2")?;
        match rejected {
            Some(old) => spotify_source::after_rejection(&favorites, kind, spotify_uri, title, old),
            None => spotify_source::container(&favorites, kind, spotify_uri, title),
        }
        .ok_or(CoreError::NoSpotifyFavorite)
    };
    let render = |(uri, metadata): &(String, String)| {
        if autoplay {
            crate::queue_source::play(t, households, coordinator, uri, metadata)
        } else {
            crate::queue_source::load(t, households, coordinator, uri, metadata)
        }
    };
    let initial = source(None)?;
    match render(&initial) {
        Err(error) if is_render_800(&error) => match render(&source(Some(&initial))?) {
            Err(error) if is_render_800(&error) => Err(CoreError::RenderParamsStale),
            other => other,
        },
        other => other,
    }
}
