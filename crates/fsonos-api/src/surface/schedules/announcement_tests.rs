//! Announcement recovery must not put an old sleep timer onto newer playback.

use super::Sleep;
use crate::surface::announce::{AnnounceRequest, Announcements};
use crate::surface::{Surface, Survey};
use chrono::{DateTime, Utc};
use fsonos_core::announce::clip::MediaStore;
use fsonos_core::clock::FakeClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::sleep::SleepOutcome;
use fsonos_core::{HouseholdState, control, resolve_room};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

const NEW_SOURCE: &str = "x-rincon-mp3radio://stream.example.invalid/newer.mp3";

struct OwnerIntervenes {
    lan: SimLan,
    pause: bool,
    installed: AtomicBool,
    changed: AtomicBool,
}

impl Transport for OwnerIntervenes {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if self.installed.load(Ordering::Acquire)
            && action.contains("#GetTransportInfo")
            && !self.changed.swap(true, Ordering::AcqRel)
        {
            if self.pause {
                soap::pause(&self.lan, host)?;
            } else {
                soap::set_av_transport_uri(&self.lan, host, NEW_SOURCE, "")?;
                soap::play(&self.lan, host)?;
            }
        }
        let result = self.lan.soap_post(host, path, action, body);
        if result.is_ok() && action.contains("#SetAVTransportURI") && body.contains("/media/") {
            self.installed.store(true, Ordering::Release);
        }
        result
    }
}

#[test]
fn newer_playback_or_a_pause_during_an_announcement_does_not_rearm_sleep() {
    for s2 in [false, true] {
        for pause in [false, true] {
            let spec = SimPlayerSpec::new(
                "Kitchen",
                if s2 {
                    SimModel::One
                } else {
                    SimModel::Play5Gen1
                },
            );
            let builder = SimHousehold::builder();
            let sim = if s2 {
                builder.s2([spec])
            } else {
                builder.s1([spec])
            }
            .spawn()
            .unwrap();
            let lan = sim.lan();
            let host = sim.player("Kitchen").unwrap().ip;
            let mut house = HouseholdState::default();
            house.apply_topology(&get_zone_group_state(&lan, host).unwrap());
            let houses = vec![house];
            let player = resolve_room(&houses, "Kitchen").unwrap().player.id.clone();
            control::play_uri(
                &lan,
                &houses,
                &player,
                "x-rincon-mp3radio://stream.example.invalid/original.mp3",
                "",
            )
            .unwrap();
            soap::set_volume(&lan, host, 30).unwrap();
            let now = DateTime::parse_from_rfc3339("2026-10-07T22:00:00Z")
                .unwrap()
                .with_timezone(&Utc);
            let sleep = Sleep::default();
            sleep
                .timers
                .start(&lan, &houses, &sleep.fader, &player, Duration::ZERO, now)
                .unwrap();
            assert_eq!(sleep.timers.due(now).len(), 1);
            let survey: Survey = {
                let houses = houses.clone();
                Box::new(move |_| Ok(houses.clone()))
            };
            let clips = tempfile::tempdir().unwrap();
            let surface = Surface::new(
                Box::new(OwnerIntervenes {
                    lan: sim.lan(),
                    pause,
                    installed: AtomicBool::new(false),
                    changed: AtomicBool::new(false),
                }),
                survey,
                Policy::default(),
                Box::new(FakeClock::new(now.fixed_offset())),
            )
            .with_sleep(sleep)
            .with_announcements(Announcements::new(
                MediaStore::new(clips.path()),
                Box::new(|| Some("http://192.0.2.250:3400".into())),
            ));
            let report = surface
                .announce(
                    &Client::LoopbackHttp,
                    &AnnounceRequest {
                        chime: Some("beep".into()),
                        rooms: vec!["Kitchen".into()],
                        ..AnnounceRequest::default()
                    },
                )
                .unwrap();
            assert!(
                report
                    .households
                    .iter()
                    .any(|house| !house.not_restored.is_empty()),
                "{report:?}"
            );
            let sleep = surface.sleep.as_ref().unwrap();
            // No worker ran while the clip played. The old interrupted dispatch
            // must retire, instead of finding a new timer silently armed here.
            assert_eq!(
                sleep
                    .timers
                    .run(&lan, &houses, &sleep.fader, &player)
                    .unwrap(),
                SleepOutcome::Interrupted
            );
            assert!(sleep.timers.get(&player).is_none());
            assert_eq!(soap::get_remaining_sleep_timer(&lan, host).unwrap(), None);
            sim.clock().advance(Duration::from_mins(10));
            assert_eq!(
                soap::get_transport_info(&lan, host).unwrap().state,
                if pause {
                    TransportState::Paused
                } else {
                    TransportState::Playing
                }
            );
            if !pause {
                assert_eq!(soap::get_media_info(&lan, host).unwrap().uri, NEW_SOURCE);
            }
        }
    }
}

const WAIT: Duration = Duration::from_secs(5);

fn playing_house(s2: bool) -> (SimHandle, SimLan, Vec<HouseholdState>, PlayerId, IpAddr) {
    let spec = SimPlayerSpec::new(
        "Kitchen",
        if s2 {
            SimModel::One
        } else {
            SimModel::Play5Gen1
        },
    );
    let builder = SimHousehold::builder();
    let sim = if s2 {
        builder.s2([spec])
    } else {
        builder.s1([spec])
    }
    .spawn()
    .unwrap();
    let lan = sim.lan();
    let host = sim.player("Kitchen").unwrap().ip;
    let mut house = HouseholdState::default();
    house.apply_topology(&get_zone_group_state(&lan, host).unwrap());
    let houses = vec![house];
    let player = resolve_room(&houses, "Kitchen").unwrap().player.id.clone();
    control::play_uri(
        &lan,
        &houses,
        &player,
        "x-rincon-mp3radio://stream.example.invalid/original.mp3",
        "",
    )
    .unwrap();
    soap::set_volume(&lan, host, 30).unwrap();
    (sim, lan, houses, player, host)
}

/// Finish each synthetic clip after one observed Playing state, then hold the
/// first nonempty device backstop write. No arbitrary announcement timing is
/// needed to expose either continuation boundary.
struct CompletionState {
    clips: AtomicUsize,
    in_clip: AtomicBool,
    reads: AtomicUsize,
    waiting: Mutex<Option<(mpsc::SyncSender<()>, mpsc::Receiver<()>)>>,
}

impl CompletionState {
    fn gated() -> (Arc<Self>, mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (arrived, entered) = mpsc::sync_channel(1);
        let (release, waiting) = mpsc::channel();
        (
            Arc::new(Self {
                clips: AtomicUsize::new(0),
                in_clip: AtomicBool::new(false),
                reads: AtomicUsize::new(0),
                waiting: Mutex::new(Some((arrived, waiting))),
            }),
            entered,
            release,
        )
    }
}

struct CompleteClip {
    lan: SimLan,
    state: Arc<CompletionState>,
}

impl Transport for CompleteClip {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if action.contains("#ConfigureSleepTimer")
            && !body.contains("<NewSleepTimerDuration></NewSleepTimerDuration>")
        {
            let gate = self.state.waiting.lock().unwrap().take();
            if let Some((arrived, release)) = gate {
                arrived.send(()).unwrap();
                release.recv_timeout(WAIT).unwrap();
            }
        }
        if action.contains("#GetTransportInfo")
            && self.state.in_clip.load(Ordering::Acquire)
            && self.state.reads.fetch_add(1, Ordering::AcqRel) == 1
        {
            soap::stop(&self.lan, host)?;
        }
        let result = self.lan.soap_post(host, path, action, body);
        if result.is_ok() && action.contains("#SetAVTransportURI") {
            let clip = body.contains("/media/");
            self.state.in_clip.store(clip, Ordering::Release);
            if clip {
                self.state.reads.store(0, Ordering::Release);
                self.state.clips.fetch_add(1, Ordering::AcqRel);
            }
        }
        result
    }
}

fn gated_surface(
    lan: SimLan,
    houses: &[HouseholdState],
    sleep: Sleep,
    state: &Arc<CompletionState>,
    now: DateTime<Utc>,
) -> Surface {
    let survey: Survey = {
        let houses = houses.to_vec();
        Box::new(move |_| Ok(houses.clone()))
    };
    Surface::new(
        Box::new(CompleteClip {
            lan,
            state: Arc::clone(state),
        }),
        survey,
        Policy::default(),
        Box::new(FakeClock::new(now.fixed_offset())),
    )
    .with_sleep(sleep)
}

#[test]
fn a_new_play_command_during_sleep_rearming_supersedes_the_announcement_receipt() {
    for s2 in [false, true] {
        let (sim, lan, houses, player, host) = playing_house(s2);
        let now = DateTime::parse_from_rfc3339("2026-10-07T22:00:00Z")
            .unwrap()
            .to_utc();
        let sleep = Sleep::default();
        sleep
            .timers
            .start(&lan, &houses, &sleep.fader, &player, Duration::ZERO, now)
            .unwrap();
        assert_eq!(sleep.timers.due(now).len(), 1);
        let (state, entered, release) = CompletionState::gated();
        let surface = gated_surface(sim.lan(), &houses, sleep, &state, now);
        let room = houses[0]
            .rooms
            .iter()
            .find(|room| room.primary == player)
            .unwrap();
        let receipts: Vec<_> = surface.announcing(room).into_iter().collect();
        assert_eq!(receipts.len(), 1);
        let sleep = surface.sleep.as_ref().unwrap();
        assert_eq!(
            sleep
                .timers
                .run(&lan, &houses, &sleep.fader, &player)
                .unwrap(),
            SleepOutcome::Interrupted
        );
        let play = crate::PlayRequest {
            zone: "Kitchen".into(),
            source_uri: NEW_SOURCE.into(),
            title: None,
        };
        std::thread::scope(|scope| {
            let resume = scope.spawn(|| surface.sleep_again(&houses, &receipts));
            entered.recv_timeout(WAIT).unwrap();
            surface
                .control(&Client::LoopbackHttp, "play", |rooms| {
                    crate::plan::plan_play(rooms, &play)
                })
                .unwrap();
            release.send(()).unwrap();
            resume.join().unwrap();
        });
        assert!(sleep.timers.get(&player).is_none());
        assert_eq!(soap::get_remaining_sleep_timer(&lan, host).unwrap(), None);
        sim.clock().advance(Duration::from_mins(10));
        assert_eq!(soap::get_media_info(&lan, host).unwrap().uri, NEW_SOURCE);
        assert_eq!(
            soap::get_transport_info(&lan, host).unwrap().state,
            TransportState::Playing
        );

        // A regular timer belongs to the listener, rather than to a temporary
        // announcement continuation, and a normal source change keeps it.
        let ordinary = sleep
            .timers
            .start(
                &lan,
                &houses,
                &sleep.fader,
                &player,
                Duration::from_mins(20),
                now,
            )
            .unwrap();
        surface
            .control(&Client::LoopbackHttp, "play", |rooms| {
                crate::plan::plan_play(
                    rooms,
                    &crate::PlayRequest {
                        source_uri: "x-rincon-mp3radio://stream.example.invalid/another.mp3".into(),
                        ..play.clone()
                    },
                )
            })
            .unwrap();
        assert_eq!(sleep.timers.get(&player), Some(ordinary));
    }
}

#[test]
fn queued_announcements_wait_for_the_previous_households_sleep_recovery() {
    for s2 in [false, true] {
        let (sim, lan, houses, player, _) = playing_house(s2);
        let now = DateTime::parse_from_rfc3339("2026-10-07T22:00:00Z")
            .unwrap()
            .to_utc();
        let sleep = Sleep::default();
        sleep
            .timers
            .start(&lan, &houses, &sleep.fader, &player, Duration::ZERO, now)
            .unwrap();
        assert_eq!(sleep.timers.due(now).len(), 1);
        let (state, entered, release) = CompletionState::gated();
        let clips = tempfile::tempdir().unwrap();
        let surface = gated_surface(sim.lan(), &houses, sleep, &state, now).with_announcements(
            Announcements::new(
                MediaStore::new(clips.path()),
                Box::new(|| Some("http://192.0.2.250:3400".into())),
            ),
        );
        let request = AnnounceRequest {
            chime: Some("beep".into()),
            rooms: vec!["Kitchen".into()],
            ..AnnounceRequest::default()
        };
        let (first, second, queued, clips_while_recovering) = std::thread::scope(|scope| {
            let first = scope.spawn(|| surface.announce(&Client::LoopbackHttp, &request));
            entered.recv_timeout(WAIT).unwrap();
            let second = scope.spawn(|| surface.announce(&Client::LoopbackHttp, &request));
            let deadline = Instant::now() + WAIT;
            while surface.announcements_pending() < 2
                && state.clips.load(Ordering::Acquire) < 2
                && Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(1));
            }
            let queued = surface.announcements_pending() == 2;
            let during = state.clips.load(Ordering::Acquire);
            release.send(()).unwrap();
            (
                first.join().unwrap().unwrap(),
                second.join().unwrap().unwrap(),
                queued,
                during,
            )
        });
        assert!(
            queued,
            "the second clip must wait through the first continuation"
        );
        assert_eq!(clips_while_recovering, 1);
        assert_eq!(state.clips.load(Ordering::Acquire), 2);
        assert!(first.clean, "{first:?}");
        assert!(second.clean, "{second:?}");
        let sleep = surface.sleep.as_ref().unwrap();
        assert!(
            sleep.timers.get(&player).is_some(),
            "the second announcement resumes the sleep it interrupted"
        );
    }
}

#[test]
fn announcement_interruption_uses_the_room_coordinator_observed_after_regrouping() {
    for s2 in [false, true] {
        let model = if s2 {
            SimModel::One
        } else {
            SimModel::Play5Gen1
        };
        let players = [
            SimPlayerSpec::new("Kitchen", model),
            SimPlayerSpec::new("Office", model),
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
        let host = sim.player("Kitchen").unwrap().ip;
        let mut before = HouseholdState::default();
        before.apply_topology(&get_zone_group_state(&lan, host).unwrap());
        let before = vec![before];
        let kitchen = resolve_room(&before, "Kitchen").unwrap().player.id.clone();
        let office = resolve_room(&before, "Office").unwrap().player.id.clone();
        let now = DateTime::parse_from_rfc3339("2026-10-07T22:00:00Z")
            .unwrap()
            .to_utc();
        let sleep = Sleep::default();
        for player in [&kitchen, &office] {
            sleep
                .timers
                .start(&lan, &before, &sleep.fader, player, Duration::ZERO, now)
                .unwrap();
        }
        assert_eq!(sleep.timers.due(now).len(), 2);
        let survey: Survey = {
            let before = before.clone();
            Box::new(move |_| Ok(before.clone()))
        };
        let surface = Surface::new(
            Box::new(sim.lan()),
            survey,
            Policy::default(),
            Box::new(FakeClock::new(now.fixed_offset())),
        )
        .with_sleep(sleep);
        // The request's initial survey saw two groups. While it waited for
        // its announcement turn, Kitchen moved into Office's group.
        control::join(&lan, &before, &kitchen, &office).unwrap();
        let mut fresh = before[0].clone();
        fresh.apply_topology(&get_zone_group_state(&lan, host).unwrap());
        let room = fresh
            .rooms
            .iter()
            .find(|room| room.primary == kitchen)
            .unwrap();
        assert_eq!(room.coordinator, office);
        let receipt = surface.announcing(room).unwrap();
        assert_eq!(receipt.coordinator(), &office);
        let sleep = surface.sleep.as_ref().unwrap();
        assert!(
            sleep
                .timers
                .interrupt_for_announcement(&sleep.fader, &kitchen)
                .is_some(),
            "the obsolete coordinator's timer was not interrupted by this announcement"
        );
    }
}
