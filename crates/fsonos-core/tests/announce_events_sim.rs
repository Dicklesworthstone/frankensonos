//! Announcements follow real accepted callbacks in both Sonos generations.
//! Only the announcement transport is counted; the live model has its own.

use fsonos_core::announce::{Announcer, Clip, Ending};
use fsonos_core::live::{Live, LiveConfig};
use fsonos_core::{HouseholdState, resolve_room};
use fsonos_proto::control as soap;
use fsonos_proto::net::Lan;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{GenaEvent, SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};
use std::net::IpAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

const CLIP: &str = "http://192.0.2.250:3400/media/live-announcement.wav";
const ALPHA: &str = "x-rincon-mp3radio://stream.example.invalid/alpha";
const BETA: &str = "x-rincon-mp3radio://stream.example.invalid/beta";
const NEWER: &str = "x-rincon-mp3radio://stream.example.invalid/newer";

struct Counted {
    lan: SimLan,
    reads: AtomicUsize,
}

impl Transport for Counted {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let action_name = action.trim_matches('"');
        if action_name.ends_with("#GetTransportInfo") || action_name.ends_with("#GetMediaInfo") {
            self.reads.fetch_add(1, Ordering::Relaxed);
        }
        self.lan.soap_post(host, path, action, body)
    }
}

struct Fixture {
    // Stop the live workers before the simulator's sockets disappear.
    live: Live,
    sim: SimHandle,
    transport: Counted,
}

impl Fixture {
    fn new(s2: bool) -> Self {
        let model = if s2 {
            SimModel::One
        } else {
            SimModel::Play5Gen1
        };
        let players = [
            SimPlayerSpec::new("Alpha", model),
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
        for (room, uri, volume, mute) in [("Alpha", ALPHA, 18, false), ("Beta", BETA, 27, true)] {
            let host = sim.player(room).unwrap().ip;
            soap::set_av_transport_uri(&lan, host, uri, "").unwrap();
            soap::play(&lan, host).unwrap();
            soap::set_volume(&lan, host, volume).unwrap();
            soap::set_mute(&lan, host, mute).unwrap();
        }
        let callbacks = Arc::new(
            Lan::start()
                .unwrap()
                .with_routes(sim.players().iter().map(|p| (p.ip, p.addr)).collect()),
        );
        let reads: Arc<dyn Transport + Send + Sync> = Arc::new(lan.clone());
        let mut config = LiveConfig::new(sim.players().iter().map(|p| p.ip).collect());
        config.interval = Duration::from_secs(3600);
        config.max_backoff = Duration::from_secs(3600);
        let live = Live::start_with(reads, callbacks, config);
        assert!(live.wait_ready(Duration::from_secs(10)));
        let fixture = Self {
            live,
            sim,
            transport: Counted {
                lan,
                reads: AtomicUsize::new(0),
            },
        };
        fixture.wait_for(|| {
            let events = fixture.sim.gena_log();
            events
                .iter()
                .filter(|entry| {
                    entry.service == "AVTransport"
                        && matches!(
                            entry.event,
                            GenaEvent::Delivered {
                                seq: 0,
                                status: 200,
                                ..
                            }
                        )
                })
                .count()
                >= 2
        });
        fixture
    }

    fn wait_for(&self, predicate: impl Fn() -> bool) {
        let until = Instant::now() + Duration::from_secs(10);
        while !predicate() {
            assert!(
                Instant::now() < until,
                "live announcement did not settle: {:?}",
                self.live.snapshot()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn houses(&self) -> Vec<HouseholdState> {
        let mut house = HouseholdState::default();
        house.apply_topology(
            &get_zone_group_state(&self.transport.lan, self.sim.player("Alpha").unwrap().ip)
                .unwrap(),
        );
        vec![house]
    }

    fn wait_for_clip(&self) {
        let lead = PlayerId(self.sim.player("Alpha").unwrap().uuid.clone());
        self.wait_for(|| {
            self.live.player(&lead).is_some_and(|p| {
                p.track_uri.as_deref() == Some(CLIP) && p.transport == Some(TransportState::Playing)
            })
        });
    }

    fn assert_restored(&self) {
        let houses = self.houses();
        for (room, uri, volume, mute) in [("Alpha", ALPHA, 18, false), ("Beta", BETA, 27, true)] {
            let target = resolve_room(&houses, room).unwrap();
            assert_eq!(target.coordinator.id, target.player.id, "{room} regrouped");
            let host = target.player.ip;
            assert_eq!(
                soap::get_media_info(&self.transport.lan, host).unwrap().uri,
                uri
            );
            assert_eq!(
                soap::get_transport_info(&self.transport.lan, host)
                    .unwrap()
                    .state,
                TransportState::Playing
            );
            assert_eq!(soap::get_volume(&self.transport.lan, host).unwrap(), volume);
            assert_eq!(soap::get_mute(&self.transport.lan, host).unwrap(), mute);
        }
    }
}

fn clip() -> Clip {
    Clip {
        url: CLIP.into(),
        title: "Live announcement".into(),
        duration: Duration::from_secs(3),
    }
}

fn completes_from_events(s2: bool) {
    let fixture = Fixture::new(s2);
    let houses = fixture.houses();
    let targets = [
        resolve_room(&houses, "Alpha").unwrap(),
        resolve_room(&houses, "Beta").unwrap(),
    ];
    let clip = clip();
    let announcer = Announcer::with_timing(Duration::from_millis(10), Duration::from_secs(5));
    let (send, receive) = mpsc::channel();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            send.send(announcer.announce_with_live(
                &fixture.transport,
                &targets,
                &clip,
                60,
                &|_| 25,
                Some(&fixture.live),
            ))
            .unwrap();
        });
        fixture.wait_for_clip();
        for room in ["Alpha", "Beta"] {
            let host = fixture.sim.player(room).unwrap().ip;
            assert_eq!(soap::get_volume(&fixture.transport.lan, host).unwrap(), 25);
            assert!(!soap::get_mute(&fixture.transport.lan, host).unwrap());
        }
        let before = fixture.transport.reads.load(Ordering::Relaxed);
        // The fake clock stands still: repeated Get* calls cannot end the clip.
        assert!(matches!(
            receive.recv_timeout(Duration::from_millis(350)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        let during = fixture.transport.reads.load(Ordering::Relaxed) - before;
        assert!(
            during <= 4,
            "the watcher resumed rapid SOAP polling: {during} reads"
        );

        // This emits EOF without a Get* request to make simulated time settle.
        fixture.sim.advance(clip.duration);
        let report = receive
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(
            report.households[0].outcome,
            Ok(Ending::Finished),
            "{report:#?}"
        );
        assert!(report.households[0].is_clean(), "{report:#?}");
    });
    fixture.assert_restored();
}

#[test]
fn s1_callbacks_finish_without_rapid_reads_and_restore_every_room() {
    completes_from_events(false);
}

#[test]
fn s2_callbacks_finish_without_rapid_reads_and_restore_every_room() {
    completes_from_events(true);
}

#[test]
fn a_new_source_during_live_announcement_is_preserved_on_both_generations() {
    for s2 in [false, true] {
        let fixture = Fixture::new(s2);
        let houses = fixture.houses();
        let targets = [resolve_room(&houses, "Alpha").unwrap()];
        let clip = clip();
        let announcer = Announcer::with_timing(Duration::from_millis(10), Duration::from_secs(5));
        let (send, receive) = mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                send.send(announcer.announce_with_live(
                    &fixture.transport,
                    &targets,
                    &clip,
                    25,
                    &|_| 25,
                    Some(&fixture.live),
                ))
                .unwrap();
            });
            fixture.wait_for_clip();
            let after = fixture.sim.soap_log().len();
            let host = fixture.sim.player("Alpha").unwrap().ip;
            soap::set_av_transport_uri(&fixture.transport.lan, host, NEWER, "").unwrap();
            soap::play(&fixture.transport.lan, host).unwrap();
            let report = receive
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            assert!(
                report.households[0]
                    .outcome
                    .as_ref()
                    .is_err_and(|why| why.contains("superseded")),
                "{report:#?}"
            );
            assert!(!report.households[0].is_clean());
            assert_eq!(
                soap::get_media_info(&fixture.transport.lan, host)
                    .unwrap()
                    .uri,
                NEWER
            );
            assert_eq!(
                soap::get_transport_info(&fixture.transport.lan, host)
                    .unwrap()
                    .state,
                TransportState::Playing
            );
            assert!(
                !fixture.sim.soap_log()[after..].iter().any(|entry| {
                    entry.action == "Stop"
                        || (entry.action == "SetAVTransportURI"
                            && entry
                                .args
                                .iter()
                                .any(|(name, value)| name == "CurrentURI" && value == ALPHA))
                }),
                "cleanup changed the newer source"
            );
        });
    }
}
