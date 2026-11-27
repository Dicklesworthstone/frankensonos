use super::*;
use chrono::DateTime;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::FILE_NAME;
use fsonos_core::store::MemStore;
use fsonos_proto::{ProtoError, Transport};
use serde_json::json;
use std::net::IpAddr;

struct NoLan;
impl Transport for NoLan {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("policy configuration must not contact speakers")
    }
}

fn fixture(text: Option<&str>) -> (tempfile::TempDir, Surface) {
    let directory = tempfile::tempdir().unwrap();
    if let Some(text) = text {
        std::fs::write(directory.path().join(FILE_NAME), text).unwrap();
    }
    let policy = Policy::from_toml(text.unwrap_or_default())
        .unwrap()
        .with_owner(Some("owner@example.test".into()));
    let surface = Surface::new(
        Box::new(NoLan),
        Box::new(|_| panic!("policy configuration must not survey the LAN")),
        policy,
        Box::new(SystemClock),
    )
    .with_policy_file(directory.path().to_path_buf())
    .with_action_log(Box::new(MemStore::default()), "policy-config-test");
    (directory, surface)
}

fn request(
    configuration: &PolicyConfigurationDto,
    change: PolicyConfigurationChange,
) -> PolicyConfigureRequest {
    PolicyConfigureRequest {
        expected_revision: configuration.revision.clone(),
        change,
    }
}

fn apply(surface: &Surface, change: PolicyConfigurationChange) -> PolicyConfiguredDto {
    let current = surface.policy_configuration(&Client::Cli).unwrap();
    surface
        .configure_policy(&Client::Cli, &request(&current, change))
        .unwrap()
}

fn defaults(volume: u8) -> PolicyConfigurationChange {
    PolicyConfigurationChange::SetDefaults {
        max_volume: volume,
        max_step: 12,
        fade_secs: 3,
    }
}

fn client_rule(client: &str, administrator: bool) -> PolicyConfigurationChange {
    PolicyConfigurationChange::SetClient {
        client: client.into(),
        allow: None,
        deny: vec![],
        grant: vec!["auth".into()],
        capped: Some(true),
        manage_others: administrator,
        mcp_profile: Some(McpProfile::Essentials),
    }
}

#[test]
fn configuration_reads_require_administration_even_with_all_read_permissions() {
    let (_directory, surface) =
        fixture(Some("[clients.reader]\nallow = ['read', 'house-config']\n"));
    for client in [
        Client::Unknown,
        Client::LoopbackHttp,
        Client::McpStdio,
        Client::Tailnet("reader".into()),
    ] {
        assert_eq!(
            surface.policy_configuration(&client).unwrap_err().code,
            ErrorCode::PolicyDenied
        );
    }
    let current = surface
        .policy_configuration(&Client::Tailnet("owner@example.test".into()))
        .unwrap();
    assert_eq!(current.policy.defaults.max_volume, 70);
    assert_eq!(current.policy.clients[0].client, "reader");
}

#[test]
fn defaults_quiet_hours_room_limits_and_client_rules_take_effect_without_restarting() {
    let (directory, surface) = fixture(Some(
        "# House notes stay here.\n[rooms.'Untouched room']\nmax_volume = 41 # retained\n",
    ));
    let first = apply(&surface, defaults(60));
    assert!(first.changed);
    assert_eq!(
        surface.policy().defaults,
        first.configuration.policy.defaults
    );
    apply(
        &surface,
        PolicyConfigurationChange::SetQuietHours {
            start: "22:00".into(),
            end: "07:00".into(),
            max_volume: 20,
        },
    );
    apply(
        &surface,
        PolicyConfigurationChange::SetRoom {
            room: "Offline bedroom".into(),
            max_volume: Some(40),
            max_step: Some(5),
        },
    );
    apply(&surface, client_rule("Listener@EXAMPLE.test", false));
    let listener = Client::Tailnet("listener@example.test".into());
    let day = DateTime::parse_from_rfc3339("2026-11-26T12:00:00+00:00").unwrap();
    let night = DateTime::parse_from_rfc3339("2026-11-26T23:00:00+00:00").unwrap();
    assert_eq!(
        surface
            .policy()
            .max_volume("Offline bedroom", &listener, day),
        40
    );
    assert_eq!(
        surface
            .policy()
            .max_volume("Offline bedroom", &listener, night),
        20
    );
    assert_eq!(
        surface.policy().mcp_profile(&listener),
        Some(McpProfile::Essentials)
    );
    assert_eq!(surface.policy().owner(), Some("owner@example.test"));
    let removed = apply(
        &surface,
        PolicyConfigurationChange::RemoveRoom {
            room: "OFFLINE BEDROOM".into(),
        },
    );
    assert_eq!(removed.configuration.policy.rooms.len(), 1);
    assert_eq!(removed.configuration.policy.rooms[0].room, "Untouched room");
    apply(&surface, PolicyConfigurationChange::ClearQuietHours {});
    assert_eq!(
        surface
            .policy()
            .max_volume("Offline bedroom", &listener, night),
        60
    );
    let removed = apply(
        &surface,
        PolicyConfigurationChange::RemoveClient {
            client: "LISTENER@example.test".into(),
        },
    );
    assert!(removed.configuration.policy.clients.is_empty());
    let stored = std::fs::read_to_string(directory.path().join(FILE_NAME)).unwrap();
    assert!(stored.contains("# House notes stay here."));
    assert!(stored.contains("max_volume = 41 # retained"));
}

#[test]
fn tool_permission_cannot_grant_its_own_administration_and_denies_still_apply() {
    let (directory, surface) = fixture(Some(
        "[clients.delegate]\nallow = ['read', 'configure_policy']\n\
         [clients.blocked]\nmanage_others = true\ndeny = ['house-config']\n",
    ));
    let initial = surface.policy_configuration(&Client::Cli).unwrap();
    let before = std::fs::read(directory.path().join(FILE_NAME)).unwrap();
    for key in ["delegate", "blocked"] {
        let client = Client::Tailnet(key.into());
        assert_eq!(
            surface
                .configure_policy(&client, &request(&initial, client_rule(key, true)))
                .unwrap_err()
                .code,
            ErrorCode::PolicyDenied
        );
    }
    assert_eq!(
        std::fs::read(directory.path().join(FILE_NAME)).unwrap(),
        before
    );
}

#[test]
fn an_administrator_can_relinquish_access_but_cannot_reuse_its_old_review() {
    let (_directory, surface) = fixture(Some("[clients.manager]\nmanage_others = true\n"));
    let manager = Client::Tailnet("manager".into());
    let initial = surface.policy_configuration(&manager).unwrap();
    let accepted = surface
        .configure_policy(&manager, &request(&initial, client_rule("manager", false)))
        .unwrap();
    assert!(accepted.changed);
    assert!(!accepted.configuration.policy.clients[0].manage_others);
    assert_eq!(
        surface.policy_configuration(&manager).unwrap_err().code,
        ErrorCode::PolicyDenied
    );
    assert_eq!(
        surface
            .configure_policy(&manager, &request(&initial, client_rule("manager", true)))
            .unwrap_err()
            .code,
        ErrorCode::PolicyDenied
    );
}

#[test]
fn lock_time_authorization_uses_the_saved_rules_and_latest_trusted_owner() {
    let (_directory, surface) = fixture(Some("[clients.manager]\nmanage_others = true\n"));
    let manager = Client::Tailnet("manager".into());
    let revoked = Policy::from_toml("[clients.manager]\nmanage_others = false\n").unwrap();
    assert!(
        surface
            .authorize_policy_snapshot(&manager, &revoked, false)
            .is_err()
    );
    surface.set_tailnet_owner(Some("replacement@example.test".into()));
    assert!(
        surface
            .authorize_policy_snapshot(
                &Client::Tailnet("owner@example.test".into()),
                &revoked,
                false
            )
            .is_err()
    );
    assert!(
        surface
            .authorize_policy_snapshot(
                &Client::Tailnet("replacement@example.test".into()),
                &revoked,
                false
            )
            .is_ok()
    );
    let saved = apply(&surface, defaults(31));
    assert_eq!(saved.configuration.policy.defaults.max_volume, 31);
    assert_eq!(surface.policy().owner(), Some("replacement@example.test"));
}

#[test]
fn stale_revision_and_malformed_values_do_not_rewrite_the_policy() {
    let (directory, surface) = fixture(None);
    let initial = surface.policy_configuration(&Client::Cli).unwrap();
    apply(&surface, defaults(55));
    let saved = std::fs::read(directory.path().join(FILE_NAME)).unwrap();
    assert_eq!(
        surface
            .configure_policy(&Client::Cli, &request(&initial, defaults(22)))
            .unwrap_err()
            .code,
        ErrorCode::ConfigChanged
    );
    let current = surface.policy_configuration(&Client::Cli).unwrap();
    for change in [
        defaults(101),
        PolicyConfigurationChange::SetQuietHours {
            start: "25:00".into(),
            end: "07:00".into(),
            max_volume: 20,
        },
        PolicyConfigurationChange::SetRoom {
            room: "bad\nroom".into(),
            max_volume: Some(20),
            max_step: None,
        },
    ] {
        assert_eq!(
            surface
                .configure_policy(&Client::Cli, &request(&current, change))
                .unwrap_err()
                .code,
            ErrorCode::InvalidArgument
        );
    }
    assert_eq!(
        std::fs::read(directory.path().join(FILE_NAME)).unwrap(),
        saved
    );
}

#[test]
fn invalid_external_policy_remains_uneditable_while_last_valid_caps_stay_in_force() {
    let (directory, surface) = fixture(Some("[defaults]\nmax_volume = 30\n"));
    let current = surface.policy_configuration(&Client::Cli).unwrap();
    std::fs::write(directory.path().join(FILE_NAME), "[broken").unwrap();
    assert_eq!(
        surface.policy_configuration(&Client::Cli).unwrap_err().code,
        ErrorCode::InvalidArgument
    );
    assert_eq!(
        surface
            .configure_policy(&Client::Cli, &request(&current, defaults(70)))
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    assert_eq!(surface.policy().defaults.max_volume, 30);
    assert_eq!(
        std::fs::read_to_string(directory.path().join(FILE_NAME)).unwrap(),
        "[broken"
    );
}

#[test]
fn exact_repeated_rule_is_a_noop_and_empty_allow_is_not_inheritance() {
    let (_directory, surface) = fixture(None);
    apply(&surface, defaults(50));
    let before = surface.policy_configuration(&Client::Cli).unwrap();
    let same = surface
        .configure_policy(&Client::Cli, &request(&before, defaults(50)))
        .unwrap();
    assert!(!same.changed);
    assert_eq!(same.configuration.revision, before.revision);
    let mut change = client_rule("reader", false);
    if let PolicyConfigurationChange::SetClient { allow, grant, .. } = &mut change {
        *allow = Some(vec![]);
        grant.clear();
    }
    apply(&surface, change);
    let reader = Client::Tailnet("reader".into());
    assert!(matches!(
        surface.policy().authorize(
            ToolCall {
                name: "list_zones",
                read_only: true
            },
            &reader
        ),
        Decision::Deny { .. }
    ));
}

#[test]
fn request_wire_format_excludes_owner_changes_and_preserves_optional_rule_meanings() {
    let raw = json!({ "expected_revision": "reviewed", "change": { "kind": "set_client", "client": "agent",
        "allow": [], "deny": [], "grant": ["auth"], "capped": null, "manage_others": false, "mcp_profile": "full" } });
    let request: PolicyConfigureRequest = serde_json::from_value(raw.clone()).unwrap();
    request.validate().unwrap();
    assert_eq!(serde_json::to_value(request).unwrap(), raw);
    let mut forged = raw.clone();
    forged["owner"] = json!("caller@example.test");
    assert!(serde_json::from_value::<PolicyConfigureRequest>(forged).is_err());
    let mut forged = raw;
    forged["change"]["owner"] = json!("caller@example.test");
    assert!(serde_json::from_value::<PolicyConfigureRequest>(forged).is_err());
    let schema = PolicyConfigurationChange::json_schema();
    let choices = schema["oneOf"].as_array().unwrap();
    assert_eq!(choices.len(), 7);
    assert!(
        choices
            .iter()
            .all(|choice| choice["properties"]["kind"].is_object()
                && choice["additionalProperties"] == false)
    );
}

#[test]
fn clearing_quiet_hours_requires_the_exact_fieldless_operation() {
    let raw = json!({ "expected_revision": "reviewed", "change": { "kind": "clear_quiet_hours" } });
    let request: PolicyConfigureRequest = serde_json::from_value(raw.clone()).unwrap();
    assert_eq!(
        request.change,
        PolicyConfigurationChange::ClearQuietHours {}
    );
    request.validate().unwrap();
    assert_eq!(serde_json::to_value(request).unwrap(), raw);

    // A caller mixing up set and clear must not silently remove the limits,
    // including when the unexpected field is null or looks administrative.
    for (field, value) in [
        ("max_volume", json!(20)),
        ("start", json!("22:00")),
        ("end", json!(null)),
        ("owner", json!("caller@example.test")),
    ] {
        let mut malformed = raw.clone();
        malformed["change"][field] = value;
        assert!(serde_json::from_value::<PolicyConfigureRequest>(malformed).is_err());
    }
}
