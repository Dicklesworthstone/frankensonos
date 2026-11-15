//! A room's tone through `Surface`, against `fsonos-sim` players over real
//! sockets: read, changed part by part, logged, and put back by undo; a level
//! out of range or a change of nothing is refused before any player hears it.

use fsonos_api::ErrorCode;
use fsonos_api::surface::eq::EqRequest;
use fsonos_api::surface::{Surface, Survey};
use fsonos_core::clock::SystemClock;
use fsonos_core::actions::BeforeState;
use fsonos_core::control::{self, Tone, ToneChange};
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::{ActionFilter, MemStore};
use fsonos_core::{HouseholdState, resolve_room};
use fsonos_sim::{SimHandle, SimHousehold};
use std::time::Duration;

fn surface(sim: &SimHandle) -> Surface {
    let survey: Survey = Box::new(|t| {
        Ok(fsonos_core::inventory::survey(t, &[], Duration::from_millis(500))?.households)
    });
    Surface::new(
        Box::new(sim.lan()),
        survey,
        Policy::default(),
        Box::new(SystemClock),
    )
    .with_action_log(Box::new(MemStore::default()), "mcp")
}

fn house(sim: &SimHandle) -> Vec<HouseholdState> {
    fsonos_core::inventory::survey(&sim.lan(), &[], Duration::from_millis(500))
        .unwrap()
        .households
}

fn ask(bass: Option<i64>, treble: Option<i64>, loudness: Option<bool>) -> EqRequest {
    EqRequest {
        zone: "Kitchen".into(),
        bass,
        treble,
        loudness,
    }
}

#[test]
fn a_tone_change_reads_back_and_undo_puts_it_back() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let lan = sim.lan();
    let h = house(&sim);
    let kitchen = resolve_room(&h, "Kitchen").unwrap().player.id.clone();
    let tone = || control::tone(&lan, &h, &kitchen).unwrap();
    let flat = Tone {
        bass: 0,
        treble: 0,
        loudness: true,
    };
    assert_eq!(tone(), flat);

    let s = surface(&sim);
    let agent = Client::McpStdio;
    let read = s.get_eq(&agent, "kitchen").unwrap();
    assert_eq!(
        (read.room.as_str(), read.bass, read.treble, read.loudness),
        ("Kitchen", 0, 0, true)
    );

    // Only what is named changes.
    let set = s.set_eq(&agent, &ask(Some(4), None, Some(false))).unwrap();
    let changed = Tone {
        bass: 4,
        treble: 0,
        loudness: false,
    };
    assert_eq!(tone(), changed);
    assert_eq!((set.eq.bass, set.eq.treble, set.eq.loudness), (4, 0, false));
    assert_eq!(
        set.done,
        "Kitchen's tone is bass +4, treble 0, loudness off"
    );
    assert_eq!(s.get_eq(&agent, "Kitchen").unwrap(), set.eq);
    let log = s.recent_actions(&agent, &ActionFilter::default()).unwrap();
    assert_eq!(log[0].action.intent, "set_eq: Kitchen bass +4 loudness off");
    assert!(log[0].action.before_state.is_some(), "{log:?}");

    let undone = s.undo(&agent, true).unwrap().expect("the change to undo");
    assert_eq!(undone.tones, std::slice::from_ref(&kitchen), "{undone:?}");
    assert!(
        undone.summary.contains("the tone of Kitchen put back"),
        "{}",
        undone.summary
    );
    assert_eq!(tone(), flat);
    sim.shutdown();
}

#[test]
fn a_level_out_of_range_or_nothing_to_change_is_refused() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let s = surface(&sim);
    for wrong in [
        ask(Some(11), None, None),
        ask(None, Some(-11), None),
        ask(None, None, None),
    ] {
        let err = s.set_eq(&Client::Cli, &wrong).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgument, "{wrong:?}: {err:?}");
    }
    let unknown = s.get_eq(&Client::Cli, "Atticc").unwrap_err();
    assert_eq!(unknown.code, ErrorCode::UnknownRoom, "{unknown:?}");
    // Nothing was logged: a refused request never reached a player.
    let log = s
        .recent_actions(&Client::Cli, &ActionFilter::default())
        .unwrap();
    assert!(log.is_empty(), "{log:?}");
    sim.shutdown();
}

#[test]
fn bass_undo_preserves_later_treble_loudness_and_playback_changes() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let lan = sim.lan();
    let h = house(&sim);
    let target = resolve_room(&h, "Kitchen").unwrap();
    let player = target.player.id.clone();
    let coordinator = target.coordinator.id.clone();
    let s = surface(&sim);
    s.set_eq(&Client::McpStdio, &ask(Some(4), None, None))
        .unwrap();

    // Another controller changes unrelated tone fields and starts new music.
    control::set_tone(
        &lan,
        &h,
        &player,
        ToneChange {
            bass: None,
            treble: Some(-3),
            loudness: Some(false),
        },
    )
    .unwrap();
    control::play_uri(
        &lan,
        &h,
        &coordinator,
        "http://example.invalid/later.mp3",
        "",
    )
    .unwrap();
    control::set_volume(&lan, &h, &player, 29).unwrap();

    let result = s.undo(&Client::McpStdio, true).unwrap().unwrap();
    assert_eq!(result.tones, [player.clone()]);
    assert!(result.tone_failures.is_empty());
    assert_eq!(
        control::tone(&lan, &h, &player).unwrap(),
        Tone {
            bass: 0,
            treble: -3,
            loudness: false,
        }
    );
    assert_eq!(
        fsonos_proto::control::get_volume(&lan, target.player.ip).unwrap(),
        29
    );
    let media = fsonos_proto::control::get_media_info(&lan, target.coordinator.ip).unwrap();
    assert_eq!(media.uri, "http://example.invalid/later.mp3");
    assert_eq!(
        fsonos_proto::control::get_transport_info(&lan, target.coordinator.ip)
            .unwrap()
            .state,
        fsonos_types::TransportState::Playing
    );
    sim.shutdown();
}

#[test]
fn a_changed_touched_field_refuses_normal_undo_and_force_keeps_its_scope() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let lan = sim.lan();
    let h = house(&sim);
    let player = resolve_room(&h, "Kitchen").unwrap().player.id.clone();
    let s = surface(&sim);
    s.set_eq(&Client::Cli, &ask(Some(4), None, None)).unwrap();
    let later = Tone {
        bass: 7,
        treble: -3,
        loudness: false,
    };
    control::set_tone(&lan, &h, &player, ToneChange::all(later)).unwrap();
    let failure = s.undo(&Client::Cli, true).unwrap_err();
    assert_eq!(failure.code, ErrorCode::InvalidArgument);
    assert!(failure.detail.contains("bass"), "{failure:?}");
    assert_eq!(control::tone(&lan, &h, &player).unwrap(), later);

    s.undo_forced(&Client::Cli, true, true).unwrap().unwrap();
    assert_eq!(
        control::tone(&lan, &h, &player).unwrap(),
        Tone { bass: 0, ..later }
    );
    sim.shutdown();
}

#[test]
fn a_partial_tone_change_undo_never_touches_refused_or_unsent_fields() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let lan = sim.lan();
    let h = house(&sim);
    let player = resolve_room(&h, "Kitchen").unwrap().player.id.clone();
    let s = surface(&sim);
    sim.upnp_fault("Kitchen", "SetTreble", 501).unwrap();
    assert!(
        s.set_eq(&Client::McpStdio, &ask(Some(4), Some(3), Some(false)))
            .is_err()
    );
    sim.clear_faults("Kitchen").unwrap();
    let logs = s
        .recent_actions(&Client::McpStdio, &ActionFilter::default())
        .unwrap();
    let before = BeforeState::parse(logs[0].action.before_state.as_deref().unwrap()).unwrap();
    assert_eq!(
        before.tones[0].scope,
        Some(ToneChange {
            bass: Some(4),
            ..ToneChange::default()
        })
    );

    control::set_tone(
        &lan,
        &h,
        &player,
        ToneChange {
            bass: None,
            treble: Some(-5),
            loudness: Some(false),
        },
    )
    .unwrap();
    s.undo(&Client::McpStdio, true).unwrap().unwrap();
    assert_eq!(
        control::tone(&lan, &h, &player).unwrap(),
        Tone {
            bass: 0,
            treble: -5,
            loudness: false,
        }
    );
    sim.shutdown();
}

#[test]
fn tone_undo_reads_only_its_touched_field_and_refuses_an_unreadable_one() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let lan = sim.lan();
    let h = house(&sim);
    let player = resolve_room(&h, "Kitchen").unwrap().player.id.clone();
    let s = surface(&sim);
    s.set_eq(&Client::McpStdio, &ask(Some(4), None, None))
        .unwrap();
    sim.upnp_fault("Kitchen", "GetBass", 501).unwrap();
    assert_eq!(
        s.undo(&Client::McpStdio, true).unwrap_err().code,
        ErrorCode::InvalidArgument
    );
    sim.clear_faults("Kitchen").unwrap();
    for action in [
        "GetTreble",
        "GetLoudness",
        "GetPositionInfo",
        "GetMediaInfo",
        "GetTransportInfo",
    ] {
        sim.upnp_fault("Kitchen", action, 501).unwrap();
    }
    s.undo(&Client::McpStdio, true).unwrap().unwrap();
    sim.clear_faults("Kitchen").unwrap();
    assert_eq!(control::tone(&lan, &h, &player).unwrap().bass, 0);
    sim.shutdown();
}
