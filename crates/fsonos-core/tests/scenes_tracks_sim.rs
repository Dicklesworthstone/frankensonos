//! Authored selections keep their exact occurrence order through saved scenes,
//! regrouping and the shared guarded queue writer on real S1/S2 SOAP sockets.

use fsonos_core::scenes::{
    self, ApplyOptions, Scene, SceneGroup, SceneOp, ScenePlan, SceneSource, SceneTrack,
};
use fsonos_core::store::MemStore;
use fsonos_core::{CoreError, HouseholdState};
use fsonos_proto::content::{QUEUE, browse_snapshot};
use fsonos_proto::control as soap;
use fsonos_proto::didl::spotify_uri_from_renderer_uri;
use fsonos_proto::play_mode::{get_play_mode, set_play_mode};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{HouseholdId, PlayMode, PlayerId, TransportState};
use std::cell::{Cell, RefCell};
use std::net::IpAddr;

const OLD: &str = "x-file-cifs://example.invalid/previous.flac";
const RADIO: &str = "x-rincon-mp3radio://example.invalid/radio.mp3";
const OTHER: &str = "x-file-cifs://example.invalid/another-owner.flac";
const SELECTED: [&str; 3] = [
    "spotify:track:0000000000000000000101",
    "spotify:episode:0000000000000000000102",
    "spotify:track:0000000000000000000101",
];

fn tracks() -> Vec<SceneTrack> {
    SELECTED
        .into_iter()
        .zip([Some("First <&>"), None, Some("First again")])
        .map(|(uri, title)| SceneTrack {
            source_uri: uri.into(),
            title: title.map(str::to_string),
        })
        .collect()
}

fn group(coordinator: &str, members: &[&str], start: bool) -> SceneGroup {
    SceneGroup {
        coordinator: coordinator.into(),
        members: members.iter().map(|name| (*name).into()).collect(),
        source: SceneSource::Tracks { tracks: tracks() },
        playing: Some(start),
    }
}

fn scene(groups: Vec<SceneGroup>) -> Scene {
    Scene {
        name: "Ordered evening".into(),
        groups,
        volumes: Default::default(),
        mutes: Default::default(),
        writer: None,
    }
}

struct Fixture {
    sim: SimHandle,
    lan: SimLan,
    houses: Vec<HouseholdState>,
    lead: PlayerId,
    host: IpAddr,
}

impl Fixture {
    fn new(generation: u8) -> Self {
        let model = if generation == 1 {
            SimModel::Play5Gen1
        } else {
            SimModel::One
        };
        let players = [
            SimPlayerSpec::new("Lead", model),
            SimPlayerSpec::new("Member", model),
            SimPlayerSpec::new("Independent", model),
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
        let host = sim.player("Lead").unwrap().ip;
        let lead = PlayerId(sim.player("Lead").unwrap().uuid.clone());
        soap::join_group(&lan, sim.player("Member").unwrap().ip, &lead).unwrap();
        for _ in 0..2 {
            soap::add_uri_to_queue(&lan, host, OLD, "", false).unwrap();
        }
        soap::set_av_transport_uri(&lan, host, RADIO, "").unwrap();
        set_play_mode(&lan, host, PlayMode::Shuffle).unwrap();
        soap::play(&lan, host).unwrap();
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&lan, host).unwrap());
        house.id = Some(HouseholdId(sim.player("Lead").unwrap().household.clone()));
        Self {
            sim,
            lan,
            houses: vec![house],
            lead,
            host,
        }
    }

    fn id(&self, room: &str) -> PlayerId {
        PlayerId(self.sim.player(room).unwrap().uuid.clone())
    }

    fn queue(&self, room: &str) -> Vec<String> {
        browse_snapshot(&self.lan, self.sim.player(room).unwrap().ip, QUEUE)
            .unwrap()
            .objects
            .into_iter()
            .map(|item| {
                let uri = item.res.unwrap().uri;
                spotify_uri_from_renderer_uri(&uri).unwrap_or(uri)
            })
            .collect()
    }

    fn plan(&self, scene: &Scene) -> ScenePlan {
        let plan = scenes::plan_apply(scene, &self.houses, &[]).unwrap();
        let before = self.sim.soap_log().len();
        scenes::check_sources(&self.lan, &self.houses, &plan).unwrap();
        assert!(
            self.sim.soap_log()[before..]
                .iter()
                .all(|call| { call.action.starts_with("Get") || call.action == "Browse" })
        );
        plan
    }
}

#[test]
fn saved_mixed_selections_preserve_order_duplicates_and_explicit_quiet_intent() {
    for generation in [1, 2] {
        for start in [false, true] {
            let f = Fixture::new(generation);
            let authored = scene(vec![group("Lead", &["Member"], start)]);
            let mut store = MemStore::default();
            scenes::save(&mut store, &authored, 1).unwrap();
            let saved = scenes::load(&store, "ordered evening").unwrap();
            let plan = f.plan(&saved);
            let before = f.sim.soap_log().len();
            let report = scenes::apply(&f.lan, &f.houses, &plan, &ApplyOptions::default());
            assert!(report.is_complete(), "{report:?}");
            assert_eq!(f.queue("Lead"), SELECTED);
            let queue = browse_snapshot(&f.lan, f.host, QUEUE).unwrap();
            assert_eq!(queue.objects[0].title, "First <&>");
            assert_eq!(queue.objects[1].title, SELECTED[1]);
            assert_eq!(queue.objects[2].title, "First again");
            assert_eq!(get_play_mode(&f.lan, f.host).unwrap(), PlayMode::Normal);
            assert_eq!(soap::get_position_info(&f.lan, f.host).unwrap().track, 1);
            assert_eq!(
                soap::get_transport_info(&f.lan, f.host).unwrap().state == TransportState::Playing,
                start
            );
            let log = f.sim.soap_log();
            let writes: Vec<_> = log[before..]
                .iter()
                .filter(|call| !call.action.starts_with("Get") && call.action != "Browse")
                .map(|call| call.action.as_str())
                .collect();
            assert_eq!(
                writes
                    .iter()
                    .filter(|action| **action == "AddURIToQueue")
                    .count(),
                3
            );
            assert_eq!(
                writes.iter().filter(|action| **action == "Play").count(),
                usize::from(start)
            );
            assert!(!writes.contains(&"RemoveAllTracksFromQueue"));
            if !start {
                let silence = writes
                    .iter()
                    .position(|action| matches!(*action, "Pause" | "Stop"))
                    .unwrap();
                let first_add = writes
                    .iter()
                    .position(|action| *action == "AddURIToQueue")
                    .unwrap();
                assert!(silence < first_add, "{writes:?}");
            }
        }
    }
}

#[test]
fn selections_follow_confirmed_regrouping_and_failed_moves_do_not_block_independent_music() {
    for generation in [1, 2] {
        for refused in [false, true] {
            let f = Fixture::new(generation);
            let authored = scene(vec![
                group("Member", &["Lead"], true),
                group("Independent", &[], false),
            ]);
            let plan = f.plan(&authored);
            if refused {
                f.sim.ignore_action("Member", "SetAVTransportURI").unwrap();
            }
            let before = f.sim.soap_log().len();
            let report = scenes::apply(&f.lan, &f.houses, &plan, &ApplyOptions::default());
            assert_eq!(report.is_complete(), !refused, "{report:?}");
            assert_eq!(f.queue("Independent"), SELECTED);
            if refused {
                assert!(report.failed.iter().any(|(op, _)| matches!(op, SceneOp::Tracks { coordinator, .. } if *coordinator == f.id("Member"))));
                assert!(
                    !f.sim.soap_log()[before..]
                        .iter()
                        .any(|call| call.room != "Independent" && call.action == "AddURIToQueue")
                );
                assert_eq!(f.queue("Lead"), [OLD, OLD]);
            } else {
                assert_eq!(f.queue("Member"), SELECTED);
                let host = f.sim.player("Member").unwrap().ip;
                assert_eq!(
                    soap::get_transport_info(&f.lan, host).unwrap().state,
                    TransportState::Playing
                );
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Interference {
    Source,
    Audience,
    Queue,
    RefusedSecondAdd,
}

struct Changes<'a> {
    f: &'a Fixture,
    kind: Interference,
    changed: Cell<bool>,
    adds: Cell<usize>,
    writes: RefCell<Vec<String>>,
}

impl Transport for Changes<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        if !name.starts_with("Get") && name != "Browse" {
            self.writes.borrow_mut().push(name.into());
        }
        if host == self.f.host && name == "AddURIToQueue" {
            self.adds.set(self.adds.get() + 1);
            if self.adds.get() == 2 && matches!(self.kind, Interference::RefusedSecondAdd) {
                return Err(ProtoError::SoapFault {
                    code: 800,
                    reason: "second selection entry refused".into(),
                });
            }
        }
        let reply = self.f.lan.soap_post(host, path, action, body)?;
        if host == self.f.host
            && name == "Browse"
            && body.contains("<ObjectID>FV:2</ObjectID>")
            && !matches!(self.kind, Interference::RefusedSecondAdd)
            && !self.changed.replace(true)
        {
            match self.kind {
                Interference::Source => soap::set_av_transport_uri(&self.f.lan, host, OTHER, "")?,
                Interference::Audience => soap::join_group(
                    &self.f.lan,
                    self.f.sim.player("Independent").unwrap().ip,
                    &self.f.lead,
                )?,
                Interference::Queue => {
                    soap::add_uri_to_queue(&self.f.lan, host, OTHER, "", false)?;
                }
                Interference::RefusedSecondAdd => unreachable!(),
            }
        }
        Ok(reply)
    }
}

#[test]
fn renderer_learning_cannot_adopt_a_new_source_audience_or_owner_queue_edit() {
    for generation in [1, 2] {
        for kind in [
            Interference::Source,
            Interference::Audience,
            Interference::Queue,
        ] {
            let f = Fixture::new(generation);
            let plan = f.plan(&scene(vec![group("Lead", &["Member"], true)]));
            let changes = Changes {
                f: &f,
                kind,
                changed: Cell::new(false),
                adds: Cell::new(0),
                writes: RefCell::new(Vec::new()),
            };
            let report = scenes::apply(&changes, &f.houses, &plan, &ApplyOptions::default());
            assert!(!report.is_complete(), "{report:?}");
            assert!(changes.changed.get());
            assert!(
                changes.writes.borrow().is_empty(),
                "{:?}",
                changes.writes.borrow()
            );
            assert_eq!(
                f.queue("Lead"),
                if matches!(kind, Interference::Queue) {
                    vec![OLD, OLD, OTHER]
                } else {
                    vec![OLD, OLD]
                }
            );
        }
    }
}

#[test]
fn an_incomplete_selection_is_never_replayed_started_or_allowed_to_fall_through_to_play() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let mut plan = f.plan(&scene(vec![
            group("Lead", &["Member"], true),
            group("Independent", &[], false),
        ]));
        plan.ops.insert(
            1,
            SceneOp::Play {
                coordinator: f.lead.clone(),
            },
        );
        let changes = Changes {
            f: &f,
            kind: Interference::RefusedSecondAdd,
            changed: Cell::new(false),
            adds: Cell::new(0),
            writes: RefCell::new(Vec::new()),
        };
        let report = scenes::apply(&changes, &f.houses, &plan, &ApplyOptions::default());
        assert_eq!(report.failed.len(), 2, "{report:?}");
        assert_eq!(changes.adds.get(), 2);
        assert_eq!(f.queue("Lead"), [OLD, OLD, SELECTED[0]]);
        assert_eq!(f.queue("Independent"), SELECTED);
        assert!(
            !changes
                .writes
                .borrow()
                .iter()
                .any(|write| matches!(write.as_str(), "Play" | "RemoveAllTracksFromQueue"))
        );
        assert_eq!(soap::get_media_info(&f.lan, f.host).unwrap().uri, RADIO);
    }
}

#[test]
fn missing_local_renderer_readiness_is_reported_before_any_scene_write() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let mut authored = scene(vec![group("Member", &["Lead"], true)]);
        authored.volumes.insert("Lead".into(), 1);
        let plan = scenes::plan_apply(&authored, &f.houses, &[]).unwrap();
        f.sim.retain_favorites(generation, |_| false).unwrap();
        let before = f.sim.soap_log().len();
        assert!(matches!(
            scenes::check_sources(&f.lan, &f.houses, &plan),
            Err(scenes::SceneError::Core(CoreError::NoSpotifyFavorite))
        ));
        assert!(
            f.sim.soap_log()[before..]
                .iter()
                .all(|call| call.action.starts_with("Get") || call.action == "Browse")
        );
        assert_eq!(soap::get_media_info(&f.lan, f.host).unwrap().uri, RADIO);
        assert_eq!(
            soap::get_transport_info(&f.lan, f.host).unwrap().state,
            TransportState::Playing
        );
    }
}
