//! Level undo restores the intended rooms over real S1/S2 sockets without
//! rewinding music or undoing an independent controller's other settings.

use fsonos_api::plan::{plan_mute, plan_mute_set, plan_volume, plan_volume_set};
use fsonos_api::surface::schedules::{FadeVolumeRequest, Sleep};
use fsonos_api::{MuteRequest, Surface, VolumeRequest};
use fsonos_core::HouseholdState;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::{ActionFilter, MemStore};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::PlayerId;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

struct MediaUnavailableAfterVolume {
    lan: SimLan,
    changed: Arc<AtomicBool>,
}

impl Transport for MediaUnavailableAfterVolume {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if self.changed.load(Ordering::Acquire) && action.ends_with("#GetMediaInfo\"") {
            return Err(ProtoError::Network {
                target: "virtual source reader".into(),
                detail: "source reads unavailable after the volume command".into(),
            });
        }
        let result = self.lan.soap_post(host, path, action, body);
        if action.ends_with("#SetVolume\"") {
            self.changed.store(true, Ordering::Release);
        }
        result
    }
}

fn setup(generation: u8, policy: Policy, fail_media: bool) -> (SimHandle, Surface) {
    let model = if generation == 1 {
        SimModel::Play5Gen1
    } else {
        SimModel::One
    };
    let players = [
        SimPlayerSpec::new("Alpha", model),
        SimPlayerSpec::new("Beta", model),
    ];
    let builder = SimHousehold::builder();
    let sim = if generation == 1 {
        builder.s1(players)
    } else {
        builder.s2(players)
    }
    .spawn()
    .unwrap();
    let lan = sim.lan();
    let host = sim.player("Alpha").unwrap().ip;
    let other = sim.player("Beta").unwrap().ip;
    let coordinator = PlayerId(sim.player("Alpha").unwrap().uuid.clone());
    for name in ["first", "second", "third"] {
        soap::enqueue_uri(
            &lan,
            host,
            &format!("x-file-cifs://example.invalid/{name}.flac"),
            "",
            0,
            false,
        )
        .unwrap();
    }
    soap::set_av_transport_uri(&lan, other, &format!("x-rincon:{}", coordinator.0), "").unwrap();
    soap::play_from_queue(&lan, host, &coordinator).unwrap();
    soap::seek_track(&lan, host, 1).unwrap();
    soap::play(&lan, host).unwrap();
    soap::set_volume(&lan, host, 18).unwrap();
    soap::set_volume(&lan, other, 32).unwrap();
    sim.advance(Duration::from_secs(17));
    let transport: Box<dyn Transport + Send + Sync> = if fail_media {
        Box::new(MediaUnavailableAfterVolume {
            lan,
            changed: Arc::new(AtomicBool::new(false)),
        })
    } else {
        Box::new(lan)
    };
    let surface = Surface::new(
        transport,
        Box::new(move |transport| {
            let mut household = HouseholdState::default();
            household.apply_topology(
                &get_zone_group_state(transport, host).map_err(fsonos_core::CoreError::from)?,
            );
            Ok(vec![household])
        }),
        policy,
        Box::new(SystemClock),
    )
    .with_action_log(Box::new(MemStore::default()), "test");
    (sim, surface)
}

fn volume(zone: &str, value: i64, group: bool) -> VolumeRequest {
    VolumeRequest {
        zone: zone.into(),
        volume: Some(value),
        delta: None,
        group,
    }
}

fn writes(sim: &SimHandle, since: usize) -> Vec<(String, String)> {
    sim.soap_log()
        .iter()
        .skip(since)
        .filter(|entry| {
            entry.action.starts_with("Set")
                || matches!(
                    entry.action.as_str(),
                    "Play"
                        | "Pause"
                        | "Stop"
                        | "Seek"
                        | "Next"
                        | "Previous"
                        | "BecomeCoordinatorOfStandaloneGroup"
                        | "AddURIToQueue"
                        | "RemoveAllTracksFromQueue"
                        | "RemoveTrackRangeFromQueue"
                )
        })
        .map(|entry| (entry.room.clone(), entry.action.clone()))
        .collect()
}

#[test]
fn room_volume_undo_preserves_a_later_track_pause_mute_and_peer_level() {
    for generation in [1, 2] {
        let (sim, surface) = setup(generation, Policy::default(), false);
        let lan = sim.lan();
        let lead = sim.player("Alpha").unwrap().ip;
        let other = sim.player("Beta").unwrap().ip;
        surface
            .control(&Client::Cli, "set_volume", |rooms| {
                plan_volume(rooms, &volume("Beta", 25, false))
            })
            .unwrap();
        soap::next(&lan, lead).unwrap();
        soap::seek_position(&lan, lead, 42).unwrap();
        soap::pause(&lan, lead).unwrap();
        soap::set_volume(&lan, lead, 27).unwrap();
        soap::set_mute(&lan, other, true).unwrap();
        let position = soap::get_position_info(&lan, lead).unwrap();
        let transport = soap::get_transport_info(&lan, lead).unwrap();
        let since = sim.soap_log().len();
        let undone = surface.undo(&Client::Cli, true).unwrap().unwrap();
        assert!(undone.failures.is_empty(), "{undone:?}");
        assert_eq!(writes(&sim, since), [("Beta".into(), "SetVolume".into())]);
        assert_eq!(soap::get_position_info(&lan, lead).unwrap(), position);
        assert_eq!(soap::get_transport_info(&lan, lead).unwrap(), transport);
        assert_eq!(soap::get_volume(&lan, other).unwrap(), 32);
        assert_eq!(soap::get_volume(&lan, lead).unwrap(), 27);
        assert!(soap::get_mute(&lan, other).unwrap());
        sim.shutdown();
    }
}

#[test]
fn mute_undo_preserves_new_music_and_both_rooms_independent_volumes() {
    for generation in [1, 2] {
        let (sim, surface) = setup(generation, Policy::default(), false);
        let lan = sim.lan();
        let lead = sim.player("Alpha").unwrap().ip;
        let other = sim.player("Beta").unwrap().ip;
        surface
            .control(&Client::Cli, "mute", |rooms| {
                plan_mute(
                    rooms,
                    &MuteRequest {
                        zone: "Beta".into(),
                        mute: true,
                    },
                )
            })
            .unwrap();
        soap::set_volume(&lan, other, 23).unwrap();
        soap::set_volume(&lan, lead, 24).unwrap();
        let source = "x-rincon-mp3radio://example.invalid/later.mp3";
        soap::set_av_transport_uri(&lan, lead, source, "").unwrap();
        soap::play(&lan, lead).unwrap();
        let since = sim.soap_log().len();
        surface.undo(&Client::Cli, true).unwrap().unwrap();
        assert_eq!(writes(&sim, since), [("Beta".into(), "SetMute".into())]);
        assert!(!soap::get_mute(&lan, other).unwrap());
        assert_eq!(soap::get_volume(&lan, other).unwrap(), 23);
        assert_eq!(soap::get_volume(&lan, lead).unwrap(), 24);
        assert_eq!(soap::get_media_info(&lan, lead).unwrap().uri, source);
        sim.shutdown();
    }
}

#[test]
fn group_volume_and_room_set_undo_restore_each_original_level_without_transport_writes() {
    for generation in [1, 2] {
        for group in [false, true] {
            let (sim, surface) = setup(generation, Policy::default(), false);
            let req = volume(if group { "Beta" } else { "all" }, 28, group);
            surface
                .control_set(&Client::Cli, "set_volume", |rooms| {
                    plan_volume_set(rooms, &req)
                })
                .unwrap();
            let lan = sim.lan();
            let lead = sim.player("Alpha").unwrap().ip;
            soap::next(&lan, lead).unwrap();
            let since = sim.soap_log().len();
            surface.undo(&Client::Cli, true).unwrap().unwrap();
            let mut changed = writes(&sim, since);
            changed.sort();
            assert_eq!(
                changed,
                [
                    ("Alpha".into(), "SetVolume".into()),
                    ("Beta".into(), "SetVolume".into())
                ]
            );
            assert_eq!(soap::get_volume(&lan, lead).unwrap(), 18);
            assert_eq!(
                soap::get_volume(&lan, sim.player("Beta").unwrap().ip).unwrap(),
                32
            );
            sim.shutdown();
        }
    }
}

#[test]
fn volume_undo_keeps_its_proof_when_source_reads_fail_after_the_command() {
    for generation in [1, 2] {
        let (sim, surface) = setup(generation, Policy::default(), true);
        surface
            .control(&Client::Cli, "set_volume", |rooms| {
                plan_volume(rooms, &volume("Beta", 25, false))
            })
            .unwrap();
        let since = sim.soap_log().len();
        surface.undo(&Client::Cli, true).unwrap().unwrap();
        assert_eq!(writes(&sim, since), [("Beta".into(), "SetVolume".into())]);
        assert_eq!(
            soap::get_volume(&sim.lan(), sim.player("Beta").unwrap().ip).unwrap(),
            32
        );
        sim.shutdown();
    }
}

#[test]
fn unmute_undo_includes_its_cap_lowering_and_keeps_current_policy() {
    let policy = Policy::from_toml("[defaults]\nmax_volume = 25\nmax_step = 100\n").unwrap();
    let (sim, surface) = setup(2, policy, false);
    let other = sim.player("Beta").unwrap().ip;
    soap::set_mute(&sim.lan(), other, true).unwrap();
    surface
        .control_set(&Client::McpStdio, "mute", |rooms| {
            plan_mute_set(
                rooms,
                &MuteRequest {
                    zone: "Beta".into(),
                    mute: false,
                },
            )
        })
        .unwrap();
    assert_eq!(soap::get_volume(&sim.lan(), other).unwrap(), 25);
    let logged = surface
        .recent_actions(&Client::McpStdio, &ActionFilter::default())
        .unwrap();
    let before =
        fsonos_core::actions::BeforeState::parse(logged[0].action.before_state.as_deref().unwrap())
            .unwrap();
    let scope = before.level_scope.unwrap();
    assert_eq!(scope.len(), 1);
    assert!(scope[0].volume && scope[0].mute);
    let since = sim.soap_log().len();
    let undone = surface.undo(&Client::McpStdio, true).unwrap().unwrap();
    assert_eq!(writes(&sim, since), [("Beta".into(), "SetMute".into())]);
    assert!(
        undone.zones.iter().any(|(_, report)| report
            .skipped
            .iter()
            .any(|(aspect, _)| *aspect == fsonos_core::snapshot::Aspect::Volume)),
        "raising an already capped room is refused: {undone:?}"
    );
    assert!(soap::get_mute(&sim.lan(), other).unwrap());
    assert_eq!(
        soap::get_volume(&sim.lan(), other).unwrap(),
        25,
        "undo respects the current cap"
    );
    sim.shutdown();
}

#[test]
fn fade_undo_and_reviewed_undo_preserve_the_current_queue_position() {
    for generation in [1, 2] {
        for fade_secs in [0, 1] {
            let (sim, surface) = setup(generation, Policy::default(), false);
            surface
                .fade_volume_to(
                    &Client::Cli,
                    &FadeVolumeRequest {
                        zone: "Beta".into(),
                        volume: 20,
                        fade_secs,
                    },
                )
                .unwrap();
            let lead = sim.player("Alpha").unwrap().ip;
            soap::next(&sim.lan(), lead).unwrap();
            let position = soap::get_position_info(&sim.lan(), lead).unwrap();
            let target = surface.preview_undo(&Client::Cli).unwrap().unwrap();
            let since = sim.soap_log().len();
            surface.undo_checked(&Client::Cli, target.id).unwrap();
            assert_eq!(writes(&sim, since), [("Beta".into(), "SetVolume".into())]);
            assert_eq!(soap::get_position_info(&sim.lan(), lead).unwrap(), position);
            assert_eq!(
                soap::get_volume(&sim.lan(), sim.player("Beta").unwrap().ip).unwrap(),
                32
            );
            sim.shutdown();
        }
    }
}

#[test]
fn a_preflight_failed_room_does_not_block_undo_of_the_successful_room() {
    for generation in [1, 2] {
        let (sim, surface) = setup(generation, Policy::default(), false);
        let lan = sim.lan();
        let other = sim.player("Beta").unwrap();
        soap::leave_group(&lan, other.ip, &PlayerId(other.uuid.clone())).unwrap();
        sim.upnp_fault("Alpha", "GetVolume", 701).unwrap();
        let outcome = surface
            .control_set(&Client::McpStdio, "set_volume", |rooms| {
                plan_volume_set(rooms, &volume("all", 25, false))
            })
            .unwrap();
        assert!(outcome.changed);
        assert!(
            outcome
                .notes
                .iter()
                .any(|note| note.code == fsonos_api::NoteCode::Skipped)
        );
        assert_eq!(soap::get_volume(&lan, other.ip).unwrap(), 25);
        let log = surface
            .recent_actions(&Client::McpStdio, &ActionFilter::default())
            .unwrap();
        assert!(
            log[0]
                .action
                .result
                .contains("before-state missing for 1 zone")
        );
        let saved = fsonos_core::actions::BeforeState::parse(
            log[0].action.before_state.as_deref().unwrap(),
        )
        .unwrap();
        let scope = saved.level_scope.unwrap();
        assert_eq!(scope.len(), 1);
        assert_eq!(scope[0].player, PlayerId(other.uuid.clone()));
        assert!(scope[0].volume && !scope[0].mute);
        let since = sim.soap_log().len();
        surface.undo(&Client::McpStdio, true).unwrap().unwrap();
        assert_eq!(writes(&sim, since), [("Beta".into(), "SetVolume".into())]);
        assert_eq!(soap::get_volume(&lan, other.ip).unwrap(), 32);

        // No captured state for a failed single-room request does not create
        // an empty scope that could masquerade as a successful restoration.
        assert!(
            surface
                .control(&Client::McpStdio, "set_volume", |rooms| {
                    plan_volume(rooms, &volume("Alpha", 25, false))
                })
                .is_err()
        );
        let log = surface
            .recent_actions(&Client::McpStdio, &ActionFilter::default())
            .unwrap();
        assert!(log[0].action.before_state.is_none());
        sim.shutdown();
    }
}

#[test]
fn a_failed_multi_room_fade_cannot_undo_a_newer_manual_level() {
    let (sim, surface) = setup(2, Policy::default(), false);
    let surface = surface.with_sleep(Sleep::default());
    let lan = sim.lan();
    let other = sim.player("Beta").unwrap();
    soap::leave_group(&lan, other.ip, &PlayerId(other.uuid.clone())).unwrap();
    let started = sim.soap_log().len();
    std::thread::scope(|threads| {
        let fading = threads.spawn(|| {
            surface.fade_volume_to(
                &Client::Cli,
                &FadeVolumeRequest {
                    zone: "all".into(),
                    volume: 60,
                    fade_secs: 4,
                },
            )
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let calls = writes(&sim, started);
            if ["Alpha", "Beta"].iter().all(|name| {
                calls
                    .iter()
                    .any(|(room, action)| room == name && action == "SetVolume")
            }) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "both fades must begin before the manual takeover"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        surface
            .control(&Client::Cli, "set_volume", |rooms| {
                plan_volume(rooms, &volume("Alpha", 11, false))
            })
            .unwrap();
        sim.upnp_fault("Beta", "SetVolume", 701).unwrap();
        assert!(fading.join().unwrap().is_err());
    });
    let lead = sim.player("Alpha").unwrap().ip;
    assert_eq!(soap::get_volume(&lan, lead).unwrap(), 11);
    let log = surface
        .recent_actions(&Client::Cli, &ActionFilter::default())
        .unwrap();
    let saved =
        fsonos_core::actions::BeforeState::parse(log[0].action.before_state.as_deref().unwrap())
            .unwrap();
    assert!(
        saved
            .not_undoable
            .as_deref()
            .is_some_and(|reason| reason.contains("failed volume fade"))
    );
    let since = sim.soap_log().len();
    for force in [false, true] {
        assert!(surface.undo_forced(&Client::Cli, true, force).is_err());
    }
    assert!(writes(&sim, since).is_empty());
    assert_eq!(soap::get_volume(&lan, lead).unwrap(), 11);
    sim.shutdown();
}
