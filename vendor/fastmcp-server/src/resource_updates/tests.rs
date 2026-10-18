use std::sync::{Arc, Mutex};

use asupersync::Cx;
use fastmcp_core::{McpContext, McpRequestCancellation, block_on};
use fastmcp_protocol::protocol_policy::ProtocolPolicy;
use fastmcp_protocol::{
    JsonRpcRequest, RequestId, Resource, ResourceContent, ResourceUpdatedNotificationParams,
    ServerNotification, SubscriptionFilter,
};
use fastmcp_transport::http::{HttpMethod, HttpRequest, HttpStatus};

use super::{MAX_RESOURCE_UPDATE_BATCH, MAX_RESOURCE_UPDATE_BATCH_BYTES, ResourceSubscriptions};
use crate::{
    NotificationSender, ResourceHandler, Server, ServerBuilder, ServerHttpEndpointResponse,
};

const ZONES: &str = "sonos://zones";
const ROOM: &str = "sonos://zone/Living%20Room%40HH-S1";

struct ZonesResource;

impl ResourceHandler for ZonesResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: ZONES.to_owned(),
            name: "zones".to_owned(),
            description: None,
            mime_type: Some("application/json".to_owned()),
            icon: None,
            version: None,
            tags: Vec::new(),
        }
    }

    fn read(&self, _ctx: &McpContext) -> fastmcp_core::McpResult<Vec<ResourceContent>> {
        Ok(vec![ResourceContent {
            uri: ZONES.to_owned(),
            mime_type: Some("application/json".to_owned()),
            text: Some("[]".to_owned()),
            blob: None,
        }])
    }
}

fn server() -> Server {
    ServerBuilder::try_new_with_fixed_protocol_policy(
        "resource-updates",
        "1.0.0",
        ProtocolPolicy::Auto,
    )
    .expect("the Auto facade policy is available in dual-era tests")
    .resource(ZonesResource)
    .build()
}

fn recorder() -> (NotificationSender, Arc<Mutex<Vec<JsonRpcRequest>>>) {
    let delivered = Arc::new(Mutex::new(Vec::new()));
    let output = Arc::clone(&delivered);
    let sender: NotificationSender = Arc::new(move |notification| {
        output
            .lock()
            .expect("notification recorder")
            .push(notification);
    });
    (sender, delivered)
}

fn filter(uris: &[&str]) -> SubscriptionFilter {
    SubscriptionFilter {
        resource_subscriptions: Some(uris.iter().map(|uri| (*uri).to_owned()).collect()),
        ..SubscriptionFilter::default()
    }
}

fn observed(output: &Mutex<Vec<JsonRpcRequest>>) -> Vec<JsonRpcRequest> {
    output.lock().expect("notification recorder").clone()
}

#[test]
fn modern_publisher_retains_exact_filters_and_correlated_delivery() {
    let server = server();
    let updates = server.resource_updates();
    let (first_sender, first) = recorder();
    let (second_sender, second) = recorder();
    let first_listen = server
        .open_subscription_listen(RequestId::Number(71), filter(&[ZONES, ROOM]), first_sender)
        .expect("first resource listen");
    let second_listen = server
        .open_subscription_listen(RequestId::Number(72), filter(&[ZONES]), second_sender)
        .expect("second resource listen");
    assert_eq!(
        updates.subscriptions(),
        ResourceSubscriptions {
            uris: vec![ROOM.to_owned(), ZONES.to_owned()],
            subscribers: 2,
        }
    );
    assert!(matches!(
        ServerNotification::decode(&observed(&first)[0]),
        Ok(ServerNotification::SubscriptionsAcknowledged(_))
    ));

    assert_eq!(
        updates
            .publish(&[ROOM.to_owned(), ROOM.to_owned()])
            .unwrap(),
        1,
        "duplicate batch URIs must generate one invalidation per matching stream"
    );
    let first_messages = observed(&first);
    assert_eq!(first_messages.len(), 2);
    assert_eq!(observed(&second).len(), 1, "a different URI stays idle");
    let ServerNotification::ResourceUpdated(event) =
        ServerNotification::decode(&first_messages[1]).expect("typed modern update")
    else {
        panic!("expected only the typed resource update");
    };
    assert_eq!(event.uri.as_str(), ROOM);
    assert_eq!(
        event
            .meta
            .as_ref()
            .and_then(|meta| meta.get(crate::FINAL_SUBSCRIPTION_ID_META_KEY)),
        Some(&serde_json::json!(71))
    );

    drop(first_listen);
    assert_eq!(
        updates.subscriptions(),
        ResourceSubscriptions {
            uris: vec![ZONES.to_owned()],
            subscribers: 1,
        }
    );
    assert_eq!(updates.publish(&[ROOM.to_owned()]).unwrap(), 0);
    assert_eq!(updates.publish(&[ZONES.to_owned()]).unwrap(), 1);
    assert_eq!(observed(&first).len(), 2);
    assert_eq!(observed(&second).len(), 2);
    drop(second_listen);
    assert_eq!(updates.subscriptions(), ResourceSubscriptions::default());
}

#[test]
fn every_batch_is_validated_before_any_subscriber_observes_it() {
    let server = server();
    let updates = server.resource_updates();
    let (sender, delivered) = recorder();
    let _listen = server
        .open_subscription_listen(RequestId::Number(73), filter(&[ZONES]), sender)
        .unwrap();

    assert!(
        updates
            .publish(&[ZONES.to_owned(), "relative/path".to_owned()])
            .is_err()
    );
    assert!(
        updates
            .publish(&vec![ZONES.to_owned(); MAX_RESOURCE_UPDATE_BATCH + 1])
            .is_err()
    );
    assert!(
        updates
            .publish(&[
                ZONES.to_owned(),
                format!("sonos://{}", "x".repeat(MAX_RESOURCE_UPDATE_BATCH_BYTES)),
            ])
            .is_err()
    );
    assert_eq!(
        observed(&delivered).len(),
        1,
        "only the acknowledgement exists after every invalid batch"
    );
    assert_eq!(updates.publish(&[ZONES.to_owned()]).unwrap(), 1);
    assert_eq!(observed(&delivered).len(), 2);
}

#[test]
fn cancelled_listeners_are_absent_even_before_their_lease_is_dropped() {
    let server = server();
    let updates = server.resource_updates();
    let (sender, delivered) = recorder();
    let cancellation = McpRequestCancellation::new();
    let _lease = server
        .final_subscriptions
        .open(
            RequestId::Number(74),
            filter(&[ZONES]),
            false,
            None,
            cancellation.clone(),
            None,
            sender,
        )
        .unwrap();
    assert_eq!(updates.subscriptions().subscribers, 1);
    cancellation.cancel();
    assert_eq!(updates.subscriptions(), ResourceSubscriptions::default());
    assert_eq!(updates.publish(&[ZONES.to_owned()]).unwrap(), 0);
    assert_eq!(observed(&delivered).len(), 1);
}

#[test]
fn a_listen_handle_cannot_keep_an_application_publisher_open_after_server_drop() {
    let server = server();
    let updates = server.resource_updates();
    let clone = updates.clone();
    let (sender, delivered) = recorder();
    let listen = server
        .open_subscription_listen(RequestId::Number(75), filter(&[ZONES]), sender)
        .unwrap();
    assert!(!updates.is_closed());

    drop(server);
    assert!(updates.is_closed());
    assert!(clone.is_closed());
    assert_eq!(updates.subscriptions(), ResourceSubscriptions::default());
    assert_eq!(updates.publish(&[ZONES.to_owned()]).unwrap(), 0);
    assert_eq!(observed(&delivered).len(), 1);
    drop(listen);
}

#[test]
fn server_termination_revokes_the_publisher_before_the_server_is_dropped() {
    let server = server();
    let updates = server.resource_updates();
    let (sender, _delivered) = recorder();
    let _listen = server
        .open_subscription_listen(RequestId::Number(76), filter(&[ZONES]), sender)
        .unwrap();
    assert_eq!(server.terminate_subscription_streams(), 1);
    assert!(updates.is_closed());
    assert_eq!(updates.subscriptions(), ResourceSubscriptions::default());
    assert_eq!(updates.publish(&[ZONES.to_owned()]).unwrap(), 0);
}

#[test]
fn legacy_weak_entries_recheck_unsubscribe_and_connection_generation() {
    let server = server();
    let updates = server.resource_updates();
    let (first_sender, first) = recorder();
    let first_uris = Arc::new(Mutex::new(vec![ZONES.to_owned(), ROOM.to_owned()]));
    let first_connection = server
        .resource_updates
        .register(Arc::clone(&first_uris), first_sender);
    let (second_sender, second) = recorder();
    let second_connection = server
        .resource_updates
        .register(Arc::new(Mutex::new(vec![ZONES.to_owned()])), second_sender);
    assert_eq!(updates.subscriptions().subscribers, 2);
    assert_eq!(updates.publish(&[ROOM.to_owned()]).unwrap(), 1);
    let first_messages = observed(&first);
    assert_eq!(first_messages.len(), 1);
    assert_eq!(first_messages[0].method, "notifications/resources/updated");
    let event: ResourceUpdatedNotificationParams =
        serde_json::from_value(first_messages[0].params.clone().unwrap()).unwrap();
    assert_eq!(event.uri, ROOM);
    assert!(observed(&second).is_empty());

    first_uris.lock().unwrap().clear();
    assert_eq!(updates.publish(&[ROOM.to_owned()]).unwrap(), 0);
    assert_eq!(updates.subscriptions().subscribers, 1);
    first_uris.lock().unwrap().push(ZONES.to_owned());
    first_connection.close();
    assert_eq!(
        updates.publish(&[ZONES.to_owned()]).unwrap(),
        1,
        "a retained closed generation cannot receive a replacement's update"
    );
    assert_eq!(observed(&first).len(), 1);
    assert_eq!(observed(&second).len(), 1);
    drop(second_connection);
    assert_eq!(updates.subscriptions(), ResourceSubscriptions::default());
    assert_eq!(updates.publish(&[ZONES.to_owned()]).unwrap(), 0);
}

#[cfg(feature = "legacy-2024-11-05")]
#[test]
fn legacy_idle_sse_receives_application_updates_after_subscribe_completes() {
    let server = server();
    let updates = server.resource_updates();
    let endpoint = server
        .into_http_endpoint("http://legacy.test")
        .expect("public dual-era endpoint");
    let cx = Cx::for_testing();
    let mut session = endpoint.open_session(&cx).unwrap();
    let ServerHttpEndpointResponse::LegacySse(mut stream) =
        block_on(session.handle_async(&cx, HttpRequest::new(HttpMethod::Get, "/sse")))
            .expect("legacy SSE opens")
    else {
        panic!("expected the legacy SSE stream");
    };
    stream.recv_event(&cx).expect("endpoint event");
    let session_id = session.legacy_session_id().to_owned();
    let post = |request: JsonRpcRequest| {
        HttpRequest::new(HttpMethod::Post, "/messages")
            .with_header("content-type", "application/json")
            .with_query("session_id", session_id.clone())
            .with_body(serde_json::to_vec(&request).unwrap())
    };
    for request in [
        JsonRpcRequest::new(
            "initialize",
            Some(serde_json::json!({
                "protocolVersion": crate::LEGACY_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "idle-resources", "version": "1.0.0"},
            })),
            81_i64,
        ),
        JsonRpcRequest::notification("notifications/initialized", None),
        JsonRpcRequest::new(
            "resources/subscribe",
            Some(serde_json::json!({"uri": ZONES})),
            82_i64,
        ),
    ] {
        assert!(matches!(
            block_on(session.handle_async(&cx, post(request))),
            Ok(ServerHttpEndpointResponse::Immediate(response))
                if response.status == HttpStatus::ACCEPTED
        ));
    }
    let initialized = stream.recv_event(&cx).expect("initialize response");
    let initialized: serde_json::Value = serde_json::from_str(&initialized.data).unwrap();
    assert_eq!(
        initialized.pointer("/result/capabilities/resources/subscribe"),
        Some(&serde_json::json!(true)),
        "resource registration alone advertises the working legacy capability"
    );
    let subscribed = stream.recv_event(&cx).expect("subscribe response");
    let subscribed: serde_json::Value = serde_json::from_str(&subscribed.data).unwrap();
    assert!(subscribed.get("error").is_none());
    assert_eq!(subscribed["id"], serde_json::json!(82));
    assert_eq!(
        updates.subscriptions(),
        ResourceSubscriptions {
            uris: vec![ZONES.to_owned()],
            subscribers: 1,
        }
    );

    // No handler or request context remains live when the application publishes.
    assert_eq!(updates.publish(&[ZONES.to_owned()]).unwrap(), 1);
    let event = stream.recv_event(&cx).expect("idle resource update");
    let event: JsonRpcRequest = serde_json::from_str(&event.data).unwrap();
    assert_eq!(event.method, "notifications/resources/updated");
    assert_eq!(event.params, Some(serde_json::json!({"uri": ZONES})));
    assert_eq!(updates.publish(&[ROOM.to_owned()]).unwrap(), 0);
    assert!(stream.try_recv_event(&cx).unwrap().is_none());

    assert!(matches!(
        block_on(session.handle_async(
            &cx,
            post(JsonRpcRequest::new(
                "resources/unsubscribe",
                Some(serde_json::json!({"uri": ZONES})),
                83_i64,
            )),
        )),
        Ok(ServerHttpEndpointResponse::Immediate(response))
            if response.status == HttpStatus::ACCEPTED
    ));
    stream.recv_event(&cx).expect("unsubscribe response");
    assert_eq!(updates.subscriptions(), ResourceSubscriptions::default());
    assert_eq!(updates.publish(&[ZONES.to_owned()]).unwrap(), 0);
    assert!(stream.try_recv_event(&cx).unwrap().is_none());

    assert!(matches!(
        block_on(session.handle_async(
            &cx,
            post(JsonRpcRequest::new(
                "resources/subscribe",
                Some(serde_json::json!({"uri": ZONES})),
                84_i64,
            )),
        )),
        Ok(ServerHttpEndpointResponse::Immediate(response))
            if response.status == HttpStatus::ACCEPTED
    ));
    stream.recv_event(&cx).expect("resubscribe response");
    assert_eq!(updates.subscriptions().subscribers, 1);
    block_on(session.close(&cx));
    assert_eq!(updates.subscriptions(), ResourceSubscriptions::default());
    assert_eq!(updates.publish(&[ZONES.to_owned()]).unwrap(), 0);
}

async fn next_modern_event(sse: &crate::ServerHttpSseResponse, cx: &Cx) -> JsonRpcRequest {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(event) = sse.pop_event().expect("live modern SSE body") {
            return serde_json::from_str(&event.data).expect("typed notification envelope");
        }
        assert!(
            std::time::Instant::now() < deadline,
            "modern notification timed out"
        );
        asupersync::time::sleep(cx.now(), std::time::Duration::from_millis(1)).await;
    }
}

#[test]
fn modern_idle_sse_receives_application_updates_without_another_request() {
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .with_reactor(
            asupersync::runtime::reactor::create_reactor()
                .expect("modern subscription test reactor"),
        )
        .build()
        .expect("modern subscription test runtime");
    runtime.block_on(async move {
        let cx = Cx::current().expect("owned request runtime");
        let server = server();
        let updates = server.resource_updates();
        let endpoint = server.into_http_endpoint("http://legacy.test").unwrap();
        let mut session = endpoint.open_session(&cx).unwrap();
        let request = JsonRpcRequest::new(
            crate::SUBSCRIPTIONS_LISTEN,
            Some(serde_json::json!({
                "_meta": {
                    crate::MODERN_PROTOCOL_VERSION_METADATA_KEY: crate::MODERN_PROTOCOL_VERSION,
                    crate::FINAL_CLIENT_CAPABILITIES_META_KEY: {},
                },
                "notifications": {"resourceSubscriptions": [ZONES]},
            })),
            RequestId::Number(91),
        );
        let ServerHttpEndpointResponse::ModernSse(sse) = session
            .handle_async(
                &cx,
                HttpRequest::new(HttpMethod::Post, "/mcp")
                    .with_header("content-type", "application/json")
                    .with_header("accept", "text/event-stream")
                    .with_header("mcp-protocol-version", crate::MODERN_PROTOCOL_VERSION)
                    .with_header("mcp-method", crate::SUBSCRIPTIONS_LISTEN)
                    .with_body(serde_json::to_vec(&request).unwrap()),
            )
            .await
            .expect("the fixed Auto server admits a modern listen")
        else {
            panic!("the modern listen owns an SSE response body");
        };
        let ack = next_modern_event(&sse, &cx).await;
        assert!(matches!(
            ServerNotification::decode(&ack),
            Ok(ServerNotification::SubscriptionsAcknowledged(_))
        ));
        assert_eq!(
            updates.subscriptions(),
            ResourceSubscriptions {
                uris: vec![ZONES.to_owned()],
                subscribers: 1,
            }
        );

        // Only the listen remains active; publication uses no request context.
        assert_eq!(updates.publish(&[ZONES.to_owned()]).unwrap(), 1);
        let update = next_modern_event(&sse, &cx).await;
        let ServerNotification::ResourceUpdated(update) =
            ServerNotification::decode(&update).expect("typed modern resource notification")
        else {
            panic!("the idle listen must receive the resource update");
        };
        assert_eq!(update.uri.as_str(), ZONES);
        assert_eq!(
            update
                .meta
                .as_ref()
                .and_then(|meta| meta.get(crate::FINAL_SUBSCRIPTION_ID_META_KEY)),
            Some(&serde_json::json!(91))
        );
        assert_eq!(updates.publish(&[ROOM.to_owned()]).unwrap(), 0);
        assert!(sse.pop_event().unwrap().is_none());
        assert!(!sse.is_finished());

        session.close(&cx).await;
        assert_eq!(updates.subscriptions(), ResourceSubscriptions::default());
        assert_eq!(updates.publish(&[ZONES.to_owned()]).unwrap(), 0);
        assert!(sse.cancellation().is_cancelled());
        drop(sse);
        drop(session);
        drop(endpoint);
        assert!(updates.is_closed());
    });
}

#[test]
fn legacy_sender_can_reenter_subscription_metadata_without_holding_its_lock() {
    let server = server();
    let updates = server.resource_updates();
    let selected = Arc::new(Mutex::new(vec![ZONES.to_owned()]));
    let callback_selected = Arc::clone(&selected);
    let callback_updates = updates.clone();
    let (sent, received) = std::sync::mpsc::channel();
    let sender: NotificationSender = Arc::new(move |_notification| {
        callback_selected.lock().unwrap().clear();
        sent.send(callback_updates.subscriptions()).unwrap();
    });
    let _connection = server.resource_updates.register(selected, sender);
    let publisher = updates.clone();
    let (finished, completion) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        finished
            .send(publisher.publish(&[ZONES.to_owned()]))
            .unwrap();
    });
    assert_eq!(
        received
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap(),
        ResourceSubscriptions::default()
    );
    assert_eq!(
        completion
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap()
            .unwrap(),
        1
    );
    worker.join().unwrap();
    assert_eq!(updates.publish(&[ZONES.to_owned()]).unwrap(), 0);
}

#[test]
fn server_drop_revokes_an_in_flight_publishers_remaining_batch() {
    let server = server();
    let updates = server.resource_updates();
    let (entered, blocked) = std::sync::mpsc::channel();
    let (resume, released) = std::sync::mpsc::channel();
    let released = Mutex::new(released);
    let (sent, observed) = std::sync::mpsc::channel();
    let sender: NotificationSender = Arc::new(move |notification| {
        if let Ok(ServerNotification::ResourceUpdated(event)) =
            ServerNotification::decode(&notification)
        {
            sent.send(event.uri.as_str().to_owned()).unwrap();
            if event.uri.as_str() == ROOM {
                entered.send(()).unwrap();
                released
                    .lock()
                    .unwrap()
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .expect("the owner-drop check releases the admitted callback");
            }
        }
    });
    // Keep the final registry alive after dropping the server itself.
    let _listen = server
        .open_subscription_listen(RequestId::Number(92), filter(&[ROOM, ZONES]), sender)
        .unwrap();
    let publisher = updates.clone();
    let worker =
        std::thread::spawn(move || publisher.publish(&[ROOM.to_owned(), ZONES.to_owned()]));
    blocked
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("the first publication holds a temporary registry reference");

    drop(server);
    assert!(
        updates.is_closed(),
        "a running publication cannot retain the server owner's authority"
    );
    assert_eq!(updates.subscriptions(), ResourceSubscriptions::default());
    assert_eq!(updates.publish(&[ZONES.to_owned()]).unwrap(), 0);
    resume.send(()).unwrap();
    assert_eq!(worker.join().unwrap().unwrap(), 1);
    assert_eq!(observed.try_iter().collect::<Vec<_>>(), [ROOM]);
}

#[test]
fn a_held_acknowledgement_hides_its_large_subscription_until_opening_finishes() {
    let server = Arc::new(server());
    let updates = server.resource_updates();
    let mut uris = (0..MAX_RESOURCE_UPDATE_BATCH + 6)
        .map(|room| format!("sonos://zones/Room{room}"))
        .collect::<Vec<_>>();
    uris.sort();
    let (entered, blocked) = std::sync::mpsc::channel();
    let (resume, released) = std::sync::mpsc::channel();
    let released = Mutex::new(released);
    let (sent, observed) = std::sync::mpsc::channel();
    let sender: NotificationSender =
        Arc::new(move |notification| {
            match ServerNotification::decode(&notification).expect("typed subscription frame") {
                ServerNotification::SubscriptionsAcknowledged(_) => {
                    entered.send(()).unwrap();
                    released
                        .lock()
                        .unwrap()
                        .recv_timeout(std::time::Duration::from_secs(5))
                        .expect("the snapshot check releases the acknowledgement");
                }
                ServerNotification::ResourceUpdated(event) => {
                    sent.send(event.uri.as_str().to_owned()).unwrap();
                }
                event => panic!("unexpected subscription frame: {event:?}"),
            }
        });
    let opening_server = Arc::clone(&server);
    let opening_uris = uris.clone();
    let opener = std::thread::spawn(move || {
        opening_server.open_subscription_listen(
            RequestId::Number(93),
            SubscriptionFilter {
                resource_subscriptions: Some(opening_uris),
                ..SubscriptionFilter::default()
            },
            sender,
        )
    });
    blocked
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("the acknowledgement is held before publication");

    assert_eq!(
        updates.subscriptions(),
        ResourceSubscriptions::default(),
        "the feed must not start seventy initial invalidations on an opening listen"
    );
    // Explicit publications still use the registry's existing pre-ACK queue.
    assert_eq!(updates.publish(&[uris[0].clone()]).unwrap(), 1);
    assert!(observed.try_recv().is_err());
    resume.send(()).unwrap();
    let _listen = opener.join().unwrap().unwrap();
    assert_eq!(observed.try_recv().unwrap(), uris[0]);
    assert_eq!(
        updates.subscriptions(),
        ResourceSubscriptions {
            uris: uris.clone(),
            subscribers: 1,
        }
    );

    for batch in uris.chunks(MAX_RESOURCE_UPDATE_BATCH) {
        assert_eq!(updates.publish(batch).unwrap(), batch.len());
    }
    let mut delivered = observed.try_iter().collect::<Vec<_>>();
    delivered.sort();
    assert_eq!(delivered, uris);
    assert_eq!(updates.subscriptions().subscribers, 1);
}
