//! Announcements retain coherent household witnesses and the complete bonded
//! audience, including when a peer serves stale topology or a speaker departs.

use fsonos_core::announce::{AnnounceReport, Announcer, Clip};
use fsonos_core::{HouseholdState, resolve_room};
use fsonos_proto::control as soap;
use fsonos_proto::didl::xml_escape;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::TransportState;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

const CLIP: &str = "http://192.0.2.250:3400/media/topology.wav";
const OLD: &str = "x-rincon-mp3radio://stream.example.invalid/original";

struct Rig {
    sim: SimHandle,
    lan: SimLan,
    houses: Vec<HouseholdState>,
}

impl Rig {
    fn new(s2: bool) -> Self {
        let model = if s2 {
            SimModel::One
        } else {
            SimModel::Play5Gen1
        };
        let players = [
            SimPlayerSpec::pair("Alpha", model),
            SimPlayerSpec::new("Beta", model),
        ];
        let builder = SimHousehold::builder();
        let sim = if s2 {
            builder.s2(players)
        } else {
            builder.s1(players)
        }
        .spawn()
        .unwrap();
        let lan = sim.lan();
        let host = sim.player("Alpha").unwrap().ip;
        soap::set_av_transport_uri(&lan, host, OLD, "").unwrap();
        soap::play(&lan, host).unwrap();
        soap::set_volume(&lan, host, 40).unwrap();
        soap::set_mute(&lan, host, true).unwrap();
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&lan, host).unwrap());
        Self {
            sim,
            lan,
            houses: vec![house],
        }
    }

    fn host(&self) -> IpAddr {
        self.sim.player("Alpha").unwrap().ip
    }

    fn remove_bonded_half(&self) {
        let hidden = self
            .sim
            .players()
            .iter()
            .find(|player| player.hidden)
            .unwrap();
        self.sim.set_offline(&hidden.uuid, true).unwrap();
    }

    fn announce<T: Transport + Sync>(
        &self,
        transport: &T,
        caps: &AtomicUsize,
        duration: Duration,
    ) -> AnnounceReport {
        let targets = [resolve_room(&self.houses, "Alpha").unwrap()];
        Announcer::with_timing(Duration::from_millis(1), Duration::ZERO)
            .announce(
                transport,
                &targets,
                &Clip {
                    url: CLIP.into(),
                    title: "Topology proof".into(),
                    duration,
                },
                25,
                &|_| {
                    caps.fetch_add(1, Ordering::AcqRel);
                    25
                },
            )
            .unwrap()
    }

    fn original_playback(&self) {
        assert_eq!(
            soap::get_media_info(&self.lan, self.host()).unwrap().uri,
            OLD
        );
        assert_eq!(
            soap::get_transport_info(&self.lan, self.host())
                .unwrap()
                .state,
            TransportState::Playing
        );
        assert_eq!(soap::get_volume(&self.lan, self.host()).unwrap(), 40);
        assert!(soap::get_mute(&self.lan, self.host()).unwrap());
    }
}

#[derive(Clone, Copy, Debug)]
enum Fault {
    Empty,
    MissingResponder,
    WrongAddress,
    DuplicatePlayer,
}

fn altered(reply: &str, host: IpAddr, fault: Fault) -> String {
    let response = fsonos_proto::soap::parse_response(reply, "GetZoneGroupState").unwrap();
    let document = response.require("ZoneGroupState").unwrap();
    let parsed = fsonos_proto::topology::parse_zone_group_state(document).unwrap();
    let responder = parsed
        .groups
        .iter()
        .flat_map(|group| &group.members)
        .find(|member| member.ip() == Some(host))
        .unwrap();
    let document = match fault {
        Fault::Empty => "<ZoneGroups/>".into(),
        Fault::MissingResponder => document.replace(&responder.uuid.0, "RINCON_UNKNOWN"),
        Fault::WrongAddress => {
            document.replace(&format!("http://{host}:1400"), "http://192.0.2.240:1400")
        }
        Fault::DuplicatePlayer => {
            let other = parsed
                .groups
                .iter()
                .flat_map(|group| &group.members)
                .find(|member| member.uuid != responder.uuid)
                .unwrap();
            document.replace(&other.uuid.0, &responder.uuid.0)
        }
    };
    format!(
        "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>\
         <u:GetZoneGroupStateResponse xmlns:u=\"urn:x\"><ZoneGroupState>{}\
         </ZoneGroupState></u:GetZoneGroupStateResponse></s:Body></s:Envelope>",
        xml_escape(&document)
    )
}

struct Replies<'a> {
    rig: &'a Rig,
    fault: Fault,
    always: bool,
    topology_reads: Mutex<Vec<IpAddr>>,
    installed: AtomicBool,
    advanced: AtomicBool,
}

impl Transport for Replies<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let result = self.rig.lan.soap_post(host, path, action, body)?;
        if action.contains("#GetZoneGroupState") {
            let mut reads = self.topology_reads.lock().unwrap();
            reads.push(host);
            if self.always || reads.len() == 1 {
                return Ok(altered(&result, host, self.fault));
            }
        }
        if action.contains("#SetAVTransportURI") && body.contains(CLIP) {
            self.installed.store(true, Ordering::Release);
        }
        if action.contains("#GetTransportInfo")
            && self.installed.load(Ordering::Acquire)
            && !self.advanced.swap(true, Ordering::AcqRel)
        {
            // The watcher receives Playing once, then the clip finishes on
            // the simulator's clock without waiting for physical playback.
            self.rig.sim.clock().advance(Duration::from_secs(31));
        }
        Ok(result)
    }
}

#[test]
fn an_incoherent_first_witness_does_not_hide_a_healthy_household() {
    for s2 in [false, true] {
        for fault in [
            Fault::Empty,
            Fault::MissingResponder,
            Fault::WrongAddress,
            Fault::DuplicatePlayer,
        ] {
            let rig = Rig::new(s2);
            let transport = Replies {
                rig: &rig,
                fault,
                always: false,
                topology_reads: Mutex::new(Vec::new()),
                installed: AtomicBool::new(false),
                advanced: AtomicBool::new(false),
            };
            let caps = AtomicUsize::new(0);
            let report = rig.announce(&transport, &caps, Duration::from_secs(30));
            assert!(report.households[0].is_clean(), "{fault:?}: {report:?}");
            assert_eq!(caps.load(Ordering::Acquire), 1);
            let reads = transport.topology_reads.lock().unwrap();
            assert_eq!(reads[0], rig.houses[0].players[0].ip);
            assert_eq!(
                reads[1], rig.houses[0].players[1].ip,
                "{fault:?}: a different witness must establish the household before admission"
            );
            rig.original_playback();
        }
    }
}

#[test]
fn no_coherent_witness_leaves_music_and_admission_callbacks_untouched() {
    for s2 in [false, true] {
        for fault in [Fault::Empty, Fault::WrongAddress, Fault::DuplicatePlayer] {
            let rig = Rig::new(s2);
            let from = rig.sim.soap_log().len();
            let transport = Replies {
                rig: &rig,
                fault,
                always: true,
                topology_reads: Mutex::new(Vec::new()),
                installed: AtomicBool::new(false),
                advanced: AtomicBool::new(false),
            };
            let caps = AtomicUsize::new(0);
            let report = rig.announce(&transport, &caps, Duration::from_secs(30));
            assert!(
                report.households[0].outcome.is_err(),
                "{fault:?}: {report:?}"
            );
            assert_eq!(caps.load(Ordering::Acquire), 0);
            assert_eq!(
                transport.topology_reads.lock().unwrap().len(),
                rig.houses[0].players.len(),
                "each known witness is tried only once"
            );
            assert!(
                rig.sim.soap_log()[from..]
                    .iter()
                    .all(|entry| entry.action.starts_with("Get"))
            );
            rig.original_playback();
        }
    }
}

#[test]
fn a_changed_bonded_room_cannot_inherit_an_already_admitted_target() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        rig.remove_bonded_half();
        let from = rig.sim.soap_log().len();
        let caps = AtomicUsize::new(0);
        let report = rig.announce(&rig.lan, &caps, Duration::from_secs(30));
        assert!(report.households[0].outcome.is_err(), "{report:?}");
        assert_eq!(caps.load(Ordering::Acquire), 0);
        assert!(
            rig.sim.soap_log()[from..]
                .iter()
                .all(|entry| entry.action.starts_with("Get"))
        );
        rig.original_playback();
    }
}

struct Departing<'a> {
    rig: &'a Rig,
    during_restore: bool,
    installed: AtomicBool,
    advanced: AtomicBool,
    changed: AtomicBool,
    after: AtomicUsize,
}

impl Transport for Departing<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let response = self.rig.lan.soap_post(host, path, action, body)?;
        if action.contains("#SetAVTransportURI") && body.contains(CLIP) {
            self.installed.store(true, Ordering::Release);
        }
        let departure = if self.during_restore {
            action.contains("#SetAVTransportURI") && body.contains(OLD)
        } else {
            action.contains("#GetTransportInfo")
        };
        if self.installed.load(Ordering::Acquire)
            && departure
            && !self.changed.swap(true, Ordering::AcqRel)
        {
            self.rig.remove_bonded_half();
            self.after
                .store(self.rig.sim.soap_log().len(), Ordering::Release);
        }
        if self.during_restore
            && self.installed.load(Ordering::Acquire)
            && action.contains("#GetTransportInfo")
            && !self.advanced.swap(true, Ordering::AcqRel)
        {
            self.rig.sim.clock().advance(Duration::from_secs(31));
        }
        Ok(response)
    }
}

#[test]
fn a_changed_bonded_audience_prevents_deadline_stop_and_obsolete_restoration() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        let transport = Departing {
            rig: &rig,
            during_restore: false,
            installed: AtomicBool::new(false),
            advanced: AtomicBool::new(false),
            changed: AtomicBool::new(false),
            after: AtomicUsize::new(0),
        };
        let report = rig.announce(&transport, &AtomicUsize::new(0), Duration::from_millis(5));
        assert!(transport.changed.load(Ordering::Acquire));
        assert!(report.households[0].outcome.is_err(), "{report:?}");
        assert!(
            report.households[0]
                .restored
                .iter()
                .any(|(_, restored)| !restored.skipped.is_empty())
        );
        let log = rig.sim.soap_log();
        let after = &log[transport.after.load(Ordering::Acquire)..];
        assert!(
            after.iter().all(|entry| entry.action.starts_with("Get")),
            "{after:?}"
        );
        assert_eq!(
            soap::get_media_info(&rig.lan, rig.host()).unwrap().uri,
            CLIP
        );
        assert_eq!(
            soap::get_transport_info(&rig.lan, rig.host())
                .unwrap()
                .state,
            TransportState::Playing
        );
        assert_eq!(soap::get_volume(&rig.lan, rig.host()).unwrap(), 25);
    }
}

#[test]
fn a_bonded_room_change_during_cleanup_prevents_remaining_restore_writes() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        let transport = Departing {
            rig: &rig,
            during_restore: true,
            installed: AtomicBool::new(false),
            advanced: AtomicBool::new(false),
            changed: AtomicBool::new(false),
            after: AtomicUsize::new(0),
        };
        let report = rig.announce(&transport, &AtomicUsize::new(0), Duration::from_secs(30));
        assert!(transport.changed.load(Ordering::Acquire));
        assert!(report.households[0].outcome.is_ok(), "{report:?}");
        assert!(
            report.households[0]
                .restored
                .iter()
                .any(|(_, restored)| !restored.skipped.is_empty())
        );
        let log = rig.sim.soap_log();
        let after = &log[transport.after.load(Ordering::Acquire)..];
        assert!(
            after.iter().all(|entry| entry.action.starts_with("Get")),
            "{after:?}"
        );
        assert_eq!(soap::get_media_info(&rig.lan, rig.host()).unwrap().uri, OLD);
        assert_eq!(soap::get_volume(&rig.lan, rig.host()).unwrap(), 25);
        assert!(!soap::get_mute(&rig.lan, rig.host()).unwrap());
        assert_ne!(
            soap::get_transport_info(&rig.lan, rig.host())
                .unwrap()
                .state,
            TransportState::Playing
        );
    }
}

#[derive(Clone, Copy, Debug)]
enum MoveAt {
    SourceWrite,
    LevelRead,
}

struct Readdressed {
    sim: Mutex<SimHandle>,
    lan: SimLan,
    stage: MoveAt,
    installed: AtomicBool,
    restoring: AtomicBool,
    advanced: AtomicBool,
    moved: AtomicBool,
    after: AtomicUsize,
}

impl Transport for Readdressed {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let response = self.lan.soap_post(host, path, action, body)?;
        if action.contains("#SetAVTransportURI") && body.contains(CLIP) {
            self.installed.store(true, Ordering::Release);
        }
        if self.installed.load(Ordering::Acquire) {
            let restored_source = action.contains("#SetAVTransportURI") && body.contains(OLD);
            if restored_source {
                self.restoring.store(true, Ordering::Release);
            }
            if action.contains("#GetTransportInfo") && !self.advanced.swap(true, Ordering::AcqRel) {
                self.sim
                    .lock()
                    .unwrap()
                    .clock()
                    .advance(Duration::from_secs(31));
            }
            let relocate = match self.stage {
                MoveAt::SourceWrite => restored_source,
                MoveAt::LevelRead => {
                    self.restoring.load(Ordering::Acquire) && action.contains("#GetVolume")
                }
            };
            if relocate && !self.moved.swap(true, Ordering::AcqRel) {
                let IpAddr::V4(old) = host else {
                    panic!("synthetic players use IPv4");
                };
                let mut sim = self.sim.lock().unwrap();
                sim.change_address("Alpha").unwrap();
                sim.move_to("Beta", old, SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
                    .unwrap();
                self.after.store(sim.soap_log().len(), Ordering::Release);
            }
        }
        Ok(response)
    }
}

#[test]
fn a_fresh_address_cannot_authorize_restore_writes_to_the_reused_old_endpoint() {
    const OTHER: &str = "x-file-cifs://example.invalid/other-room.flac";
    for s2 in [false, true] {
        for stage in [MoveAt::SourceWrite, MoveAt::LevelRead] {
            let Rig { sim, lan, houses } = Rig::new(s2);
            let other = sim.player("Beta").unwrap().ip;
            soap::set_av_transport_uri(&lan, other, OTHER, "").unwrap();
            soap::play(&lan, other).unwrap();
            soap::pause(&lan, other).unwrap();
            let transport = Readdressed {
                sim: Mutex::new(sim),
                lan,
                stage,
                installed: AtomicBool::new(false),
                restoring: AtomicBool::new(false),
                advanced: AtomicBool::new(false),
                moved: AtomicBool::new(false),
                after: AtomicUsize::new(0),
            };
            let targets = [resolve_room(&houses, "Alpha").unwrap()];
            let report = Announcer::with_timing(Duration::from_millis(1), Duration::ZERO)
                .announce(
                    &transport,
                    &targets,
                    &Clip {
                        url: CLIP.into(),
                        title: "Address retention".into(),
                        duration: Duration::from_secs(30),
                    },
                    25,
                    &|_| 25,
                )
                .unwrap();
            assert!(
                transport.moved.load(Ordering::Acquire),
                "{stage:?}: {report:?}"
            );
            assert!(!report.households[0].is_clean(), "{stage:?}: {report:?}");
            assert!(
                report.households[0].restored.iter().any(|(_, restored)| {
                    restored
                        .skipped
                        .iter()
                        .any(|(_, why)| why.contains("address"))
                }),
                "{report:?}"
            );
            let sim = transport.sim.lock().unwrap();
            let log = sim.soap_log();
            let after = &log[transport.after.load(Ordering::Acquire)..];
            assert!(
                after.iter().all(|entry| entry.action.starts_with("Get")),
                "{after:?}"
            );
            let other = sim.player("Beta").unwrap().ip;
            assert_eq!(
                soap::get_media_info(&transport.lan, other).unwrap().uri,
                OTHER
            );
            assert_eq!(soap::get_volume(&transport.lan, other).unwrap(), 20);
            assert!(!soap::get_mute(&transport.lan, other).unwrap());
            assert_eq!(
                soap::get_transport_info(&transport.lan, other)
                    .unwrap()
                    .state,
                TransportState::Paused
            );
        }
    }
}
