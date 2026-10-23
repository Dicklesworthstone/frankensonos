//! A pasted Spotify source the renderer cannot play must fail while planning,
//! before action snapshots or volume caps can disturb the current music.
//! The shared Surface is exercised against real S1 and S2 simulator sockets.

use fsonos_api::plan::plan_play;
use fsonos_api::surface::schedules::Sleep;
use fsonos_api::{ErrorCode, PlayRequest, Surface};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::{ActionFilter, MemStore};
use fsonos_core::{HouseholdState, control, inventory, resolve_room};
use fsonos_proto::content::{QUEUE, browse_all};
use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use fsonos_types::TransportState;
use std::time::Duration;

const ID: &str = "0123456789ABCDEFabcdef";
const CAP: u8 = 35;

fn sim() -> SimHandle {
    SimHousehold::builder()
        .s1([SimPlayerSpec::new("Den", SimModel::Play5Gen1)])
        .s2([SimPlayerSpec::new("Den", SimModel::One)])
        .spawn()
        .unwrap()
}

fn households(sim: &SimHandle) -> Vec<HouseholdState> {
    inventory::survey(&sim.lan(), &[], Duration::from_millis(500))
        .unwrap()
        .households
}

fn surface(sim: &SimHandle, houses: &[HouseholdState]) -> Surface {
    // Discovery is already complete: any subsequent speaker read belongs to
    // playback, cap enforcement or an action snapshot, not this fixture.
    let known = houses.to_vec();
    let mut policy = Policy::default();
    policy.defaults.max_volume = CAP;
    Surface::new(
        Box::new(sim.lan()),
        Box::new(move |_| Ok(known.clone())),
        policy,
        Box::new(SystemClock),
    )
    .with_action_log(Box::new(MemStore::default()), "mcp")
    .with_sleep(Sleep::default())
}

fn request(zone: &str, source_uri: String) -> PlayRequest {
    PlayRequest {
        zone: zone.into(),
        source_uri,
        title: None,
    }
}

fn queue(sim: &SimHandle, houses: &[HouseholdState], zone: &str) -> Vec<String> {
    let target = resolve_room(houses, zone).unwrap();
    browse_all(&sim.lan(), target.coordinator.ip, QUEUE)
        .unwrap()
        .into_iter()
        .map(|item| item.res.unwrap().uri)
        .collect()
}

#[test]
fn unsupported_sources_do_not_snapshot_or_change_an_over_cap_house() {
    let sim = sim();
    let houses = households(&sim);
    let s = surface(&sim, &houses);
    for zone in ["Den@S1", "Den@S2"] {
        let target = resolve_room(&houses, zone).unwrap();
        let first = control::queue_spotify_tracks(
            &sim.lan(),
            &houses,
            &target.coordinator.id,
            &[
                ("spotify:track:0123456789ABCDEFabcdef", "Original first"),
                ("spotify:track:0123456789ABCDEFabcdeg", "Original second"),
            ],
        )
        .unwrap()
        .expect("the simulator has a Spotify favorite to learn from");
        control::play_queue_from(&sim.lan(), &houses, &target.coordinator.id, first).unwrap();
        control::set_volume(&sim.lan(), &houses, &target.player.id, 90).unwrap();
        let before = control::playback(&sim.lan(), &houses, &target.coordinator.id).unwrap();
        let original_queue = queue(&sim, &houses, zone);

        // Tracks, albums, playlists, artists, episodes and shows all play
        // (b3ac8880); an audiobook, pasted either way, is a kind none of the
        // playback paths can render.
        for source_uri in [
            format!("spotify:audiobook:{ID}"),
            format!("https://open.spotify.com/intl-en/audiobook/{ID}?si=pasted"),
        ] {
            let req = request(zone, source_uri);
            let calls_before = sim.soap_log().len();
            let failure = s
                .control(&Client::McpStdio, "play", |rooms| plan_play(rooms, &req))
                .unwrap_err();
            assert_eq!(failure.code, ErrorCode::InvalidArgument, "{req:?}");
            assert_eq!(failure.status(), 422);
            assert!(
                failure
                    .detail
                    .contains("unsupported Spotify kind \"audiobook\""),
                "{failure:?}"
            );
            assert_eq!(
                sim.soap_log().len(),
                calls_before,
                "rejected {req:?} performed speaker I/O before failing"
            );
        }

        assert_eq!(
            control::volume(&sim.lan(), &houses, &target.player.id).unwrap(),
            90,
            "a rejected source must not lower the current music to the caller's cap"
        );
        let after = control::playback(&sim.lan(), &houses, &target.coordinator.id).unwrap();
        assert_eq!(after.transport.state, before.transport.state);
        assert_eq!(after.position.uri, before.position.uri);
        assert_eq!(queue(&sim, &houses, zone), original_queue);
    }
    assert!(
        s.recent_actions(&Client::Cli, &ActionFilter::default())
            .unwrap()
            .is_empty(),
        "preflight failures have no playback action or undo snapshot"
    );
    sim.shutdown();
}

#[test]
fn tracks_albums_playlists_and_streams_still_play_with_caps() {
    let sim = sim();
    let houses = households(&sim);
    let s = surface(&sim, &houses);
    for zone in ["Den@S1", "Den@S2"] {
        let target = resolve_room(&houses, zone).unwrap();
        for source_uri in [
            format!("spotify:track:{ID}"),
            format!("https://open.spotify.com/album/{ID}?si=pasted"),
            format!("spotify:user:owner:playlist:{ID}"),
            "x-rincon-mp3radio://stream.example.invalid/current.mp3".into(),
        ] {
            control::set_volume(&sim.lan(), &houses, &target.player.id, 90).unwrap();
            let req = request(zone, source_uri);
            let outcome = s
                .control(&Client::McpStdio, "play", |rooms| plan_play(rooms, &req))
                .unwrap_or_else(|error| panic!("{req:?}: {error}"));
            assert!(outcome.changed);
            assert_eq!(
                control::volume(&sim.lan(), &houses, &target.player.id).unwrap(),
                CAP,
                "{req:?}"
            );
            let now = control::playback(&sim.lan(), &houses, &target.coordinator.id).unwrap();
            assert_eq!(now.transport.state, TransportState::Playing, "{req:?}");
        }
    }
    sim.shutdown();
}
