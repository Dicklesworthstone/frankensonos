use super::*;
use crate::zones::fixtures::households;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::Policy;
use fsonos_core::store::{ActionFilter, MemStore};
use fsonos_proto::{ProtoError, Transport};
use serde_json::json;
use std::net::IpAddr;
use std::time::Instant;

struct NoLan;
impl Transport for NoLan {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("room configuration must not contact speakers")
    }
}

fn fixture(policy: Policy, text: Option<&str>, cached: bool) -> (tempfile::TempDir, Surface) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aliases.toml");
    if let Some(text) = text {
        std::fs::write(&path, text).unwrap();
    }
    let surface = Surface::new(
        Box::new(NoLan),
        Box::new(|_| panic!("room configuration must not survey the LAN")),
        policy,
        Box::new(SystemClock),
    )
    .with_aliases_file(path)
    .with_action_log(Box::new(MemStore::default()), "room-config-test");
    if cached {
        *surface.cache.lock().unwrap() = Some((Instant::now(), households()));
    }
    (dir, surface)
}

fn request(
    configuration: &RoomConfigurationDto,
    change: RoomConfigurationChange,
) -> RoomConfigureRequest {
    RoomConfigureRequest {
        expected_revision: configuration.revision.clone(),
        change,
    }
}

fn alias(name: &str, rooms: &[&str]) -> RoomConfigurationChange {
    RoomConfigurationChange::SetAlias {
        name: name.into(),
        rooms: rooms.iter().map(|room| (*room).into()).collect(),
    }
}

fn apply(surface: &Surface, change: RoomConfigurationChange) -> RoomConfiguredDto {
    let current = surface.room_configuration(&Client::Cli).unwrap();
    surface
        .configure_rooms(&Client::Cli, &request(&current, change))
        .unwrap()
}

#[test]
fn aliases_and_client_defaults_immediately_change_resolution_without_playback() {
    let (dir, surface) = fixture(Policy::default(), Some("# Owner's room names.\n"), true);
    let created = apply(
        &surface,
        alias("Together", &["Patio@S2", "Kitchen@S1", "Patio@S2"]),
    );
    assert!(created.changed && created.configuration.warnings.is_empty());
    assert_eq!(
        created.configuration.aliases[0].rooms,
        ["Patio@S2", "Kitchen@S1", "Patio@S2"]
    );
    let resolved = surface.resolve_rooms(&Client::Unknown, "together").unwrap();
    assert_eq!(resolved.matched, "alias");
    assert_eq!(
        resolved.rooms.len(),
        2,
        "saved duplicates do not duplicate physical commands"
    );

    let unchanged = apply(
        &surface,
        alias("TOGETHER", &["Patio@S2", "Kitchen@S1", "Patio@S2"]),
    );
    assert!(!unchanged.changed);
    assert_eq!(unchanged.configuration, created.configuration);
    let global = apply(
        &surface,
        RoomConfigurationChange::SetDefault {
            room: "Den@S1".into(),
            client: None,
        },
    );
    assert_eq!(global.configuration.default_room.as_deref(), Some("Den@S1"));
    let client = Client::Tailnet("listener@example.test".into());
    let personal = apply(
        &surface,
        RoomConfigurationChange::SetDefault {
            room: "Patio@S2".into(),
            client: Some(client.key().into()),
        },
    );
    assert_eq!(
        personal.configuration.client_defaults[client.key()],
        "Patio@S2"
    );
    assert_eq!(
        surface.resolve_rooms(&client, "here").unwrap().rooms[0].room,
        "Patio"
    );
    assert_eq!(
        surface
            .resolve_rooms(&Client::Unknown, "here")
            .unwrap()
            .rooms[0]
            .room,
        "Den"
    );
    let cleared = apply(
        &surface,
        RoomConfigurationChange::ClearDefault {
            client: Some(client.key().into()),
        },
    );
    assert!(cleared.changed && cleared.configuration.client_defaults.is_empty());
    assert_eq!(
        surface.resolve_rooms(&client, "here").unwrap().rooms[0].room,
        "Den"
    );
    let removed = apply(
        &surface,
        RoomConfigurationChange::RemoveAlias {
            name: "together".into(),
        },
    );
    assert!(removed.changed && removed.configuration.aliases.is_empty());
    assert_eq!(
        removed.configuration.default_room.as_deref(),
        Some("Den@S1")
    );
    assert!(
        std::fs::read_to_string(dir.path().join("aliases.toml"))
            .unwrap()
            .starts_with("# Owner's room names.")
    );
}

#[test]
fn global_configuration_requires_an_owner_or_explicit_administrator() {
    let policy = Policy::from_toml(
        "[clients.'manager@example.test']\nmanage_others = true\n[clients.'blocked@example.test']\nmanage_others = true\ndeny = ['house-config']\n[clients.'delegate@example.test']\nallow = ['read', 'house-config']\n",
    ).unwrap().with_owner(Some("owner@example.test".into()));
    let (_dir, surface) = fixture(policy, None, false);
    let initial = surface.room_configuration(&Client::Unknown).unwrap();
    for client in [
        Client::Unknown,
        Client::LoopbackHttp,
        Client::Tailnet("delegate@example.test".into()),
        Client::Tailnet("blocked@example.test".into()),
    ] {
        assert_eq!(
            surface
                .configure_rooms(
                    &client,
                    &request(&initial, alias("later", &["Offline Room"]))
                )
                .unwrap_err()
                .code,
            ErrorCode::PolicyDenied
        );
    }
    for client in [
        Client::Tailnet("owner@example.test".into()),
        Client::Tailnet("manager@example.test".into()),
        Client::Cli,
    ] {
        let current = surface.room_configuration(&client).unwrap();
        assert!(
            surface
                .configure_rooms(
                    &client,
                    &request(&current, alias("later", &["Offline Room"]))
                )
                .is_ok()
        );
    }
    let (_dir, denied) = fixture(
        Policy::from_toml("[clients.'reader@example.test']\ndeny = ['room_configuration']")
            .unwrap(),
        None,
        false,
    );
    assert_eq!(
        denied
            .room_configuration(&Client::Tailnet("reader@example.test".into()))
            .unwrap_err()
            .code,
        ErrorCode::PolicyDenied
    );
}

#[test]
fn stale_or_malformed_edits_preserve_the_complete_file_and_no_ops_keep_the_revision() {
    let (dir, surface) = fixture(
        Policy::default(),
        Some("# keep this note\n[aliases]\nnight = 'Patio@S2'\n"),
        false,
    );
    let first = surface.room_configuration(&Client::Cli).unwrap();
    let saved = surface
        .configure_rooms(&Client::Cli, &request(&first, alias("night", &["Den@S1"])))
        .unwrap();
    let accepted = std::fs::read(dir.path().join("aliases.toml")).unwrap();
    assert_ne!(saved.configuration.revision, first.revision);
    assert_eq!(
        surface
            .configure_rooms(
                &Client::Cli,
                &request(&first, alias("night", &["Patio@S2"]))
            )
            .unwrap_err()
            .code,
        ErrorCode::ConfigChanged
    );
    for change in [
        alias("all", &["Den"]),
        alias("empty", &[]),
        alias("bad\nname", &["Den"]),
        RoomConfigurationChange::SetDefault {
            room: "here".into(),
            client: None,
        },
        RoomConfigurationChange::ClearDefault {
            client: Some(String::new()),
        },
    ] {
        assert_eq!(
            surface
                .configure_rooms(&Client::Cli, &request(&saved.configuration, change))
                .unwrap_err()
                .code,
            ErrorCode::InvalidArgument
        );
    }
    assert_eq!(
        std::fs::read(dir.path().join("aliases.toml")).unwrap(),
        accepted
    );
    let no_op = apply(
        &surface,
        RoomConfigurationChange::RemoveAlias {
            name: "absent".into(),
        },
    );
    assert!(!no_op.changed);
    assert_eq!(no_op.configuration.revision, saved.configuration.revision);
    let actions = surface
        .recent_actions(&Client::Cli, &ActionFilter::default())
        .unwrap();
    assert_eq!(
        actions
            .iter()
            .filter(|action| action
                .action
                .before_state
                .as_deref()
                .is_some_and(|before| before.contains("not_undoable")))
            .count(),
        1
    );
}

#[test]
fn offline_configuration_and_inventory_warnings_never_require_a_survey() {
    let (dir, offline) = fixture(Policy::default(), None, false);
    let first = offline.room_configuration(&Client::Cli).unwrap();
    assert_eq!(first.warnings[0].line, 0);
    assert!(first.warnings[0].message.contains("not checked"));
    let saved = apply(&offline, alias("unreachable", &["Offline Room"]));
    assert_eq!(saved.configuration.aliases[0].rooms, ["Offline Room"]);
    assert!(dir.path().join("aliases.toml").is_file());

    let (_dir, cached) = fixture(
        Policy::default(),
        Some("[aliases]\nDen = ['Kitchen', 'Offline Room']\n[defaults]\nroom = 'Den'\n"),
        true,
    );
    let warnings = cached.room_configuration(&Client::Cli).unwrap().warnings;
    assert!(
        warnings
            .iter()
            .any(|warning| warning.message.contains("Offline Room"))
    );
    assert!(
        warnings
            .iter()
            .any(|warning| warning.message.contains("Kitchen"))
    );
    assert!(
        warnings
            .iter()
            .any(|warning| warning.message.contains("room wins"))
    );
    assert!(warnings.iter().all(|warning| warning.line > 0));
}

#[test]
fn a_committed_room_edit_stays_successful_when_action_logging_is_unavailable() {
    let (_dir, surface) = fixture(Policy::default(), None, false);
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            let _store = surface.log.as_ref().unwrap().store.lock().unwrap();
            panic!("simulate an unavailable action store");
        });
        assert!(worker.join().is_err());
    });
    let saved = apply(&surface, alias("saved", &["Offline Room"]));
    assert!(saved.changed);
    assert_eq!(
        surface.room_configuration(&Client::Cli).unwrap(),
        saved.configuration
    );
}

#[test]
fn strict_requests_and_schema_agree_on_the_kind_tagged_change() {
    for change in [
        json!({"kind":"set_alias","name":"night","rooms":["Den"],"force":true}),
        json!({"kind":"remove_alias","name":"night","rooms":["Den"]}),
        json!({"kind":"clear_default","room":"Den"}),
        json!({"type":"SetAlias","name":"night","rooms":["Den"]}),
    ] {
        assert!(serde_json::from_value::<RoomConfigurationChange>(change).is_err());
    }
    assert!(
        serde_json::from_value::<RoomConfigureRequest>(json!({
            "expected_revision":"observed", "change":{"kind":"clear_default"}, "creator":"cli"
        }))
        .is_err()
    );
    for revision in ["", "a\nb"] {
        assert_eq!(
            RoomConfigureRequest {
                expected_revision: revision.into(),
                change: alias("night", &["Den"])
            }
            .validate()
            .unwrap_err()
            .code,
            ErrorCode::InvalidArgument
        );
    }
    let schema = RoomConfigurationChange::json_schema();
    let branches = schema["oneOf"].as_array().unwrap();
    assert_eq!(branches.len(), 4);
    for (branch, kind) in
        branches
            .iter()
            .zip(["set_alias", "remove_alias", "set_default", "clear_default"])
    {
        assert_eq!(branch["properties"]["kind"]["enum"], json!([kind]));
        assert_eq!(branch["additionalProperties"], false);
        assert!(branch["properties"].get("type").is_none());
    }
}
