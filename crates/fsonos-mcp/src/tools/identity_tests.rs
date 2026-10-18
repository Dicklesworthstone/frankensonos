//! Request identity reaches real speaker policy, the action log, and undo.
//! Each test owns its backend and simulator; none installs the global backend.

use super::{Backend, Daemon, Survey};
use asupersync::runtime::{Runtime, RuntimeBuilder};
use fastmcp::prelude::{McpContext, McpResult};
use fastmcp::{AuthContext, FinalCallToolResult};
use fsonos_api::{ActionDto, ActionsQuery, ErrorCode, Failure, Surface, UndoDto, VolumeRequest};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::MemStore;
use fsonos_core::{HouseholdState, control, resolve_room};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

const ALICE: &str = "alice@example.com";
const BOB: &str = "bob@example.com";
const CAROL: &str = "carol@example.com";
const BLOCKED: &str = "blocked@example.com";

fn households(sim: &SimHandle) -> Vec<HouseholdState> {
    let mut household = HouseholdState::default();
    household.apply_topology(
        &get_zone_group_state(&sim.lan(), sim.player("Kitchen").unwrap().ip).unwrap(),
    );
    vec![household]
}

fn fixture() -> (SimHandle, Backend) {
    let sim = SimHousehold::builder()
        .s1([
            SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1),
            SimPlayerSpec::new("Office", SimModel::Play5Gen1),
        ])
        .spawn()
        .unwrap();
    let houses = households(&sim);
    for (room, level) in [("Kitchen", 10), ("Office", 15)] {
        let player = &resolve_room(&houses, room).unwrap().player.id;
        control::set_volume(&sim.lan(), &houses, player, level).unwrap();
    }
    let kitchen = sim.player("Kitchen").unwrap().ip;
    let survey: Survey = Box::new(move |transport| {
        let mut household = HouseholdState::default();
        household.apply_topology(
            &get_zone_group_state(transport, kitchen).map_err(fsonos_core::CoreError::from)?,
        );
        Ok(vec![household])
    });
    let policy = Policy::from_toml(
        r#"
[defaults]
max_volume = 35
max_step = 100

[clients."alice@example.com"]
allow = ["read", "set_volume", "undo_last"]

[clients."bob@example.com"]
allow = ["read"]

[clients."carol@example.com"]
allow = ["read", "set_volume", "undo_last"]

[clients."blocked@example.com"]
allow = []

[clients."zone-reader@example.com"]
allow = ["list_zones"]
"#,
    )
    .unwrap();
    let surface = Surface::new(Box::new(sim.lan()), survey, policy, Box::new(SystemClock))
        .with_action_log(Box::new(MemStore::default()), "mcp");
    (sim, Backend::shared(Arc::new(surface), Client::Unknown))
}

fn runtime() -> Runtime {
    RuntimeBuilder::new().worker_threads(1).build().unwrap()
}

fn request(runtime: &Runtime, id: u64, subject: &str) -> McpContext {
    let context = anonymous(runtime, id);
    assert!(context.set_auth(AuthContext::with_subject(subject)));
    context
}

fn anonymous(runtime: &Runtime, id: u64) -> McpContext {
    McpContext::new(
        runtime.request_cx_with_budget(asupersync::Budget::INFINITE),
        id,
    )
}

fn volume_request(room: &str, volume: i64) -> VolumeRequest {
    VolumeRequest {
        zone: room.into(),
        volume: Some(volume),
        delta: None,
        group: false,
    }
}

fn volume(sim: &SimHandle, room: &str) -> u8 {
    let houses = households(sim);
    let player = &resolve_room(&houses, room).unwrap().player.id;
    control::volume(&sim.lan(), &houses, player).unwrap()
}

fn output(result: McpResult<FinalCallToolResult>) -> Value {
    let result = result.unwrap();
    assert!(!result.is_error);
    result.structured_content.expect("structured MCP response")
}

fn denied(result: McpResult<FinalCallToolResult>) {
    let error = result.unwrap_err();
    assert_eq!(error.data.unwrap()["code"], "POLICY_DENIED");
}

fn actions(backend: &Backend) -> Vec<ActionDto> {
    let value = output(backend.recent_actions(&ActionsQuery::default()));
    serde_json::from_value(value["actions"].clone()).unwrap()
}

fn undo(backend: &Backend) -> UndoDto {
    serde_json::from_value(output(backend.undo_last())).unwrap()
}

#[test]
fn named_callers_keep_their_own_caps_actions_and_undo_on_shared_speakers() {
    let runtime = runtime();
    let (sim, backend) = fixture();
    // Hold all three request backends before any action. Deriving a later
    // caller must not change the identity used by an earlier request.
    let alice = backend.for_request(&request(&runtime, 1, ALICE)).unwrap();
    let bob = backend.for_request(&request(&runtime, 2, BOB)).unwrap();
    let carol = backend.for_request(&request(&runtime, 3, CAROL)).unwrap();
    assert_eq!(backend.client, Client::Unknown);

    let zones = output(bob.list_zones());
    assert_eq!(zones["zones"].as_array().unwrap().len(), 2);
    bob.authorize_reads(&["get_zone_state"]).unwrap();

    output(alice.set_volume(&volume_request("Kitchen", 90)));
    assert_eq!(
        volume(&sim, "Kitchen"),
        35,
        "the named agent remains capped"
    );
    let logged = actions(&bob);
    assert_eq!(logged.len(), 1);
    assert_eq!(logged[0].client, ALICE);
    assert_eq!(logged[0].surface, "mcp");
    assert!(logged[0].decision.starts_with("clamp:"));
    assert!(logged[0].undoable);
    let alice_action = logged[0].id;

    denied(bob.set_volume(&volume_request("Kitchen", 5)));
    assert_eq!(volume(&sim, "Kitchen"), 35);
    output(carol.set_volume(&volume_request("Office", 30)));
    assert_eq!(volume(&sim, "Office"), 30);
    let carol_action = actions(&bob)
        .into_iter()
        .find(|action| action.client == CAROL)
        .expect("Carol's own action is logged")
        .id;

    denied(bob.undo_last());
    assert_eq!(volume(&sim, "Kitchen"), 35);
    assert_eq!(volume(&sim, "Office"), 30);

    let restored = undo(&alice);
    assert_eq!(restored.undone, Some(alice_action));
    assert_eq!(volume(&sim, "Kitchen"), 10);
    assert_eq!(
        volume(&sim, "Office"),
        30,
        "Alice must leave Carol's newer action in place"
    );
    assert_eq!(
        undo(&alice).undone,
        None,
        "exhausting Alice's history must not take Carol's action"
    );
    assert_eq!(volume(&sim, "Office"), 30);
    assert_eq!(undo(&carol).undone, Some(carol_action));
    assert_eq!(volume(&sim, "Office"), 15);

    let logged = actions(&bob);
    assert!(logged.iter().all(|action| {
        [ALICE, BOB, CAROL].contains(&action.client.as_str()) && action.surface == "mcp"
    }));
    assert!(logged.iter().any(|action| {
        action.client == BOB && action.decision.starts_with("deny:") && !action.undoable
    }));
    assert!(
        logged
            .iter()
            .any(|action| { action.client == ALICE && action.undo_of == Some(alice_action) })
    );
    assert!(
        logged
            .iter()
            .any(|action| { action.client == CAROL && action.undo_of == Some(carol_action) })
    );
    sim.shutdown();
}

#[test]
fn named_requests_do_not_change_anonymous_or_stdio_authority() {
    let runtime = runtime();
    let (sim, backend) = fixture();
    let alice = backend.for_request(&request(&runtime, 10, ALICE)).unwrap();
    output(alice.set_volume(&volume_request("Kitchen", 20)));

    let unknown = backend.for_request(&anonymous(&runtime, 11)).unwrap();
    assert_eq!(unknown.client, Client::Unknown);
    denied(unknown.set_volume(&volume_request("Kitchen", 25)));
    assert_eq!(volume(&sim, "Kitchen"), 20);
    output(unknown.list_zones());

    let stdio = Backend::shared(Arc::clone(&backend.surface), Client::McpStdio);
    let local = stdio.for_request(&anonymous(&runtime, 12)).unwrap();
    assert_eq!(local.client, Client::McpStdio);
    output(local.set_volume(&volume_request("Kitchen", 25)));
    assert_eq!(volume(&sim, "Kitchen"), 25);
    assert!(
        actions(&alice).iter().any(|action| {
            action.client == "mcp-stdio" && action.intent.starts_with("set_volume")
        })
    );

    for subject in ["cli", "CLI", "mcp-stdio", " CLI ", "unknown", ""] {
        let caller = stdio.for_request(&request(&runtime, 13, subject)).unwrap();
        assert_eq!(caller.client, Client::Unknown, "{subject:?}");
        denied(caller.set_volume(&volume_request("Kitchen", 90)));
        assert_eq!(volume(&sim, "Kitchen"), 25);
    }
    // An authenticated context without a subject is also unknown; it must
    // not inherit the fixed stdio backend's authority.
    let unnamed = anonymous(&runtime, 14);
    let mut auth = AuthContext::with_subject(ALICE);
    auth.subject = None;
    assert!(unnamed.set_auth(auth));
    let caller = stdio.for_request(&unnamed).unwrap();
    assert_eq!(caller.client, Client::Unknown);
    denied(caller.set_volume(&volume_request("Kitchen", 90)));

    let blocked = backend
        .for_request(&request(&runtime, 15, BLOCKED))
        .unwrap();
    for read in [
        "events",
        "list_zones",
        "get_zone_state",
        "dj_status",
        "dj_moods",
    ] {
        assert_eq!(
            blocked.authorize_reads(&[read]).unwrap_err().code,
            ErrorCode::PolicyDenied,
            "{read}"
        );
    }
    denied(blocked.list_zones());
    let zones_only = backend
        .for_request(&request(&runtime, 16, "zone-reader@example.com"))
        .unwrap();
    output(zones_only.list_zones());
    zones_only.authorize_reads(&["list_zones"]).unwrap();
    assert_eq!(
        zones_only
            .authorize_reads(&["list_zones", "events"])
            .unwrap_err()
            .code,
        ErrorCode::PolicyDenied,
        "reading zones alone must not authorize subscription invalidations"
    );
    assert_eq!(volume(&sim, "Kitchen"), 25);
    assert_eq!(backend.client, Client::Unknown);
    assert_eq!(stdio.client, Client::McpStdio);
    sim.shutdown();
}

#[derive(Default)]
struct RecordingDaemon {
    calls: AtomicUsize,
}

impl Daemon for RecordingDaemon {
    fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Option<Result<Value, Failure>> {
        assert_eq!((method, path), ("GET", "/zones"));
        assert!(body.is_none());
        self.calls.fetch_add(1, Ordering::SeqCst);
        Some(Ok(json!([])))
    }
}

#[test]
fn a_named_request_cannot_be_forwarded_as_the_fixed_stdio_daemon_client() {
    let runtime = runtime();
    let (sim, backend) = fixture();
    let daemon = Arc::new(RecordingDaemon::default());
    let stdio =
        Backend::shared(Arc::clone(&backend.surface), Client::McpStdio).with_daemon(daemon.clone());

    let local = stdio.for_request(&anonymous(&runtime, 20)).unwrap();
    assert_eq!(local.client, Client::McpStdio);
    assert_eq!(output(local.list_zones())["zones"], json!([]));
    assert_eq!(daemon.calls.load(Ordering::SeqCst), 1);

    for subject in [ALICE, BOB, "unknown", "mcp-stdio"] {
        let error = stdio
            .for_request(&request(&runtime, 21, subject))
            .err()
            .expect("an auth-bearing HTTP request cannot borrow the stdio bridge");
        assert_eq!(error.code, ErrorCode::NotReady, "{subject}");
    }
    assert_eq!(
        daemon.calls.load(Ordering::SeqCst),
        1,
        "rejected identities must not dispatch to the daemon"
    );
    assert_eq!(volume(&sim, "Kitchen"), 10);
    assert_eq!(volume(&sim, "Office"), 15);
    assert!(actions(&backend).is_empty());
    sim.shutdown();
}
