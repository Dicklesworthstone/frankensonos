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
use fsonos_proto::content::{self, BrowseFlag, QUEUE};
use fsonos_proto::control::{self as soap, PositionInfo, TransportInfo};
use fsonos_proto::didl::{
    SpotifyContainer, SpotifyRenderParams, learn_spotify_params, spotify_container_didl,
    spotify_container_uri, spotify_queue_uri, spotify_track_didl, spotify_track_uri,
};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_types::{Player, PlayerId, TransportState};
use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::time::{Duration, Instant};

/// How long a player that left its group may take to show alone: real ones
/// settle within about a second, but one that just finished an announcement
/// can take well over 3 s (fleet-verified 2026-10-12: a real S1 group lead's
/// restore 402'd spuriously at 3 s while the topology ended up correct; the
/// sim, which settles instantly, never reproduces it).
const LEAVE_WAIT: Duration = Duration::from_secs(10);
const LEAVE_POLL: Duration = Duration::from_millis(100);

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

/// Pause the group `coordinator` leads (see [`pause_at`]).
pub fn pause<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
) -> Result<(), CoreError> {
    pause_at(t, addr(households, coordinator)?)
}

/// Pause the group whose coordinator answers at `host`. A player refuses
/// Pause (UPnP 701) both when its group is not playing and when what plays
/// is a stream that cannot pause, so a refusal is read back: a group already
/// paused or stopped stays as it is (a set's pause goes on to the next
/// group), and one still playing is stopped, as a stream resumes live anyway.
pub fn pause_at<T: Transport + ?Sized>(t: &T, host: IpAddr) -> Result<(), CoreError> {
    let refused = match soap::pause(t, host) {
        Err(e @ ProtoError::SoapFault { code: 701, .. }) => e,
        done => return Ok(done?),
    };
    match soap::get_transport_info(t, host).map(|info| info.state) {
        Ok(TransportState::Paused | TransportState::Stopped) => Ok(()),
        Ok(TransportState::Playing | TransportState::Transitioning) => Ok(soap::stop(t, host)?),
        _ => Err(refused.into()),
    }
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
/// parameter drift as [`play_spotify_track`] does, whether the renderer
/// refuses the enqueue or the play that follows it (docs/PROTOCOL.md: 800
/// "at/after SetAVTransportURI").
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
    let render = |(uri, didl): &(String, String)| -> Result<(), CoreError> {
        soap::remove_all_tracks_from_queue(t, host)?;
        let first = soap::add_uri_to_queue(t, host, uri, didl, false)?;
        play_queue_from(t, households, coordinator, first)
    };
    match render(&source()?) {
        Ok(()) => Ok(()),
        Err(e) if is_render_800(&e) => match render(&source()?) {
            Ok(()) => Ok(()),
            Err(e) if is_render_800(&e) => Err(CoreError::RenderParamsStale),
            Err(e) => Err(e),
        },
        Err(e) => Err(e),
    }
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

/// Tracks [`enqueue_spotify`] added to a group's queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Queued {
    /// The queue position (1-based) of the first.
    pub first: u32,
    /// What each added track renders, in queue order.
    pub uris: Vec<String>,
    /// Whether the group plays from its queue now, so they will play.
    pub playing_queue: bool,
}

/// Add a Spotify track, album or playlist (`spotify:<kind>:<id>`) to the
/// queue of the group `coordinator` leads: at its end, or with `next` right
/// after the track playing (when the group plays its queue; else at the
/// end too). An album or playlist adds its tracks. Rendered with the
/// household's own Spotify account, relearned and retried once on UPnP 800
/// as [`play_spotify_track`] does.
pub fn enqueue_spotify<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    spotify_uri: &str,
    title: &str,
    next: bool,
) -> Result<Queued, CoreError> {
    let host = addr(households, coordinator)?;
    let playing_queue = soap::get_media_info(t, host)?
        .uri
        .starts_with("x-rincon-queue:");
    let at = if next && playing_queue {
        soap::get_position_info(t, host)?.track.saturating_add(1)
    } else {
        0
    };
    let kind = SpotifyContainer::of(spotify_uri);
    let source = || -> Result<(String, String), CoreError> {
        let favorites = content::browse_all(t, host, "FV:2")?;
        let params = learn_spotify_params(&favorites).ok_or(CoreError::NoSpotifyFavorite)?;
        Ok(match kind {
            Some(kind) => {
                let prefix = kind.prefix_in(&favorites);
                (
                    spotify_container_uri(spotify_uri, &prefix, &params),
                    spotify_container_didl(spotify_uri, title, kind, &prefix, &params),
                )
            }
            None => (
                spotify_queue_uri(spotify_uri),
                spotify_track_didl(spotify_uri, title, &params),
            ),
        })
    };
    let add = |(uri, didl): &(String, String)| -> Result<soap::Enqueued, CoreError> {
        Ok(soap::enqueue_uri(t, host, uri, didl, at, next)?)
    };
    let added = match add(&source()?) {
        Err(e) if is_render_800(&e) => match add(&source()?) {
            Err(e) if is_render_800(&e) => return Err(CoreError::RenderParamsStale),
            other => other?,
        },
        other => other?,
    };
    let (uris, _) = queue_uris(t, host, added.first, added.count)?;
    Ok(Queued {
        first: added.first,
        uris,
        playing_queue,
    })
}

/// Take tracks [`enqueue_spotify`] added (`uris`, from queue position
/// `first`) off the queue of the group `coordinator` leads again.
/// `Ok(false)`, with nothing removed, when the queue no longer holds them
/// there.
pub fn unqueue<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    first: u32,
    uris: &[String],
) -> Result<bool, CoreError> {
    if uris.is_empty() {
        return Ok(true);
    }
    let host = addr(households, coordinator)?;
    let count = u32::try_from(uris.len()).unwrap_or(u32::MAX);
    let (now, update_id) = queue_uris(t, host, first, count)?;
    if now != uris {
        return Ok(false);
    }
    soap::remove_track_range_from_queue(t, host, update_id, first, count)?;
    Ok(true)
}

/// The URIs of up to `count` queue tracks from position `first` (1-based)
/// of the coordinator at `host`, and the queue's update id as read.
fn queue_uris<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    first: u32,
    count: u32,
) -> Result<(Vec<String>, u32), CoreError> {
    let mut uris = Vec::new();
    let mut update_id = 0;
    let mut got = 0;
    while got < count {
        let page = content::browse(
            t,
            host,
            QUEUE,
            BrowseFlag::DirectChildren,
            first.saturating_sub(1).saturating_add(got),
            (count - got).min(100),
        )?;
        update_id = page.update_id;
        if page.objects.is_empty() {
            break;
        }
        uris.extend(
            page.objects
                .into_iter()
                .map(|o| o.res.map_or_else(String::new, |r| r.uri)),
        );
        got = u32::try_from(uris.len()).unwrap_or(u32::MAX);
    }
    Ok((uris, update_id))
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

/// A room's tone: bass and treble from -10 to 10 (0 is flat), and whether
/// loudness compensation is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tone {
    pub bass: i8,
    pub treble: i8,
    pub loudness: bool,
}

/// What to change of a room's tone; `None` leaves that part as it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ToneChange {
    pub bass: Option<i8>,
    pub treble: Option<i8>,
    pub loudness: Option<bool>,
}

impl ToneChange {
    /// Every part, as `tone` has it (an undo puts a tone back whole).
    #[must_use]
    pub fn all(tone: Tone) -> Self {
        Self {
            bass: Some(tone.bass),
            treble: Some(tone.treble),
            loudness: Some(tone.loudness),
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bass.is_none() && self.treble.is_none() && self.loudness.is_none()
    }
}

/// The tone of `player` (a room's primary: it sets it for the room).
pub fn tone<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    player: &PlayerId,
) -> Result<Tone, CoreError> {
    let host = addr(households, player)?;
    Ok(Tone {
        bass: soap::get_bass(t, host)?,
        treble: soap::get_treble(t, host)?,
        loudness: soap::get_loudness(t, host)?,
    })
}

/// Change what `change` names of `player`'s tone.
pub fn set_tone<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    player: &PlayerId,
    change: ToneChange,
) -> Result<(), CoreError> {
    let host = addr(households, player)?;
    if let Some(bass) = change.bass {
        soap::set_bass(t, host, bass)?;
    }
    if let Some(treble) = change.treble {
        soap::set_treble(t, host, treble)?;
    }
    if let Some(loudness) = change.loudness {
        soap::set_loudness(t, host, loudness)?;
    }
    Ok(())
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

/// Take `member` out of its group into a group of its own. Players answer
/// the request with UPnP 402 and leave anyway (docs/PROTOCOL.md §6), so
/// then the topology is read back until `member` leads a group of its own;
/// still grouped after [`LEAVE_WAIT`], the player's 402 stands.
pub fn leave<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    member: &PlayerId,
) -> Result<(), CoreError> {
    let host = addr(households, member)?;
    if soap::leave_group(t, host, member)? == soap::Leaving::Done {
        return Ok(());
    }
    let started = Instant::now();
    loop {
        // Alone: it leads its group, with nobody visible but itself (a
        // stereo pair's other half and a sub are invisible members).
        let alone = get_zone_group_state(t, host)?.groups.iter().any(|g| {
            g.coordinator == *member && g.members.iter().all(|m| m.uuid == *member || m.invisible)
        });
        if alone {
            return Ok(());
        }
        let left = LEAVE_WAIT.saturating_sub(started.elapsed());
        if left.is_zero() {
            return Err(ProtoError::SoapFault {
                code: 402,
                reason: "asked to leave its group, the player is still in it".into(),
            }
            .into());
        }
        std::thread::sleep(LEAVE_POLL.min(left));
    }
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
