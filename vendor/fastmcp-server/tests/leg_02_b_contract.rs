//! Literal frozen LEG-02 B runner entries through the public adapter surface.

use std::collections::BTreeMap;

use fastmcp_protocol::methods::{
    LOGGING_SET_LEVEL, Legacy2024Direction, Legacy2024ListChangedCapability,
    Legacy2024ResourcesCapability, Legacy2024ServerCapabilities, NOTIFICATIONS_CANCELLED,
    NOTIFICATIONS_INITIALIZED, NOTIFICATIONS_PROGRESS, PING, RESOURCES_SUBSCRIBE,
    RESOURCES_UNSUBSCRIBE, ROOTS_LIST, SAMPLING_CREATE_MESSAGE,
};
use fastmcp_server::legacy_2024::{
    LEGACY_2024_MAX_ADAPTER_RESERVATIONS, Legacy2024Handler, Legacy2024HandlerError,
    Legacy2024Lifecycle, Legacy2024Outbound, Legacy2024ServerAdapter, Legacy2024ServerConfig,
    Legacy2024ServerInfo, Legacy2024StateSnapshot, LegacyAuthenticatedPeerPartition,
    LegacyPeerBinding, legacy_2024_b_digest_preimage,
};
use serde_json::{Value, json};

const LEFT_OWNER: [u8; 32] = [0x41; 32];
const RIGHT_OWNER: [u8; 32] = [0x42; 32];
const LEFT_CONNECTION: u64 = 101;
const RIGHT_CONNECTION: u64 = 202;

struct NoopHandler;

impl Legacy2024Handler for NoopHandler {
    fn handle_legacy_2024(
        &mut self,
        _method: &'static str,
        _params: Option<&Value>,
    ) -> Result<Value, Legacy2024HandlerError> {
        Ok(json!({}))
    }
}

fn binding(owner: [u8; 32], connection: u64) -> LegacyPeerBinding {
    LegacyPeerBinding::from_authenticated_transport(
        LegacyAuthenticatedPeerPartition::from_authenticated_transport(owner),
        connection,
    )
}

fn adapter(binding: LegacyPeerBinding) -> Legacy2024ServerAdapter<NoopHandler> {
    Legacy2024ServerAdapter::install(
        binding,
        Legacy2024ServerConfig {
            capabilities: Legacy2024ServerCapabilities {
                logging: Some(BTreeMap::default()),
                resources: Some(Legacy2024ResourcesCapability {
                    subscribe: true,
                    ..Legacy2024ResourcesCapability::default()
                }),
                prompts: Some(Legacy2024ListChangedCapability::default()),
                tools: Some(Legacy2024ListChangedCapability::default()),
                ..Legacy2024ServerCapabilities::default()
            },
            server_info: Legacy2024ServerInfo {
                name: "leg-02-b-public-server".to_owned(),
                version: "1.0.0".to_owned(),
            },
            instructions: None,
        },
        NoopHandler,
    )
    .expect("exact public adapter configuration must install")
}

fn initialize_wire() -> Value {
    json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {"sampling": {}, "roots": {"listChanged": true}},
            "clientInfo": {"name": "leg-02-b-client", "version": "1.0.0"},
        },
    })
}

fn operating_adapter(binding: LegacyPeerBinding) -> Legacy2024ServerAdapter<NoopHandler> {
    let mut adapter = adapter(binding);
    assert!(matches!(
        adapter.receive(binding, initialize_wire()),
        Ok(Legacy2024Outbound::Response(_))
    ));
    assert_eq!(
        adapter.receive(
            binding,
            json!({"jsonrpc": "2.0", "method": NOTIFICATIONS_INITIALIZED}),
        ),
        Ok(Legacy2024Outbound::NoResponse)
    );
    assert_eq!(adapter.lifecycle(), Legacy2024Lifecycle::Operating);
    adapter
}

fn expected_install_receipt(owner: &[u8; 32], connection: u64) -> Vec<u8> {
    let mut receipt = b"fastmcp-legacy-server-install-v2\0".to_vec();
    let connection = connection.to_be_bytes();
    let binding = [owner.as_slice(), connection.as_slice()].concat();
    for field in [b"2024-11-05".as_slice(), binding.as_slice()] {
        receipt.extend_from_slice(&(field.len() as u32).to_be_bytes());
        receipt.extend_from_slice(field);
    }
    receipt
}

fn expected_b_preimage(
    partition: u32,
    operation: u32,
    method: &[u8],
    direction: Legacy2024Direction,
    owner: &[u8],
    connection: &[u8],
    before: &[u8],
    after: &[u8],
    lifecycle: Legacy2024Lifecycle,
    reservations: u64,
    releases: u64,
) -> Vec<u8> {
    let direction = match direction {
        Legacy2024Direction::ClientToServer => b"ClientToServer".as_slice(),
        Legacy2024Direction::ServerToClient => b"ServerToClient".as_slice(),
        Legacy2024Direction::Bidirectional => b"Bidirectional".as_slice(),
    };
    let lifecycle = match lifecycle {
        Legacy2024Lifecycle::AwaitInitialize => b"AwaitInitialize".as_slice(),
        Legacy2024Lifecycle::AwaitInitialized => b"AwaitInitialized".as_slice(),
        Legacy2024Lifecycle::Operating => b"Operating".as_slice(),
        Legacy2024Lifecycle::Closed => b"Closed".as_slice(),
    };
    let partition = partition.to_be_bytes();
    let operation = operation.to_be_bytes();
    let reservations = reservations.to_be_bytes();
    let releases = releases.to_be_bytes();
    let mut bytes = b"fastmcp-leg-02-b-v1\0".to_vec();
    for field in [
        partition.as_slice(),
        operation.as_slice(),
        method,
        direction,
        owner,
        connection,
        before,
        after,
        lifecycle,
        reservations.as_slice(),
        releases.as_slice(),
    ] {
        bytes.extend_from_slice(&(field.len() as u32).to_be_bytes());
        bytes.extend_from_slice(field);
    }
    bytes
}

fn assert_row(
    adapter: &Legacy2024ServerAdapter<NoopHandler>,
    binding: LegacyPeerBinding,
    partition: u32,
    operation: u32,
    method: &[u8],
    direction: Legacy2024Direction,
    owner: &[u8; 32],
    connection: u64,
    before: &Legacy2024StateSnapshot,
) -> Vec<u8> {
    let after = adapter.snapshot();
    let before_digest = before.canonical_digest();
    let after_digest = after.canonical_digest();
    let connection_bytes = connection.to_be_bytes();
    assert!(adapter.installed_receipt().matches_binding(binding));
    assert_eq!(
        adapter.installed_receipt().canonical_bytes(),
        expected_install_receipt(owner, connection)
    );
    let expected = expected_b_preimage(
        partition,
        operation,
        method,
        direction,
        owner,
        &connection_bytes,
        &before_digest,
        &after_digest,
        after.lifecycle,
        after.reservation_count,
        after.close_release_count,
    );
    assert_eq!(
        legacy_2024_b_digest_preimage(
            partition,
            operation,
            method,
            direction,
            owner,
            &connection_bytes,
            &before_digest,
            &after_digest,
            after.lifecycle,
            after.reservation_count,
            after.close_release_count,
        ),
        expected
    );
    expected
}

fn complete_reverse_request(
    adapter: &mut Legacy2024ServerAdapter<NoopHandler>,
    binding: LegacyPeerBinding,
    request_id: i64,
) {
    assert_eq!(
        adapter.snapshot().pending_reverse_request_ids,
        vec![request_id]
    );
    assert_eq!(adapter.snapshot().reservation_count, 1);
    assert_eq!(
        adapter.receive(
            binding,
            json!({"jsonrpc": "2.0", "id": request_id, "result": {}}),
        ),
        Ok(Legacy2024Outbound::NoResponse)
    );
    assert_eq!(
        adapter.snapshot().pending_reverse_request_ids,
        Vec::<i64>::new()
    );
    assert_eq!(adapter.snapshot().reservation_count, 0);
}

fn assert_reservation_limit(binding: LegacyPeerBinding) {
    let mut adapter = operating_adapter(binding);
    for _ in 0..LEGACY_2024_MAX_ADAPTER_RESERVATIONS {
        assert!(matches!(
            adapter.make_reverse_request(binding, PING, json!({})),
            Ok(Legacy2024Outbound::ReverseRequest(_))
        ));
    }

    let full_state = adapter.snapshot();
    let error = adapter
        .make_reverse_request(binding, PING, json!({}))
        .expect_err("the adapter must reject the next reservation at its fixed bound");
    assert_eq!(error.code().as_i32(), Some(-32600));
    assert_eq!(error.message(), "legacy adapter reservation limit reached");
    assert_eq!(adapter.snapshot(), full_state);
    assert_eq!(
        adapter.snapshot().reservation_count,
        LEGACY_2024_MAX_ADAPTER_RESERVATIONS as u64
    );
    assert_eq!(adapter.close(binding), Ok(()));
}

fn exercise_partition(
    partition: u32,
    owner: [u8; 32],
    connection: u64,
) -> (
    LegacyPeerBinding,
    Legacy2024ServerAdapter<NoopHandler>,
    Vec<Vec<u8>>,
) {
    let binding = binding(owner, connection);
    let mut adapter = operating_adapter(binding);
    let mut rows = Vec::new();

    let before = adapter.snapshot();
    assert_eq!(
        adapter.make_reverse_request(binding, PING, json!({})),
        Ok(Legacy2024Outbound::ReverseRequest(
            json!({"jsonrpc": "2.0", "id": 1, "method": PING, "params": {}})
        ))
    );
    complete_reverse_request(&mut adapter, binding, 1);
    rows.push(assert_row(
        &adapter,
        binding,
        partition,
        1,
        PING.as_bytes(),
        Legacy2024Direction::ServerToClient,
        &owner,
        connection,
        &before,
    ));

    let before = adapter.snapshot();
    assert_eq!(
        adapter.make_reverse_request(
            binding,
            SAMPLING_CREATE_MESSAGE,
            json!({"messages": [], "maxTokens": 16}),
        ),
        Ok(Legacy2024Outbound::ReverseRequest(json!({
            "jsonrpc": "2.0", "id": 2, "method": SAMPLING_CREATE_MESSAGE,
            "params": {"messages": [], "maxTokens": 16},
        })))
    );
    complete_reverse_request(&mut adapter, binding, 2);
    rows.push(assert_row(
        &adapter,
        binding,
        partition,
        2,
        SAMPLING_CREATE_MESSAGE.as_bytes(),
        Legacy2024Direction::ServerToClient,
        &owner,
        connection,
        &before,
    ));

    let before = adapter.snapshot();
    assert_eq!(
        adapter.make_reverse_request(binding, ROOTS_LIST, json!({})),
        Ok(Legacy2024Outbound::ReverseRequest(
            json!({"jsonrpc": "2.0", "id": 3, "method": ROOTS_LIST, "params": {}})
        ))
    );
    complete_reverse_request(&mut adapter, binding, 3);
    rows.push(assert_row(
        &adapter,
        binding,
        partition,
        3,
        ROOTS_LIST.as_bytes(),
        Legacy2024Direction::ServerToClient,
        &owner,
        connection,
        &before,
    ));

    let before = adapter.snapshot();
    assert_eq!(
        adapter.receive(
            binding,
            json!({
                "jsonrpc": "2.0", "id": 4, "method": LOGGING_SET_LEVEL,
                "params": {"level": "info"},
            }),
        ),
        Ok(Legacy2024Outbound::Response(
            json!({"jsonrpc": "2.0", "id": 4, "result": {}})
        ))
    );
    rows.push(assert_row(
        &adapter,
        binding,
        partition,
        4,
        LOGGING_SET_LEVEL.as_bytes(),
        Legacy2024Direction::ClientToServer,
        &owner,
        connection,
        &before,
    ));

    let subscription = json!({"uri": "file:///leg-02-b"});
    let before = adapter.snapshot();
    assert_eq!(
        adapter.receive(
            binding,
            json!({
                "jsonrpc": "2.0", "id": 5, "method": RESOURCES_SUBSCRIBE,
                "params": subscription.clone(),
            }),
        ),
        Ok(Legacy2024Outbound::Response(
            json!({"jsonrpc": "2.0", "id": 5, "result": {}})
        ))
    );
    rows.push(assert_row(
        &adapter,
        binding,
        partition,
        5,
        RESOURCES_SUBSCRIBE.as_bytes(),
        Legacy2024Direction::ClientToServer,
        &owner,
        connection,
        &before,
    ));

    let before = adapter.snapshot();
    assert_eq!(
        adapter.receive(
            binding,
            json!({
                "jsonrpc": "2.0", "id": 6, "method": RESOURCES_UNSUBSCRIBE,
                "params": subscription,
            }),
        ),
        Ok(Legacy2024Outbound::Response(
            json!({"jsonrpc": "2.0", "id": 6, "result": {}})
        ))
    );
    rows.push(assert_row(
        &adapter,
        binding,
        partition,
        6,
        RESOURCES_UNSUBSCRIBE.as_bytes(),
        Legacy2024Direction::ClientToServer,
        &owner,
        connection,
        &before,
    ));

    let before = adapter.snapshot();
    assert_eq!(
        adapter.receive(
            binding,
            json!({
                "jsonrpc": "2.0", "method": NOTIFICATIONS_CANCELLED,
                "params": {"requestId": 5},
            }),
        ),
        Ok(Legacy2024Outbound::NoResponse)
    );
    assert_eq!(
        adapter.receive(
            binding,
            json!({
                "jsonrpc": "2.0", "method": NOTIFICATIONS_PROGRESS,
                "params": {"progressToken": 1, "progress": 1},
            }),
        ),
        Ok(Legacy2024Outbound::NoResponse)
    );
    rows.push(assert_row(
        &adapter,
        binding,
        partition,
        7,
        b"notifications/cancelled+notifications/progress",
        Legacy2024Direction::ClientToServer,
        &owner,
        connection,
        &before,
    ));

    let before = adapter.snapshot();
    assert_eq!(adapter.close(binding), Ok(()));
    rows.push(assert_row(
        &adapter,
        binding,
        partition,
        8,
        b"close",
        Legacy2024Direction::ClientToServer,
        &owner,
        connection,
        &before,
    ));
    assert_eq!(adapter.lifecycle(), Legacy2024Lifecycle::Closed);
    assert_eq!(adapter.snapshot().reservation_count, 0);
    assert_eq!(adapter.snapshot().close_release_count, 1);
    assert_eq!(adapter.snapshot().subscriptions, Vec::<String>::new());
    assert_eq!(
        adapter.snapshot().pending_reverse_request_ids,
        Vec::<i64>::new()
    );
    (binding, adapter, rows)
}

#[test]
fn leg_02_b_positive() {
    let (left_binding, left, left_rows) = exercise_partition(1, LEFT_OWNER, LEFT_CONNECTION);
    let (right_binding, right, right_rows) = exercise_partition(2, RIGHT_OWNER, RIGHT_CONNECTION);

    assert_ne!(left_binding, right_binding);
    assert_eq!(left_rows.len(), 8);
    assert_eq!(right_rows.len(), 8);
    assert_eq!(left_rows.iter().chain(&right_rows).count(), 16);
    assert!(
        left_rows
            .iter()
            .zip(&right_rows)
            .all(|(left, right)| left != right)
    );
    assert_eq!(left.lifecycle(), Legacy2024Lifecycle::Closed);
    assert_eq!(right.lifecycle(), Legacy2024Lifecycle::Closed);
    assert_eq!(left.snapshot().close_release_count, 1);
    assert_eq!(right.snapshot().close_release_count, 1);
    assert_eq!(left.snapshot().reservation_count, 0);
    assert_eq!(right.snapshot().reservation_count, 0);
    assert_reservation_limit(binding(LEFT_OWNER, LEFT_CONNECTION + 1));
}

#[test]
fn leg_02_b_planted_negative() {
    let (left_binding, mut left, left_rows) = exercise_partition(1, LEFT_OWNER, LEFT_CONNECTION);
    let (right_binding, right, right_rows) = exercise_partition(2, RIGHT_OWNER, RIGHT_CONNECTION);
    let left_before = left.snapshot();
    let right_before = right.snapshot();
    let left_receipt = left.installed_receipt().canonical_bytes();
    let right_receipt = right.installed_receipt().canonical_bytes();

    let wrong_owner = binding(RIGHT_OWNER, LEFT_CONNECTION);
    let error = left
        .close(wrong_owner)
        .expect_err("changed owner must be rejected before closed-state selection");
    assert_eq!(error.code().as_i32(), Some(-32600));
    assert_eq!(
        error.message(),
        "legacy peer binding does not own this adapter lifecycle"
    );
    assert_eq!(left.snapshot(), left_before);
    assert_eq!(right.snapshot(), right_before);
    assert_eq!(
        left.snapshot().canonical_digest(),
        left_before.canonical_digest()
    );
    assert_eq!(
        right.snapshot().canonical_digest(),
        right_before.canonical_digest()
    );
    assert_eq!(left.installed_receipt().canonical_bytes(), left_receipt);
    assert_eq!(right.installed_receipt().canonical_bytes(), right_receipt);
    assert_eq!(left.snapshot().close_release_count, 1);
    assert_eq!(right.snapshot().close_release_count, 1);
    assert_eq!(left_rows.len() + right_rows.len(), 16);
    assert_ne!(left_binding, right_binding);
}
