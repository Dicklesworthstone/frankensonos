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
use fsonos_proto::content::{self, QUEUE};
use fsonos_proto::control::{self as soap, PositionInfo, TransportInfo};
use fsonos_proto::didl::{
    SpotifyContainer, SpotifyRenderParams, learn_spotify_params, spotify_queue_uri,
    spotify_track_didl, spotify_track_uri,
};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_types::{Player, PlayerId};
use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::time::{Duration, Instant};

mod loading;
mod queue_start;
mod seek_relative;
mod source_start;
mod spotify_source;
mod transport_state;

pub use loading::{
    load_queue_from, load_queue_from_in_order, load_spotify_container, load_spotify_track,
    load_uri, pause_confirmed,
};

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

/// Resume (or start) playback on the group `coordinator` leads. A lost
/// acknowledgement is confirmed by read-back, not another Play.
pub fn resume<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
) -> Result<(), CoreError> {
    Ok(transport_state::play(
        t,
        addr(households, coordinator)?,
        coordinator,
    )?)
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
/// Lost or malformed replies are observed rather than replayed or treated as
/// a 701 refusal. A confirmed inactive state satisfies the pause request.
pub fn pause_at<T: Transport + ?Sized>(t: &T, host: IpAddr) -> Result<(), CoreError> {
    Ok(transport_state::pause(t, host)?)
}

pub fn stop<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
) -> Result<(), CoreError> {
    Ok(transport_state::stop(t, addr(households, coordinator)?)?)
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
/// An uncertain relative-seek reply is read back against that same track,
/// never retried as another offset. Absolute seeks keep their existing path.
/// Returns the requested absolute position, in seconds from the start.
pub fn seek<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    to: Seek,
) -> Result<u32, CoreError> {
    let host = addr(households, coordinator)?;
    let secs = match to {
        Seek::At(secs) => secs,
        Seek::By(by) => return Ok(seek_relative::seek(t, host, coordinator, by)?),
    };
    soap::seek_position(t, host, secs)?;
    Ok(secs)
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
    Ok(source_start::play(t, host, coordinator, uri, metadata)?)
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
    loading::spotify_track(t, households, coordinator, spotify_uri, title, true)
}

/// Replace the queue of the group `coordinator` leads with a Spotify album
/// or playlist (`spotify:album:<id>`, `spotify:playlist:<id>`) and play it
/// from its first track: the speaker expands the container into its tracks
/// as it is enqueued. Rendered with the household's own Spotify account and
/// container prefix (learned together from an album or playlist favorite;
/// no track favorite is required for that kind), self-healing one round of
/// parameter drift as [`play_spotify_track`] does, whether the renderer
/// refuses the enqueue or the play that follows it (docs/PROTOCOL.md: 800
/// "at/after SetAVTransportURI"). The old queue remains intact until the
/// replacement has been accepted and verified; an uncertain append never
/// triggers another render attempt.
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
/// (1-based), so playback continues through the queue. Each stage is sent
/// once. A lost or malformed reply advances only when read-back confirms
/// its effect; no source reset, seek or Play is repeated during recovery.
pub fn play_queue_from<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    position: u32,
) -> Result<(), CoreError> {
    Ok(queue_start::play(
        t,
        addr(households, coordinator)?,
        coordinator,
        position,
    )?)
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
    Ok(queue_start::play_in_order(
        t,
        addr(households, coordinator)?,
        coordinator,
        position,
    )?)
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
    let source = |rejected: Option<&(String, String)>| -> Result<(String, String), CoreError> {
        let favorites = content::browse_all(t, host, "FV:2")?;
        if let Some(kind) = kind {
            return match rejected {
                Some(old) => {
                    spotify_source::after_rejection(&favorites, kind, spotify_uri, title, old)
                }
                None => spotify_source::container(&favorites, kind, spotify_uri, title),
            }
            .ok_or(CoreError::NoSpotifyFavorite);
        }
        let params = learn_spotify_params(&favorites).ok_or(CoreError::NoSpotifyFavorite)?;
        Ok((
            spotify_queue_uri(spotify_uri),
            spotify_track_didl(spotify_uri, title, &params),
        ))
    };
    let add = |(uri, didl): &(String, String)| -> Result<soap::Enqueued, CoreError> {
        Ok(soap::enqueue_uri(t, host, uri, didl, at, next)?)
    };
    let initial = source(None)?;
    let added = match add(&initial) {
        Err(e) if is_render_800(&e) => match add(&source(Some(&initial))?) {
            Err(e) if is_render_800(&e) => return Err(CoreError::RenderParamsStale),
            other => other?,
        },
        other => other?,
    };
    let uris = queue_uris(t, host, added.first, added.count)?;
    if usize::try_from(added.count).ok() != Some(uris.len()) {
        // The mutation already happened. Retry reads only, never enqueue
        // again to repair a missing or changed read-back.
        return Err(ProtoError::Malformed(
            "the enqueued range changed before it could be read back completely".into(),
        )
        .into());
    }
    Ok(Queued {
        first: added.first,
        uris,
        playing_queue,
    })
}

/// Take tracks [`enqueue_spotify`] added (`uris`, from queue position
/// `first`) off the queue of the group `coordinator` leads again.
/// `Ok(false)`, with nothing removed, when a consistent read no longer
/// finds them there. The read and removal are separate device operations.
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
    if uris.iter().any(|uri| uri.trim().is_empty()) {
        return Err(ProtoError::Malformed("queued track identity is empty".into()).into());
    }
    let host = addr(households, coordinator)?;
    let count = u32::try_from(uris.len())
        .map_err(|_| ProtoError::Malformed("queued range exceeds u32 tracks".into()))?;
    if queue_uris(t, host, first, count)? != uris {
        return Ok(false);
    }
    // UpdateID 0 takes the queue as it is, as SoCo and node-sonos send it:
    // the tracks were just read back where they were added, and players
    // need not count a queue's changes the way its Browse does.
    soap::remove_track_range_from_queue(t, host, 0, first, count)?;
    Ok(true)
}

/// The URIs of up to `count` queue tracks from position `first` (1-based)
/// of the coordinator at `host`, all from one observed container revision.
fn queue_uris<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    first: u32,
    count: u32,
) -> Result<Vec<String>, CoreError> {
    if count == 0 {
        return Ok(Vec::new());
    }
    let offset = first
        .checked_sub(1)
        .ok_or_else(|| ProtoError::Malformed("queue positions start at one".into()))?;
    let snapshot = content::browse_range_snapshot(t, host, QUEUE, offset, count)?;
    snapshot
        .objects
        .into_iter()
        .map(|object| {
            object.res.map(|resource| resource.uri).ok_or_else(|| {
                CoreError::Proto(ProtoError::Malformed("queued track has no URI".into()))
            })
        })
        .collect()
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
pub fn leave<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    member: &PlayerId,
) -> Result<(), CoreError> {
    let host = addr(households, member)?;
    let delegated = match heir_of(households, member) {
        Some(heir) => match soap::delegate_group(t, host, &heir) {
            Ok(()) => true,
            // Refused: the self-join, and the topology decides.
            Err(ProtoError::SoapFault { .. }) => false,
            Err(e) => return Err(e.into()),
        },
        None => false,
    };
    if !delegated && soap::leave_group(t, host, member)? == soap::Leaving::Done {
        return Ok(());
    }
    wait_until_alone(t, households, member, host)?;
    if delegated {
        reunite_room(t, households, member, host);
    }
    Ok(())
}

/// Read the topology back until `member` leads a group of its own (see
/// [`leave`]); still grouped after [`LEAVE_WAIT`], the player's 402 stands.
fn wait_until_alone<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    member: &PlayerId,
    host: IpAddr,
) -> Result<(), CoreError> {
    let household = households
        .iter()
        .find(|h| h.player(member).is_some())
        .unwrap_or_else(|| households.first().expect("addr() found the household"));
    let same_room = |id: &PlayerId| {
        household
            .rooms
            .iter()
            .any(|r| r.players.contains(id) && r.players.contains(member))
    };
    // A witness in another room of the same household: a player of the
    // member's own room (a stereo pair's other half) shares its view, and a
    // player of another household never lists this one's groups at all.
    let mut witness = household
        .players
        .iter()
        .find(|p| &p.id != member && !same_room(&p.id))
        .or_else(|| household.players.iter().find(|p| &p.id != member))
        .map(|p| p.ip);
    let leads_own = |groups: &[fsonos_proto::topology::ZoneGroupInfo]| {
        groups.iter().any(|g| {
            g.coordinator == *member && g.members.iter().all(|m| m.uuid == *member || m.invisible)
        })
    };
    let started = Instant::now();
    loop {
        // Alone: it leads its group, with nobody visible but itself (a
        // stereo pair's other half and a sub are invisible members).
        if leads_own(&get_zone_group_state(t, host)?.groups) {
            return Ok(());
        }
        // Its own read may be stale; a witness player's is not. The witness
        // is best effort — one try: a player that cannot be read is not
        // asked again this leave (each dead read would stall the single LAN
        // worker a whole SOAP timeout, p9f's review note), and the member's
        // own view still decides.
        if let Some(other) = witness {
            match get_zone_group_state(t, other) {
                Ok(groups) if leads_own(&groups.groups) => return Ok(()),
                Ok(_) => {}
                Err(_) => witness = None,
            }
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

/// After a coordinator handed its group on, bring back the players of its
/// own room that stayed in that group: an S1 stereo pair leaves its hidden
/// half behind with the heir, and it rejoins its primary when asked to
/// (verified live 2026-10-14). Best effort: the leave itself has happened.
fn reunite_room<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    member: &PlayerId,
    host: IpAddr,
) {
    let Some(room) = households
        .iter()
        .flat_map(|h| h.rooms.iter())
        .find(|r| r.players.contains(member))
    else {
        return;
    };
    let Ok(now) = get_zone_group_state(t, host) else {
        return;
    };
    let own = now.groups.iter().find(|g| g.coordinator == *member);
    for p in room.players.iter().filter(|p| *p != member) {
        if own.is_some_and(|g| g.members.iter().any(|m| m.uuid == *p)) {
            continue;
        }
        if let Ok(ip) = addr(households, p)
            && let Err(e) = soap::join_group(t, ip, member)
        {
            tracing::warn!(
                player = p.0.as_str(),
                "a bonded player did not rejoin its room: {e}"
            );
        }
    }
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
