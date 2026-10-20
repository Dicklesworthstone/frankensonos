//! Native handoff recovery over real S1/S2 loopback SOAP. An uncertain
//! delegation must not be repeated or turned into a destructive queue copy.

use fsonos_core::moving::{MoveMethod, MoveReport, move_playback_within};
use fsonos_core::{HouseholdState, control, resolve_room};
use fsonos_proto::control as soap;
use fsonos_proto::play_mode::{get_play_mode, set_play_mode};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayMode, PlayerId, TransportState};
use std::cell::{Cell, RefCell};
use std::net::IpAddr;
use std::time::Duration;

const RADIO: &str = "x-rincon-mp3radio://example.invalid/owner-radio";

struct Fixture {
    sim: SimHandle,
    houses: Vec<HouseholdState>,
    source: PlayerId,
    target: PlayerId,
    source_host: IpAddr,
    target_host: IpAddr,
}

impl Fixture {
    fn new(generation: u8, paired: bool) -> Self {
        let model = if generation == 1 {
            SimModel::Play5Gen1
        } else {
            SimModel::One
        };
        let room = |name| {
            if paired {
                SimPlayerSpec::pair(name, model)
            } else {
                SimPlayerSpec::new(name, model)
            }
        };
        let specs = [room("Source"), room("Target"), room("Companion")];
        let builder = SimHousehold::builder();
        let builder = if generation == 1 {
            builder.s1(specs)
        } else {
            builder.s2(specs)
        };
        let sim = builder.spawn().unwrap();
        let source_host = sim.player("Source").unwrap().ip;
        let target_host = sim.player("Target").unwrap().ip;
        let source = PlayerId(sim.player("Source").unwrap().uuid.clone());
        let target = PlayerId(sim.player("Target").unwrap().uuid.clone());
        let lan = sim.lan();
        for room in ["Target", "Companion"] {
            soap::join_group(&lan, sim.player(room).unwrap().ip, &source).unwrap();
        }
        for n in 1..=3 {
            let uri = format!("x-file-cifs://example.invalid/movement{n}.flac");
            soap::add_uri_to_queue(&lan, source_host, &uri, "", false).unwrap();
        }
        soap::play_from_queue(&lan, source_host, &source).unwrap();
        soap::seek_track(&lan, source_host, 2).unwrap();
        soap::seek_position(&lan, source_host, 37).unwrap();
        soap::play(&lan, source_host).unwrap();
        set_play_mode(&lan, source_host, PlayMode::ShuffleNoRepeat).unwrap();
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&lan, source_host).unwrap());
        Self {
            sim,
            houses: vec![house],
            source,
            target,
            source_host,
            target_host,
        }
    }

    fn handoff<T: Transport>(&self, t: &T, deadline: Duration) -> Result<MoveReport, String> {
        let from = resolve_room(&self.houses, "Source").unwrap();
        let to = resolve_room(&self.houses, "Target").unwrap();
        move_playback_within(t, &self.houses, &from, &to, deadline).map_err(|e| e.to_string())
    }

    fn preserved(&self, report: &MoveReport, paired: bool) {
        assert_eq!(report.method, MoveMethod::Delegated);
        assert_eq!(report.old_coordinator, self.source);
        assert_eq!(report.new_coordinator, self.target);
        let lan = self.sim.lan();
        let position = soap::get_position_info(&lan, self.target_host).unwrap();
        assert_eq!((position.track, position.position_secs), (2, Some(37)));
        assert_eq!(position.uri, "x-file-cifs://example.invalid/movement2.flac");
        assert_eq!(
            get_play_mode(&lan, self.target_host).unwrap(),
            PlayMode::ShuffleNoRepeat
        );
        assert_eq!(
            soap::get_transport_info(&lan, self.target_host)
                .unwrap()
                .state,
            TransportState::Playing
        );
        let topology = get_zone_group_state(&lan, self.target_host).unwrap();
        let from = topology
            .groups
            .iter()
            .find(|g| g.coordinator == self.source)
            .unwrap();
        let to = topology
            .groups
            .iter()
            .find(|g| g.coordinator == self.target)
            .unwrap();
        assert_eq!(from.members.len(), if paired { 2 } else { 1 });
        assert_eq!(to.members.len(), if paired { 4 } else { 2 });
    }

    fn only_native_write(&self, since: usize) {
        let writes: Vec<_> = self.sim.soap_log()[since..]
            .iter()
            .filter(|e| !e.action.starts_with("Get") && e.action != "Browse")
            .map(|e| e.action.clone())
            .collect();
        assert_eq!(writes, ["DelegateGroupCoordinationTo"]);
    }
}

#[derive(Clone, Copy)]
enum Fault {
    None,
    Lost,
    Malformed,
    NotSent,
    SourceUnavailable,
    BothUnavailable,
    DelayedTopology,
    UnrelatedGroup,
    RejoinedSource,
    ChangedMedia,
    Ignored,
    Refused,
    RefusedNewSource,
    PreflightChanged,
}

struct Replies {
    inner: SimLan,
    target: PlayerId,
    source_host: IpAddr,
    target_host: IpAddr,
    houses: Vec<HouseholdState>,
    fault: Fault,
    delegations: Cell<usize>,
    topology_reads: Cell<usize>,
    before: RefCell<Option<String>>,
    injected: Cell<bool>,
}

impl Replies {
    fn new(f: &Fixture, fault: Fault) -> Self {
        Self {
            inner: f.sim.lan(),
            target: f.target.clone(),
            source_host: f.source_host,
            target_host: f.target_host,
            houses: f.houses.clone(),
            fault,
            delegations: Cell::new(0),
            topology_reads: Cell::new(0),
            before: RefCell::new(None),
            injected: Cell::new(false),
        }
    }

    fn lost(host: IpAddr) -> ProtoError {
        ProtoError::Network {
            target: host.to_string(),
            detail: "injected lost handoff reply".into(),
        }
    }

    fn refuse() -> ProtoError {
        ProtoError::SoapFault {
            code: 401,
            reason: "injected unavailable action".into(),
        }
    }

    fn leave_target(&self) {
        control::leave(&self.inner, &self.houses, &self.target).unwrap();
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
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        let sent = self.delegations.get() > 0;
        if sent && name == "GetZoneGroupState" {
            self.topology_reads.set(self.topology_reads.get() + 1);
            if matches!(self.fault, Fault::BothUnavailable)
                || (matches!(self.fault, Fault::SourceUnavailable) && host == self.source_host)
            {
                return Err(Self::lost(host));
            }
            if matches!(self.fault, Fault::DelayedTopology) && self.topology_reads.get() == 1 {
                return Ok(self.before.borrow().as_ref().unwrap().clone());
            }
        }
        if name != "DelegateGroupCoordinationTo" {
            let response = self.inner.soap_post(host, path, action, body)?;
            if !sent && name == "GetZoneGroupState" {
                *self.before.borrow_mut() = Some(response.clone());
            }
            if !sent
                && name == "GetMediaInfo"
                && matches!(self.fault, Fault::PreflightChanged)
                && !self.injected.replace(true)
            {
                self.leave_target();
            }
            return Ok(response);
        }
        self.delegations.set(self.delegations.get() + 1);
        if matches!(self.fault, Fault::NotSent) {
            return Err(Self::lost(host));
        }
        if matches!(self.fault, Fault::Refused | Fault::RefusedNewSource) {
            if matches!(self.fault, Fault::RefusedNewSource) {
                soap::set_av_transport_uri(&self.inner, host, RADIO, "")?;
                soap::play(&self.inner, host)?;
            }
            return Err(Self::refuse());
        }
        let response = self.inner.soap_post(host, path, action, body)?;
        match self.fault {
            Fault::Lost
            | Fault::SourceUnavailable
            | Fault::BothUnavailable
            | Fault::DelayedTopology => Err(Self::lost(host)),
            Fault::Malformed => Ok("<invalid-response>".into()),
            Fault::UnrelatedGroup => {
                self.leave_target();
                Ok(response)
            }
            Fault::RejoinedSource => {
                soap::join_group(&self.inner, self.source_host, &self.target)?;
                Ok(response)
            }
            Fault::ChangedMedia => {
                soap::set_av_transport_uri(&self.inner, self.target_host, RADIO, "")?;
                Ok(response)
            }
            _ => Ok(response),
        }
    }
}

#[test]
fn lost_and_malformed_native_handoffs_preserve_playback_in_both_generations() {
    for generation in [1, 2] {
        for fault in [Fault::None, Fault::Lost, Fault::Malformed] {
            let f = Fixture::new(generation, false);
            let t = Replies::new(&f, fault);
            let since = f.sim.soap_log().len();
            let report = f.handoff(&t, Duration::ZERO).unwrap();
            assert_eq!(t.delegations.get(), 1);
            f.preserved(&report, false);
            f.only_native_write(since);
        }
    }
}

#[test]
fn source_and_destination_stereo_pairs_remain_whole_on_handoff() {
    for generation in [1, 2] {
        let f = Fixture::new(generation, true);
        let t = Replies::new(&f, Fault::Lost);
        let since = f.sim.soap_log().len();
        let report = f.handoff(&t, Duration::ZERO).unwrap();
        f.preserved(&report, true);
        f.only_native_write(since);
    }
}

#[test]
fn the_new_coordinator_proves_handoff_when_the_old_one_stops_answering() {
    for generation in [1, 2] {
        let f = Fixture::new(generation, false);
        let t = Replies::new(&f, Fault::SourceUnavailable);
        let report = f.handoff(&t, Duration::ZERO).unwrap();
        f.preserved(&report, false);
    }
}

#[test]
fn an_unapplied_lost_handoff_never_starts_a_fallback_copy() {
    for generation in [1, 2] {
        let f = Fixture::new(generation, false);
        let t = Replies::new(&f, Fault::NotSent);
        let since = f.sim.soap_log().len();
        assert!(f.handoff(&t, Duration::ZERO).is_err());
        assert_eq!(t.delegations.get(), 1);
        assert!(
            f.sim.soap_log()[since..]
                .iter()
                .all(|e| e.action.starts_with("Get"))
        );
        assert_eq!(
            soap::get_transport_info(&f.sim.lan(), f.source_host)
                .unwrap()
                .state,
            TransportState::Playing
        );
    }
}

#[test]
fn unreadable_handoff_outcomes_never_repeat_the_native_write_or_clear_a_queue() {
    for generation in [1, 2] {
        let f = Fixture::new(generation, false);
        let t = Replies::new(&f, Fault::BothUnavailable);
        let since = f.sim.soap_log().len();
        assert!(f.handoff(&t, Duration::ZERO).is_err());
        assert_eq!(t.delegations.get(), 1);
        f.only_native_write(since);
        assert_eq!(
            soap::get_position_info(&f.sim.lan(), f.target_host)
                .unwrap()
                .track,
            2
        );
    }
}

#[test]
fn a_delayed_topology_is_polled_without_resending_delegation() {
    for generation in [1, 2] {
        let f = Fixture::new(generation, false);
        let t = Replies::new(&f, Fault::DelayedTopology);
        let since = f.sim.soap_log().len();
        let report = f.handoff(&t, Duration::from_secs(1)).unwrap();
        assert!(t.topology_reads.get() >= 2);
        assert_eq!(t.delegations.get(), 1);
        f.preserved(&report, false);
        f.only_native_write(since);
    }
}

#[test]
fn confirmed_refusal_and_acknowledged_noop_keep_the_working_copy_fallback() {
    for generation in [1, 2] {
        for fault in [Fault::Refused, Fault::Ignored] {
            let f = Fixture::new(generation, false);
            if matches!(fault, Fault::Ignored) {
                f.sim
                    .ignore_action("Source", "DelegateGroupCoordinationTo")
                    .unwrap();
            }
            let t = Replies::new(&f, fault);
            let report = f.handoff(&t, Duration::ZERO).unwrap();
            assert_eq!(report.method, MoveMethod::Copied);
            assert_eq!(report.new_coordinator, f.target);
            assert_eq!(t.delegations.get(), 1);
            assert_eq!(
                soap::get_position_info(&f.sim.lan(), f.target_host)
                    .unwrap()
                    .track,
                2
            );
            assert_eq!(
                get_play_mode(&f.sim.lan(), f.target_host).unwrap(),
                PlayMode::ShuffleNoRepeat
            );
        }
    }
}

#[test]
fn an_unrelated_target_group_is_not_a_successful_transfer_or_permission_to_copy() {
    for generation in [1, 2] {
        let f = Fixture::new(generation, false);
        f.sim
            .ignore_action("Source", "DelegateGroupCoordinationTo")
            .unwrap();
        let t = Replies::new(&f, Fault::UnrelatedGroup);
        let since = f.sim.soap_log().len();
        assert!(f.handoff(&t, Duration::ZERO).is_err());
        assert_eq!(t.delegations.get(), 1);
        assert!(f.sim.soap_log()[since..].iter().all(|e| !matches!(
            e.action.as_str(),
            "RemoveAllTracksFromQueue" | "AddURIToQueue" | "Play" | "Stop"
        )));
    }
}

#[test]
fn a_source_that_rejoins_or_a_target_playing_different_music_is_not_confirmed() {
    for generation in [1, 2] {
        for fault in [Fault::RejoinedSource, Fault::ChangedMedia] {
            let f = Fixture::new(generation, false);
            let t = Replies::new(&f, fault);
            let since = f.sim.soap_log().len();
            assert!(f.handoff(&t, Duration::ZERO).is_err());
            assert_eq!(t.delegations.get(), 1);
            assert!(f.sim.soap_log()[since..].iter().all(|e| !matches!(
                e.action.as_str(),
                "RemoveAllTracksFromQueue" | "AddURIToQueue" | "Play" | "Stop"
            )));
        }
    }
}

#[test]
fn a_refused_handoff_does_not_copy_the_owners_new_source() {
    for generation in [1, 2] {
        let f = Fixture::new(generation, false);
        let t = Replies::new(&f, Fault::RefusedNewSource);
        assert!(f.handoff(&t, Duration::ZERO).is_err());
        assert_eq!(
            soap::get_media_info(&f.sim.lan(), f.source_host)
                .unwrap()
                .uri,
            RADIO
        );
        assert_eq!(
            soap::get_transport_info(&f.sim.lan(), f.source_host)
                .unwrap()
                .state,
            TransportState::Playing
        );
        assert!(
            soap::get_media_info(&f.sim.lan(), f.target_host)
                .unwrap()
                .uri
                .starts_with("x-rincon:")
        );
    }
}

#[test]
fn a_target_leaving_during_preflight_prevents_the_delegation() {
    for generation in [1, 2] {
        let f = Fixture::new(generation, false);
        let t = Replies::new(&f, Fault::PreflightChanged);
        assert!(f.handoff(&t, Duration::ZERO).is_err());
        assert_eq!(t.delegations.get(), 0);
    }
}

#[test]
fn a_direct_stream_can_be_confirmed_after_a_lost_native_handoff_reply() {
    for generation in [1, 2] {
        let f = Fixture::new(generation, false);
        soap::set_av_transport_uri(&f.sim.lan(), f.source_host, RADIO, "").unwrap();
        soap::play(&f.sim.lan(), f.source_host).unwrap();
        let t = Replies::new(&f, Fault::Lost);
        let since = f.sim.soap_log().len();
        assert_eq!(
            f.handoff(&t, Duration::ZERO).unwrap().method,
            MoveMethod::Delegated
        );
        assert_eq!(
            soap::get_media_info(&f.sim.lan(), f.target_host)
                .unwrap()
                .uri,
            RADIO
        );
        f.only_native_write(since);
    }
}

#[test]
fn native_recovery_keeps_a_paused_queue_inaudible_and_in_place() {
    for generation in [1, 2] {
        let f = Fixture::new(generation, false);
        soap::pause(&f.sim.lan(), f.source_host).unwrap();
        let t = Replies::new(&f, Fault::Lost);
        let since = f.sim.soap_log().len();
        assert_eq!(
            f.handoff(&t, Duration::ZERO).unwrap().method,
            MoveMethod::Delegated
        );
        assert_eq!(
            soap::get_transport_info(&f.sim.lan(), f.target_host)
                .unwrap()
                .state,
            TransportState::Paused
        );
        let position = soap::get_position_info(&f.sim.lan(), f.target_host).unwrap();
        assert_eq!((position.track, position.position_secs), (2, Some(37)));
        f.only_native_write(since);
    }
}
