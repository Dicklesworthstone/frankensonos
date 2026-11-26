//! One native input reaches a retained set of independent groups. Every source
//! and destination is prepared before any mutation, and the shared executor
//! preserves per-group results, caps, native queues and one undo boundary.

use fsonos_api::{ControlResultDto, PlayLineInRequest, Surface};
use fsonos_core::actions::BeforeState;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::rooms::ControlTarget;
use fsonos_core::store::{ActionFilter, MemStore};
use fsonos_core::{HouseholdState, inventory, resolve_room};
use fsonos_proto::content::{QUEUE, browse_all};
use fsonos_proto::control as soap;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec, SoapLogEntry};
use fsonos_types::{PlayerId, TransportState};
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const CAP: u8 = 25;
const ORIGINAL: [&str; 2] = [
    "x-file-cifs://example.invalid/first.flac",
    "x-file-cifs://example.invalid/second.flac",
];
const EXTERNAL: &str = "x-file-cifs://example.invalid/new-owner.flac";

struct House {
    sim: SimHandle,
    households: Vec<HouseholdState>,
    directory: tempfile::TempDir,
}

impl House {
    fn new() -> Self {
        let players = |model| {
            [
                SimPlayerSpec::new("Lead", model),
                SimPlayerSpec::new("Member", model),
                SimPlayerSpec::new("Guest", model),
                SimPlayerSpec::pair("Turntable", model),
            ]
        };
        let sim = SimHousehold::builder()
            .s1(players(SimModel::Play5Gen1))
            .s2(players(SimModel::Play5Gen2))
            .spawn()
            .unwrap();
        for generation in [1, 2] {
            let player = |room| {
                sim.players()
                    .iter()
                    .find(|player| player.generation == generation && player.room == room)
                    .unwrap()
            };
            soap::join_group(
                &sim.lan(),
                player("Member").ip,
                &PlayerId(player("Lead").uuid.clone()),
            )
            .unwrap();
        }
        let households = inventory::survey(&sim.lan(), &[], Duration::from_millis(500))
            .unwrap()
            .households;
        let house = Self {
            sim,
            households,
            directory: tempfile::tempdir().unwrap(),
        };
        for generation in [1, 2] {
            for room in ["Lead", "Guest", "Turntable"] {
                let target = house.target(room, generation);
                for uri in ORIGINAL {
                    soap::add_uri_to_queue(&house.sim.lan(), target.coordinator.ip, uri, "", false)
                        .unwrap();
                }
                soap::play_from_queue(
                    &house.sim.lan(),
                    target.coordinator.ip,
                    &target.coordinator.id,
                )
                .unwrap();
                soap::seek_track(&house.sim.lan(), target.coordinator.ip, 2).unwrap();
                soap::seek_position(&house.sim.lan(), target.coordinator.ip, 17).unwrap();
                soap::play(&house.sim.lan(), target.coordinator.ip).unwrap();
                soap::pause(&house.sim.lan(), target.coordinator.ip).unwrap();
            }
        }
        for player in house.sim.players() {
            soap::set_volume(&house.sim.lan(), player.ip, 80).unwrap();
        }
        house
    }

    fn target(&self, room: &str, generation: u8) -> ControlTarget<'_> {
        resolve_room(&self.households, &format!("{room}@S{generation}")).unwrap()
    }

    fn aliases(&self, generation: u8) {
        std::fs::write(
            self.directory.path().join("aliases.toml"),
            format!(
                "[aliases]\nselected = ['Member@S{generation}', 'Lead@S{generation}', \
                 'Guest@S{generation}']\none_group = ['Member@S{generation}', 'Lead@S{generation}']\n\
                 input = ['Turntable@S{generation}']\n"
            ),
        )
        .unwrap();
    }

    fn surface(&self) -> Surface {
        self.surface_over(Box::new(self.sim.lan()))
    }

    fn surface_over(&self, transport: Box<dyn Transport + Send + Sync>) -> Surface {
        let known = self.households.clone();
        let mut policy = Policy::default();
        policy.defaults.max_volume = CAP;
        Surface::new(
            transport,
            Box::new(move |_| Ok(known.clone())),
            policy,
            Box::new(SystemClock),
        )
        .with_aliases_file(self.directory.path().join("aliases.toml"))
        .with_action_log(Box::new(MemStore::default()), "http")
    }

    fn queue(&self, room: &str, generation: u8) -> Vec<String> {
        browse_all(
            &self.sim.lan(),
            self.target(room, generation).coordinator.ip,
            QUEUE,
        )
        .unwrap()
        .into_iter()
        .map(|item| item.res.unwrap().uri)
        .collect()
    }

    fn writes_since(&self, start: usize) -> Vec<SoapLogEntry> {
        self.sim
            .soap_log()
            .into_iter()
            .skip(start)
            .filter(|call| call.action != "Browse" && !call.action.starts_with("Get"))
            .collect()
    }

    fn original(&self, room: &str, generation: u8) {
        let host = self.target(room, generation).coordinator.ip;
        let position = soap::get_position_info(&self.sim.lan(), host).unwrap();
        assert_eq!(self.queue(room, generation), ORIGINAL);
        assert_eq!((position.track, position.position_secs), (2, Some(17)));
        assert_eq!(position.uri, ORIGINAL[1]);
        assert!(matches!(
            soap::get_transport_info(&self.sim.lan(), host)
                .unwrap()
                .state,
            TransportState::Paused | TransportState::Stopped
        ));
    }

    fn playing(&self, room: &str, generation: u8, source: &PlayerId) {
        let host = self.target(room, generation).coordinator.ip;
        assert_eq!(
            soap::get_media_info(&self.sim.lan(), host).unwrap().uri,
            format!("x-rincon-stream:{}", source.0)
        );
        assert_eq!(
            soap::get_transport_info(&self.sim.lan(), host)
                .unwrap()
                .state,
            TransportState::Playing
        );
        assert_eq!(self.queue(room, generation), ORIGINAL);
    }
}

fn request(zone: &str, source: &str) -> PlayLineInRequest {
    PlayLineInRequest {
        zone: zone.into(),
        source: source.into(),
    }
}

#[test]
fn aliases_play_an_exact_bonded_input_once_per_group_with_caps_and_one_undo() {
    for generation in [1, 2] {
        let house = House::new();
        house.aliases(generation);
        let surface = house.surface();
        let input = house.target("Turntable", generation);
        let secondary = input
            .room
            .players
            .iter()
            .find(|id| **id != input.player.id)
            .unwrap()
            .clone();
        let start = house.sim.soap_log().len();
        let outcome = surface
            .play_line_in(
                &Client::McpStdio,
                &request("selected", &secondary.0.to_ascii_lowercase()),
            )
            .unwrap();
        assert!(outcome.changed && outcome.complete());
        let batch = outcome.batch.unwrap();
        assert_eq!(batch.results.len(), 2);
        for room in ["Lead", "Guest"] {
            let target = house.target(room, generation);
            assert!(batch.results.iter().any(|result| {
                result.target == target.coordinator.id.0 && result.result.complete()
            }));
            house.playing(room, generation, &secondary);
        }
        for room in ["Lead", "Member", "Guest"] {
            assert_eq!(
                soap::get_volume(&house.sim.lan(), house.target(room, generation).player.ip)
                    .unwrap(),
                CAP
            );
        }
        house.original("Turntable", generation);
        assert_eq!(
            soap::get_volume(&house.sim.lan(), input.player.ip).unwrap(),
            80
        );
        for room in ["Lead", "Guest", "Turntable"] {
            house.original(room, 3 - generation);
        }
        let writes = house.writes_since(start);
        for verb in ["SetAVTransportURI", "Play"] {
            assert_eq!(writes.iter().filter(|call| call.action == verb).count(), 2);
        }
        assert!(!writes.iter().any(|call| call.action.contains("Queue")));
        let actions = surface
            .recent_actions(&Client::McpStdio, &ActionFilter::default())
            .unwrap();
        assert_eq!(actions.len(), 1);
        let before =
            BeforeState::parse(actions[0].action.before_state.as_deref().unwrap()).unwrap();
        assert!(before.not_undoable.is_none());
        let restored = surface.undo(&Client::McpStdio, true).unwrap().unwrap();
        assert!(restored.failures.is_empty(), "{restored:?}");
        for room in ["Lead", "Guest"] {
            house.original(room, generation);
        }
    }
}

#[test]
fn a_room_set_with_one_group_keeps_the_single_result_shape() {
    let house = House::new();
    house.aliases(1);
    let surface = house.surface();
    let start = house.sim.soap_log().len();
    let outcome = surface
        .play_line_in(&Client::McpStdio, &request("one_group", "input"))
        .unwrap();
    assert!(outcome.changed && outcome.complete());
    assert!(outcome.batch.is_none());
    let source = &house.target("Turntable", 1).player.id;
    house.playing("Lead", 1, source);
    house.original("Guest", 1);
    assert_eq!(
        house
            .writes_since(start)
            .iter()
            .filter(|call| call.action == "SetAVTransportURI")
            .count(),
        1
    );
}

#[test]
fn qualified_everywhere_plays_all_independent_groups_in_only_the_sources_household() {
    for generation in [1, 2] {
        let house = House::new();
        let surface = house.surface();
        let source = &house.target("Turntable", generation).player.id;
        let outcome = surface
            .play_line_in(
                &Client::McpStdio,
                &request(&format!("all@S{generation}"), &source.0),
            )
            .unwrap();
        assert!(outcome.changed && outcome.complete());
        assert_eq!(outcome.batch.unwrap().results.len(), 3);
        for room in ["Lead", "Guest", "Turntable"] {
            house.playing(room, generation, source);
            house.original(room, 3 - generation);
        }
    }
}

#[test]
fn cross_household_sets_fail_before_any_caps_or_playback_changes() {
    let house = House::new();
    let surface = house.surface();
    let start = house.sim.soap_log().len();
    assert!(
        surface
            .play_line_in(&Client::McpStdio, &request("all", "Turntable@S1"))
            .is_err()
    );
    assert!(house.writes_since(start).is_empty());
    for generation in [1, 2] {
        for room in ["Lead", "Guest", "Turntable"] {
            house.original(room, generation);
        }
        for room in ["Lead", "Member", "Guest", "Turntable"] {
            assert_eq!(
                soap::get_volume(&house.sim.lan(), house.target(room, generation).player.ip)
                    .unwrap(),
                80
            );
        }
    }
}

struct DuringPreparation {
    lan: SimLan,
    trigger: IpAddr,
    changed_player: IpAddr,
    changed: Arc<AtomicBool>,
}

impl Transport for DuringPreparation {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let reply = self.lan.soap_post(host, path, action, body)?;
        if host == self.trigger
            && action.contains("#GetMediaInfo")
            && !self.changed.swap(true, Ordering::SeqCst)
        {
            soap::set_av_transport_uri(&self.lan, self.changed_player, EXTERNAL, "")?;
        }
        Ok(reply)
    }

    fn http_get(&self, url: &str) -> Result<String, ProtoError> {
        self.lan.http_get(url)
    }
}

#[test]
fn preparing_a_later_group_cannot_hide_a_changed_earlier_destination() {
    let house = House::new();
    house.aliases(1);
    let lead = house.target("Lead", 1).coordinator.ip;
    let changed = Arc::new(AtomicBool::new(false));
    let surface = house.surface_over(Box::new(DuringPreparation {
        lan: house.sim.lan(),
        trigger: house.target("Guest", 1).coordinator.ip,
        changed_player: lead,
        changed: Arc::clone(&changed),
    }));
    let start = house.sim.soap_log().len();
    assert!(
        surface
            .play_line_in(&Client::McpStdio, &request("selected", "input"))
            .is_err()
    );
    assert!(changed.load(Ordering::SeqCst));
    let writes = house.writes_since(start);
    assert_eq!(
        writes.len(),
        1,
        "only the newer owner's source may be written"
    );
    assert_eq!(writes[0].action, "SetAVTransportURI");
    assert_eq!(
        soap::get_media_info(&house.sim.lan(), lead).unwrap().uri,
        EXTERNAL
    );
    house.original("Guest", 1);
    for room in ["Lead", "Member", "Guest", "Turntable"] {
        assert_eq!(
            soap::get_volume(&house.sim.lan(), house.target(room, 1).player.ip).unwrap(),
            80
        );
    }
}

#[test]
fn a_refused_group_retains_the_successful_groups_receipt_and_one_action() {
    let house = House::new();
    house.aliases(1);
    let surface = house.surface();
    let failed = house.target("Guest", 1).coordinator.id.clone();
    house
        .sim
        .upnp_fault(&failed.0, "SetAVTransportURI", 701)
        .unwrap();
    let start = house.sim.soap_log().len();
    let outcome = surface
        .play_line_in(&Client::McpStdio, &request("selected", "input"))
        .unwrap();
    assert!(outcome.changed && !outcome.complete());
    let batch = outcome.batch.unwrap();
    assert_eq!(batch.results.len(), 2);
    assert!(batch.results.iter().any(|result| {
        result.target == failed.0 && matches!(result.result, ControlResultDto::Failed { .. })
    }));
    let source = &house.target("Turntable", 1).player.id;
    house.playing("Lead", 1, source);
    house.original("Guest", 1);
    let writes = house.writes_since(start);
    assert_eq!(
        writes
            .iter()
            .filter(|call| call.player == failed.0 && call.action == "SetAVTransportURI")
            .count(),
        1,
        "a failed set member is never replayed"
    );
    assert!(
        !writes
            .iter()
            .any(|call| call.player == failed.0 && call.action == "Play")
    );
    assert_eq!(
        surface
            .recent_actions(&Client::McpStdio, &ActionFilter::default())
            .unwrap()
            .len(),
        1
    );
}
