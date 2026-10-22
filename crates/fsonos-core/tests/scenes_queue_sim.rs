//! Queue scene bookmarks preserve occurrences and refuse stale playback
//! contexts. No test relies on clearing or reconstructing a saved queue.

use fsonos_core::HouseholdState;
use fsonos_core::scenes::{self, ApplyOptions, Scene, SceneOp, SceneSource};
use fsonos_core::snapshot;
use fsonos_core::store::MemStore;
use fsonos_proto::content::browse_snapshot;
use fsonos_proto::control as soap;
use fsonos_proto::play_mode::{get_play_mode, set_play_mode};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayMode, PlayerId, TransportState};
use std::cell::Cell;
use std::net::IpAddr;
use std::time::Duration;

const RADIO: &str = "x-rincon-mp3radio://example.invalid/other.mp3";
const DUPLICATE: &str = "x-file-cifs://example.invalid/repeated.flac";

struct Fixture {
    sim: SimHandle,
    lan: SimLan,
    houses: Vec<HouseholdState>,
    coordinator: PlayerId,
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
        let coordinator = PlayerId(sim.player("Alpha").unwrap().uuid.clone());
        soap::join_group(&lan, sim.player("Beta").unwrap().ip, &coordinator).unwrap();
        for uri in [
            DUPLICATE,
            DUPLICATE,
            "x-file-cifs://example.invalid/last.flac",
        ] {
            soap::add_uri_to_queue(&lan, host, uri, "", false).unwrap();
        }
        soap::play_from_queue(&lan, host, &coordinator).unwrap();
        soap::seek_track(&lan, host, 2).unwrap();
        soap::seek_position(&lan, host, 17).unwrap();
        set_play_mode(&lan, host, PlayMode::RepeatAll).unwrap();
        soap::play(&lan, host).unwrap();
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&lan, host).unwrap());
        Self {
            sim,
            lan,
            houses: vec![house],
            coordinator,
            host,
        }
    }

    fn capture(&self) -> Scene {
        scenes::capture(&self.lan, &self.houses, "Queue evening", &[], 1).unwrap()
    }

    fn radio(&self) {
        soap::set_av_transport_uri(&self.lan, self.host, RADIO, "").unwrap();
        set_play_mode(&self.lan, self.host, PlayMode::Normal).unwrap();
        soap::play(&self.lan, self.host).unwrap();
    }

    fn apply<T: Transport + ?Sized>(&self, transport: &T, scene: &Scene) -> scenes::SceneReport {
        let snaps: Vec<_> = scenes::zones(scene, &self.houses)
            .iter()
            .map(|id| snapshot::capture(&self.lan, &self.houses, id, 2).unwrap())
            .collect();
        let plan = scenes::plan_apply(scene, &self.houses, &snaps).unwrap();
        assert!(
            plan.ops
                .iter()
                .all(|op| !matches!(op, SceneOp::Play { .. } | SceneOp::Pause { .. })),
            "{plan:?}"
        );
        scenes::apply(transport, &self.houses, &plan, &ApplyOptions::default())
    }

    fn writes_since(&self, start: usize) -> Vec<String> {
        self.sim.soap_log()[start..]
            .iter()
            .filter(|call| !call.action.starts_with("Get") && call.action != "Browse")
            .map(|call| call.action.clone())
            .collect()
    }
}

#[test]
fn a_queue_bookmark_round_trips_duplicates_mode_and_time_without_editing_either_generation() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let before = browse_snapshot(&f.lan, f.host, "Q:0").unwrap();
        let scene = f.capture();
        let SceneSource::Queue { queue } = &scene.groups[0].source else {
            panic!("{scene:?}")
        };
        assert_eq!(queue.coordinator, f.coordinator);
        assert_eq!(
            (queue.track, queue.position_secs, queue.boot_seq),
            (2, 17, Some(1))
        );
        assert_eq!(queue.play_mode, Some(PlayMode::RepeatAll));
        let mut store = MemStore::default();
        scenes::save(&mut store, &scene, 1).unwrap();
        let scene = scenes::load(&store, "Queue evening").unwrap();
        f.radio();
        let start = f.sim.soap_log().len();
        let report = f.apply(&f.lan, &scene);
        assert!(report.is_complete(), "{report:?}");
        assert!(matches!(
            report.done.as_slice(),
            [SceneOp::Queue { start: true, .. }]
        ));
        let position = soap::get_position_info(&f.lan, f.host).unwrap();
        assert_eq!((position.track, position.position_secs), (2, Some(17)));
        assert_eq!(position.uri, DUPLICATE);
        assert_eq!(get_play_mode(&f.lan, f.host).unwrap(), PlayMode::RepeatAll);
        assert_eq!(
            soap::get_transport_info(&f.lan, f.host).unwrap().state,
            TransportState::Playing
        );
        assert_eq!(browse_snapshot(&f.lan, f.host, "Q:0").unwrap(), before);
        assert!(
            f.writes_since(start)
                .iter()
                .all(|name| !name.contains("Queue")),
            "queue contents were edited"
        );
        let start = f.sim.soap_log().len();
        let again = f.apply(&f.lan, &scene);
        assert!(again.is_complete() && again.done.is_empty(), "{again:?}");
        assert!(f.writes_since(start).is_empty());
    }
}

#[test]
fn a_paused_bookmark_and_the_beginning_of_its_track_are_recalled_without_play() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        soap::pause(&f.lan, f.host).unwrap();
        soap::seek_position(&f.lan, f.host, 0).unwrap();
        let scene = f.capture();
        f.radio();
        let start = f.sim.soap_log().len();
        let report = f.apply(&f.lan, &scene);
        assert!(report.is_complete(), "{report:?}");
        assert!(!f.writes_since(start).iter().any(|name| name == "Play"));
        let position = soap::get_position_info(&f.lan, f.host).unwrap();
        assert_eq!((position.track, position.position_secs), (2, Some(0)));
        assert_ne!(
            soap::get_transport_info(&f.lan, f.host).unwrap().state,
            TransportState::Playing
        );
        assert!(f.sim.soap_log()[start..].iter().any(|call| {
            call.action == "Seek"
                && call.args.contains(&("Unit".into(), "REL_TIME".into()))
                && call
                    .args
                    .iter()
                    .any(|(name, value)| name == "Target" && soap::parse_hms(value) == Some(0))
        }));
        // A stopped one-second offset is not natural playback progress and
        // cannot make an explicit zero bookmark look unchanged.
        soap::seek_position(&f.lan, f.host, 1).unwrap();
        let again = f.apply(&f.lan, &scene);
        assert!(again.is_complete() && !again.done.is_empty(), "{again:?}");
        assert_eq!(
            soap::get_position_info(&f.lan, f.host)
                .unwrap()
                .position_secs,
            Some(0)
        );
    }
}

#[test]
fn a_changed_queue_or_reboot_refuses_recall_before_any_playback_write() {
    for reboot in [false, true] {
        let f = Fixture::new(2);
        let scene = f.capture();
        f.radio();
        if reboot {
            f.sim.reboot("Alpha", Duration::ZERO).unwrap();
        } else {
            soap::add_uri_to_queue(
                &f.lan,
                f.host,
                "x-file-cifs://example.invalid/new.flac",
                "",
                false,
            )
            .unwrap();
        }
        let start = f.sim.soap_log().len();
        let report = f.apply(&f.lan, &scene);
        assert!(
            !report.is_complete() && report.done.is_empty(),
            "{report:?}"
        );
        let why = &report.failed[0].1;
        assert!(
            why.contains(if reboot { "rebooted" } else { "queue changed" }),
            "{why}"
        );
        assert!(f.writes_since(start).is_empty());
        assert_eq!(soap::get_media_info(&f.lan, f.host).unwrap().uri, RADIO);
    }
}

#[test]
fn matching_room_names_do_not_retarget_a_queue_reference_to_another_player() {
    let f = Fixture::new(2);
    let mut scene = f.capture();
    let SceneSource::Queue { queue } = &mut scene.groups[0].source else {
        panic!("missing queue")
    };
    queue.coordinator = PlayerId("RINCON_ORIGINAL_PLAYER_NO_LONGER_PRESENT".into());
    f.radio();
    let start = f.sim.soap_log().len();
    let report = f.apply(&f.lan, &scene);
    assert!(!report.is_complete(), "{report:?}");
    assert!(report.failed[0].1.contains("original coordinator differs"));
    assert!(f.writes_since(start).is_empty());
}

#[derive(Clone, Copy)]
enum Interference {
    QueueEdit,
    OwnerSeek,
    GroupLeave,
    ModeChange,
    OwnerPlay,
    LostSourceReply,
}

struct Interleaved<'a> {
    fixture: &'a Fixture,
    event: Interference,
    fired: Cell<bool>,
}

impl Transport for Interleaved<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let result = self.fixture.lan.soap_post(host, path, action, body);
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        let trigger = match self.event {
            Interference::OwnerSeek => name == "Seek" && body.contains("<Unit>TRACK_NR</Unit>"),
            Interference::ModeChange => name == "SetPlayMode",
            _ => name == "SetAVTransportURI",
        };
        if host == self.fixture.host && trigger && !self.fired.replace(true) {
            let lan = &self.fixture.lan;
            match self.event {
                Interference::QueueEdit => {
                    soap::add_uri_to_queue(
                        lan,
                        host,
                        "x-file-cifs://example.invalid/race.flac",
                        "",
                        false,
                    )?;
                }
                Interference::OwnerSeek => soap::seek_position(lan, host, 99)?,
                Interference::GroupLeave => {
                    let member = self.fixture.sim.player("Beta").unwrap();
                    soap::leave_group(lan, member.ip, &PlayerId(member.uuid.clone()))?;
                }
                Interference::ModeChange => set_play_mode(lan, host, PlayMode::Normal)?,
                Interference::OwnerPlay => soap::play(lan, host)?,
                Interference::LostSourceReply => {
                    return Err(ProtoError::Network {
                        target: host.to_string(),
                        detail: "injected lost source reply".into(),
                    });
                }
            }
        }
        result
    }
}

#[test]
fn queue_group_time_and_mode_changes_between_writes_block_remaining_playback() {
    for event in [
        Interference::QueueEdit,
        Interference::OwnerSeek,
        Interference::GroupLeave,
        Interference::ModeChange,
    ] {
        let f = Fixture::new(2);
        let scene = f.capture();
        f.radio();
        let transport = Interleaved {
            fixture: &f,
            event,
            fired: Cell::new(false),
        };
        let start = f.sim.soap_log().len();
        let report = f.apply(&transport, &scene);
        assert!(transport.fired.get());
        assert!(!report.is_complete(), "{report:?}");
        assert!(
            !f.writes_since(start).iter().any(|name| name == "Play"),
            "{report:?}"
        );
    }
}

#[test]
fn a_lost_source_reply_is_proven_without_repeating_source_selection() {
    let f = Fixture::new(2);
    let scene = f.capture();
    f.radio();
    let transport = Interleaved {
        fixture: &f,
        event: Interference::LostSourceReply,
        fired: Cell::new(false),
    };
    let start = f.sim.soap_log().len();
    let report = f.apply(&transport, &scene);
    assert!(report.is_complete(), "{report:?}");
    assert_eq!(
        f.writes_since(start)
            .iter()
            .filter(|name| name.as_str() == "SetAVTransportURI")
            .count(),
        1
    );
    assert_eq!(soap::get_position_info(&f.lan, f.host).unwrap().track, 2);
}

#[test]
fn a_failed_fence_cannot_be_recovered_by_source_only_readback() {
    let f = Fixture::new(2);
    let scene = f.capture();
    // The owner has chosen a different occurrence in the same unchanged
    // queue. An injected Play after our source write recreates this exact
    // prior state, so later reads alone cannot erase the failed fence.
    soap::seek_track(&f.lan, f.host, 1).unwrap();
    set_play_mode(&f.lan, f.host, PlayMode::Normal).unwrap();
    let transport = Interleaved {
        fixture: &f,
        event: Interference::OwnerPlay,
        fired: Cell::new(false),
    };
    let start = f.sim.soap_log().len();
    let report = f.apply(&transport, &scene);
    assert!(transport.fired.get());
    assert!(!report.is_complete(), "{report:?}");
    // The sole Play belongs to the injected owner, not scene recall. No
    // mode change, seek or fallback Play may follow the failed proof.
    assert_eq!(f.writes_since(start), ["SetAVTransportURI", "Play"]);
    assert_eq!(soap::get_position_info(&f.lan, f.host).unwrap().track, 1);
    assert_eq!(get_play_mode(&f.lan, f.host).unwrap(), PlayMode::Normal);
}

#[test]
fn an_empty_queue_remains_keep_without_a_fabricated_occurrence() {
    let f = Fixture::new(2);
    soap::remove_all_tracks_from_queue(&f.lan, f.host).unwrap();
    let scene = f.capture();
    assert_eq!(scene.groups[0].source, SceneSource::Keep);
    assert_eq!(scene.groups[0].playing, Some(false));
}

struct Unselected<'a>(&'a SimLan);

impl Transport for Unselected<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let mut response = self.0.soap_post(host, path, action, body)?;
        if action.contains("#GetPositionInfo") {
            response = response.replace("<Track>2</Track>", "<Track>0</Track>");
            if let (Some(start), Some(end)) =
                (response.find("<TrackURI>"), response.find("</TrackURI>"))
            {
                response.replace_range(start + "<TrackURI>".len()..end, "");
            }
        }
        Ok(response)
    }
}

#[test]
fn a_nonempty_queue_without_a_selection_is_not_silently_saved_as_keep() {
    let f = Fixture::new(2);
    soap::pause(&f.lan, f.host).unwrap();
    let error = scenes::capture(&Unselected(&f.lan), &f.houses, "Unselected", &[], 1).unwrap_err();
    assert!(error.to_string().contains("selected occurrence"), "{error}");
}
