//! Scenes against `fsonos-sim` over real loopback sockets, in both
//! generations: save the house, change it, apply the scene, and get the
//! same house back.

use fsonos_core::fade::Fader;
use fsonos_core::scenes::{
    self, ApplyOptions, Scene, SceneError, SceneGroup, SceneHooks, SceneOp, SceneSource,
};
use fsonos_core::snapshot;
use fsonos_core::store::MemStore;
use fsonos_core::{HouseholdState, control, favorites, resolve_room};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::PlayerId;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

fn sim() -> SimHandle {
    SimHousehold::builder()
        .s1([
            SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1),
            SimPlayerSpec::new("Office", SimModel::Play5Gen1),
            SimPlayerSpec::new("Den", SimModel::Play5Gen1),
        ])
        .s2([
            SimPlayerSpec::new("Living Room", SimModel::One),
            SimPlayerSpec::new("Bedroom", SimModel::Play1),
        ])
        .spawn()
        .unwrap()
}

fn houses(sim: &SimHandle, lan: &SimLan) -> Vec<HouseholdState> {
    ["Kitchen", "Living Room"]
        .iter()
        .map(|room| {
            let mut st = HouseholdState::default();
            st.apply_topology(&get_zone_group_state(lan, sim.player(room).unwrap().ip).unwrap());
            st
        })
        .collect()
}

fn id(h: &[HouseholdState], room: &str) -> PlayerId {
    resolve_room(h, room).unwrap().player.id.clone()
}

fn play_favorite(lan: &SimLan, h: &[HouseholdState], room: &str, title: &str) {
    let coordinator = resolve_room(h, room).unwrap().coordinator.id.clone();
    let all = favorites::list(lan, h, &coordinator).unwrap();
    let favorite = favorites::find(&all, title).unwrap();
    favorites::play(lan, h, &coordinator, favorite).unwrap();
}

/// Plan and apply `scene` the way a surface does: snapshot its zones, diff,
/// run.
fn apply(
    lan: &SimLan,
    h: &[HouseholdState],
    scene: &Scene,
    options: &ApplyOptions<'_>,
) -> (Vec<SceneOp>, scenes::SceneReport) {
    let snaps: Vec<_> = scenes::zones(scene, h)
        .iter()
        .map(|z| snapshot::capture(lan, h, z, 0).unwrap())
        .collect();
    let plan = scenes::plan_apply(scene, h, &snaps).unwrap();
    let report = scenes::apply(lan, h, &plan, options);
    (plan.ops, report)
}

/// `scene` with its groups in a stable order, for comparing.
fn sorted(mut scene: Scene) -> Scene {
    scene
        .groups
        .sort_by(|a, b| a.coordinator.cmp(&b.coordinator));
    scene
}

#[test]
fn a_saved_scene_brings_the_house_back_in_both_generations() {
    let sim = sim();
    let lan = sim.lan();
    let h = houses(&sim, &lan);
    play_favorite(&lan, &h, "Kitchen", "Sim Radio");
    play_favorite(&lan, &h, "Living Room", "Aria");
    control::join(&lan, &h, &id(&h, "Office"), &id(&h, "Kitchen")).unwrap();
    for (room, level) in [
        ("Kitchen", 30),
        ("Office", 20),
        ("Den", 12),
        ("Living Room", 25),
    ] {
        control::set_volume(&lan, &h, &id(&h, room), level).unwrap();
    }
    control::set_mute(&lan, &h, &id(&h, "Den"), true).unwrap();

    let h = houses(&sim, &lan);
    let scene = scenes::capture(&lan, &h, " Evening ", &[], 0).unwrap();
    assert_eq!(scene.name, "Evening");
    let kitchen = scene
        .groups
        .iter()
        .find(|g| g.coordinator == "Kitchen")
        .unwrap();
    assert_eq!(kitchen.members, ["Office"]);
    assert!(kitchen.playing);
    assert!(
        matches!(&kitchen.source, SceneSource::Favorite { name, uri: Some(_) } if name == "Sim Radio"),
        "{scene:#?}"
    );
    let living = scene
        .groups
        .iter()
        .find(|g| g.coordinator == "Living Room")
        .unwrap();
    assert!(matches!(&living.source, SceneSource::Favorite { name, .. } if name == "Aria"));
    let den = scene
        .groups
        .iter()
        .find(|g| g.coordinator == "Den")
        .unwrap();
    assert_eq!((&den.source, den.playing), (&SceneSource::Keep, false));
    assert_eq!(
        scene.groups.len(),
        4,
        "Kitchen+Office, Den, Living Room, Bedroom"
    );
    assert_eq!((scene.volumes["Den"], scene.mutes["Den"]), (12, true));

    let mut store = MemStore::default();
    scenes::save(&mut store, &scene, 1_000).unwrap();
    let scene = scenes::load(&store, "evening").unwrap();

    // Change everything the scene covers.
    control::leave(&lan, &h, &id(&h, "Office")).unwrap();
    control::join(&lan, &h, &id(&h, "Den"), &id(&h, "Kitchen")).unwrap();
    control::set_volume(&lan, &h, &id(&h, "Kitchen"), 60).unwrap();
    control::set_mute(&lan, &h, &id(&h, "Den"), false).unwrap();
    control::pause(&lan, &h, &id(&h, "Kitchen")).unwrap();
    play_favorite(&lan, &h, "Living Room", "Nocturne in E-flat");
    control::join(&lan, &h, &id(&h, "Bedroom"), &id(&h, "Living Room")).unwrap();

    let h = houses(&sim, &lan);
    let (ops, report) = apply(&lan, &h, &scene, &ApplyOptions::default());
    assert!(report.is_complete(), "{report:#?}");
    assert_eq!(report.done, ops);
    let names: Vec<&str> = ops
        .iter()
        .map(|op| match op {
            SceneOp::Leave { .. } => "leave",
            SceneOp::Join { .. } => "join",
            SceneOp::Volume { .. } => "volume",
            SceneOp::Mute { .. } => "mute",
            SceneOp::Favorite { .. } => "favorite",
            SceneOp::Uri { .. } => "uri",
            SceneOp::Dj { .. } => "dj",
            SceneOp::Play { .. } => "play",
            SceneOp::Pause { .. } => "pause",
        })
        .collect();
    assert_eq!(
        names,
        [
            "leave", "leave", "join", "volume", "mute", "play", "favorite"
        ],
        "Den and Bedroom out, Office in, Kitchen's level and Den's mute, \
         Kitchen resumes its station, Living Room gets Aria back: {ops:#?}"
    );

    // The same house again, and nothing left to do.
    let h = houses(&sim, &lan);
    let again = scenes::capture(&lan, &h, "Evening", &[], 0).unwrap();
    assert_eq!(sorted(again), sorted(scene.clone()));
    let (ops, _) = apply(&lan, &h, &scene, &ApplyOptions::default());
    assert_eq!(ops, []);
}

/// Records what the DJ was asked to do.
#[derive(Default)]
struct Dj(Mutex<Vec<(PlayerId, Option<String>)>>);

impl SceneHooks for Dj {
    fn start_dj(&self, coordinator: &PlayerId, mood: Option<&str>) -> Result<(), String> {
        self.0
            .lock()
            .unwrap()
            .push((coordinator.clone(), mood.map(str::to_string)));
        Ok(())
    }
}

#[test]
fn volumes_fade_the_dj_is_asked_and_failures_are_reported() {
    let sim = sim();
    let lan = sim.lan();
    let h = houses(&sim, &lan);
    let scene = Scene {
        name: "Calm".into(),
        groups: vec![
            SceneGroup {
                coordinator: "Den".into(),
                members: vec!["Office".into()],
                source: SceneSource::Dj {
                    mood: Some("calm".into()),
                },
                playing: true,
            },
            SceneGroup {
                coordinator: "Kitchen".into(),
                members: Vec::new(),
                source: SceneSource::Favorite {
                    name: "No Such Station".into(),
                    uri: None,
                },
                playing: true,
            },
        ],
        volumes: [("Den", 40), ("Office", 40), ("Kitchen", 5)]
            .iter()
            .map(|(r, v)| ((*r).to_string(), *v))
            .collect(),
        mutes: BTreeMap::new(),
    };
    let fader = Fader::with_timing(Duration::from_millis(5), Duration::from_millis(5));
    let dj = Dj::default();
    let options = ApplyOptions {
        fade: Some((&fader, Duration::from_millis(100))),
        hooks: &dj,
    };
    let (_, report) = apply(&lan, &h, &scene, &options);
    assert_eq!(report.failed.len(), 1, "{report:#?}");
    let (op, why) = &report.failed[0];
    assert!(matches!(op, SceneOp::Favorite { name, .. } if name == "No Such Station"));
    assert!(why.contains("No Such Station"), "{why}");
    assert_eq!(
        *dj.0.lock().unwrap(),
        [(id(&h, "Den"), Some("calm".into()))]
    );
    let h = houses(&sim, &lan);
    for (room, level) in [("Den", 40), ("Office", 40), ("Kitchen", 5)] {
        let ip = resolve_room(&h, room).unwrap().player.ip;
        assert_eq!(soap::get_volume(&lan, ip).unwrap(), level, "{room}");
    }
    assert_eq!(
        resolve_room(&h, "Office").unwrap().coordinator.id,
        id(&h, "Den")
    );

    // Without a DJ, its op fails and says why; the rest still runs.
    let (_, report) = apply(&lan, &h, &scene, &ApplyOptions::default());
    let reasons: Vec<&str> = report.failed.iter().map(|(_, why)| why.as_str()).collect();
    assert!(
        reasons.contains(&"the DJ is not available here"),
        "{reasons:?}"
    );

    // An S1 room and an S2 room can never share a group.
    let mut across = scene;
    across.groups = vec![SceneGroup {
        coordinator: "Kitchen".into(),
        members: vec!["Living Room".into()],
        source: SceneSource::Keep,
        playing: false,
    }];
    let err = scenes::plan_apply(&across, &h, &[]).unwrap_err();
    assert!(matches!(err, SceneError::CrossHousehold { .. }), "{err}");
}
