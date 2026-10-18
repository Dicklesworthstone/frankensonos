//! Scene fades synthesize volume writes for newly audible rooms, even when
//! the scene has no explicit volume setting. Those writes must obey the
//! caller's and saved writer's permissions, just like the scene's own steps.

use fsonos_api::surface::scenes::{SceneApplyRequest, SceneDefinition};
use fsonos_api::{NoteCode, Surface};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::MemStore;
use fsonos_core::{HouseholdState, control, inventory, resolve_room};
use fsonos_proto::soap;
use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use fsonos_types::TransportState;
use std::time::Duration;

const BEFORE: &str = "x-rincon-mp3radio://stream.example.invalid/before.mp3";
const SCENE: &str = "x-rincon-mp3radio://stream.example.invalid/scene.mp3";
const LEVEL: u8 = 30;

fn fixture() -> (SimHandle, Vec<HouseholdState>) {
    let sim = SimHousehold::builder()
        .s2([SimPlayerSpec::new("Kitchen", SimModel::One)])
        .spawn()
        .unwrap();
    let houses = inventory::survey(&sim.lan(), &[], Duration::from_millis(500))
        .unwrap()
        .households;
    (sim, houses)
}

fn surface(sim: &SimHandle, houses: &[HouseholdState], policy: Policy) -> Surface {
    let known = houses.to_vec();
    Surface::new(
        Box::new(sim.lan()),
        Box::new(move |_| Ok(known.clone())),
        policy,
        Box::new(SystemClock),
    )
    .with_action_log(Box::new(MemStore::default()), "mcp")
}

fn prepare(sim: &SimHandle, houses: &[HouseholdState], unmute: bool) {
    let target = resolve_room(houses, "Kitchen").unwrap();
    let player = &target.player.id;
    let lan = sim.lan();
    control::play_uri(&lan, houses, player, BEFORE, "").unwrap();
    if !unmute {
        control::stop(&lan, houses, player).unwrap();
    }
    // Below the house cap: any volume write would come from the scene's
    // optional fade, not the mandatory lowering of an over-cap room.
    control::set_volume(&lan, houses, player, LEVEL).unwrap();
    control::set_mute(&lan, houses, player, unmute).unwrap();
}

fn definition(unmute: bool) -> SceneDefinition {
    serde_json::from_value(if unmute {
        serde_json::json!({"mutes": {"Kitchen": false}})
    } else {
        serde_json::json!({
            "groups": [{
                "coordinator": "Kitchen",
                "source": {"kind": "uri", "uri": SCENE}
            }]
        })
    })
    .unwrap()
}

fn assert_applied(sim: &SimHandle, houses: &[HouseholdState], unmute: bool) {
    let target = resolve_room(houses, "Kitchen").unwrap();
    let lan = sim.lan();
    assert_eq!(
        control::volume(&lan, houses, &target.player.id).unwrap(),
        LEVEL
    );
    assert!(!soap::get_mute(&lan, target.player.ip).unwrap());
    let playing = control::playback(&lan, houses, &target.coordinator.id).unwrap();
    assert_eq!(playing.transport.state, TransportState::Playing);
    assert_eq!(playing.position.uri, if unmute { BEFORE } else { SCENE });
}

#[test]
fn implicit_scene_fades_obey_the_callers_and_saved_writers_denials() {
    let (sim, houses) = fixture();
    let restricted = Client::Tailnet("tag:agent".into());
    let cli = Client::Cli;
    for denied_tool in ["set_volume", "fade_volume", "volume"] {
        for denied_writer in [false, true] {
            for unmute in [false, true] {
                // The volume class also denies mute itself; these cases
                // exercise an allowed source change instead.
                if unmute && denied_tool == "volume" {
                    continue;
                }
                prepare(&sim, &houses, unmute);
                let policy = Policy::from_toml(&format!(
                    "[clients.\"tag:agent\"]\ndeny = [\"{denied_tool}\"]\n"
                ))
                .unwrap();
                let s = surface(&sim, &houses, policy);
                let (caller, writer) = if denied_writer {
                    (&cli, &restricted)
                } else {
                    (&restricted, &cli)
                };
                s.save_scene_from(writer, "Evening", &definition(unmute))
                    .unwrap();

                let before = sim.soap_log().len();
                let applied = s
                    .apply_scene_with(caller, "Evening", &SceneApplyRequest { fade_secs: Some(1) })
                    .unwrap();
                assert!(applied.complete && applied.changed, "{applied:?}");
                let volume_writes: Vec<_> = sim
                    .soap_log()
                    .into_iter()
                    .skip(before)
                    .filter(|entry| entry.action == "SetVolume")
                    .collect();
                assert!(
                    volume_writes.is_empty(),
                    "denied {denied_tool}, writer={denied_writer}, unmute={unmute}: {volume_writes:?}"
                );
                assert!(
                    applied.notes.iter().any(|note| {
                        note.code == NoteCode::Skipped
                            && note.detail.starts_with("scene fade omitted: ")
                            && note.detail.contains(restricted.key())
                            && note.detail.contains("fade_volume")
                    }),
                    "{applied:?}"
                );
                assert!(!applied.done.contains("faded over"), "{applied:?}");
                assert_applied(&sim, &houses, unmute);
            }
        }
    }
    sim.shutdown();
}

#[test]
fn allowed_scene_fades_still_ramp_implicit_source_and_unmute_volumes() {
    let (sim, houses) = fixture();
    let s = surface(&sim, &houses, Policy::default());
    let agent = Client::McpStdio;
    for unmute in [false, true] {
        prepare(&sim, &houses, unmute);
        s.save_scene_from(&agent, "Evening", &definition(unmute))
            .unwrap();

        let before = sim.soap_log().len();
        let applied = s
            .apply_scene_with(&agent, "Evening", &SceneApplyRequest { fade_secs: Some(1) })
            .unwrap();
        assert!(applied.complete && applied.changed, "{applied:?}");
        assert!(applied.notes.is_empty(), "{applied:?}");
        assert!(applied.done.contains("faded over 1s"), "{applied:?}");
        let levels: Vec<u8> = sim
            .soap_log()
            .into_iter()
            .skip(before)
            .filter(|entry| entry.action == "SetVolume")
            .map(|entry| {
                entry
                    .args
                    .iter()
                    .find(|(name, _)| name == "DesiredVolume")
                    .unwrap()
                    .1
                    .parse()
                    .unwrap()
            })
            .collect();
        assert_eq!(levels.first(), Some(&0), "{levels:?}");
        assert_eq!(levels.last(), Some(&LEVEL), "{levels:?}");
        assert!(
            levels.iter().any(|level| *level > 0 && *level < LEVEL),
            "{levels:?}"
        );
        assert_applied(&sim, &houses, unmute);
    }
    sim.shutdown();
}
