//! Direct relocation over real S1/S2 simulator sockets. A lost reply may
//! complete a proven transition, but cannot replay writes or adopt an owner
//! action. Queued origins retain their existing release safeguards.

use fsonos_core::moving::{self, MoveError, MoveMethod, MoveReport};
use fsonos_core::{HouseholdState, resolve_room};
use fsonos_proto::content::{QUEUE, browse_snapshot};
use fsonos_proto::control as soap;
use fsonos_proto::play_mode::set_play_mode;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayMode, PlayerId, TransportState};
use std::cell::{Cell, RefCell};
use std::net::IpAddr;
use std::time::Duration;

const RADIO: &str = "x-rincon-mp3radio://stream.example.invalid/transfer.mp3";
const FILE: &str = "x-file-cifs://example.invalid/transfer.flac";
const OLD: &str = "x-rincon-mp3radio://stream.example.invalid/before.mp3";
const OTHER: &str = "x-rincon-mp3radio://stream.example.invalid/owner.mp3";
const PAIRS: [(&str, &str); 2] = [("S1 Source", "S2 Target"), ("S2 Source", "S1 Target")];
const WRITES: [&str; 4] = ["Stop", "SetAVTransportURI", "Seek", "Play"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Radio,
    File,
    Queue,
}

fn metadata() -> String {
    format!(
        r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/"
        xmlns:dc="http://purl.org/dc/elements/1.1/"
        xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">
        <item id="synthetic-transfer" parentID="0" restricted="true">
        <dc:title>Transfer track</dc:title>
        <upnp:class>object.item.audioItem.musicTrack</upnp:class>
        <res protocolInfo="http-get:*:audio/flac:*" duration="0:05:00">{FILE}</res>
        </item></DIDL-Lite>"#
    )
}

struct Rig {
    sim: SimHandle,
    houses: Vec<HouseholdState>,
    from: String,
    to: String,
    kind: Kind,
    paused: bool,
}

impl Rig {
    fn new(from: &str, to: &str, kind: Kind, paused: bool) -> Self {
        let sim = SimHousehold::builder()
            .s1([
                SimPlayerSpec::new("S1 Source", SimModel::Play5Gen1),
                SimPlayerSpec::new("S1 Target", SimModel::Play5Gen1),
            ])
            .s2([
                SimPlayerSpec::new("S2 Source", SimModel::One),
                SimPlayerSpec::new("S2 Target", SimModel::One),
            ])
            .spawn()
            .unwrap();
        let lan = sim.lan();
        let houses = ["S1 Source", "S2 Source"]
            .map(|room| {
                let mut house = HouseholdState::default();
                house.apply_topology(
                    &get_zone_group_state(&lan, sim.player(room).unwrap().ip).unwrap(),
                );
                house
            })
            .into();
        let this = Self {
            sim,
            houses,
            from: from.into(),
            to: to.into(),
            kind,
            paused,
        };
        let source = this.host(&this.from);
        match kind {
            Kind::Radio => soap::set_av_transport_uri(&lan, source, RADIO, "").unwrap(),
            Kind::File => {
                soap::set_av_transport_uri(&lan, source, FILE, &metadata()).unwrap();
            }
            Kind::Queue => {
                soap::add_uri_to_queue(&lan, source, FILE, &metadata(), false).unwrap();
                soap::play_from_queue(&lan, source, &this.id(&this.from)).unwrap();
                soap::seek_track(&lan, source, 1).unwrap();
            }
        }
        if kind != Kind::Radio {
            soap::seek_position(&lan, source, 37).unwrap();
        }
        soap::play(&lan, source).unwrap();
        if paused {
            soap::pause(&lan, source).unwrap();
        }
        let destination = this.host(&this.to);
        soap::add_uri_to_queue(&lan, destination, OLD, "", false).unwrap();
        soap::set_av_transport_uri(&lan, destination, OLD, "").unwrap();
        soap::play(&lan, destination).unwrap();
        this
    }

    fn host(&self, room: &str) -> IpAddr {
        self.sim.player(room).unwrap().ip
    }

    fn id(&self, room: &str) -> PlayerId {
        PlayerId(self.sim.player(room).unwrap().uuid.clone())
    }

    fn copy(&self, t: &dyn Transport) -> Result<MoveReport, MoveError> {
        moving::copy_playback(
            t,
            &self.houses,
            &resolve_room(&self.houses, &self.from).unwrap(),
            &resolve_room(&self.houses, &self.to).unwrap(),
        )
    }

    fn source_playing(&self) {
        assert_eq!(
            soap::get_transport_info(&self.sim.lan(), self.host(&self.from))
                .unwrap()
                .state,
            TransportState::Playing
        );
    }

    fn moved(&self) {
        let lan = self.sim.lan();
        let host = self.host(&self.to);
        let media = soap::get_media_info(&lan, host).unwrap();
        let expected = match self.kind {
            Kind::Radio => RADIO.to_string(),
            Kind::File => FILE.to_string(),
            Kind::Queue => format!("x-rincon-queue:{}#0", self.id(&self.to).0),
        };
        assert_eq!(media.uri, expected);
        if self.kind == Kind::File {
            assert_eq!(media.uri_metadata, metadata());
        }
        if self.kind != Kind::Radio {
            assert_eq!(
                soap::get_position_info(&lan, host).unwrap().position_secs,
                Some(37)
            );
        }
        assert_eq!(
            soap::get_transport_info(&lan, host).unwrap().state,
            if self.paused {
                TransportState::Stopped
            } else {
                TransportState::Playing
            }
        );
        assert_eq!(
            soap::get_transport_info(&lan, self.host(&self.from))
                .unwrap()
                .state,
            TransportState::Stopped
        );
        if self.kind != Kind::Queue {
            let queue = browse_snapshot(&lan, host, QUEUE).unwrap();
            assert_eq!(queue.objects.len(), 1);
            assert_eq!(queue.objects[0].res.as_ref().unwrap().uri, OLD);
        }
    }
}

fn name(action: &str) -> &str {
    action
        .trim_matches('"')
        .rsplit('#')
        .next()
        .unwrap_or(action)
}

fn lost(host: IpAddr) -> ProtoError {
    ProtoError::Network {
        target: host.to_string(),
        detail: "injected lost relocation reply".into(),
    }
}

fn writes(calls: &RefCell<Vec<(IpAddr, String)>>, host: IpAddr) -> Vec<String> {
    calls
        .borrow()
        .iter()
        .filter(|(at, action)| *at == host && WRITES.contains(&action.as_str()))
        .map(|(_, action)| action.clone())
        .collect()
}

fn field(response: &mut String, tag: &str, value: &str) {
    let open = format!("<{tag}>");
    let start = response.find(&open).unwrap() + open.len();
    let end = response[start..].find(&format!("</{tag}>")).unwrap() + start;
    response.replace_range(start..end, value);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reply {
    Pass,
    Lost,
    Malformed,
    Before,
    Ignored,
    Refused,
    Unreadable,
}

struct Replies {
    inner: SimLan,
    fault: Reply,
    at: Option<(IpAddr, &'static str)>,
    calls: RefCell<Vec<(IpAddr, String)>>,
    fired: Cell<bool>,
    clockless: bool,
}

impl Replies {
    fn new(sim: &SimHandle, fault: Reply, at: Option<(IpAddr, &'static str)>) -> Self {
        Self {
            inner: sim.lan(),
            fault,
            at,
            calls: RefCell::default(),
            fired: Cell::new(false),
            clockless: false,
        }
    }
}

impl Transport for Replies {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let verb = name(action);
        self.calls.borrow_mut().push((host, verb.into()));
        if self.fault == Reply::Unreadable
            && self.fired.get()
            && self.at.is_none_or(|(at, _)| at == host)
            && (verb.starts_with("Get") || verb == "Browse")
        {
            return Err(ProtoError::Malformed("readback is unavailable".into()));
        }
        let selected = WRITES.contains(&verb)
            && self
                .at
                .is_none_or(|(at, stage)| host == at && verb == stage);
        if selected {
            self.fired.set(true);
            match self.fault {
                Reply::Before => return Err(lost(host)),
                Reply::Ignored => {
                    return Ok(format!(
                        "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\">\
                         <s:Body><u:{verb}Response xmlns:u=\"urn:test\"/>\
                         </s:Body></s:Envelope>"
                    ));
                }
                Reply::Refused => {
                    return Err(ProtoError::SoapFault {
                        code: 701,
                        reason: "injected relocation refusal".into(),
                    });
                }
                _ => {}
            }
        }
        let mut response = self.inner.soap_post(host, path, action, body)?;
        if self.clockless && verb == "GetPositionInfo" {
            // A real station can expose neither a duration nor a seek clock,
            // and TrackURI can name a resolved stream rather than CurrentURI.
            field(&mut response, "TrackDuration", "NOT_IMPLEMENTED");
            field(&mut response, "RelTime", "NOT_IMPLEMENTED");
            field(
                &mut response,
                "TrackURI",
                "http://audio.example.invalid/live.mp3",
            );
        }
        if selected {
            match self.fault {
                Reply::Lost | Reply::Unreadable => return Err(lost(host)),
                Reply::Malformed => return Ok("<incomplete-reply>".into()),
                _ => {}
            }
        }
        Ok(response)
    }
}

#[test]
fn direct_sources_finish_once_after_lost_or_malformed_replies_in_both_directions() {
    for (from, to) in PAIRS {
        for kind in [Kind::Radio, Kind::File] {
            for paused in [false, true] {
                for fault in [Reply::Pass, Reply::Lost, Reply::Malformed] {
                    let rig = Rig::new(from, to, kind, paused);
                    let mut replies = Replies::new(&rig.sim, fault, None);
                    replies.clockless = kind == Kind::Radio;
                    let report = rig.copy(&replies).unwrap();
                    assert_eq!(report.method, MoveMethod::Copied);
                    let mut expected = vec!["Stop", "SetAVTransportURI"];
                    if kind == Kind::File {
                        expected.push("Seek");
                    }
                    if !paused {
                        expected.push("Play");
                    }
                    assert_eq!(writes(&replies.calls, rig.host(to)), expected);
                    assert_eq!(writes(&replies.calls, rig.host(from)), ["Stop"]);
                    rig.moved();
                }
            }
        }
    }
}

#[test]
fn unapplied_ignored_and_refused_steps_do_not_advance_or_repeat() {
    for (from, to) in PAIRS {
        for fault in [Reply::Before, Reply::Ignored, Reply::Refused] {
            for (index, stage) in WRITES.iter().enumerate() {
                let rig = Rig::new(from, to, Kind::File, false);
                let replies = Replies::new(&rig.sim, fault, Some((rig.host(to), stage)));
                assert!(rig.copy(&replies).is_err(), "{fault:?}: {stage}");
                assert_eq!(writes(&replies.calls, rig.host(to)), WRITES[..=index]);
                assert!(writes(&replies.calls, rig.host(from)).is_empty());
                rig.source_playing();
            }
        }
    }
}

#[test]
fn failed_readback_keeps_the_original_error_and_does_not_release_the_source() {
    for (from, to) in PAIRS {
        let rig = Rig::new(from, to, Kind::File, false);
        let replies = Replies::new(
            &rig.sim,
            Reply::Unreadable,
            Some((rig.host(to), "SetAVTransportURI")),
        );
        let error = rig.copy(&replies).unwrap_err().to_string();
        assert!(error.contains("injected lost relocation reply"), "{error}");
        assert!(!error.contains("readback is unavailable"), "{error}");
        assert_eq!(writes(&replies.calls, rig.host(to)), WRITES[..2]);
        assert!(writes(&replies.calls, rig.host(from)).is_empty());
        rig.source_playing();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Change {
    Source,
    Seek,
    Pause,
    Group,
    Reboot,
    TrackReply,
    MetadataReply,
}

struct Interruption<'a> {
    rig: &'a Rig,
    inner: SimLan,
    watched: IpAddr,
    affected: IpAddr,
    stage: &'static str,
    change: Change,
    calls: RefCell<Vec<(IpAddr, String)>>,
    fired: Cell<bool>,
    lose_reply: bool,
}

impl<'a> Interruption<'a> {
    fn new(
        rig: &'a Rig,
        watched: IpAddr,
        affected: IpAddr,
        stage: &'static str,
        change: Change,
    ) -> Self {
        Self {
            rig,
            inner: rig.sim.lan(),
            watched,
            affected,
            stage,
            change,
            calls: RefCell::default(),
            fired: Cell::new(false),
            lose_reply: false,
        }
    }

    fn change(&self) -> Result<(), ProtoError> {
        match self.change {
            Change::Source => {
                soap::set_av_transport_uri(&self.inner, self.affected, OTHER, "")?;
                soap::play(&self.inner, self.affected)?;
            }
            Change::Seek => soap::seek_position(&self.inner, self.affected, 88)?,
            Change::Pause => {
                // The owner may start and pause the newly loaded source.
                soap::play(&self.inner, self.affected)?;
                soap::pause(&self.inner, self.affected)?;
            }
            Change::Group => {
                let peer = if self.rig.to.starts_with("S1") {
                    "S1 Source"
                } else {
                    "S2 Source"
                };
                soap::join_group(&self.inner, self.affected, &self.rig.id(peer))?;
            }
            Change::Reboot => {
                self.rig
                    .sim
                    .reboot(&self.rig.to, Duration::ZERO)
                    .map_err(|error| ProtoError::Malformed(error.to_string()))?;
            }
            Change::TrackReply | Change::MetadataReply => {}
        }
        Ok(())
    }
}

impl Transport for Interruption<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let verb = name(action);
        self.calls.borrow_mut().push((host, verb.into()));
        let mut response = self.inner.soap_post(host, path, action, body)?;
        if host == self.watched && verb == self.stage && !self.fired.replace(true) {
            self.change()?;
            if self.lose_reply {
                return Err(lost(host));
            }
        }
        if self.fired.get() && host == self.affected {
            if self.change == Change::TrackReply && verb == "GetPositionInfo" {
                field(
                    &mut response,
                    "TrackURI",
                    "x-file-cifs://example.invalid/unrelated.flac",
                );
            }
            if self.change == Change::MetadataReply && verb == "GetMediaInfo" {
                field(&mut response, "CurrentURIMetaData", "");
            }
        }
        Ok(response)
    }
}

#[test]
fn owner_changes_after_each_stage_block_all_remaining_transfer_writes() {
    let cases = [
        ("Stop", Change::Source),
        ("SetAVTransportURI", Change::Source),
        ("SetAVTransportURI", Change::Seek),
        ("SetAVTransportURI", Change::Pause),
        ("SetAVTransportURI", Change::Group),
        ("SetAVTransportURI", Change::Reboot),
        ("SetAVTransportURI", Change::TrackReply),
        ("SetAVTransportURI", Change::MetadataReply),
        ("Seek", Change::Seek),
        ("Play", Change::Pause),
    ];
    for (from, to) in PAIRS {
        for (stage, change) in cases {
            let rig = Rig::new(from, to, Kind::File, false);
            let host = rig.host(to);
            let t = Interruption::new(&rig, host, host, stage, change);
            assert!(rig.copy(&t).is_err(), "{stage}: {change:?}");
            let index = WRITES.iter().position(|verb| *verb == stage).unwrap();
            assert_eq!(writes(&t.calls, host), WRITES[..=index], "{change:?}");
            assert!(writes(&t.calls, rig.host(from)).is_empty(), "{change:?}");
            rig.source_playing();
            match change {
                Change::Source => {
                    assert_eq!(
                        soap::get_media_info(&rig.sim.lan(), host).unwrap().uri,
                        OTHER
                    );
                }
                Change::Seek => {
                    assert_eq!(
                        soap::get_position_info(&rig.sim.lan(), host)
                            .unwrap()
                            .position_secs,
                        Some(88)
                    );
                }
                Change::Pause => {
                    assert_eq!(
                        soap::get_transport_info(&rig.sim.lan(), host)
                            .unwrap()
                            .state,
                        TransportState::Paused
                    );
                }
                _ => {}
            }
        }
    }
}

#[test]
fn a_new_origin_source_cannot_be_adopted_for_release() {
    for (from, to) in PAIRS {
        let rig = Rig::new(from, to, Kind::File, false);
        let t = Interruption::new(&rig, rig.host(to), rig.host(from), "Play", Change::Source);
        assert!(rig.copy(&t).is_err());
        assert!(writes(&t.calls, rig.host(from)).is_empty());
        assert_eq!(
            soap::get_media_info(&rig.sim.lan(), rig.host(from))
                .unwrap()
                .uri,
            OTHER
        );
        rig.source_playing();
    }
}

#[test]
fn origin_stop_replies_are_proven_for_direct_and_queued_playback() {
    for (from, to) in PAIRS {
        for kind in [Kind::File, Kind::Queue] {
            for fault in [
                Reply::Lost,
                Reply::Malformed,
                Reply::Before,
                Reply::Ignored,
                Reply::Refused,
            ] {
                let rig = Rig::new(from, to, kind, false);
                let replies = Replies::new(&rig.sim, fault, Some((rig.host(from), "Stop")));
                let result = rig.copy(&replies);
                assert_eq!(writes(&replies.calls, rig.host(from)), ["Stop"]);
                if matches!(fault, Reply::Lost | Reply::Malformed) {
                    assert_eq!(result.unwrap().method, MoveMethod::Copied);
                    rig.moved();
                } else {
                    assert!(result.is_err(), "{kind:?}: {fault:?}");
                    rig.source_playing();
                }
            }
        }
    }
}

#[test]
fn source_takeover_after_an_uncertain_stop_is_not_overwritten() {
    for (from, to) in PAIRS {
        let rig = Rig::new(from, to, Kind::File, false);
        let host = rig.host(from);
        let mut t = Interruption::new(&rig, host, host, "Stop", Change::Source);
        t.lose_reply = true;
        let error = rig.copy(&t).unwrap_err().to_string();
        assert!(error.contains("injected lost relocation reply"));
        assert_eq!(writes(&t.calls, host), ["Stop"]);
        assert_eq!(
            soap::get_media_info(&rig.sim.lan(), host).unwrap().uri,
            OTHER
        );
        rig.source_playing();
    }
}

#[test]
fn native_fallback_finishes_after_a_lost_origin_stop_reply() {
    for (from, to) in [("S1 Source", "S1 Target"), ("S2 Source", "S2 Target")] {
        let rig = Rig::new(from, to, Kind::File, false);
        rig.sim
            .upnp_fault(from, "DelegateGroupCoordinationTo", 401)
            .unwrap();
        let replies = Replies::new(&rig.sim, Reply::Lost, Some((rig.host(from), "Stop")));
        let report = moving::move_playback_within(
            &replies,
            &rig.houses,
            &resolve_room(&rig.houses, from).unwrap(),
            &resolve_room(&rig.houses, to).unwrap(),
            Duration::from_millis(50),
        )
        .unwrap();
        assert_eq!(report.method, MoveMethod::Copied);
        assert_eq!(writes(&replies.calls, rig.host(from)), ["Stop"]);
        rig.moved();
    }
}

/// The first and third release-mode reads fail around a successful original
/// mode confirmation. Treating both failures as None would authorize Stop.
struct ModeGap {
    inner: SimLan,
    source: IpAddr,
    target: IpAddr,
    unsupported: bool,
    destination_played: Cell<bool>,
    reads: Cell<usize>,
    calls: RefCell<Vec<(IpAddr, String)>>,
}

impl Transport for ModeGap {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let verb = name(action);
        self.calls.borrow_mut().push((host, verb.into()));
        if host == self.source && verb == "GetTransportSettings" {
            if self.unsupported {
                return Err(ProtoError::SoapFault {
                    code: 401,
                    reason: "this player does not expose ordering".into(),
                });
            }
            if self.destination_played.get() {
                let read = self.reads.get() + 1;
                self.reads.set(read);
                if read == 1 || read == 3 {
                    return Err(lost(host));
                }
            }
        }
        let response = self.inner.soap_post(host, path, action, body)?;
        if host == self.target && verb == "Play" {
            self.destination_played.set(true);
        }
        if host == self.source
            && verb == "GetTransportSettings"
            && self.destination_played.get()
            && self.reads.get() == 2
        {
            set_play_mode(&self.inner, host, PlayMode::RepeatOne)?;
        }
        Ok(response)
    }
}

#[test]
fn unknown_queue_modes_remain_supported_but_known_mode_read_failures_block_release() {
    for (from, to) in PAIRS {
        for unsupported in [false, true] {
            let rig = Rig::new(from, to, Kind::Queue, false);
            let t = ModeGap {
                inner: rig.sim.lan(),
                source: rig.host(from),
                target: rig.host(to),
                unsupported,
                destination_played: Cell::new(false),
                reads: Cell::new(0),
                calls: RefCell::default(),
            };
            let result = rig.copy(&t);
            if unsupported {
                assert_eq!(result.unwrap().method, MoveMethod::Copied);
                rig.moved();
            } else {
                assert!(result.is_err());
                assert!(writes(&t.calls, rig.host(from)).is_empty());
                rig.source_playing();
            }
        }
    }
}
