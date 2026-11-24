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
    SpotifyBare, SpotifyContainer, SpotifyRenderParams, learn_spotify_params, spotify_bare_didl,
    spotify_bare_uri, spotify_queue_uri, spotify_track_didl,
};
use fsonos_proto::topology::{ZoneGroupState, get_zone_group_state};
use fsonos_types::{Player, PlayerId};
use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::time::{Duration, Instant};

mod grouping_state;
mod loading;
mod native_tracks;
mod navigation;
mod queue_insert;
pub mod queue_removal;
mod queue_start;
mod seek_absolute;
mod seek_relative;
mod source_start;
mod spotify_source;
mod spotify_tracks;
mod track_batch;
mod transport_state;

pub use crate::queue_source::QueueReplacement;
pub use crate::queue_source::destination::Destination as SourceDestination;
pub use loading::{
    load_queue_from, load_queue_from_in_order, load_spotify_container,
    load_spotify_container_with_receipt, load_spotify_track, load_uri, pause_confirmed,
};
pub use native_tracks::{enqueue_native_tracks_bound, play_native_tracks_bound_with_receipt};
pub use spotify_tracks::{
    MAX_SPOTIFY_TRACK_BATCH, enqueue_spotify_tracks, enqueue_spotify_tracks_bound,
    load_spotify_tracks, load_spotify_tracks_bound, load_spotify_tracks_bound_with_receipt,
    play_spotify_tracks, play_spotify_tracks_bound, play_spotify_tracks_bound_with_receipt,
    play_spotify_tracks_with_receipt,
};
pub use track_batch::{MAX_TRACK_BATCH, TrackBatchDestination};

/// How long a player that left its group may take to show alone (real ones
/// do within about a second). Not a settle-time knob: a fleet-verified
/// 2026-10-12 case (announce restore on a stereo-pair fleet) showed the
/// player's own ZoneGroupState read-back staying stale far past 10 s while
/// the leave had in fact happened (the 402 on this verb means "leaving
/// anyway", docs/PROTOCOL.md §6); waiting longer only adds latency to the
/// false-failure report. See bead frankensonos-p9f for the real fix.
const LEAVE_WAIT: Duration = Duration::from_secs(3);
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

/// Verify that the addressed endpoint still belongs to the requested player
/// before a room-addressed read or mutation. Group members and bonded
/// satellites remain valid targets; bridges cannot render audio. Unavailable
/// or incoherent topology retains its protocol error. A stale Location needs
/// the endpoint's own matching device description and stable topology.
/// This sends no writes.
pub fn require_player<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
    player: &PlayerId,
) -> Result<(), CoreError> {
    let topology = crate::topology::read_at(transport, host, player)?;
    crate::topology::validate(&topology)?;
    if topology
        .groups
        .iter()
        .flat_map(|group| &group.members)
        .flat_map(|member| std::iter::once(member).chain(&member.satellites))
        .any(|member| member.uuid == *player && member.ip() == Some(host) && !member.is_zone_bridge)
    {
        Ok(())
    } else {
        Err(CoreError::TargetChanged(
            "the requested player no longer owns this address".into(),
        ))
    }
}

/// Verify that the addressed endpoint still belongs to the requested group
/// coordinator before a coordinator-addressed mutation. A household-wide
/// topology merely naming the player is not proof that it owns this address.
/// A known mismatch returns [`CoreError::TargetChanged`]; unavailable or
/// incoherent topology retains its protocol error. A stale Location needs
/// independent endpoint identity, without changing the coordinator or group.
/// This function sends no writes.
pub fn require_coordinator<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
    coordinator: &PlayerId,
) -> Result<(), CoreError> {
    let topology = crate::topology::read_at(transport, host, coordinator)?;
    if crate::topology::coordinator_at(&topology, host, coordinator)? {
        Ok(())
    } else {
        Err(coordinator_changed())
    }
}

fn coordinator_changed() -> CoreError {
    CoreError::TargetChanged(
        "the requested coordinator no longer leads its group at this address".into(),
    )
}

fn topology_player_at(topology: &ZoneGroupState, host: IpAddr, player: &PlayerId) -> bool {
    topology
        .groups
        .iter()
        .flat_map(|group| &group.members)
        .flat_map(|member| std::iter::once(member).chain(&member.satellites))
        .any(|member| member.uuid == *player && member.ip() == Some(host))
}

/// Cleanup immediately after a confirmed leave may use a household witness
/// when the player's own coherent topology still shows its old membership.
/// Address movement, missing players and unreadable self evidence stay strict.
/// Every candidate is read once within the leave budget; no writes occur here.
pub(crate) fn require_coordinator_after_leave<T: Transport + ?Sized>(
    transport: &T,
    households: &[HouseholdState],
    host: IpAddr,
    coordinator: &PlayerId,
) -> Result<(), CoreError> {
    let until = Instant::now() + LEAVE_WAIT;
    let topology = crate::topology::read_at(transport, host, coordinator)?;
    if crate::topology::coordinator_at(&topology, host, coordinator)? {
        return Ok(());
    }
    let original = coordinator_changed();
    if !topology_player_at(&topology, host, coordinator) {
        return Err(original);
    }
    let mut matches = households
        .iter()
        .filter(|house| house.player(coordinator).is_some());
    let Some(household) = matches.next() else {
        return Err(original);
    };
    if matches.next().is_some() {
        return Err(original);
    }
    let mut read = vec![host];
    for witness in household_witnesses(household, coordinator) {
        if read.contains(&witness.ip) {
            continue;
        }
        if Instant::now() >= until {
            break;
        }
        read.push(witness.ip);
        if let Ok(topology) = get_zone_group_state(transport, witness.ip)
            && crate::topology::coordinator_at(&topology, host, coordinator).unwrap_or(false)
            && topology_player_at(&topology, witness.ip, &witness.id)
            && Instant::now() < until
        {
            return Ok(());
        }
    }
    Err(original)
}

/// Prefer another room's view; a bonded player's view can share the same lag.
fn household_witnesses<'a>(household: &'a HouseholdState, member: &PlayerId) -> Vec<&'a Player> {
    let mut players: Vec<_> = household
        .players
        .iter()
        .filter(|p| &p.id != member)
        .collect();
    players.sort_by_key(|player| {
        household
            .rooms
            .iter()
            .any(|room| room.players.contains(&player.id) && room.players.contains(member))
    });
    players
}

/// Resume (or start) playback on the group `coordinator` leads. A lost
/// acknowledgement is confirmed by read-back, not another Play.
pub fn resume<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
) -> Result<(), CoreError> {
    transport_state::play(t, addr(households, coordinator)?, coordinator)
}

/// Pause the group `coordinator` leads (see [`pause_at`]).
pub fn pause<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
) -> Result<(), CoreError> {
    let host = addr(households, coordinator)?;
    require_coordinator(t, host, coordinator)?;
    Ok(transport_state::pause(t, host, Some(coordinator))?)
}

/// Pause the group whose coordinator answers at `host`. A player refuses
/// Pause (UPnP 701) both when its group is not playing and when what plays
/// is a stream that cannot pause, so a refusal is read back: a group already
/// paused or stopped stays as it is (a set's pause goes on to the next
/// group), and one still playing is stopped, as a stream resumes live anyway.
/// Lost or malformed replies are observed rather than replayed or treated as
/// a 701 refusal. A confirmed inactive state satisfies the pause request.
pub fn pause_at<T: Transport + ?Sized>(t: &T, host: IpAddr) -> Result<(), CoreError> {
    Ok(transport_state::pause(t, host, None)?)
}

pub fn stop<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
) -> Result<(), CoreError> {
    let host = addr(households, coordinator)?;
    require_coordinator(t, host, coordinator)?;
    Ok(transport_state::stop(t, host, Some(coordinator))?)
}

/// Advance once. An uncertain reply can be confirmed only against the exact
/// adjacent occurrence of an unchanged, normally ordered queue; never replayed.
pub fn next<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
) -> Result<(), CoreError> {
    navigation::step(
        t,
        addr(households, coordinator)?,
        coordinator,
        navigation::Step::Next,
    )
}

/// Go back once, retaining the renderer's native Previous behavior. A lost
/// reply is recovered only when an exact adjacent queue occurrence proves it.
pub fn previous<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
) -> Result<(), CoreError> {
    navigation::step(
        t,
        addr(households, coordinator)?,
        coordinator,
        navigation::Step::Previous,
    )
}

/// Where [`seek`] goes in the current track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seek {
    /// This many seconds from the track's start.
    At(u32),
    /// This many seconds from where it is now; negative goes back.
    By(i64),
}

/// Move the group `coordinator` leads to `to` in its current track. A
/// relative move requires a reported position and stays within the track.
/// An uncertain reply is read back against the retained track occurrence,
/// never retried. Absolute seeks retain optional source and queue evidence;
/// unavailable evidence does not prevent the original one-shot request.
/// Returns the requested absolute position, in seconds from the start.
pub fn seek<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    to: Seek,
) -> Result<u32, CoreError> {
    let host = addr(households, coordinator)?;
    match to {
        Seek::At(secs) => seek_absolute::seek(t, host, coordinator, secs),
        Seek::By(by) => seek_relative::seek(t, host, coordinator, by),
    }
}

/// Point the group `coordinator` leads at a renderer-ready `uri` (with its
/// DIDL-Lite `metadata`, possibly empty) and start it. Each stage is sent
/// once; an ambiguous reply advances only when read-back confirms its effect.
pub fn play_uri<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    uri: &str,
    metadata: &str,
) -> Result<(), CoreError> {
    let host = addr(households, coordinator)?;
    source_start::play(t, host, coordinator, uri, metadata)
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
    let favorites = content::browse_all(t, addr(households, coordinator)?, "FV:2")?;
    Ok(spotify_source::track(
        &favorites,
        spotify_uri,
        title,
        spotify_source::TrackUse::Direct,
        None,
    ))
}

/// Check that a canonical Spotify source has a renderer template in this
/// player's household, without changing playback or the queue. The player
/// may be a group member that a scene will make coordinator later. Container
/// learning uses the same complete favorite (or track fallback) as playback;
/// an album-only favorite is sufficient for an album, but not for a track.
/// This proves local prerequisites, not that Spotify will deliver the audio.
pub fn check_spotify_source<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    player: &PlayerId,
    spotify_uri: &str,
) -> Result<(), CoreError> {
    if spotify_uri.starts_with("spotify:track:")
        || matches!(SpotifyBare::of(spotify_uri), Some(SpotifyBare::Episode))
    {
        return spotify_params(t, households, player)?
            .map(|_| ())
            .ok_or(CoreError::NoSpotifyFavorite);
    }
    if let Some(kind) = SpotifyContainer::of(spotify_uri) {
        let favorites = content::browse_all(t, addr(households, player)?, "FV:2")?;
        return spotify_source::container(&favorites, kind, spotify_uri, spotify_uri)
            .map(|_| ())
            .ok_or(CoreError::NoSpotifyFavorite);
    }
    Err(ProtoError::Malformed("unsupported Spotify source kind".into()).into())
}

/// A UPnP fault code the renderer uses to refuse a render whose service
/// parameters it does not accept (stale `sid`/`flags`/`sn`/descriptor).
fn is_render_800(e: &CoreError) -> bool {
    matches!(e, CoreError::Proto(ProtoError::SoapFault { code: 800, .. }))
}

/// Play `spotify_uri` (a `spotify:track:<id>`) on the group `coordinator`
/// leads, self-healing one round of parameter drift: if the renderer
/// refuses the initial source selection with UPnP 800, the household's render
/// parameters are relearned from its favorites and the render retried
/// exactly once. A second 800 (or a household that lost its Spotify
/// favorites mid-flight) fails with [`CoreError::RenderParamsStale`] /
/// [`CoreError::NoSpotifyFavorite`]; the retry never loops. Once the source is
/// selected, a failed Play is an uncertain overall start and never replays
/// the completed source change, even when Play explicitly returns 800.
pub fn play_spotify_track<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    spotify_uri: &str,
    title: &str,
) -> Result<(), CoreError> {
    loading::spotify_track(t, households, coordinator, spotify_uri, title, true)
}

/// Replace the queue of the group `coordinator` leads with a Spotify album
/// or playlist (`spotify:album:<id>`, `spotify:playlist:<id>`) and play it
/// from its first track: the speaker expands the container into its tracks
/// as it is enqueued. Rendered with the household's own Spotify account and
/// container prefix (learned together from an album or playlist favorite;
/// no track favorite is required for that kind), with one fresh parameter
/// learning attempt if the renderer explicitly refuses the initial enqueue.
/// The old queue remains intact until the replacement has been accepted and
/// verified. An uncertain append or any failed start after queue commit never
/// replays the replacement; read the queue before trying again.
pub fn play_spotify_container<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    kind: SpotifyContainer,
    spotify_uri: &str,
    title: &str,
) -> Result<(), CoreError> {
    loading::spotify_container(t, households, coordinator, kind, spotify_uri, title, true)
}

/// Play a Spotify album or playlist and retain the exact staged replacement
/// receipt. Pair it with a prior bounded [`crate::actions::capture_queue_undo`]
/// to make a successful replacement reversible. Receipt collection never
/// replays a mutation or turns unavailable undo evidence into a Play failure.
pub fn play_spotify_container_with_receipt<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    kind: SpotifyContainer,
    spotify_uri: &str,
    title: &str,
) -> Result<QueueReplacement, CoreError> {
    loading::spotify_container_with_receipt(t, households, coordinator, kind, spotify_uri, title)
}

/// Play a bare Spotify kind (`spotify:artist:<id>`, `spotify:episode:<id>`,
/// `spotify:show:<id>`) on the group `coordinator` leads. The account id is
/// taken from the household's Spotify render parameters, so it needs a
/// Spotify favorite ([`CoreError::NoSpotifyFavorite`] otherwise);
/// `flags`/`sn`/the descriptor are not sent for these kinds (they are
/// refused with 402 when present).
pub fn play_spotify_bare<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    spotify_uri: &str,
    title: &str,
) -> Result<(), CoreError> {
    loading::spotify_bare(t, households, coordinator, spotify_uri, title, true)
}

/// [`play_spotify_bare`] without the Play: the group is paused first and the
/// source is selected, ready to resume.
pub fn load_spotify_bare<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    spotify_uri: &str,
    title: &str,
) -> Result<(), CoreError> {
    loading::pause_confirmed(t, households, coordinator)?;
    loading::spotify_bare(t, households, coordinator, spotify_uri, title, false)
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
    let params = spotify_params(t, households, coordinator)?;
    if params.is_none() && !tracks.iter().any(|(uri, _)| SpotifyBare::of(uri).is_some()) {
        return Ok(None);
    }
    let host = addr(households, coordinator)?;
    let mut first = None;
    for (uri, title) in tracks {
        let (queued, metadata) = if SpotifyBare::of(uri).is_some() {
            let sid = params.as_ref().ok_or(CoreError::NoSpotifyFavorite)?.sid;
            (
                spotify_bare_uri(uri, sid),
                spotify_bare_didl(uri, title, sid),
            )
        } else {
            let params = params.as_ref().ok_or(CoreError::NoSpotifyFavorite)?;
            (
                spotify_queue_uri(uri),
                spotify_track_didl(uri, title, params),
            )
        };
        let at = soap::add_uri_to_queue(t, host, &queued, &metadata, false)?;
        first.get_or_insert(at);
    }
    Ok(first)
}

/// Make the group `coordinator` leads play its own queue from `position`
/// (1-based), so playback continues through the queue. Each stage is sent
/// once. A lost or malformed reply advances only when read-back confirms
/// its effect; no source reset, seek or Play is repeated during recovery.
pub fn play_queue_from<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    position: u32,
) -> Result<(), CoreError> {
    queue_start::play(t, addr(households, coordinator)?, coordinator, position)
}

/// Start the queue in insertion order, with shuffle and repeat disabled.
/// Select the queue before setting its mode: some players reject SetPlayMode
/// while a stream or another source is active. A failed mode change prevents
/// the following seek and Play; it is never mistaken for a lost Play reply.
pub fn play_queue_from_in_order<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    position: u32,
) -> Result<(), CoreError> {
    queue_start::play_in_order(t, addr(households, coordinator)?, coordinator, position)
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
    /// The exact insertion and original group/boot/source, when verified.
    /// Missing evidence does not fail an accepted add, but prevents later undo.
    pub proof: Option<queue_removal::QueuedProof>,
}

/// Add a Spotify track, album or playlist (`spotify:<kind>:<id>`) to the
/// queue of the group `coordinator` leads: at its end, or with `next` right
/// after the track playing (when the group plays its queue; else at the
/// end too). An album or playlist adds its tracks. Rendered with the
/// household's own Spotify account, relearned and retried once on UPnP 800
/// after an unchanged, explicitly refused attempt. A lost track reply is
/// accepted only when a coherent read shows exactly the requested insertion,
/// with the original neighbors intact; an uncertain write is never replayed.
/// Container expansions require an acknowledged range. No playback is started.
pub fn enqueue_spotify<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    spotify_uri: &str,
    title: &str,
    next: bool,
) -> Result<Queued, CoreError> {
    enqueue_spotify_observed(t, households, coordinator, spotify_uri, title, next, None)
}

/// Enqueue only while the source, group, boot and queue observed before a
/// multi-group or catalog request still hold. The original proof survives
/// renderer learning, internal queue reads and the one explicit-800 retry.
/// This never adopts a newer destination or repeats an uncertain write.
pub fn enqueue_spotify_bound<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    spotify_uri: &str,
    title: &str,
    next: bool,
    destination: &TrackBatchDestination,
) -> Result<Queued, CoreError> {
    enqueue_spotify_observed(
        t,
        households,
        coordinator,
        spotify_uri,
        title,
        next,
        Some(destination),
    )
}

fn enqueue_spotify_observed<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    spotify_uri: &str,
    title: &str,
    next: bool,
    destination: Option<&TrackBatchDestination>,
) -> Result<Queued, CoreError> {
    let host = addr(households, coordinator)?;
    if let Some(destination) = destination {
        destination.confirm(t, host, coordinator)?;
    }
    let kind = SpotifyContainer::of(spotify_uri);
    let source = |rejected: Option<&(String, String)>| -> Result<(String, String), CoreError> {
        let favorites = content::browse_all(t, host, "FV:2")?;
        if SpotifyBare::of(spotify_uri).is_some() {
            let params = learn_spotify_params(&favorites).ok_or(CoreError::NoSpotifyFavorite)?;
            return Ok((
                spotify_bare_uri(spotify_uri, params.sid),
                spotify_bare_didl(spotify_uri, title, params.sid),
            ));
        }
        if let Some(kind) = kind {
            return match rejected {
                Some(old) => {
                    spotify_source::after_rejection(&favorites, kind, spotify_uri, title, old)
                }
                None => spotify_source::container(&favorites, kind, spotify_uri, title),
            }
            .ok_or(CoreError::NoSpotifyFavorite);
        }
        spotify_source::track(
            &favorites,
            spotify_uri,
            title,
            spotify_source::TrackUse::Queue,
            rejected,
        )
        .ok_or(CoreError::NoSpotifyFavorite)
    };
    let add = |(uri, metadata): &(String, String)| {
        queue_insert::enqueue(
            t,
            host,
            coordinator,
            &queue_insert::Request {
                source_uri: spotify_uri,
                uri,
                metadata,
                next,
            },
            destination,
        )
    };
    let initial = source(None)?;
    match add(&initial) {
        Err(e) if is_render_800(&e) => match add(&source(Some(&initial))?) {
            Err(e) if is_render_800(&e) => Err(CoreError::RenderParamsStale),
            other => other,
        },
        other => other,
    }
}

/// Remove the currently matching URI range from the coordinator's queue.
/// A stale range returns `Ok(false)` without writing. A matching range uses
/// a nonzero revision and exact read-back; lost replies never repeat removal.
/// This low-level helper does not prove historical occurrence ownership.
/// Action-log undo uses its retained `QueuedProof` instead.
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
    Ok(queue_removal::undo(
        t,
        addr(households, coordinator)?,
        coordinator,
        first,
        uris,
    )?)
}

/// Set one room's volume (on the room's own player); returns the new level.
pub fn set_volume<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    player: &PlayerId,
    level: u8,
) -> Result<u8, CoreError> {
    let level = level.min(100);
    let host = addr(households, player)?;
    require_player(t, host, player)?;
    soap::set_volume(t, host, level)?;
    Ok(level)
}

/// Raise or lower one room's volume; returns the new level.
pub fn adjust_volume<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    player: &PlayerId,
    delta: i32,
) -> Result<u8, CoreError> {
    let host = addr(households, player)?;
    require_player(t, host, player)?;
    Ok(soap::set_relative_volume(t, host, delta)?)
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
    let host = addr(households, coordinator)?;
    require_coordinator(t, host, coordinator)?;
    soap::snapshot_group_volume(t, host)?;
    require_coordinator(t, host, coordinator)?;
    soap::set_group_volume_from_snapshot(t, host, level)?;
    Ok(level)
}

/// Raise or lower the group volume; returns the new level.
pub fn adjust_group_volume<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    delta: i32,
) -> Result<u8, CoreError> {
    let host = addr(households, coordinator)?;
    require_coordinator(t, host, coordinator)?;
    soap::snapshot_group_volume(t, host)?;
    require_coordinator(t, host, coordinator)?;
    Ok(soap::set_relative_group_volume_from_snapshot(
        t, host, delta,
    )?)
}

pub fn set_mute<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    player: &PlayerId,
    mute: bool,
) -> Result<(), CoreError> {
    let host = addr(households, player)?;
    require_player(t, host, player)?;
    Ok(soap::set_mute(t, host, mute)?)
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ToneChange {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bass: Option<i8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub treble: Option<i8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loudness: Option<bool>,
}

impl ToneChange {
    /// Every part, as `tone` has it.
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
    require_player(t, host, player)?;
    let tone = Tone {
        bass: soap::get_bass(t, host)?,
        treble: soap::get_treble(t, host)?,
        loudness: soap::get_loudness(t, host)?,
    };
    require_player(t, host, player)?;
    Ok(tone)
}

/// Change what `change` names of `player`'s tone. A lost acknowledgement is
/// confirmed by reading that field before continuing with the remaining
/// changes; an uncertain write is never repeated.
pub fn set_tone<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    player: &PlayerId,
    change: ToneChange,
) -> Result<(), CoreError> {
    set_tone_tracked(t, households, player, change, &mut ToneChange::default())
}

/// Apply a tone change, retaining only fields whose writes were accepted or
/// may have taken effect. A definite refusal and fields not yet sent are left
/// out, so partial-action undo cannot restore settings it never changed.
pub fn set_tone_tracked<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    player: &PlayerId,
    change: ToneChange,
    touched: &mut ToneChange,
) -> Result<(), CoreError> {
    *touched = ToneChange::default();
    let host = addr(households, player)?;
    if let Some(bass) = change.bass {
        require_player(t, host, player)?;
        let result = confirm_tone_change("SetBass", soap::set_bass(t, host, bass), || {
            require_player(t, host, player)?;
            let matches = soap::get_bass(t, host)? == bass.clamp(-10, 10);
            require_player(t, host, player)?;
            Ok(matches)
        });
        if !matches!(&result, Err(ProtoError::SoapFault { .. })) {
            touched.bass = Some(bass.clamp(-10, 10));
        }
        result?;
    }
    if let Some(treble) = change.treble {
        require_player(t, host, player)?;
        let result = confirm_tone_change("SetTreble", soap::set_treble(t, host, treble), || {
            require_player(t, host, player)?;
            let matches = soap::get_treble(t, host)? == treble.clamp(-10, 10);
            require_player(t, host, player)?;
            Ok(matches)
        });
        if !matches!(&result, Err(ProtoError::SoapFault { .. })) {
            touched.treble = Some(treble.clamp(-10, 10));
        }
        result?;
    }
    if let Some(loudness) = change.loudness {
        require_player(t, host, player)?;
        let result =
            confirm_tone_change("SetLoudness", soap::set_loudness(t, host, loudness), || {
                require_player(t, host, player)?;
                let matches = soap::get_loudness(t, host)? == loudness;
                require_player(t, host, player)?;
                Ok(matches)
            });
        if !matches!(&result, Err(ProtoError::SoapFault { .. })) {
            touched.loudness = Some(loudness);
        }
        result?;
    }
    Ok(())
}

fn confirm_tone_change(
    operation: &'static str,
    result: Result<(), ProtoError>,
    observe: impl FnOnce() -> Result<bool, CoreError>,
) -> Result<(), ProtoError> {
    let Err(error) = result else {
        return Ok(());
    };
    if matches!(
        &error,
        ProtoError::Network { .. } | ProtoError::Malformed(_)
    ) && observe().unwrap_or(false)
    {
        tracing::warn!(operation, cause = %error, "tone change confirmed without another write");
        Ok(())
    } else {
        // A definite refusal, a different value or an unreadable field keeps
        // the original failure and stops the rest of a multi-field change.
        Err(error)
    }
}

/// Move `member` into the group `coordinator` leads. A lost or malformed
/// acknowledgement is confirmed by fresh topology for the whole bonded room;
/// the join is never repeated to recover its reply. Both endpoints and the
/// destination's coordination are checked before the moving room is changed.
pub fn join<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    member: &PlayerId,
    coordinator: &PlayerId,
) -> Result<(), CoreError> {
    let current = grouping_state::prepare_join(t, households, member, coordinator)?;
    let households = std::slice::from_ref(&current);
    let result = soap::join_group(t, addr(households, member)?, coordinator);
    match result {
        Err(cause)
            if grouping_state::uncertain(&cause)
                && grouping_state::joined(t, households, member, coordinator) =>
        {
            tracing::warn!(cause = %cause, "group join confirmed by topology; not repeated");
            Ok(())
        }
        other => Ok(other?),
    }
}

/// Take `member` out of its group into a group of its own. A member joins
/// itself: players answer that with UPnP 402 and leave anyway
/// (docs/PROTOCOL.md §6). A coordinator that leads other rooms hands its
/// group to one of them, which plays on (a coordinator's self-join is
/// refused and changes nothing; S1 and S2, live 2026-10-14). Then the
/// topology is read back until `member` leads a group of its own; still
/// grouped after [`LEAVE_WAIT`], the player's 402 stands.
///
/// The read-back is cross-checked against another player of the household:
/// a player that just acted (an announcement, a regroup) can serve its own
/// `ZoneGroupState` stale for far longer than [`LEAVE_WAIT`] — fleet
/// verified 2026-10-12, bead frankensonos-p9f — while the rest of the
/// household already shows the leave.
/// A lost delegation or self-join reply is reconciled from topology without
/// replaying the write. Every completed leave requires the room's bonded
/// players too, including when the handoff itself was acknowledged. Failed
/// reunification reports a partial handoff, never a replayable preflight error.
/// Read the current household before choosing an heir: earlier rooms in an
/// ungroup set may have left or inherited coordination since it was planned.
pub fn leave<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    member: &PlayerId,
) -> Result<(), CoreError> {
    let current = grouping_state::prepare_leave(t, households, member)?;
    let households = std::slice::from_ref(&current);
    let host = addr(households, member)?;
    let mut lost_delegation = None;
    let delegated = match heir_of(households, member) {
        Some(heir) => match soap::delegate_group(t, host, &heir) {
            Ok(()) => {
                let state =
                    grouping_state::delegated(t, households, member, &heir).ok_or_else(|| {
                        incomplete_handoff("the acknowledged group handoff could not be confirmed")
                    })?;
                Some(state)
            }
            // Refused: the self-join, and the topology decides.
            Err(ProtoError::SoapFault { .. }) => None,
            Err(cause) if grouping_state::uncertain(&cause) => {
                let Some(state) = grouping_state::delegated(t, households, member, &heir) else {
                    return Err(cause.into());
                };
                lost_delegation = Some(cause);
                Some(state)
            }
            Err(e) => return Err(e.into()),
        },
        None => None,
    };
    let Some(departed) = delegated else {
        match soap::leave_group(t, host, member) {
            Ok(result) => {
                if grouping_state::left(t, households, member) {
                    return Ok(());
                }
                return Err(match result {
                    soap::Leaving::Done => CoreError::Uncertain(
                        "group leave was acknowledged, but the complete room could not be \
                         confirmed alone; inspect the groups before trying again"
                            .into(),
                    ),
                    soap::Leaving::Unconfirmed => ProtoError::SoapFault {
                        code: 402,
                        reason:
                            "asked to leave its group, the complete room is not confirmed alone"
                                .into(),
                    }
                    .into(),
                });
            }
            Err(cause)
                if grouping_state::uncertain(&cause)
                    && grouping_state::left(t, households, member) =>
            {
                tracing::warn!(cause = %cause, "group leave confirmed by topology; not repeated");
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        }
    };
    reunite_room(t, households, member, host, &departed).map_err(incomplete_handoff)?;
    if let Some(cause) = lost_delegation {
        tracing::warn!(cause = %cause, "group handoff and leave confirmed by topology; not repeated");
    }
    Ok(())
}

fn incomplete_handoff(cause: impl std::fmt::Display) -> CoreError {
    CoreError::Uncertain(format!(
        "group handoff may have completed, but the room's bonded players are not confirmed \
         together: {cause}; inspect the groups before trying again"
    ))
}

/// After a coordinator handed its group on, bring back the players of its
/// own room that stayed in that group: an S1 stereo pair leaves its hidden
/// half behind with the heir, and it rejoins its primary when asked to
/// (verified live 2026-10-14). Retain the confirmed handoff, send each rejoin
/// at most once, and require the complete room before reporting success.
fn reunite_room<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    member: &PlayerId,
    host: IpAddr,
    departed: &ZoneGroupState,
) -> Result<(), CoreError> {
    let room = households
        .iter()
        .flat_map(|h| h.rooms.iter())
        .find(|r| r.primary == *member)
        .ok_or_else(|| CoreError::UnknownPlayer(member.0.clone()))?;
    let own = departed.groups.iter().find(|g| g.coordinator == *member);
    let mut lost_rejoin = None;
    for p in room.players.iter().filter(|p| *p != member) {
        if own.is_some_and(|g| {
            g.members
                .iter()
                .flat_map(|m| std::iter::once(m).chain(&m.satellites))
                .any(|m| m.uuid == *p)
        }) {
            continue;
        }
        let ip = addr(households, p)?;
        require_player(t, ip, p)?;
        require_coordinator_after_leave(t, households, host, member)?;
        if let Err(cause) = soap::join_group(t, ip, member) {
            if !grouping_state::uncertain(&cause) {
                return Err(cause.into());
            }
            lost_rejoin.get_or_insert(cause);
        }
    }
    if !grouping_state::left(t, households, member) {
        return Err(lost_rejoin.map_or_else(
            || {
                CoreError::Uncertain(
                    "the complete room was not observed after reunification".into(),
                )
            },
            CoreError::from,
        ));
    }
    if let Some(cause) = lost_rejoin {
        tracing::warn!(cause = %cause, "bonded room reunion confirmed by topology; not repeated");
    }
    Ok(())
}

/// The room that takes over the group `member` leads when it leaves: the
/// first other room in the group's member order. `None` when `member` leads
/// no other room (it is a member, or alone with its own pair or sub).
fn heir_of(households: &[HouseholdState], member: &PlayerId) -> Option<PlayerId> {
    households.iter().find_map(|h| {
        let group = h.groups.iter().find(|g| g.coordinator == *member)?;
        group.members.iter().find_map(|m| {
            h.rooms
                .iter()
                .find(|r| r.primary == *m && !r.players.contains(member))
                .map(|r| r.primary.clone())
        })
    })
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
