//! Control orchestration: carry out a command addressed to a player.
//!
//! The surfaces decide which player a request addresses (fsonos-api plans a
//! request into a coordinator- or room-addressed command); this module finds
//! that player on the LAN and sends the SOAP actions. Every mutating command
//! from every surface comes through here, so this is where house policy and
//! the action log hook in.

use crate::{CoreError, HouseholdState};
use fsonos_proto::ProtoError;
use fsonos_proto::Transport;
use fsonos_proto::content;
use fsonos_proto::control::{self as soap, PositionInfo, TransportInfo};
use fsonos_proto::didl::{
    SpotifyContainer, SpotifyRenderParams, learn_spotify_params, spotify_container_didl,
    spotify_container_uri, spotify_queue_uri, spotify_track_didl, spotify_track_uri,
};
use fsonos_types::{Player, PlayerId};
use std::net::IpAddr;

/// The player `id`, in whichever household knows it.
pub fn locate<'a>(
    households: &'a [HouseholdState],
    id: &PlayerId,
) -> Result<&'a Player, CoreError> {
    households
        .iter()
        .find_map(|h| h.player(id))
        .ok_or_else(|| CoreError::UnknownPlayer(id.0.clone()))
}

fn addr(households: &[HouseholdState], id: &PlayerId) -> Result<IpAddr, CoreError> {
    locate(households, id).map(|p| p.ip)
}

/// Resume (or start) playback on the group `coordinator` leads.
pub fn resume<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
) -> Result<(), CoreError> {
    Ok(soap::play(t, addr(households, coordinator)?)?)
}

pub fn pause<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
) -> Result<(), CoreError> {
    Ok(soap::pause(t, addr(households, coordinator)?)?)
}

pub fn stop<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
) -> Result<(), CoreError> {
    Ok(soap::stop(t, addr(households, coordinator)?)?)
}

pub fn next<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
) -> Result<(), CoreError> {
    Ok(soap::next(t, addr(households, coordinator)?)?)
}

pub fn previous<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
) -> Result<(), CoreError> {
    Ok(soap::previous(t, addr(households, coordinator)?)?)
}

/// Point the group `coordinator` leads at a renderer-ready `uri` (with its
/// DIDL-Lite `metadata`, possibly empty) and start it.
pub fn play_uri<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    uri: &str,
    metadata: &str,
) -> Result<(), CoreError> {
    let host = addr(households, coordinator)?;
    soap::set_av_transport_uri(t, host, uri, metadata)?;
    Ok(soap::play(t, host)?)
}

/// The household's Spotify render parameters, learned from its own
/// favorites (browsed through `coordinator`). `None` when the household has
/// no Spotify track favorite to learn from: link Spotify in that household's
/// Sonos app and add any Spotify track to My Sonos.
pub fn spotify_params<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
) -> Result<Option<SpotifyRenderParams>, CoreError> {
    let favorites = content::browse_all(t, addr(households, coordinator)?, "FV:2")?;
    Ok(learn_spotify_params(&favorites))
}

/// The renderer-ready URI and DIDL-Lite metadata that play `spotify_uri` (a
/// `spotify:track:<id>`) in the household `coordinator` belongs to. Pass them
/// to [`play_uri`]. A single track plays to its end and stops; for continuous
/// play use [`queue_spotify_tracks`]. `Ok(None)` as for [`spotify_params`].
pub fn spotify_track_source<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    spotify_uri: &str,
    title: &str,
) -> Result<Option<(String, String)>, CoreError> {
    Ok(spotify_params(t, households, coordinator)?.map(|p| {
        (
            spotify_track_uri(spotify_uri, &p),
            spotify_track_didl(spotify_uri, title, &p),
        )
    }))
}

/// A UPnP fault code the renderer uses to refuse a render whose service
/// parameters it does not accept (stale `sid`/`flags`/`sn`/descriptor).
fn is_render_800(e: &CoreError) -> bool {
    matches!(e, CoreError::Proto(ProtoError::SoapFault { code: 800, .. }))
}

/// Play `spotify_uri` (a `spotify:track:<id>`) on the group `coordinator`
/// leads, self-healing one round of parameter drift: if the renderer
/// refuses the first attempt with UPnP 800, the household's render
/// parameters are relearned from its favorites and the render retried
/// exactly once. A second 800 (or a household that lost its Spotify
/// favorites mid-flight) fails with [`CoreError::RenderParamsStale`] /
/// [`CoreError::NoSpotifyFavorite`]; the retry never loops.
pub fn play_spotify_track<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    spotify_uri: &str,
    title: &str,
) -> Result<(), CoreError> {
    let source = |uri: &str| spotify_track_source(t, households, coordinator, uri, title);
    let Some((uri, didl)) = source(spotify_uri)? else {
        return Err(CoreError::NoSpotifyFavorite);
    };
    match play_uri(t, households, coordinator, &uri, &didl) {
        Ok(()) => Ok(()),
        // Parameters drift when Spotify is relinked, the service updates,
        // or the household is rebuilt: what the favorites carried when we
        // learned them is no longer what the renderer accepts. Learn them
        // again and render once more.
        Err(e) if is_render_800(&e) => {
            let Some((uri, didl)) = source(spotify_uri)? else {
                return Err(CoreError::NoSpotifyFavorite);
            };
            match play_uri(t, households, coordinator, &uri, &didl) {
                Ok(()) => Ok(()),
                Err(e) if is_render_800(&e) => Err(CoreError::RenderParamsStale),
                Err(e) => Err(e),
            }
        }
        Err(e) => Err(e),
    }
}

/// Replace the queue of the group `coordinator` leads with a Spotify album
/// or playlist (`spotify:album:<id>`, `spotify:playlist:<id>`) and play it
/// from its first track: the speaker expands the container into its tracks
/// as it is enqueued. Rendered with the household's own Spotify account and
/// container prefix (learned from its favorites), self-healing one round of
/// parameter drift as [`play_spotify_track`] does.
pub fn play_spotify_container<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    kind: SpotifyContainer,
    spotify_uri: &str,
    title: &str,
) -> Result<(), CoreError> {
    let host = addr(households, coordinator)?;
    let source = || -> Result<(String, String), CoreError> {
        let favorites = content::browse_all(t, host, "FV:2")?;
        let params = learn_spotify_params(&favorites).ok_or(CoreError::NoSpotifyFavorite)?;
        let prefix = kind.prefix_in(&favorites);
        Ok((
            spotify_container_uri(spotify_uri, &prefix, &params),
            spotify_container_didl(spotify_uri, title, kind, &prefix, &params),
        ))
    };
    let enqueue = |(uri, didl): &(String, String)| -> Result<u32, CoreError> {
        soap::remove_all_tracks_from_queue(t, host)?;
        Ok(soap::add_uri_to_queue(t, host, uri, didl, false)?)
    };
    let first = match enqueue(&source()?) {
        Ok(first) => first,
        Err(e) if is_render_800(&e) => match enqueue(&source()?) {
            Ok(first) => first,
            Err(e) if is_render_800(&e) => return Err(CoreError::RenderParamsStale),
            Err(e) => return Err(e),
        },
        Err(e) => return Err(e),
    };
    play_queue_from(t, households, coordinator, first)
}

/// Append Spotify tracks (`(spotify:track URI, title)`) to the queue of the
/// group `coordinator` leads, in order. Returns the queue position of the
/// first one added, for [`play_queue_from`]. `Ok(None)` when nothing was
/// enqueued: `tracks` is empty, or the household has no Spotify track
/// favorite to learn from (see [`spotify_params`]).
pub fn queue_spotify_tracks<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    tracks: &[(&str, &str)],
) -> Result<Option<u32>, CoreError> {
    let Some(params) = spotify_params(t, households, coordinator)? else {
        return Ok(None);
    };
    let host = addr(households, coordinator)?;
    let mut first = None;
    for (uri, title) in tracks {
        let at = soap::add_uri_to_queue(
            t,
            host,
            &spotify_queue_uri(uri),
            &spotify_track_didl(uri, title, &params),
            false,
        )?;
        first.get_or_insert(at);
    }
    Ok(first)
}

/// Make the group `coordinator` leads play its own queue from `position`
/// (1-based), so playback continues through the queue.
pub fn play_queue_from<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    position: u32,
) -> Result<(), CoreError> {
    let host = addr(households, coordinator)?;
    soap::play_from_queue(t, host, coordinator)?;
    soap::seek_track(t, host, position)?;
    Ok(soap::play(t, host)?)
}

/// Set one room's volume (on the room's own player); returns the new level.
pub fn set_volume<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    player: &PlayerId,
    level: u8,
) -> Result<u8, CoreError> {
    let level = level.min(100);
    soap::set_volume(t, addr(households, player)?, level)?;
    Ok(level)
}

/// Raise or lower one room's volume; returns the new level.
pub fn adjust_volume<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    player: &PlayerId,
    delta: i32,
) -> Result<u8, CoreError> {
    Ok(soap::set_relative_volume(
        t,
        addr(households, player)?,
        delta,
    )?)
}

/// Set the group volume of the group `coordinator` leads; members keep their
/// balance. Returns the new level.
pub fn set_group_volume<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    level: u8,
) -> Result<u8, CoreError> {
    let level = level.min(100);
    soap::set_group_volume(t, addr(households, coordinator)?, level)?;
    Ok(level)
}

/// Raise or lower the group volume; returns the new level.
pub fn adjust_group_volume<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    delta: i32,
) -> Result<u8, CoreError> {
    Ok(soap::set_relative_group_volume(
        t,
        addr(households, coordinator)?,
        delta,
    )?)
}

pub fn set_mute<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    player: &PlayerId,
    mute: bool,
) -> Result<(), CoreError> {
    Ok(soap::set_mute(t, addr(households, player)?, mute)?)
}

/// Move `member` into the group `coordinator` leads.
pub fn join<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    member: &PlayerId,
    coordinator: &PlayerId,
) -> Result<(), CoreError> {
    // Both must be known: a join across households can never work.
    locate(households, coordinator)?;
    Ok(soap::join_group(t, addr(households, member)?, coordinator)?)
}

/// Take `member` out of its group into a group of its own.
pub fn leave<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    member: &PlayerId,
) -> Result<(), CoreError> {
    Ok(soap::leave_group(t, addr(households, member)?)?)
}

/// What the group `coordinator` leads is doing right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Playback {
    pub transport: TransportInfo,
    pub position: PositionInfo,
}

pub fn playback<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
) -> Result<Playback, CoreError> {
    let host = addr(households, coordinator)?;
    Ok(Playback {
        transport: soap::get_transport_info(t, host)?,
        position: soap::get_position_info(t, host)?,
    })
}

/// One room's volume, read from its own player.
pub fn volume<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    player: &PlayerId,
) -> Result<u8, CoreError> {
    Ok(soap::get_volume(t, addr(households, player)?)?)
}
