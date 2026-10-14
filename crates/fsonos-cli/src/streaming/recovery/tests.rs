use super::{PLAYBACK, restore, same_source, verify_start};
use fsonos_core::snapshot::{self, Aspect, ZoneSnapshot};
use fsonos_core::{HouseholdState, control, resolve_room};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport, control as soap};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::TransportState;
use std::net::IpAddr;
use std::sync::atomic::{AtomicUsize, Ordering};

const OLD: &str = "x-rincon-mp3radio://stream.example.invalid/original";
const NEW: &str = "x-rincon-mp3radio://stream.example.invalid/newer";
const STREAM: &str = "x-rincon-mp3radio://192.0.2.250:3400/stream/synthetic.mp3";

struct Rig {
    sim: SimHandle,
    lan: SimLan,
    houses: Vec<HouseholdState>,
    before: ZoneSnapshot,
    host: IpAddr,
}

impl Rig {
    fn new(s2: bool) -> Self {
        let model = if s2 {
            SimModel::One
        } else {
            SimModel::Play5Gen1
        };
        let players = [
            SimPlayerSpec::new("Alpha", model),
            SimPlayerSpec::new("Beta", model),
            SimPlayerSpec::new("Gamma", model),
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
        let mut state = HouseholdState::default();
        state.apply_topology(&get_zone_group_state(&lan, host).unwrap());
        let coordinator = resolve_room(std::slice::from_ref(&state), "Alpha")
            .unwrap()
            .player
            .id
            .clone();
        soap::join_group(&lan, sim.player("Beta").unwrap().ip, &coordinator).unwrap();
        state.apply_topology(&get_zone_group_state(&lan, host).unwrap());
        let houses = vec![state];
        soap::set_av_transport_uri(&lan, host, OLD, "").unwrap();
        soap::play(&lan, host).unwrap();
        soap::set_volume(&lan, host, 25).unwrap();
        let before = snapshot::capture(&lan, &houses, &coordinator, 1_000).unwrap();
        Self {
            sim,
            lan,
            houses,
            before,
            host,
        }
    }

    fn stream(&self, play: bool) {
        soap::set_av_transport_uri(&self.lan, self.host, STREAM, "").unwrap();
        if play {
            soap::play(&self.lan, self.host).unwrap();
        }
    }

    fn queue(&mut self) {
        control::queue_spotify_tracks(
            &self.lan,
            &self.houses,
            &self.before.coordinator,
            &[
                ("spotify:track:0SimSnapA0000000000001", "First"),
                ("spotify:track:0SimSnapB0000000000002", "Second"),
            ],
        )
        .unwrap()
        .unwrap();
        control::play_queue_from(&self.lan, &self.houses, &self.before.coordinator, 2).unwrap();
        self.before =
            snapshot::capture(&self.lan, &self.houses, &self.before.coordinator, 2_000).unwrap();
    }

    fn readonly_since(&self, index: usize) {
        let log = self.sim.soap_log();
        assert!(
            log[index..]
                .iter()
                .all(|entry| entry.action.starts_with("Get") || entry.action == "Browse"),
            "{:?}",
            &log[index..]
        );
    }
}

#[test]
fn source_identity_accepts_only_the_exact_radio_http_alias() {
    assert!(same_source(STREAM, STREAM));
    let http = STREAM.replacen("x-rincon-mp3radio://", "http://", 1);
    assert!(same_source(STREAM, &http));
    assert!(same_source(&http, STREAM));
    assert!(!same_source(STREAM, &http.replace("http://", "https://")));
    assert!(!same_source(STREAM, &format!("{http}?other=1")));
    assert!(!same_source(
        STREAM,
        "192.0.2.250:3400/stream/synthetic.mp3"
    ));
    assert!(!same_source(STREAM, NEW));
}

#[test]
fn startup_refuses_to_displace_new_playback_during_encoder_warmup() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        verify_start(&rig.lan, &rig.houses, &rig.before).unwrap();
        soap::set_av_transport_uri(&rig.lan, rig.host, NEW, "").unwrap();
        soap::play(&rig.lan, rig.host).unwrap();
        let after = rig.sim.soap_log().len();
        assert!(verify_start(&rig.lan, &rig.houses, &rig.before).is_err());
        rig.readonly_since(after);
        soap::set_av_transport_uri(&rig.lan, rig.host, OLD, "").unwrap();
        soap::play(&rig.lan, rig.host).unwrap();
        soap::pause(&rig.lan, rig.host).unwrap();
        let after = rig.sim.soap_log().len();
        assert!(verify_start(&rig.lan, &rig.houses, &rig.before).is_err());
        rig.readonly_since(after);
    }
}

#[test]
fn restoration_preserves_new_member_volumes_and_mute_in_both_generations() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        rig.stream(true);
        let member = rig.sim.player("Beta").unwrap().ip;
        soap::set_volume(&rig.lan, rig.host, 55).unwrap();
        soap::set_volume(&rig.lan, member, 61).unwrap();
        soap::set_mute(&rig.lan, member, true).unwrap();
        let report = restore(&rig.lan, &rig.houses, &rig.before, STREAM, true).unwrap();
        assert!(report.skipped.is_empty(), "{report:?}");
        assert!(
            report
                .restored
                .iter()
                .all(|aspect| PLAYBACK.contains(aspect))
        );
        assert_eq!(soap::get_media_info(&rig.lan, rig.host).unwrap().uri, OLD);
        assert_eq!(
            soap::get_transport_info(&rig.lan, rig.host).unwrap().state,
            TransportState::Playing
        );
        assert_eq!(soap::get_volume(&rig.lan, rig.host).unwrap(), 55);
        assert_eq!(soap::get_volume(&rig.lan, member).unwrap(), 61);
        assert!(soap::get_mute(&rig.lan, member).unwrap());
    }
}

#[test]
fn an_installed_uri_is_recovered_even_when_play_was_not_acknowledged() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        rig.stream(false);
        let report = restore(&rig.lan, &rig.houses, &rig.before, STREAM, false).unwrap();
        assert!(report.skipped.is_empty(), "{report:?}");
        assert_eq!(soap::get_media_info(&rig.lan, rig.host).unwrap().uri, OLD);
        assert_eq!(
            soap::get_transport_info(&rig.lan, rig.host).unwrap().state,
            TransportState::Playing
        );
    }
}

#[test]
fn newer_source_and_group_membership_are_not_rolled_back() {
    for s2 in [false, true] {
        for regroup in [false, true] {
            let rig = Rig::new(s2);
            rig.stream(true);
            if regroup {
                let gamma = resolve_room(&rig.houses, "Gamma")
                    .unwrap()
                    .player
                    .id
                    .clone();
                soap::join_group(&rig.lan, rig.sim.player("Beta").unwrap().ip, &gamma).unwrap();
            } else {
                soap::set_av_transport_uri(&rig.lan, rig.host, NEW, "").unwrap();
                soap::play(&rig.lan, rig.host).unwrap();
            }
            let after = rig.sim.soap_log().len();
            let report = restore(&rig.lan, &rig.houses, &rig.before, STREAM, true).unwrap();
            assert!(report.restored.is_empty(), "{report:?}");
            assert_eq!(report.skipped.len(), PLAYBACK.len());
            rig.readonly_since(after);
        }
    }
}

#[test]
fn a_manual_pause_does_not_turn_into_an_automatic_resume() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        rig.stream(true);
        soap::pause(&rig.lan, rig.host).unwrap();
        let report = restore(&rig.lan, &rig.houses, &rig.before, STREAM, true).unwrap();
        assert_eq!(soap::get_media_info(&rig.lan, rig.host).unwrap().uri, OLD);
        assert_ne!(
            soap::get_transport_info(&rig.lan, rig.host).unwrap().state,
            TransportState::Playing
        );
        assert!(
            report
                .skipped
                .iter()
                .any(|(aspect, _)| *aspect == Aspect::Transport)
        );
    }
}

#[derive(Clone, Copy)]
enum Interference {
    Source,
    Queue,
    ReadFailure,
}

struct Changing<'a> {
    rig: &'a Rig,
    change: Interference,
    reads: AtomicUsize,
    after: AtomicUsize,
}

impl Transport for Changing<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if action.ends_with("#GetMediaInfo") {
            if matches!(self.change, Interference::ReadFailure) {
                return Err(ProtoError::Network {
                    target: host.to_string(),
                    detail: "synthetic stream ownership read failure".into(),
                });
            }
            if self.reads.fetch_add(1, Ordering::SeqCst) == 1 {
                match self.change {
                    Interference::Source => {
                        soap::set_av_transport_uri(&self.rig.lan, host, NEW, "")?;
                        soap::play(&self.rig.lan, host)?;
                    }
                    Interference::Queue => {
                        control::queue_spotify_tracks(
                            &self.rig.lan,
                            &self.rig.houses,
                            &self.rig.before.coordinator,
                            &[("spotify:track:0SimSnapC0000000000003", "New queue item")],
                        )
                        .unwrap()
                        .unwrap();
                    }
                    Interference::ReadFailure => unreachable!(),
                }
                self.after
                    .store(self.rig.sim.soap_log().len(), Ordering::SeqCst);
            }
        }
        self.rig.lan.soap_post(host, path, action, body)
    }
}

#[test]
fn ownership_is_rechecked_at_the_first_restore_write() {
    for s2 in [false, true] {
        for queue in [false, true] {
            let mut rig = Rig::new(s2);
            if queue {
                rig.queue();
            }
            rig.stream(true);
            let transport = Changing {
                rig: &rig,
                change: if queue {
                    Interference::Queue
                } else {
                    Interference::Source
                },
                reads: AtomicUsize::new(0),
                after: AtomicUsize::new(0),
            };
            let report = restore(&transport, &rig.houses, &rig.before, STREAM, true).unwrap();
            assert!(transport.reads.load(Ordering::SeqCst) >= 2);
            assert!(!report.skipped.is_empty(), "{report:?}");
            let after = transport.after.load(Ordering::SeqCst);
            assert!(after > 0, "the concurrent change must actually run");
            rig.readonly_since(after);
        }
    }
}

#[test]
fn an_unreadable_owner_never_authorizes_a_restore_write() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        rig.stream(true);
        let after = rig.sim.soap_log().len();
        let transport = Changing {
            rig: &rig,
            change: Interference::ReadFailure,
            reads: AtomicUsize::new(0),
            after: AtomicUsize::new(0),
        };
        assert!(restore(&transport, &rig.houses, &rig.before, STREAM, true).is_err());
        rig.readonly_since(after);
    }
}
