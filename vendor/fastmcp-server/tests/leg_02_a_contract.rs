//! Literal frozen LEG-02 A runner entries through the non-test public surface.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use fastmcp_protocol::methods::{
    COMPLETION_COMPLETE, Legacy2024Direction, Legacy2024ListChangedCapability,
    Legacy2024ResourcesCapability, Legacy2024ServerCapabilities, NOTIFICATIONS_CANCELLED,
    NOTIFICATIONS_INITIALIZED, NOTIFICATIONS_PROGRESS, RESOURCES_SUBSCRIBE,
    RESOURCES_TEMPLATES_LIST, ROOTS_LIST, SAMPLING_CREATE_MESSAGE, TOOLS_LIST,
};
use fastmcp_server::legacy_2024::{
    Legacy2024Handler, Legacy2024HandlerError, Legacy2024Lifecycle, Legacy2024Outbound,
    Legacy2024ServerAdapter, Legacy2024ServerConfig, Legacy2024ServerInfo,
    LegacyAuthenticatedPeerPartition, LegacyPeerBinding, legacy_2024_a_digest_preimage,
};
use serde_json::{Value, json};

const OWNER_PARTITION: LegacyAuthenticatedPeerPartition =
    LegacyAuthenticatedPeerPartition::from_authenticated_transport([0xA5; 32]);
const BINDING_GENERATION: u64 = 20_241_105;

#[derive(Clone)]
struct RecordingHandler {
    methods: Rc<RefCell<Vec<&'static str>>>,
}

impl Legacy2024Handler for RecordingHandler {
    fn handle_legacy_2024(
        &mut self,
        method: &'static str,
        _params: Option<&Value>,
    ) -> Result<Value, Legacy2024HandlerError> {
        self.methods.borrow_mut().push(method);
        Ok(json!({"handled": method}))
    }
}

fn binding() -> LegacyPeerBinding {
    LegacyPeerBinding::from_authenticated_transport(OWNER_PARTITION, BINDING_GENERATION)
}

fn adapter(methods: Rc<RefCell<Vec<&'static str>>>) -> Legacy2024ServerAdapter<RecordingHandler> {
    Legacy2024ServerAdapter::install(
        binding(),
        Legacy2024ServerConfig {
            capabilities: Legacy2024ServerCapabilities {
                logging: Some(BTreeMap::default()),
                tools: Some(Legacy2024ListChangedCapability::default()),
                resources: Some(Legacy2024ResourcesCapability {
                    subscribe: true,
                    ..Legacy2024ResourcesCapability::default()
                }),
                prompts: Some(Legacy2024ListChangedCapability::default()),
                ..Legacy2024ServerCapabilities::default()
            },
            server_info: Legacy2024ServerInfo {
                name: "public-legacy-server".to_owned(),
                version: "1.0.0".to_owned(),
            },
            instructions: Some("exact 2024 server surface".to_owned()),
        },
        RecordingHandler { methods },
    )
    .expect("exact public server configuration must install")
}

fn initialize() -> Value {
    json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "sampling": {},
                "roots": {"listChanged": true},
                "elicitation": {"form": {}}
            },
            "clientInfo": {"name": "public-legacy-client", "version": "1.0.0"},
        },
    })
}

fn expected_install_receipt() -> Vec<u8> {
    let mut receipt = b"fastmcp-legacy-server-install-v2\0".to_vec();
    let binding_bytes = [[0xA5; 32].as_slice(), &BINDING_GENERATION.to_be_bytes()].concat();
    for field in [b"2024-11-05".as_slice(), binding_bytes.as_slice()] {
        receipt.extend_from_slice(&(field.len() as u32).to_be_bytes());
        receipt.extend_from_slice(field);
    }
    receipt
}

fn expected_row_preimage(
    ordinal: u32,
    group: &[u8],
    input: Legacy2024Lifecycle,
    wire: &[u8],
    capabilities: &[u8],
    method: &[u8],
    direction: Legacy2024Direction,
    output: Legacy2024Lifecycle,
    state_digest: &[u8],
) -> Vec<u8> {
    let lifecycle = |value| match value {
        Legacy2024Lifecycle::AwaitInitialize => b"AwaitInitialize".as_slice(),
        Legacy2024Lifecycle::AwaitInitialized => b"AwaitInitialized".as_slice(),
        Legacy2024Lifecycle::Operating => b"Operating".as_slice(),
        Legacy2024Lifecycle::Closed => b"Closed".as_slice(),
    };
    let direction = match direction {
        Legacy2024Direction::ClientToServer => b"ClientToServer".as_slice(),
        Legacy2024Direction::ServerToClient => b"ServerToClient".as_slice(),
        Legacy2024Direction::Bidirectional => b"Bidirectional".as_slice(),
    };
    let mut preimage = b"fastmcp-leg-02-a-v1\0".to_vec();
    let ordinal = ordinal.to_be_bytes();
    for field in [
        ordinal.as_slice(),
        group,
        lifecycle(input),
        wire,
        capabilities,
        method,
        direction,
        lifecycle(output),
        state_digest,
    ] {
        preimage.extend_from_slice(&(field.len() as u32).to_be_bytes());
        preimage.extend_from_slice(field);
    }
    preimage
}

fn assert_row(
    adapter: &Legacy2024ServerAdapter<RecordingHandler>,
    ordinal: u32,
    group: &[u8],
    input: Legacy2024Lifecycle,
    wire: &[u8],
    capabilities: &[u8],
    method: &[u8],
    direction: Legacy2024Direction,
    output: Legacy2024Lifecycle,
) {
    let receipt = adapter.installed_receipt();
    assert_eq!(receipt.protocol_version(), "2024-11-05");
    assert!(receipt.matches_binding(binding()));
    assert_eq!(receipt.canonical_bytes(), expected_install_receipt());
    assert_eq!(adapter.lifecycle(), output);
    let state_digest = adapter.snapshot().canonical_digest();
    let expected = expected_row_preimage(
        ordinal,
        group,
        input,
        wire,
        capabilities,
        method,
        direction,
        output,
        &state_digest,
    );
    assert_eq!(
        legacy_2024_a_digest_preimage(
            ordinal,
            group,
            input,
            wire,
            capabilities,
            method,
            direction,
            output,
            &state_digest,
        ),
        expected
    );
}

#[test]
fn leg_02_a_positive() {
    let methods = Rc::new(RefCell::new(Vec::new()));
    let mut adapter = adapter(methods.clone());
    let capabilities = serde_json::to_vec(&initialize()["params"]["capabilities"])
        .expect("exact capability object must serialize");

    let initialize_wire = initialize();
    let wire = serde_json::to_vec(&initialize_wire).expect("initialize wire must serialize");
    assert_eq!(adapter.lifecycle(), Legacy2024Lifecycle::AwaitInitialize);
    assert_eq!(
        adapter.receive(binding(), initialize_wire),
        Ok(Legacy2024Outbound::Response(json!({
            "jsonrpc": "2.0", "id": 1,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "logging": {}, "prompts": {},
                    "resources": {"subscribe": true}, "tools": {},
                },
                "serverInfo": {"name": "public-legacy-server", "version": "1.0.0"},
                "instructions": "exact 2024 server surface",
            },
        })))
    );
    assert_row(
        &adapter,
        1,
        b"initialize/initialized lifecycle",
        Legacy2024Lifecycle::AwaitInitialize,
        &wire,
        &capabilities,
        b"initialize",
        Legacy2024Direction::ClientToServer,
        Legacy2024Lifecycle::AwaitInitialized,
    );

    let initialized_wire = json!({"jsonrpc": "2.0", "method": NOTIFICATIONS_INITIALIZED});
    let wire = serde_json::to_vec(&initialized_wire).expect("initialized wire must serialize");
    assert_eq!(
        adapter.receive(binding(), initialized_wire),
        Ok(Legacy2024Outbound::NoResponse)
    );
    assert_row(
        &adapter,
        2,
        b"initialize/initialized lifecycle",
        Legacy2024Lifecycle::AwaitInitialized,
        &wire,
        &capabilities,
        NOTIFICATIONS_INITIALIZED.as_bytes(),
        Legacy2024Direction::ClientToServer,
        Legacy2024Lifecycle::Operating,
    );

    let rows = [
        (
            b"tools".as_slice(),
            TOOLS_LIST,
            json!({"jsonrpc": "2.0", "id": 2, "method": TOOLS_LIST}),
        ),
        (
            b"resources/resource templates".as_slice(),
            RESOURCES_TEMPLATES_LIST,
            json!({"jsonrpc": "2.0", "id": 3, "method": RESOURCES_TEMPLATES_LIST}),
        ),
        (
            b"prompts".as_slice(),
            "prompts/list",
            json!({"jsonrpc": "2.0", "id": 4, "method": "prompts/list"}),
        ),
        (
            b"completion".as_slice(),
            COMPLETION_COMPLETE,
            json!({
                "jsonrpc": "2.0", "id": 5, "method": COMPLETION_COMPLETE,
                "params": {
                    "ref": {"type": "ref/prompt", "name": "legacy-prompt"},
                    "argument": {"name": "topic", "value": "legacy"},
                },
            }),
        ),
        (
            b"resource subscribe/unsubscribe".as_slice(),
            RESOURCES_SUBSCRIBE,
            json!({
                "jsonrpc": "2.0", "id": 6, "method": RESOURCES_SUBSCRIBE,
                "params": {"uri": "file:///workspace"},
            }),
        ),
    ];
    for (offset, (group, method, request)) in rows.into_iter().enumerate() {
        let wire = serde_json::to_vec(&request).expect("exact request wire must serialize");
        let expected = if method == RESOURCES_SUBSCRIBE {
            json!({"jsonrpc": "2.0", "id": 6, "result": {}})
        } else {
            json!({"jsonrpc": "2.0", "id": (offset as i64) + 2, "result": {"handled": method}})
        };
        assert_eq!(
            adapter.receive(binding(), request),
            Ok(Legacy2024Outbound::Response(expected))
        );
        assert_row(
            &adapter,
            (offset as u32) + 3,
            group,
            Legacy2024Lifecycle::Operating,
            &wire,
            &capabilities,
            method.as_bytes(),
            Legacy2024Direction::ClientToServer,
            Legacy2024Lifecycle::Operating,
        );
    }

    let roots = adapter
        .make_reverse_request(binding(), ROOTS_LIST, json!({}))
        .expect("negotiated roots must be permitted");
    assert!(matches!(&roots, Legacy2024Outbound::ReverseRequest(_)));
    let Legacy2024Outbound::ReverseRequest(roots_wire) = &roots else {
        return;
    };
    let wire = serde_json::to_vec(roots_wire).expect("roots wire must serialize");
    assert_eq!(
        roots,
        Legacy2024Outbound::ReverseRequest(
            json!({"jsonrpc": "2.0", "id": 1, "method": ROOTS_LIST, "params": {}})
        )
    );
    assert_row(
        &adapter,
        8,
        b"roots",
        Legacy2024Lifecycle::Operating,
        &wire,
        &capabilities,
        ROOTS_LIST.as_bytes(),
        Legacy2024Direction::ServerToClient,
        Legacy2024Lifecycle::Operating,
    );

    let sampling = adapter
        .make_reverse_request(
            binding(),
            SAMPLING_CREATE_MESSAGE,
            json!({"messages": [], "maxTokens": 16}),
        )
        .expect("negotiated sampling must be permitted");
    assert!(matches!(&sampling, Legacy2024Outbound::ReverseRequest(_)));
    let Legacy2024Outbound::ReverseRequest(sampling_wire) = &sampling else {
        return;
    };
    let wire = serde_json::to_vec(sampling_wire).expect("sampling wire must serialize");
    assert_eq!(
        sampling,
        Legacy2024Outbound::ReverseRequest(json!({
            "jsonrpc": "2.0", "id": 2, "method": SAMPLING_CREATE_MESSAGE,
            "params": {"messages": [], "maxTokens": 16},
        }))
    );
    assert_row(
        &adapter,
        9,
        b"sampling/createMessage",
        Legacy2024Lifecycle::Operating,
        &wire,
        &capabilities,
        SAMPLING_CREATE_MESSAGE.as_bytes(),
        Legacy2024Direction::ServerToClient,
        Legacy2024Lifecycle::Operating,
    );

    let logging = json!({
        "jsonrpc": "2.0", "id": 7, "method": "logging/setLevel",
        "params": {"level": "info"},
    });
    let wire = serde_json::to_vec(&logging).expect("logging wire must serialize");
    assert_eq!(
        adapter.receive(binding(), logging),
        Ok(Legacy2024Outbound::Response(
            json!({"jsonrpc": "2.0", "id": 7, "result": {}})
        ))
    );
    assert_row(
        &adapter,
        10,
        b"logging/setLevel",
        Legacy2024Lifecycle::Operating,
        &wire,
        &capabilities,
        b"logging/setLevel",
        Legacy2024Direction::ClientToServer,
        Legacy2024Lifecycle::Operating,
    );

    for (ordinal, method, request) in [
        (
            11,
            NOTIFICATIONS_CANCELLED,
            json!({
                "jsonrpc": "2.0", "method": NOTIFICATIONS_CANCELLED,
                "params": {"requestId": 6},
            }),
        ),
        (
            12,
            NOTIFICATIONS_PROGRESS,
            json!({
                "jsonrpc": "2.0", "method": NOTIFICATIONS_PROGRESS,
                "params": {"progressToken": 1, "progress": 1},
            }),
        ),
    ] {
        let wire = serde_json::to_vec(&request).expect("notification wire must serialize");
        assert_eq!(
            adapter.receive(binding(), request),
            Ok(Legacy2024Outbound::NoResponse)
        );
        assert_row(
            &adapter,
            ordinal,
            b"cancellation/progress",
            Legacy2024Lifecycle::Operating,
            &wire,
            &capabilities,
            method.as_bytes(),
            Legacy2024Direction::ClientToServer,
            Legacy2024Lifecycle::Operating,
        );
    }

    let state = adapter.snapshot();
    assert_eq!(state.operating_transition_count, 1);
    assert_eq!(state.subscriptions, ["file:///workspace"]);
    assert_eq!(state.logging_level.as_deref(), Some("info"));
    assert_eq!(state.control_notification_count, 2);
    assert_eq!(
        methods.borrow().as_slice(),
        [
            TOOLS_LIST,
            RESOURCES_TEMPLATES_LIST,
            "prompts/list",
            COMPLETION_COMPLETE,
            RESOURCES_SUBSCRIBE,
        ]
    );
}

#[test]
fn leg_02_a_planted_negative() {
    let methods = Rc::new(RefCell::new(Vec::new()));
    let mut adapter = adapter(methods.clone());
    let before = adapter.snapshot();
    let before_digest = before.canonical_digest();
    let receipt = adapter.installed_receipt().canonical_bytes();
    let mut wrong_era = initialize();
    wrong_era["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"] = json!("2026-07-28");
    let response = adapter
        .receive(binding(), wrong_era)
        .expect("invalid request IDs still receive JSON-RPC errors");
    assert_eq!(
        response,
        Legacy2024Outbound::Response(json!({
            "jsonrpc": "2.0", "id": 1,
            "error": {
                "code": -32600,
                "message": "invalid exact MCP 2024-11-05 envelope",
            },
        }))
    );
    assert_eq!(adapter.snapshot(), before);
    assert_eq!(adapter.snapshot().canonical_digest(), before_digest);
    assert_eq!(adapter.installed_receipt().canonical_bytes(), receipt);
    assert_eq!(adapter.lifecycle(), Legacy2024Lifecycle::AwaitInitialize);
    assert_eq!(adapter.snapshot().close_release_count, 0);
    assert!(methods.borrow().is_empty());
}
