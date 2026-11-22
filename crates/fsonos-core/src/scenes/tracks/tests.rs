use super::*;
use crate::Room;
use crate::scenes::{self, Scene, SceneGroup, SceneOp, SceneSource};
use crate::store::MemStore;
use fsonos_proto::ProtoError;
use fsonos_types::ZoneGroup;
use std::net::IpAddr;

fn selection() -> Vec<SceneTrack> {
    [
        ("spotify:track:1111111111111111111111", Some("First <&>")),
        ("spotify:episode:2222222222222222222222", None),
        ("spotify:track:1111111111111111111111", Some("First again")),
    ]
    .into_iter()
    .map(|(uri, title)| SceneTrack {
        source_uri: uri.into(),
        title: title.map(str::to_string),
    })
    .collect()
}

fn scene(playing: Option<bool>) -> Scene {
    Scene {
        name: "Ordered evening".into(),
        groups: vec![SceneGroup {
            coordinator: "Lead".into(),
            members: Vec::new(),
            source: SceneSource::Tracks {
                tracks: selection(),
            },
            playing,
        }],
        volumes: Default::default(),
        mutes: Default::default(),
        writer: None,
    }
}

#[test]
fn stored_track_scenes_preserve_titles_order_and_duplicate_occurrences() {
    let scene = scene(Some(false));
    let mut store = MemStore::default();
    scenes::save(&mut store, &scene, 42).unwrap();
    let restored = scenes::load(&store, "ordered evening").unwrap();
    assert_eq!(restored, scene);
    let wire = serde_json::to_value(&restored.groups[0].source).unwrap();
    assert_eq!(wire["kind"], "tracks");
    assert_eq!(wire["tracks"].as_array().unwrap().len(), 3);
    assert!(wire["tracks"][1].get("title").is_none());
    assert_eq!(
        wire["tracks"][0]["source_uri"],
        wire["tracks"][2]["source_uri"]
    );
}

#[test]
fn invalid_or_unbounded_persisted_selections_cannot_be_planned_or_saved() {
    assert!(validate_track_selection(&selection()).is_ok());
    assert!(validate_track_selection(&[]).is_err());
    assert!(validate_track_selection(&vec![selection()[0].clone(); 11]).is_err());
    for uri in [
        "spotify:album:1111111111111111111111",
        "spotify:track:short",
        "spotify:track:111111111111111111111!",
        "https://open.spotify.com/track/1111111111111111111111",
        " spotify:track:1111111111111111111111",
    ] {
        let mut scene = scene(Some(true));
        let SceneSource::Tracks { tracks } = &mut scene.groups[0].source else {
            unreachable!()
        };
        tracks[0].source_uri = uri.into();
        assert!(scenes::check(&scene, &[]).is_err(), "{uri}");
        assert!(scenes::plan_apply(&scene, &[], &[]).is_err(), "{uri}");
        assert!(
            scenes::save(&mut MemStore::default(), &scene, 1).is_err(),
            "{uri}"
        );
    }
    for title in [" ".into(), "x".repeat(257), "invalid\0title".into()] {
        let mut tracks = selection();
        tracks[0].title = Some(title);
        assert!(validate_track_selection(&tracks).is_err());
    }
    let mut tracks = selection();
    tracks[0].title = Some("A title\nwith XML whitespace".into());
    assert!(validate_track_selection(&tracks).is_ok());
}

#[test]
fn one_source_step_owns_the_complete_playback_intent() {
    let id = PlayerId("fixture-lead".into());
    let house = HouseholdState {
        groups: vec![ZoneGroup {
            coordinator: id.clone(),
            members: vec![id.clone()],
        }],
        rooms: vec![Room {
            name: "Lead".into(),
            primary: id.clone(),
            players: vec![id.clone()],
            missing: Vec::new(),
            coordinator: id.clone(),
        }],
        ..HouseholdState::default()
    };
    for start in [false, true] {
        let planned =
            scenes::plan_apply(&scene(Some(start)), std::slice::from_ref(&house), &[]).unwrap();
        assert_eq!(
            planned.ops,
            vec![SceneOp::Tracks {
                coordinator: id.clone(),
                tracks: selection(),
                start
            }]
        );
        assert_eq!(planned.ops[0].starts(), start);
        assert_eq!(
            scenes::fade_players(std::slice::from_ref(&house), &planned).contains(&id),
            start
        );
    }
}

struct NoIo;
impl Transport for NoIo {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("a refused prepared selection must not fall back to speaker I/O")
    }
}
struct PreparedRefusal;
impl SceneHooks for PreparedRefusal {
    fn load_track_selection(
        &self,
        _: &[HouseholdState],
        _: &PlayerId,
        _: &[SceneTrack],
        _: bool,
    ) -> Option<Result<(), String>> {
        Some(Err("the original selection destination changed".into()))
    }
    fn queue_replacement_started(&self, _: &PlayerId) -> bool {
        panic!("the prepared hook owns receipt recording")
    }
    fn start_dj(&self, _: &PlayerId, _: Option<&str>) -> Result<(), String> {
        panic!("a track selection is not a DJ start")
    }
}

#[test]
fn a_prepared_refusal_is_terminal_without_an_unprepared_fallback_or_second_attempt() {
    assert_eq!(
        recall(
            &NoIo,
            &[],
            &PlayerId("fixture-lead".into()),
            &selection(),
            true,
            &PreparedRefusal
        ),
        Err("the original selection destination changed".into()),
    );
    let bad = scenes::ScenePlan {
        ops: vec![SceneOp::Tracks {
            coordinator: PlayerId("fixture-lead".into()),
            tracks: Vec::new(),
            start: true,
        }],
        notes: Vec::new(),
    };
    assert!(scenes::check_sources(&NoIo, &[], &bad).is_err());
}
