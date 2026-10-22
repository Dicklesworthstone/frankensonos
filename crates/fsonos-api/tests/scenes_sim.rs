//! Scenes through `Surface`, against `fsonos-sim` players over real sockets:
//! save the house, change it, apply the scene (only the steps the house
//! needs, volumes within the caller's caps), undo the apply, and the
//! failures every surface reports (UNKNOWN_SCENE, CROSS_HOUSEHOLD_GROUP,
//! INVALID_ARGUMENT, POLICY_DENIED, and no store).

use fsonos_api::dj::{
    DjEngine, DjMoodDto, DjMoodsDto, DjSpeakers, DjStatusDto, DjSteer, DjSteeringDto,
    SteerConstraints,
};
use fsonos_api::plan::{
    DjAction, TransportAction, plan_group, plan_mute, plan_play, plan_transport, plan_ungroup,
    plan_volume,
};
use fsonos_api::surface::scenes::{SceneApplyRequest, SceneDefinition};
use fsonos_api::surface::schedules::FadeVolumeRequest;
use fsonos_api::surface::{Surface, Survey};
use fsonos_api::{
    ErrorCode, Failure, GroupRequest, MuteRequest, NoteCode, OutcomeDto, PlayRequest,
    VolumeRequest, ZoneRequest,
};
use fsonos_core::clock::{Clock, SystemClock};
use fsonos_core::playback::PlayerPlayback;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::scenes::{self, Scene, SceneGroup, SceneSource};
use fsonos_core::store::{ActionFilter, DjSession, MemStore, Store, StoredScene};
use fsonos_core::{HouseholdState, control, resolve_room};
use fsonos_sim::{SimHandle, SimHousehold};
use fsonos_types::{PlayerId, TransportState};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

const STREAM: &str = "x-rincon-mp3radio://stream.example.invalid/evening.mp3";

fn survey() -> Survey {
    Box::new(|t| Ok(fsonos_core::inventory::survey(t, &[], Duration::from_millis(500))?.households))
}

fn surface(sim: &SimHandle, store: MemStore) -> Surface {
    Surface::new(
        Box::new(sim.lan()),
        survey(),
        Policy::default(),
        Box::new(SystemClock),
    )
    .with_action_log(Box::new(store), "mcp")
}

fn agent() -> Client {
    Client::Tailnet("tag:agent".into())
}

/// The households as they are right now (the surface caches its survey).
fn house(sim: &SimHandle) -> Vec<HouseholdState> {
    fsonos_core::inventory::survey(&sim.lan(), &[], Duration::from_millis(500))
        .unwrap()
        .households
}

fn id(houses: &[HouseholdState], room: &str) -> PlayerId {
    resolve_room(houses, room).unwrap().player.id.clone()
}

/// Kitchen's volume, whether Office plays in Kitchen's group, and what
/// Kitchen's group is doing and on.
fn kitchen(sim: &SimHandle) -> (u8, bool, TransportState, String) {
    let lan = sim.lan();
    let houses = house(sim);
    let (kitchen, office) = (id(&houses, "Kitchen"), id(&houses, "Office"));
    let together = houses
        .iter()
        .any(|h| h.coordinator_of(&office) == Some(&kitchen));
    let playback = control::playback(&lan, &houses, &kitchen).unwrap();
    (
        control::volume(&lan, &houses, &kitchen).unwrap(),
        together,
        playback.transport.state,
        playback.position.uri,
    )
}

fn volume(s: &Surface, client: &Client, zone: &str, level: i64) {
    let req = VolumeRequest {
        zone: zone.into(),
        volume: Some(level),
        delta: None,
        group: false,
    };
    s.control(client, "set_volume", |h| plan_volume(h, &req))
        .unwrap();
}

fn zone(name: &str) -> ZoneRequest {
    ZoneRequest { zone: name.into() }
}

#[test]
fn a_scene_written_out_is_checked_saved_and_applied() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let s = surface(&sim, MemStore::default());
    let cli = Client::Cli;
    let def = |v: serde_json::Value| -> SceneDefinition { serde_json::from_value(v).unwrap() };
    let dinner = def(serde_json::json!({
        "groups": [{
            "coordinator": "kitchen",
            "members": ["Office"],
            "source": {"kind": "uri", "uri": STREAM}
        }],
        "volumes": {"Kitchen": 22, "office": 18}
    }));
    let saved = s.save_scene_from(&cli, "Dinner", &dinner).unwrap();
    let lead = &saved.groups[0];
    assert_eq!(lead.coordinator, "Kitchen", "named as the house names it");
    assert_eq!(lead.members, ["Office"]);
    assert_eq!(
        lead.playing,
        Some(true),
        "a group with a source plays by default"
    );
    assert_eq!(saved.volumes.get("Office"), Some(&18));

    let applied = s.apply_scene(&cli, "dinner").unwrap();
    assert!(applied.complete, "{applied:?}");
    let (level, together, state, uri) = kitchen(&sim);
    assert_eq!(
        (level, together, state, uri.as_str()),
        (22, true, TransportState::Playing, STREAM)
    );

    for (bad, code) in [
        // Nothing written out is not "a scene that does nothing".
        (serde_json::json!({}), ErrorCode::InvalidArgument),
        (
            serde_json::json!({"groups": [{"coordinator": "Atticc"}]}),
            ErrorCode::UnknownRoom,
        ),
        (
            serde_json::json!({"groups": [{"coordinator": "Kitchen",
                "source": {"kind": "uri", "uri": "spotify:artist:0SimArtist0000000000001"}}]}),
            ErrorCode::InvalidArgument,
        ),
        (
            serde_json::json!({"groups": [{"coordinator": "Kitchen", "source": {"kind": "radio"}}]}),
            ErrorCode::InvalidArgument,
        ),
        (
            serde_json::json!({"groups": [{"coordinator": "Kitchen", "members": ["Living Room"]}]}),
            ErrorCode::CrossHouseholdGroup,
        ),
        (
            serde_json::json!({"groups": [{"coordinator": "Kitchen",
                "source": {"kind": "favorite", "name": "No Such Favorite Anywhere"}}]}),
            ErrorCode::UnknownFavorite,
        ),
    ] {
        let err = s
            .save_scene_from(&cli, "Bad", &def(bad.clone()))
            .unwrap_err();
        assert_eq!(err.code, code, "{bad}: {err:?}");
    }
    assert!(s.scene(&cli, "Bad").is_err(), "nothing refused was saved");
}

#[test]
fn a_scene_can_play_a_spotify_link() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let s = surface(&sim, MemStore::default());
    let morning: SceneDefinition = serde_json::from_value(serde_json::json!({
        "groups": [{"coordinator": "Kitchen",
            "source": {"kind": "uri", "uri": "spotify:track:0SimQueueTrack00000001"}}]
    }))
    .unwrap();
    s.save_scene_from(&Client::Cli, "Morning", &morning)
        .unwrap();
    let applied = s.apply_scene(&Client::Cli, "Morning").unwrap();
    assert!(applied.complete, "{applied:?}");
    let (_, _, state, uri) = kitchen(&sim);
    assert_eq!(state, TransportState::Playing, "{applied:?}");
    assert!(
        uri.starts_with("x-sonos-spotify:spotify%3atrack%3a0SimQueueTrack00000001"),
        "{uri}"
    );
    // Applied again, the track is already on: nothing to send.
    let again = s.apply_scene(&Client::Cli, "Morning").unwrap();
    assert!(again.steps.is_empty(), "{again:?}");
}

#[test]
fn a_scene_can_fade_its_volumes_in() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let s = surface(&sim, MemStore::default());
    let cli = Client::Cli;
    volume(&s, &cli, "Kitchen", 40);
    s.save_scene(&cli, "Quiet").unwrap();
    volume(&s, &cli, "Kitchen", 10);

    let fade = |secs| SceneApplyRequest {
        fade_secs: Some(secs),
    };
    let too_long = s.apply_scene_with(&cli, "Quiet", &fade(61)).unwrap_err();
    assert_eq!(too_long.code, ErrorCode::InvalidArgument, "{too_long:?}");
    assert_eq!(kitchen(&sim).0, 10, "a refused fade changes nothing");

    let started = Instant::now();
    let applied = s.apply_scene_with(&cli, "Quiet", &fade(2)).unwrap();
    assert!(
        started.elapsed() >= Duration::from_millis(1500),
        "it ramped rather than jumped: {:?}",
        started.elapsed()
    );
    assert!(applied.done.contains("faded over 2s"), "{}", applied.done);
    assert_eq!(kitchen(&sim).0, 40, "the scene's volume, reached");
}

#[test]
fn apply_puts_the_saved_house_back_and_undo_restores_what_it_changed() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let s = surface(&sim, MemStore::default());
    let cli = Client::Cli;
    let play = PlayRequest {
        zone: "Kitchen".into(),
        source_uri: STREAM.into(),
        title: None,
    };
    s.control(&cli, "play", |h| plan_play(h, &play)).unwrap();
    volume(&s, &cli, "Kitchen", 30);
    let group = GroupRequest {
        zone: "Office".into(),
        to: "Kitchen".into(),
    };
    s.control(&cli, "group", |h| plan_group(h, &group)).unwrap();

    let saved = s.save_scene(&cli, " Evening ").unwrap();
    assert_eq!(saved.name, "Evening");
    let lead = saved
        .groups
        .iter()
        .find(|g| g.coordinator == "Kitchen")
        .expect("Kitchen leads a group");
    assert_eq!(lead.members, ["Office"], "{saved:?}");
    assert_eq!(
        (lead.source.kind.as_str(), lead.source.uri.as_deref()),
        ("uri", Some(STREAM))
    );
    assert_eq!(lead.playing, Some(true));
    assert_eq!(saved.volumes.get("Kitchen"), Some(&30));
    assert_eq!(
        saved.groups.len(),
        3,
        "Kitchen+Office, Living Room, Bedroom"
    );
    let listed = s.scenes(&Client::Unknown).unwrap();
    assert_eq!(listed.len(), 1, "anyone may read the scenes");
    assert_eq!(listed[0], saved);
    assert_eq!(s.scene(&cli, "EVENING").unwrap(), saved);

    // Change the house: Office leaves, Kitchen louder and paused.
    s.control(&cli, "ungroup", |h| plan_ungroup(h, &zone("Office")))
        .unwrap();
    volume(&s, &cli, "Kitchen", 60);
    s.control(&cli, "pause", |h| {
        plan_transport(h, &zone("Kitchen"), TransportAction::Pause)
    })
    .unwrap();
    let changed = kitchen(&sim);
    assert_eq!(
        (changed.0, changed.1),
        (60, false),
        "the house changed: {changed:?}"
    );

    let applied = s.apply_scene(&cli, "evening").unwrap();
    assert!(applied.changed && applied.complete, "{applied:?}");
    assert_eq!(applied.scene, "Evening");
    assert!(
        applied.steps.iter().any(|s| s == "Office joins Kitchen")
            && applied.steps.iter().any(|s| s == "Kitchen volume 30"),
        "{applied:?}"
    );
    assert_eq!(
        kitchen(&sim),
        (30, true, TransportState::Playing, STREAM.to_string()),
        "{applied:?}"
    );

    // The house matches now: nothing is sent, and nothing logged.
    let again = s.apply_scene(&cli, "Evening").unwrap();
    assert!(!again.changed && again.steps.is_empty(), "{again:?}");
    let log = s.recent_actions(&cli, &ActionFilter::default()).unwrap();
    assert_eq!(log[0].action.intent, "apply_scene: Evening", "{log:?}");
    assert!(log[0].action.before_state.is_some());
    assert_eq!(
        log.iter()
            .filter(|a| a.action.intent.starts_with("apply_scene"))
            .count(),
        1
    );

    // Undo puts back what the apply changed.
    let report = s.undo(&cli, false).unwrap().expect("the apply to undo");
    assert_eq!(report.undone, log[0].id);
    assert!(report.failures.is_empty(), "{report:?}");
    let undone = kitchen(&sim);
    assert_eq!((undone.0, undone.1), (60, false), "{undone:?}");
    assert_ne!(undone.2, TransportState::Playing, "{undone:?}");
    sim.shutdown();
}

#[test]
fn a_scene_never_sets_a_volume_its_writer_may_not() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let s = surface(&sim, MemStore::default());
    let cli = Client::Cli;
    volume(&s, &cli, "Kitchen", 40);
    let loud: SceneDefinition =
        serde_json::from_value(serde_json::json!({"volumes": {"Kitchen": 90}})).unwrap();
    // Saved by a capped agent, applied by the CLI (or a schedule made from it).
    s.save_scene_from(&agent(), "Loud", &loud).unwrap();
    let applied = s.apply_scene(&cli, "Loud").unwrap();
    let level = kitchen(&sim).0;
    assert!(
        level > 40 && level <= 70,
        "within the agent's cap: {level}, {applied:?}"
    );
    // The CLI's own scene is the CLI's to set.
    s.save_scene_from(&cli, "Loud", &loud).unwrap();
    s.apply_scene(&cli, "Loud").unwrap();
    assert_eq!(kitchen(&sim).0, 90);
}

/// A client denied `set_volume` applies a scene with the volumes left as
/// they are, each with a note, and the rest done; and cannot fade a volume
/// either, a fade being a run of volume sets.
#[test]
fn a_client_denied_set_volume_applies_a_scene_without_its_volumes_and_cannot_fade() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let policy = Policy::from_toml("[clients.\"tag:agent\"]\ndeny = [\"set_volume\"]\n").unwrap();
    let s = Surface::new(Box::new(sim.lan()), survey(), policy, Box::new(SystemClock))
        .with_action_log(Box::new(MemStore::default()), "mcp");
    volume(&s, &Client::Cli, "Kitchen", 30);
    let dinner: SceneDefinition = serde_json::from_value(serde_json::json!({
        "groups": [{
            "coordinator": "Kitchen",
            "members": ["Office"],
            "source": {"kind": "uri", "uri": STREAM}
        }],
        "volumes": {"Kitchen": 22}
    }))
    .unwrap();
    s.save_scene_from(&Client::Cli, "Dinner", &dinner).unwrap();

    let applied = s.apply_scene(&agent(), "Dinner").unwrap();
    let (level, together, state, uri) = kitchen(&sim);
    assert_eq!(
        (level, together, state, uri.as_str()),
        (30, true, TransportState::Playing, STREAM),
        "{applied:?}"
    );
    assert!(
        applied
            .notes
            .iter()
            .any(|n| n.code == NoteCode::VolumeClamped
                && n.detail.starts_with("Kitchen's volume left as it is: ")
                && n.detail.contains("set_volume")),
        "{applied:?}"
    );

    let fade = FadeVolumeRequest {
        zone: "Kitchen".into(),
        volume: 10,
        fade_secs: 2,
    };
    let denied = s.fade_volume_to(&agent(), &fade).unwrap_err();
    assert_eq!(denied.code, ErrorCode::PolicyDenied, "{denied:?}");
    assert!(
        denied.detail.contains("fade_volume carries out set_volume"),
        "{denied:?}"
    );
    assert_eq!(kitchen(&sim).0, 30);
    sim.shutdown();
}

#[test]
fn an_agent_applies_a_scene_within_its_volume_cap() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let s = surface(&sim, MemStore::default());
    volume(&s, &Client::Cli, "Kitchen", 90);
    s.save_scene(&Client::Cli, "Loud").unwrap();
    volume(&s, &Client::Cli, "Kitchen", 40);

    let applied = s.apply_scene(&agent(), "Loud").unwrap();
    let level = kitchen(&sim).0;
    assert!(
        level > 40 && level <= 70,
        "capped at 70: {level}, {applied:?}"
    );
    assert!(
        applied
            .notes
            .iter()
            .any(|n| n.code == NoteCode::VolumeClamped),
        "{applied:?}"
    );
    let log = s
        .recent_actions(&agent(), &ActionFilter::default())
        .unwrap();
    assert!(log[0].action.decision.starts_with("clamp: "), "{log:?}");

    // Unidentified callers may read scenes, not save or apply them; the
    // refusal is logged.
    let denied = s.save_scene(&Client::Unknown, "Mine").unwrap_err();
    assert_eq!(denied.code, ErrorCode::PolicyDenied, "{denied:?}");
    let denied = s.apply_scene(&Client::Unknown, "Loud").unwrap_err();
    assert_eq!(denied.code, ErrorCode::PolicyDenied, "{denied:?}");
    let log = s
        .recent_actions(&agent(), &ActionFilter::default())
        .unwrap();
    assert!(log[0].action.decision.starts_with("deny: "), "{log:?}");
    assert_eq!(s.scenes(&Client::Unknown).unwrap().len(), 1);
    sim.shutdown();
}

/// A scene that starts music holds the rooms it sounds in to the caller's
/// caps, as a play does, even when it sets no volume: an agent cannot start
/// a scene at 90 in a room someone left loud.
#[test]
fn a_scene_that_starts_music_lowers_loud_rooms_to_the_callers_cap_first() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let s = surface(&sim, MemStore::default());
    let cli = Client::Cli;
    let evening: SceneDefinition = serde_json::from_value(serde_json::json!({
        "groups": [{"coordinator": "Kitchen", "members": ["Office"],
            "source": {"kind": "uri", "uri": STREAM}}]
    }))
    .unwrap();
    s.save_scene_from(&cli, "Evening", &evening).unwrap();
    let office = || {
        let houses = house(&sim);
        control::volume(&sim.lan(), &houses, &id(&houses, "Office")).unwrap()
    };
    volume(&s, &cli, "Kitchen", 90);
    volume(&s, &cli, "Office", 85);

    let applied = s.apply_scene(&agent(), "Evening").unwrap();
    assert!(applied.complete, "{applied:?}");
    let (level, together, state, uri) = kitchen(&sim);
    assert_eq!(
        (level, together, state, uri.as_str(), office()),
        (70, true, TransportState::Playing, STREAM, 70),
        "{applied:?}"
    );
    let lowered: Vec<&str> = applied
        .notes
        .iter()
        .filter(|n| n.code == NoteCode::VolumeClamped)
        .map(|n| n.detail.as_str())
        .collect();
    assert!(
        lowered
            .iter()
            .any(|d| d.starts_with("Kitchen lowered from 90 to 70 first"))
            && lowered
                .iter()
                .any(|d| d.starts_with("Office lowered from 85 to 70 first")),
        "{lowered:?}"
    );

    // The CLI runs uncapped: its apply starts the music at the level the
    // room has.
    s.control(&cli, "pause", |h| {
        plan_transport(h, &zone("Kitchen"), TransportAction::Pause)
    })
    .unwrap();
    volume(&s, &cli, "Kitchen", 90);
    let applied = s.apply_scene(&cli, "Evening").unwrap();
    let (level, _, state, _) = kitchen(&sim);
    assert_eq!((level, state), (90, TransportState::Playing), "{applied:?}");
    assert!(applied.notes.is_empty(), "{applied:?}");
    sim.shutdown();
}

/// A scene that only unmutes a loud room whose group already plays lowers
/// it to the caller's cap first, as starting music there would (p7vq.13).
#[test]
fn a_scene_that_unmutes_a_loud_playing_room_lowers_it_first() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let s = surface(&sim, MemStore::default());
    let cli = Client::Cli;
    let def = |v: serde_json::Value| -> SceneDefinition { serde_json::from_value(v).unwrap() };
    let evening = def(serde_json::json!({
        "groups": [{"coordinator": "Kitchen", "source": {"kind": "uri", "uri": STREAM}}]
    }));
    let unmuted = def(serde_json::json!({
        "groups": [{"coordinator": "Kitchen"}],
        "mutes": {"Kitchen": false}
    }));
    s.save_scene_from(&cli, "Evening", &evening).unwrap();
    s.save_scene_from(&cli, "Unmuted", &unmuted).unwrap();
    s.apply_scene(&cli, "Evening").unwrap();
    volume(&s, &cli, "Kitchen", 90);
    let mute = MuteRequest {
        zone: "Kitchen".into(),
        mute: true,
    };
    s.control(&cli, "mute", |h| plan_mute(h, &mute)).unwrap();

    let applied = s.apply_scene(&agent(), "Unmuted").unwrap();
    assert!(applied.complete, "{applied:?}");
    let (level, _, state, _) = kitchen(&sim);
    assert_eq!((level, state), (70, TransportState::Playing), "{applied:?}");
    assert!(
        applied
            .notes
            .iter()
            .any(|n| n.code == NoteCode::VolumeClamped
                && n.detail.starts_with("Kitchen lowered from 90 to 70 first")),
        "{:?}",
        applied.notes
    );
}

#[test]
fn scene_failures_carry_their_codes() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let mut store = MemStore::default();
    // S1 and S2 rooms can never group; such a scene saves but cannot apply.
    let across = Scene {
        name: "Across".into(),
        groups: vec![SceneGroup {
            coordinator: "Kitchen".into(),
            members: vec!["Living Room".into()],
            source: SceneSource::Keep,
            playing: Some(false),
        }],
        volumes: BTreeMap::new(),
        mutes: BTreeMap::new(),
        writer: None,
    };
    scenes::save(&mut store, &across, 0).unwrap();
    let s = surface(&sim, store);

    let err = s.apply_scene(&Client::Cli, "Across").unwrap_err();
    assert_eq!(
        (err.code, err.status()),
        (ErrorCode::CrossHouseholdGroup, 422),
        "{err:?}"
    );
    let err = s.apply_scene(&Client::Cli, "Acros").unwrap_err();
    assert_eq!(
        (err.code, err.status(), err.exit_code()),
        (ErrorCode::UnknownScene, 404, 3),
        "{err:?}"
    );
    assert_eq!(err.suggestions, ["Across"], "{err:?}");
    let err = s.save_scene(&Client::Cli, "  ").unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgument, "{err:?}");
    let err = s.save_scene(&Client::Cli, &"x".repeat(65)).unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgument, "{err:?}");

    let deleted = s.delete_scene(&Client::Cli, "across").unwrap();
    assert_eq!(deleted.deleted, "Across");
    assert!(s.scenes(&Client::Cli).unwrap().is_empty());
    let err = s.delete_scene(&Client::Cli, "Across").unwrap_err();
    assert_eq!(err.code, ErrorCode::UnknownScene, "{err:?}");

    // Without a store there are no scenes to keep.
    let bare = Surface::new(
        Box::new(sim.lan()),
        survey(),
        Policy::default(),
        Box::new(SystemClock),
    );
    let err = bare.scenes(&Client::Cli).unwrap_err();
    assert_eq!(err.code, ErrorCode::NotImplemented, "{err:?}");
    sim.shutdown();
}

/// A DJ that plays in the groups `.0` lead, and only there.
struct PlaysIn(Vec<PlayerId>);

impl DjEngine for PlaysIn {
    fn act(
        &self,
        _: DjSpeakers<'_>,
        _: &mut dyn Store,
        _: DjAction,
        _: &dyn Clock,
    ) -> Result<OutcomeDto, Failure> {
        Err(Failure::new(ErrorCode::NotImplemented, "not in this test"))
    }

    fn steer(
        &self,
        _: DjSpeakers<'_>,
        _: &mut dyn Store,
        _: &DjSteer,
        _: &dyn Clock,
    ) -> Result<OutcomeDto, Failure> {
        Err(Failure::new(ErrorCode::NotImplemented, "not in this test"))
    }

    fn status(
        &self,
        _: DjSpeakers<'_>,
        _: &dyn Store,
        _: Option<u32>,
        _: &dyn Clock,
    ) -> Result<DjStatusDto, Failure> {
        Err(Failure::new(ErrorCode::NotImplemented, "not in this test"))
    }

    fn moods(
        &self,
        _: Option<DjSpeakers<'_>>,
        _: &dyn Store,
        _: &dyn Clock,
    ) -> Result<DjMoodsDto, Failure> {
        Ok(DjMoodsDto {
            moods: vec![DjMoodDto {
                name: "calm".into(),
                constraints: SteerConstraints::default(),
            }],
            programs: Vec::new(),
            now: DjSteeringDto {
                source: "none".into(),
                mood: None,
                constraints: SteerConstraints::default(),
                added: SteerConstraints::default(),
                expires_at: None,
                expires_in_secs: None,
            },
        })
    }

    fn feeds(&self, coordinator: &PlayerId) -> bool {
        self.0.contains(coordinator)
    }

    fn on_playback(&self, _: DjSpeakers<'_>, _: &mut dyn Store, _: &PlayerPlayback, _: &dyn Clock) {
    }

    fn moving(&self, _: &PlayerId) {}

    fn moved(&self, _: &PlayerId, _: &PlayerId) {}
}

#[test]
fn a_written_scene_names_a_mood_the_dj_has() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let s = surface(&sim, MemStore::default()).with_dj(Box::new(PlaysIn(Vec::new())));
    let cli = Client::Cli;
    let dj = |mood: &str| -> SceneDefinition {
        serde_json::from_value(serde_json::json!({
            "groups": [{"coordinator": "Kitchen", "source": {"kind": "dj", "mood": mood}}]
        }))
        .unwrap()
    };
    let typo = s.save_scene_from(&cli, "Typo", &dj("clam")).unwrap_err();
    assert_eq!(typo.code, ErrorCode::UnknownMood, "{typo:?}");
    assert_eq!(typo.suggestions, ["calm"]);
    assert!(s.scene(&cli, "Typo").is_err(), "nothing refused was saved");
    let saved = s.save_scene_from(&cli, "Evening", &dj("Calm")).unwrap();
    assert_eq!(
        saved.groups[0].source.mood.as_deref(),
        Some("calm"),
        "as the DJ names it"
    );
}

#[test]
fn a_scene_has_the_dj_where_it_plays_not_where_it_is_only_steered() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let houses = house(&sim);
    let (kitchen, office, living) = (
        id(&houses, "Kitchen"),
        id(&houses, "Office"),
        id(&houses, "Living Room"),
    );
    // Steered in the Kitchen and the Living Room; the DJ plays in the
    // Kitchen and, unsteered, in the Office, but not in the Living Room.
    let mut store = MemStore::default();
    for (zone, mood) in [(&kitchen, "calm"), (&living, "focus")] {
        store
            .save_dj_session(&DjSession {
                coordinator: zone.0.clone(),
                mood: Some(mood.into()),
                constraints: None,
                expires: i64::MAX,
            })
            .unwrap();
    }
    let s = surface(&sim, store).with_dj(Box::new(PlaysIn(vec![kitchen, office])));
    let saved = s.save_scene(&Client::Cli, "Steered").unwrap();
    let source = |room: &str| {
        saved
            .groups
            .iter()
            .find(|g| g.coordinator == room)
            .map(|g| (g.source.kind.as_str(), g.source.mood.as_deref()))
    };
    assert_eq!(source("Kitchen"), Some(("dj", Some("calm"))), "{saved:#?}");
    assert_eq!(source("Office"), Some(("dj", None)), "{saved:#?}");
    assert!(
        source("Living Room").is_some_and(|(kind, _)| kind != "dj"),
        "steering alone isn't the DJ playing: {saved:#?}"
    );
}

/// A scene is its saver's (p7vq.4): another client may neither delete it
/// nor save over it, and the refusal is logged; its saver, a client
/// policy.toml lets manage others' and the owner may. A scene saved before
/// savers were kept is the owner's.
#[test]
fn another_clients_scene_is_not_yours_to_change() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let mut store = MemStore::default();
    let old = Scene {
        name: "Old".into(),
        groups: vec![SceneGroup {
            coordinator: "Kitchen".into(),
            members: Vec::new(),
            source: SceneSource::Keep,
            playing: Some(false),
        }],
        volumes: BTreeMap::new(),
        mutes: BTreeMap::new(),
        writer: None,
    };
    scenes::save(&mut store, &old, 0).unwrap();
    let policy = Policy::from_toml("[clients.\"tag:keeper\"]\nmanage_others = true\n").unwrap();
    let s = Surface::new(Box::new(sim.lan()), survey(), policy, Box::new(SystemClock))
        .with_action_log(Box::new(store), "mcp");
    let other = Client::Tailnet("tag:other".into());
    s.save_scene(&agent(), "Mine").unwrap();
    for (refused, whose) in [
        (s.delete_scene(&other, "mine").map(drop), "tag:agent's"),
        (s.save_scene(&other, "MINE").map(drop), "tag:agent's"),
        (s.delete_scene(&agent(), "Old").map(drop), "the owner's"),
    ] {
        let refused = refused.unwrap_err();
        assert_eq!(refused.code, ErrorCode::PolicyDenied, "{refused:?}");
        assert!(refused.detail.contains(whose), "{}", refused.detail);
    }
    let log = s
        .recent_actions(&Client::Cli, &ActionFilter::default())
        .unwrap();
    assert!(
        log[0].action.intent == "delete_scene: the scene \"Old\""
            && log[0].action.decision.starts_with("deny: "),
        "{log:?}"
    );
    assert_eq!(s.scenes(&Client::Cli).unwrap().len(), 2);

    s.save_scene(&agent(), "Mine").unwrap();
    s.delete_scene(&Client::Tailnet("tag:keeper".into()), "Mine")
        .unwrap();
    s.delete_scene(&Client::Cli, "Old").unwrap();
    assert!(s.scenes(&Client::Cli).unwrap().is_empty());
    sim.shutdown();
}

/// A scene that no longer reads is the owner's to save over or delete, as
/// its hint says (UNKNOWN is still UNKNOWN_SCENE, before any ownership).
#[test]
fn a_scene_that_no_longer_reads_is_still_the_owners_to_replace() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let mut store = MemStore::default();
    for name in ["Broken", "Gone"] {
        store
            .save_scene(&StoredScene {
                name: name.into(),
                spec: "{".into(),
                updated: 0,
            })
            .unwrap();
    }
    let s = surface(&sim, store);
    let refused = s.save_scene(&agent(), "broken").unwrap_err();
    assert_eq!(refused.code, ErrorCode::PolicyDenied, "{refused:?}");
    let unknown = s.delete_scene(&agent(), "Nowhere").unwrap_err();
    assert_eq!(unknown.code, ErrorCode::UnknownScene, "{unknown:?}");
    s.save_scene(&Client::Cli, "broken").unwrap();
    assert_eq!(s.scene(&Client::Cli, "Broken").unwrap().name, "broken");
    assert_eq!(
        s.delete_scene(&Client::Cli, "gone").unwrap().deleted,
        "Gone"
    );
    sim.shutdown();
}

/// Deleting a scene, or saving over it, can be undone (p7vq.4): undo puts
/// back exactly the scene that was, and refuses when a scene was saved under
/// its name since.
#[test]
fn a_deleted_or_overwritten_scene_comes_back_on_undo() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let s = surface(&sim, MemStore::default());
    let scene = |s: &Surface| s.scene(&Client::Cli, "Evening").unwrap();
    volume(&s, &Client::Cli, "Kitchen", 30);
    s.save_scene(&Client::Cli, "Evening").unwrap();
    let first = scene(&s);

    s.delete_scene(&Client::Cli, "Evening").unwrap();
    assert!(s.scenes(&Client::Cli).unwrap().is_empty());
    let undone = s.undo(&Client::Cli, true).unwrap().unwrap();
    assert!(
        undone.summary.contains("the scene \"Evening\" put back"),
        "{}",
        undone.summary
    );
    assert_eq!(scene(&s), first);

    volume(&s, &Client::Cli, "Kitchen", 50);
    s.save_scene(&Client::Cli, "evening").unwrap();
    assert_ne!(scene(&s).volumes, first.volumes);
    s.undo(&Client::Cli, true).unwrap().unwrap();
    assert_eq!(scene(&s), first);

    s.delete_scene(&Client::Cli, "Evening").unwrap();
    s.save_scene(&Client::Cli, "EVENING").unwrap();
    let refused = s.undo(&Client::Cli, true).unwrap_err();
    assert!(
        refused.detail.contains("a scene \"EVENING\" was saved"),
        "{refused:?}"
    );
    assert_eq!(s.scenes(&Client::Cli).unwrap()[0].name, "EVENING");
    sim.shutdown();
}
