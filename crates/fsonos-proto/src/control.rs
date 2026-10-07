//! Control actions on a player's AVTransport, RenderingControl, and
//! GroupRenderingControl services: transport, the renderer URI and queue,
//! grouping, volume and mute, and the reads that report playback state.
//!
//! Each function builds the action's arguments, sends it through
//! [`Transport`], and decodes the out-arguments. Group-wide actions
//! (transport, the URI and queue, group volume) must go to the group's
//! coordinator; room volume and mute go to the room's own player. Choosing
//! the right player is fsonos-core's job.

use crate::didl::{DidlObject, parse_didl};
use crate::soap::{
    self, AV_TRANSPORT, GROUP_RENDERING_CONTROL, RENDERING_CONTROL, Service, SoapResponse,
};
use crate::{ProtoError, Transport};
use fsonos_types::{PlayerId, TransportState};
use std::net::IpAddr;

fn call<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    service: &Service,
    action: &str,
    args: &[(&str, &str)],
) -> Result<SoapResponse, ProtoError> {
    soap::call(t, host, service, action, &soap::args_xml(args))
}

const INSTANCE: (&str, &str) = ("InstanceID", "0");
const MASTER: (&str, &str) = ("Channel", "Master");

// ---------------------------------------------------------------- transport

/// Start (or resume) playback of the current URI or queue.
pub fn play<T: Transport + ?Sized>(t: &T, host: IpAddr) -> Result<(), ProtoError> {
    call(t, host, &AV_TRANSPORT, "Play", &[INSTANCE, ("Speed", "1")]).map(drop)
}

pub fn pause<T: Transport + ?Sized>(t: &T, host: IpAddr) -> Result<(), ProtoError> {
    call(t, host, &AV_TRANSPORT, "Pause", &[INSTANCE]).map(drop)
}

pub fn stop<T: Transport + ?Sized>(t: &T, host: IpAddr) -> Result<(), ProtoError> {
    call(t, host, &AV_TRANSPORT, "Stop", &[INSTANCE]).map(drop)
}

pub fn next<T: Transport + ?Sized>(t: &T, host: IpAddr) -> Result<(), ProtoError> {
    call(t, host, &AV_TRANSPORT, "Next", &[INSTANCE]).map(drop)
}

pub fn previous<T: Transport + ?Sized>(t: &T, host: IpAddr) -> Result<(), ProtoError> {
    call(t, host, &AV_TRANSPORT, "Previous", &[INSTANCE]).map(drop)
}

/// Jump to queue position `track` (1-based).
pub fn seek_track<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    track: u32,
) -> Result<(), ProtoError> {
    let target = track.to_string();
    call(
        t,
        host,
        &AV_TRANSPORT,
        "Seek",
        &[INSTANCE, ("Unit", "TRACK_NR"), ("Target", &target)],
    )
    .map(drop)
}

/// Seek within the current track to `secs` from its start.
pub fn seek_position<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    secs: u32,
) -> Result<(), ProtoError> {
    let target = hms(secs);
    call(
        t,
        host,
        &AV_TRANSPORT,
        "Seek",
        &[INSTANCE, ("Unit", "REL_TIME"), ("Target", &target)],
    )
    .map(drop)
}

/// Point the renderer at `uri` with its DIDL-Lite `metadata` (may be empty).
pub fn set_av_transport_uri<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    uri: &str,
    metadata: &str,
) -> Result<(), ProtoError> {
    call(
        t,
        host,
        &AV_TRANSPORT,
        "SetAVTransportURI",
        &[
            INSTANCE,
            ("CurrentURI", uri),
            ("CurrentURIMetaData", metadata),
        ],
    )
    .map(drop)
}

/// Make the player render the queue of the group `coordinator` leads.
pub fn play_from_queue<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    coordinator: &PlayerId,
) -> Result<(), ProtoError> {
    set_av_transport_uri(t, host, &format!("x-rincon-queue:{}#0", coordinator.0), "")
}

/// Append `uri` (with its DIDL metadata) to the queue of the coordinator at
/// `host`, or insert it after the current track when `as_next`. Returns the
/// queue position of the first track added.
pub fn add_uri_to_queue<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    uri: &str,
    metadata: &str,
    as_next: bool,
) -> Result<u32, ProtoError> {
    call(
        t,
        host,
        &AV_TRANSPORT,
        "AddURIToQueue",
        &[
            INSTANCE,
            ("EnqueuedURI", uri),
            ("EnqueuedURIMetaData", metadata),
            ("DesiredFirstTrackNumberEnqueued", "0"),
            ("EnqueueAsNext", if as_next { "1" } else { "0" }),
        ],
    )?
    .require_u32("FirstTrackNumberEnqueued")
}

pub fn remove_all_tracks_from_queue<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
) -> Result<(), ProtoError> {
    call(
        t,
        host,
        &AV_TRANSPORT,
        "RemoveAllTracksFromQueue",
        &[INSTANCE],
    )
    .map(drop)
}

// ---------------------------------------------------------------- grouping

/// Join the player at `host` to the group `coordinator` leads.
pub fn join_group<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    coordinator: &PlayerId,
) -> Result<(), ProtoError> {
    set_av_transport_uri(t, host, &format!("x-rincon:{}", coordinator.0), "")
}

/// Take the player at `host` out of its group into a group of its own.
pub fn leave_group<T: Transport + ?Sized>(t: &T, host: IpAddr) -> Result<(), ProtoError> {
    call(
        t,
        host,
        &AV_TRANSPORT,
        "BecomeCoordinatorOfStandaloneGroup",
        &[INSTANCE],
    )
    .map(drop)
}

// ---------------------------------------------------------------- volume

pub fn get_volume<T: Transport + ?Sized>(t: &T, host: IpAddr) -> Result<u8, ProtoError> {
    let r = call(
        t,
        host,
        &RENDERING_CONTROL,
        "GetVolume",
        &[INSTANCE, MASTER],
    )?;
    volume_arg(&r, "CurrentVolume")
}

pub fn set_volume<T: Transport + ?Sized>(t: &T, host: IpAddr, level: u8) -> Result<(), ProtoError> {
    let level = level.min(100).to_string();
    call(
        t,
        host,
        &RENDERING_CONTROL,
        "SetVolume",
        &[INSTANCE, MASTER, ("DesiredVolume", &level)],
    )
    .map(drop)
}

/// Change the room volume by `delta`; returns the new volume (the player
/// clamps it to 0–100).
pub fn set_relative_volume<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    delta: i32,
) -> Result<u8, ProtoError> {
    let adjustment = delta.to_string();
    let r = call(
        t,
        host,
        &RENDERING_CONTROL,
        "SetRelativeVolume",
        &[INSTANCE, MASTER, ("Adjustment", &adjustment)],
    )?;
    volume_arg(&r, "NewVolume")
}

pub fn get_mute<T: Transport + ?Sized>(t: &T, host: IpAddr) -> Result<bool, ProtoError> {
    let r = call(t, host, &RENDERING_CONTROL, "GetMute", &[INSTANCE, MASTER])?;
    Ok(r.require("CurrentMute")?.trim() == "1")
}

pub fn set_mute<T: Transport + ?Sized>(t: &T, host: IpAddr, mute: bool) -> Result<(), ProtoError> {
    call(
        t,
        host,
        &RENDERING_CONTROL,
        "SetMute",
        &[
            INSTANCE,
            MASTER,
            ("DesiredMute", if mute { "1" } else { "0" }),
        ],
    )
    .map(drop)
}

/// The group volume, as the coordinator at `host` reports it.
pub fn get_group_volume<T: Transport + ?Sized>(t: &T, host: IpAddr) -> Result<u8, ProtoError> {
    let r = call(
        t,
        host,
        &GROUP_RENDERING_CONTROL,
        "GetGroupVolume",
        &[INSTANCE],
    )?;
    volume_arg(&r, "CurrentVolume")
}

/// Set the group volume on the coordinator at `host`. Members keep their
/// relative balance: the player snapshots their ratios first.
pub fn set_group_volume<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    level: u8,
) -> Result<(), ProtoError> {
    call(
        t,
        host,
        &GROUP_RENDERING_CONTROL,
        "SnapshotGroupVolume",
        &[INSTANCE],
    )?;
    let level = level.min(100).to_string();
    call(
        t,
        host,
        &GROUP_RENDERING_CONTROL,
        "SetGroupVolume",
        &[INSTANCE, ("DesiredVolume", &level)],
    )
    .map(drop)
}

/// Change the group volume by `delta`; returns the new group volume.
pub fn set_relative_group_volume<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    delta: i32,
) -> Result<u8, ProtoError> {
    call(
        t,
        host,
        &GROUP_RENDERING_CONTROL,
        "SnapshotGroupVolume",
        &[INSTANCE],
    )?;
    let adjustment = delta.to_string();
    let r = call(
        t,
        host,
        &GROUP_RENDERING_CONTROL,
        "SetRelativeGroupVolume",
        &[INSTANCE, ("Adjustment", &adjustment)],
    )?;
    volume_arg(&r, "NewVolume")
}

fn volume_arg(r: &SoapResponse, name: &str) -> Result<u8, ProtoError> {
    let v = r.require_u32(name)?;
    u8::try_from(v.min(100)).map_err(|_| ProtoError::Malformed(format!("{name} out of range: {v}")))
}

/// How a [`ramp_to_volume`] gets to its target. The player picks the speed;
/// RampToVolume takes no duration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RampType {
    /// Linear from the current volume (the sleep-timer fade).
    SleepTimer,
    /// Drops to 0, pauses, then ramps up (the alarm fade-in).
    Alarm,
    /// Drops to 0, then ramps up quickly.
    Autoplay,
}

impl RampType {
    fn as_str(self) -> &'static str {
        match self {
            Self::SleepTimer => "SLEEP_TIMER_RAMP_TYPE",
            Self::Alarm => "ALARM_RAMP_TYPE",
            Self::Autoplay => "AUTOPLAY_RAMP_TYPE",
        }
    }
}

/// Let the player fade itself to `level`; returns the ramp time in seconds
/// it reports. Cancel a ramp by sending [`set_volume`].
pub fn ramp_to_volume<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    ramp: RampType,
    level: u8,
) -> Result<u32, ProtoError> {
    let level = level.min(100).to_string();
    call(
        t,
        host,
        &RENDERING_CONTROL,
        "RampToVolume",
        &[
            INSTANCE,
            MASTER,
            ("RampType", ramp.as_str()),
            ("DesiredVolume", &level),
            ("ResetVolumeAfter", "0"),
            ("ProgramURI", ""),
        ],
    )?
    .require_u32("RampTime")
}

// ---------------------------------------------------------------- reads

/// `GetTransportInfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportInfo {
    pub state: TransportState,
    /// `CurrentTransportStatus` (`OK`, or an error string).
    pub status: String,
}

pub fn get_transport_info<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
) -> Result<TransportInfo, ProtoError> {
    let r = call(t, host, &AV_TRANSPORT, "GetTransportInfo", &[INSTANCE])?;
    Ok(TransportInfo {
        state: transport_state(r.require("CurrentTransportState")?),
        status: r.get("CurrentTransportStatus").unwrap_or("").to_string(),
    })
}

/// Map a UPnP `TransportState` value.
#[must_use]
pub fn transport_state(value: &str) -> TransportState {
    match value.trim() {
        "PLAYING" => TransportState::Playing,
        "PAUSED_PLAYBACK" => TransportState::Paused,
        "STOPPED" => TransportState::Stopped,
        "TRANSITIONING" => TransportState::Transitioning,
        _ => TransportState::Unknown,
    }
}

/// `GetPositionInfo`: what the renderer is on and how far in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PositionInfo {
    /// Queue position (1-based); 0 when not playing from the queue.
    pub track: u32,
    pub duration_secs: Option<u32>,
    pub position_secs: Option<u32>,
    pub uri: String,
    /// The current track's DIDL-Lite metadata, when the player reports any.
    pub metadata: Option<DidlObject>,
}

pub fn get_position_info<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
) -> Result<PositionInfo, ProtoError> {
    let r = call(t, host, &AV_TRANSPORT, "GetPositionInfo", &[INSTANCE])?;
    let metadata = match r.get("TrackMetaData").map(str::trim) {
        None | Some("" | "NOT_IMPLEMENTED") => None,
        Some(doc) => parse_didl(doc)?.into_iter().next(),
    };
    Ok(PositionInfo {
        track: r
            .get("Track")
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0),
        duration_secs: r.get("TrackDuration").and_then(parse_hms),
        position_secs: r.get("RelTime").and_then(parse_hms),
        uri: r.get("TrackURI").unwrap_or("").to_string(),
        metadata,
    })
}

/// `GetMediaInfo`: the source the renderer is set to (a queue, a stream, a
/// group join).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaInfo {
    pub tracks: u32,
    pub uri: String,
    pub uri_metadata: String,
}

pub fn get_media_info<T: Transport + ?Sized>(t: &T, host: IpAddr) -> Result<MediaInfo, ProtoError> {
    let r = call(t, host, &AV_TRANSPORT, "GetMediaInfo", &[INSTANCE])?;
    Ok(MediaInfo {
        tracks: r
            .get("NrTracks")
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0),
        uri: r.get("CurrentURI").unwrap_or("").to_string(),
        uri_metadata: r.get("CurrentURIMetaData").unwrap_or("").to_string(),
    })
}

/// Parse an `H:MM:SS` (or `HH:MM:SS`, optionally with a fraction) duration.
/// `NOT_IMPLEMENTED` and empty values are `None`.
#[must_use]
pub fn parse_hms(value: &str) -> Option<u32> {
    let mut parts = value.trim().split(':');
    let (h, m, s) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let s = s.split('.').next()?;
    let (h, m, s): (u32, u32, u32) = (h.parse().ok()?, m.parse().ok()?, s.parse().ok()?);
    (m < 60 && s < 60).then_some(h * 3600 + m * 60 + s)
}

fn hms(secs: u32) -> String {
    format!("{}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Records each request and answers with a canned `<Action>Response`.
    struct Canned {
        sent: RefCell<Vec<(String, String, String)>>,
        out_args: &'static str,
    }

    impl Canned {
        fn new(out_args: &'static str) -> Self {
            Self {
                sent: RefCell::new(Vec::new()),
                out_args,
            }
        }

        fn last(&self) -> (String, String, String) {
            self.sent.borrow().last().cloned().expect("a request")
        }
    }

    impl Transport for Canned {
        fn soap_post(
            &self,
            _host: IpAddr,
            control_path: &str,
            soap_action: &str,
            body: &str,
        ) -> Result<String, ProtoError> {
            self.sent
                .borrow_mut()
                .push((control_path.into(), soap_action.into(), body.into()));
            let action = soap_action
                .trim_matches('"')
                .rsplit('#')
                .next()
                .unwrap_or_default()
                .to_string();
            Ok(format!(
                "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>\
                 <u:{action}Response xmlns:u=\"urn:x\">{}</u:{action}Response></s:Body></s:Envelope>",
                self.out_args
            ))
        }
    }

    fn host() -> IpAddr {
        "192.0.2.10".parse().unwrap()
    }

    #[test]
    fn transport_actions_target_av_transport() {
        let t = Canned::new("");
        play(&t, host()).unwrap();
        let (path, action, body) = t.last();
        assert_eq!(path, "/MediaRenderer/AVTransport/Control");
        assert_eq!(
            action,
            "\"urn:schemas-upnp-org:service:AVTransport:1#Play\""
        );
        assert!(body.contains("<InstanceID>0</InstanceID><Speed>1</Speed>"));
        for (f, name) in [
            (pause::<Canned> as fn(&Canned, IpAddr) -> _, "Pause"),
            (stop::<Canned>, "Stop"),
            (next::<Canned>, "Next"),
            (previous::<Canned>, "Previous"),
        ] {
            f(&t, host()).unwrap();
            assert!(t.last().1.ends_with(&format!("#{name}\"")), "{name}");
        }
    }

    #[test]
    fn grouping_uses_rincon_uris() {
        let t = Canned::new("");
        let coord = PlayerId("RINCON_000E58A0000001400".into());
        join_group(&t, host(), &coord).unwrap();
        assert!(
            t.last()
                .2
                .contains("<CurrentURI>x-rincon:RINCON_000E58A0000001400</CurrentURI>")
        );
        play_from_queue(&t, host(), &coord).unwrap();
        assert!(
            t.last()
                .2
                .contains("x-rincon-queue:RINCON_000E58A0000001400#0")
        );
        leave_group(&t, host()).unwrap();
        assert!(
            t.last()
                .1
                .ends_with("#BecomeCoordinatorOfStandaloneGroup\"")
        );
    }

    #[test]
    fn uri_metadata_is_escaped() {
        let t = Canned::new("");
        set_av_transport_uri(&t, host(), "x-sonos-spotify:a?b=1&c=2", "<DIDL-Lite/>").unwrap();
        let body = t.last().2;
        assert!(body.contains("x-sonos-spotify:a?b=1&amp;c=2"));
        assert!(body.contains("<CurrentURIMetaData>&lt;DIDL-Lite/&gt;</CurrentURIMetaData>"));
    }

    #[test]
    fn queue_add_reports_first_track() {
        let t = Canned::new(
            "<FirstTrackNumberEnqueued>7</FirstTrackNumberEnqueued><NumTracksAdded>1</NumTracksAdded>",
        );
        assert_eq!(
            add_uri_to_queue(&t, host(), "x-file:a", "", true).unwrap(),
            7
        );
        assert!(t.last().2.contains("<EnqueueAsNext>1</EnqueueAsNext>"));
    }

    #[test]
    fn volume_reads_and_writes() {
        let t = Canned::new("<CurrentVolume>23</CurrentVolume>");
        assert_eq!(get_volume(&t, host()).unwrap(), 23);
        let (path, _, body) = t.last();
        assert_eq!(path, "/MediaRenderer/RenderingControl/Control");
        assert!(body.contains("<Channel>Master</Channel>"));
        set_volume(&t, host(), 140).unwrap();
        assert!(
            t.last().2.contains("<DesiredVolume>100</DesiredVolume>"),
            "clamped"
        );

        let t = Canned::new("<NewVolume>18</NewVolume>");
        assert_eq!(set_relative_volume(&t, host(), -5).unwrap(), 18);
        assert!(t.last().2.contains("<Adjustment>-5</Adjustment>"));
    }

    #[test]
    fn group_volume_snapshots_first() {
        let t = Canned::new("<NewVolume>30</NewVolume>");
        assert_eq!(set_relative_group_volume(&t, host(), 4).unwrap(), 30);
        let sent = t.sent.borrow();
        assert_eq!(sent.len(), 2);
        assert!(sent[0].1.ends_with("#SnapshotGroupVolume\""));
        assert_eq!(sent[1].0, "/MediaRenderer/GroupRenderingControl/Control");
        assert!(sent[1].1.ends_with("#SetRelativeGroupVolume\""));
    }

    #[test]
    fn ramp_to_volume_names_the_ramp_type() {
        let t = Canned::new("<RampTime>12</RampTime>");
        assert_eq!(
            ramp_to_volume(&t, host(), RampType::SleepTimer, 20).unwrap(),
            12
        );
        let body = t.last().2;
        assert!(body.contains(
            "<RampType>SLEEP_TIMER_RAMP_TYPE</RampType><DesiredVolume>20</DesiredVolume>"
        ));
        assert!(body.contains("<ResetVolumeAfter>0</ResetVolumeAfter><ProgramURI></ProgramURI>"));
    }

    #[test]
    fn mute_round_trip() {
        let t = Canned::new("<CurrentMute>1</CurrentMute>");
        assert!(get_mute(&t, host()).unwrap());
        set_mute(&t, host(), false).unwrap();
        assert!(t.last().2.contains("<DesiredMute>0</DesiredMute>"));
    }

    #[test]
    fn transport_info_maps_states() {
        let t = Canned::new(
            "<CurrentTransportState>PAUSED_PLAYBACK</CurrentTransportState>\
             <CurrentTransportStatus>OK</CurrentTransportStatus><CurrentSpeed>1</CurrentSpeed>",
        );
        let info = get_transport_info(&t, host()).unwrap();
        assert_eq!(info.state, TransportState::Paused);
        assert_eq!(info.status, "OK");
        assert_eq!(transport_state("PLAYING"), TransportState::Playing);
        assert_eq!(transport_state("WHAT"), TransportState::Unknown);
    }

    #[test]
    fn position_info_parses_times_and_metadata() {
        let t = Canned::new(
            "<Track>3</Track><TrackDuration>0:04:05</TrackDuration>\
             <TrackMetaData>&lt;DIDL-Lite xmlns:dc=&quot;http://purl.org/dc/elements/1.1/&quot; \
             xmlns:upnp=&quot;urn:schemas-upnp-org:metadata-1-0/upnp/&quot; \
             xmlns=&quot;urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/&quot;&gt;&lt;item id=&quot;-1&quot; \
             parentID=&quot;-1&quot; restricted=&quot;true&quot;&gt;&lt;dc:title&gt;Aria&lt;/dc:title&gt;\
             &lt;dc:creator&gt;J. S. Bach&lt;/dc:creator&gt;&lt;upnp:class&gt;object.item.audioItem.musicTrack\
             &lt;/upnp:class&gt;&lt;/item&gt;&lt;/DIDL-Lite&gt;</TrackMetaData>\
             <TrackURI>x-sonos-spotify:spotify%3atrack%3aabc</TrackURI><RelTime>0:01:02</RelTime>",
        );
        let p = get_position_info(&t, host()).unwrap();
        assert_eq!(p.track, 3);
        assert_eq!(p.duration_secs, Some(245));
        assert_eq!(p.position_secs, Some(62));
        let m = p.metadata.expect("metadata");
        assert_eq!(m.title, "Aria");
        assert_eq!(m.creator.as_deref(), Some("J. S. Bach"));

        let t = Canned::new(
            "<Track>0</Track><TrackDuration>NOT_IMPLEMENTED</TrackDuration>\
             <TrackMetaData>NOT_IMPLEMENTED</TrackMetaData><TrackURI></TrackURI><RelTime>NOT_IMPLEMENTED</RelTime>",
        );
        let p = get_position_info(&t, host()).unwrap();
        assert_eq!(
            (p.duration_secs, p.position_secs, p.metadata),
            (None, None, None)
        );
    }

    #[test]
    fn hms_round_trip() {
        assert_eq!(parse_hms("1:02:03"), Some(3723));
        assert_eq!(parse_hms("00:00:07.250"), Some(7));
        assert_eq!(parse_hms("0:61:00"), None);
        assert_eq!(parse_hms("NOT_IMPLEMENTED"), None);
        assert_eq!(hms(3723), "1:02:03");
        let t = Canned::new("");
        seek_position(&t, host(), 75).unwrap();
        assert!(
            t.last()
                .2
                .contains("<Unit>REL_TIME</Unit><Target>0:01:15</Target>")
        );
        seek_track(&t, host(), 4).unwrap();
        assert!(
            t.last()
                .2
                .contains("<Unit>TRACK_NR</Unit><Target>4</Target>")
        );
    }
}
