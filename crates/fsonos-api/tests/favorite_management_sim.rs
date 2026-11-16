//! Favorites authoring crosses the actual surface/policy/logging and native
//! SOAP paths. Both households retain playback while their catalogs change.

use fsonos_api::surface::favorite_management::{
    FavoriteOperation, InspectFavoriteRequest, RemoveFavoriteRequest, RenameFavoriteRequest,
    SaveFavoriteRequest,
};
use fsonos_api::{ErrorCode, Surface};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::{ActionFilter, MemStore};
use fsonos_core::{HouseholdState, control, inventory, resolve_room};
use fsonos_proto::control as soap;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use std::net::IpAddr;
use std::time::Duration;

fn fixture() -> (SimHandle, Vec<HouseholdState>) {
    let sim = SimHousehold::builder()
        .s1([SimPlayerSpec::new("Den", SimModel::Play5Gen1)])
        .s2([SimPlayerSpec::new("Den", SimModel::One)])
        .spawn()
        .unwrap();
    let houses = inventory::survey(&sim.lan(), &[], Duration::from_millis(500))
        .unwrap()
        .households;
    (sim, houses)
}

fn surface(
    transport: Box<dyn Transport + Send + Sync>,
    houses: &[HouseholdState],
    policy: Policy,
) -> Surface {
    let known = houses.to_vec();
    Surface::new(
        transport,
        Box::new(move |_| Ok(known.clone())),
        policy,
        Box::new(SystemClock),
    )
    .with_action_log(Box::new(MemStore::default()), "favorite-management")
}

fn start_stream(sim: &SimHandle, houses: &[HouseholdState], zone: &str, path: &str) {
    let target = resolve_room(houses, zone).unwrap();
    soap::set_av_transport_uri(
        &sim.lan(),
        target.coordinator.ip,
        &format!("https://stream.example.invalid/{path}"),
        "",
    )
    .unwrap();
    soap::play(&sim.lan(), target.coordinator.ip).unwrap();
}

#[test]
fn favorite_save_rename_and_remove_are_household_local_and_leave_playback_running() {
    for generation in ["S1", "S2"] {
        let (sim, houses) = fixture();
        let zone = format!("Den@{generation}");
        let other = if generation == "S1" {
            "Den@S2"
        } else {
            "Den@S1"
        };
        start_stream(&sim, &houses, &zone, "station.mp3");
        let s = surface(Box::new(sim.lan()), &houses, Policy::default());
        let target = resolve_room(&houses, &zone).unwrap();
        let playback = control::playback(&sim.lan(), &houses, &target.coordinator.id).unwrap();
        let other_catalog = s.favorites(&Client::Cli, other).unwrap();
        let observed = s.current_favorite(&Client::McpStdio, &zone).unwrap();
        assert!(observed.favorite.id.is_empty());
        assert_eq!(observed.favorite.kind, "stream");
        let start = sim.soap_log().len();
        let saved = s
            .save_favorite(
                &Client::McpStdio,
                &SaveFavoriteRequest {
                    zone: zone.clone(),
                    expected_revision: observed.revision,
                    title: "  Café & radio  ".into(),
                },
            )
            .unwrap();
        assert_eq!(saved.operation, FavoriteOperation::Save);
        assert!(saved.changed && saved.logged);
        assert_eq!(saved.favorite.title, "Café & radio");
        let inspect = InspectFavoriteRequest {
            zone: zone.clone(),
            favorite: saved.favorite.id.clone(),
        };
        let current = s.inspect_favorite(&Client::McpStdio, &inspect).unwrap();
        let renamed = s
            .rename_favorite(
                &Client::McpStdio,
                &RenameFavoriteRequest {
                    zone: zone.clone(),
                    favorite: inspect.favorite.clone(),
                    expected_revision: current.revision,
                    title: "Night radio".into(),
                },
            )
            .unwrap();
        assert!(renamed.changed && renamed.logged);
        assert_eq!(renamed.favorite.id, saved.favorite.id);
        let current = s.inspect_favorite(&Client::McpStdio, &inspect).unwrap();
        let noop = s
            .rename_favorite(
                &Client::McpStdio,
                &RenameFavoriteRequest {
                    zone: zone.clone(),
                    favorite: inspect.favorite.clone(),
                    expected_revision: current.revision.clone(),
                    title: "Night radio".into(),
                },
            )
            .unwrap();
        assert!(!noop.changed);
        let removed = s
            .remove_favorite(
                &Client::McpStdio,
                &RemoveFavoriteRequest {
                    zone: zone.clone(),
                    favorite: inspect.favorite.clone(),
                    expected_revision: current.revision,
                },
            )
            .unwrap();
        assert_eq!(removed.favorite, renamed.favorite);
        assert!(removed.changed && removed.logged);
        let writes: Vec<_> = sim.soap_log()[start..]
            .iter()
            .filter(|call| !call.action.starts_with("Get") && call.action != "Browse")
            .map(|call| call.action.clone())
            .collect();
        assert_eq!(writes, ["CreateObject", "UpdateObject", "DestroyObject"]);
        assert_eq!(s.favorites(&Client::Cli, other).unwrap(), other_catalog);
        assert_eq!(
            control::playback(&sim.lan(), &houses, &target.coordinator.id).unwrap(),
            playback
        );
        let actions = s
            .recent_actions(&Client::Cli, &ActionFilter::default())
            .unwrap();
        assert_eq!(actions.len(), 4);
        assert!(
            actions
                .iter()
                .all(|row| row.action.client == "mcp-stdio" && row.action.before_state.is_none())
        );
    }
}

#[test]
fn changing_current_source_or_crossing_households_refuses_before_catalog_writes() {
    let (sim, houses) = fixture();
    let s = surface(Box::new(sim.lan()), &houses, Policy::default());
    start_stream(&sim, &houses, "Den@S1", "first.mp3");
    start_stream(&sim, &houses, "Den@S2", "first.mp3");
    let before = s.current_favorite(&Client::Cli, "Den@S1").unwrap();
    let request = SaveFavoriteRequest {
        zone: "Den@S2".into(),
        expected_revision: before.revision.clone(),
        title: "Selected".into(),
    };
    assert_eq!(
        s.save_favorite(&Client::Cli, &request).unwrap_err().code,
        ErrorCode::QueueChanged
    );
    start_stream(&sim, &houses, "Den@S1", "second.mp3");
    let request = SaveFavoriteRequest {
        zone: "Den@S1".into(),
        ..request
    };
    assert_eq!(
        s.save_favorite(&Client::Cli, &request).unwrap_err().code,
        ErrorCode::QueueChanged
    );
    assert!(
        sim.soap_log()
            .iter()
            .all(|call| call.action != "CreateObject")
    );
}

struct LoseReply(SimLan);
impl Transport for LoseReply {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let response = self.0.soap_post(host, path, action, body)?;
        if action.ends_with("#CreateObject\"") {
            Err(ProtoError::Network {
                target: host.to_string(),
                detail: "lost acknowledgement after persistence".into(),
            })
        } else {
            Ok(response)
        }
    }
}

#[test]
fn lost_favorite_save_acknowledgement_is_logged_once_and_never_replayed() {
    let (sim, houses) = fixture();
    start_stream(&sim, &houses, "Den@S1", "saved-once.mp3");
    let s = surface(Box::new(LoseReply(sim.lan())), &houses, Policy::default());
    let before = s.current_favorite(&Client::Cli, "Den@S1").unwrap();
    let failure = s
        .save_favorite(
            &Client::McpStdio,
            &SaveFavoriteRequest {
                zone: "Den@S1".into(),
                expected_revision: before.revision,
                title: "Save once".into(),
            },
        )
        .unwrap_err();
    assert_eq!(failure.code, ErrorCode::OutcomeUncertain);
    assert_eq!(
        sim.soap_log()
            .iter()
            .filter(|call| call.action == "CreateObject")
            .count(),
        1
    );
    assert_eq!(
        s.favorites(&Client::Cli, "Den@S1")
            .unwrap()
            .iter()
            .filter(|favorite| favorite.title == "Save once")
            .count(),
        1
    );
    let log = s
        .recent_actions(&Client::Cli, &ActionFilter::default())
        .unwrap();
    assert_eq!(log.len(), 1);
    assert!(log[0].action.result.starts_with("failed:"));
}

struct NoSpeakerIo;
impl Transport for NoSpeakerIo {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("policy must precede native I/O")
    }
}

#[test]
fn house_config_denial_and_current_source_read_permissions_precede_native_io() {
    let s = surface(
        Box::new(NoSpeakerIo),
        &[],
        Policy::from_toml("[clients.\"mcp-stdio\"]\nallow = ['read']").unwrap(),
    );
    assert_eq!(
        s.save_favorite(
            &Client::McpStdio,
            &SaveFavoriteRequest {
                zone: "Den".into(),
                expected_revision: "r".into(),
                title: "Song".into()
            }
        )
        .unwrap_err()
        .code,
        ErrorCode::PolicyDenied
    );
    assert_eq!(
        s.rename_favorite(
            &Client::McpStdio,
            &RenameFavoriteRequest {
                zone: "Den".into(),
                favorite: "FV:2/7".into(),
                expected_revision: "r".into(),
                title: "Song".into()
            }
        )
        .unwrap_err()
        .code,
        ErrorCode::PolicyDenied
    );
    assert_eq!(
        s.remove_favorite(
            &Client::McpStdio,
            &RemoveFavoriteRequest {
                zone: "Den".into(),
                favorite: "FV:2/7".into(),
                expected_revision: "r".into()
            }
        )
        .unwrap_err()
        .code,
        ErrorCode::PolicyDenied
    );
    let s = surface(
        Box::new(NoSpeakerIo),
        &[],
        Policy::from_toml("[clients.\"mcp-stdio\"]\nallow = ['list_favorites']").unwrap(),
    );
    assert_eq!(
        s.current_favorite(&Client::McpStdio, "Den")
            .unwrap_err()
            .code,
        ErrorCode::PolicyDenied
    );
}
