use super::*;
use crate::resources::{DjResource, ZoneResource, ZonesResource};
use crate::tools::{Backend, Daemon};
use fastmcp::{RequestId, SubscriptionFilter};
use fastmcp_server::{NotificationSender, Server, ServerBuilder, SubscriptionListenHandle};
use fsonos_api::ErrorCode;
use fsonos_api::events::{EventDto, EventsDto};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_proto::{ProtoError, Transport};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::net::IpAddr;
use std::sync::{Arc, mpsc};

const WAIT: Duration = Duration::from_secs(5);
const ZONES: &str = "sonos://zones";
const ROOM: &str = "sonos://zones/Room%20%2F%20%26%40S2";
const DJ: &str = "sonos://dj";

fn server() -> Server {
    ServerBuilder::try_new_with_fixed_protocol_policy(
        "resource-event-tests",
        "1.0",
        fastmcp::ProtocolPolicy::Auto,
    )
    .unwrap()
    .resource(ZonesResource)
    .resource(ZoneResource)
    .resource(DjResource)
    .build()
}

fn listen(
    server: &Server,
    id: i64,
    uris: Vec<String>,
) -> (SubscriptionListenHandle, mpsc::Receiver<String>) {
    let (output, received) = mpsc::channel();
    let sender: NotificationSender = Arc::new(move |message| {
        if message.method == "notifications/resources/updated" {
            let params = message.params.expect("resource notification parameters");
            let uri = params["uri"].as_str().expect("exact resource URI");
            let _ = output.send(uri.to_owned());
        }
    });
    let listen = server
        .open_subscription_listen(
            RequestId::Number(id),
            SubscriptionFilter {
                resource_subscriptions: Some(uris),
                ..SubscriptionFilter::default()
            },
            sender,
        )
        .unwrap();
    (listen, received)
}

fn selected(server: &Server, uris: &[&str]) -> (SubscriptionListenHandle, mpsc::Receiver<String>) {
    listen(
        server,
        1,
        uris.iter().map(|uri| (*uri).to_owned()).collect(),
    )
}

fn take(received: &mpsc::Receiver<String>, expected: &[&str]) {
    let mut actual = (0..expected.len())
        .map(|_| received.recv_timeout(WAIT).expect("resource update"))
        .collect::<Vec<_>>();
    let mut expected = expected
        .iter()
        .map(|uri| (*uri).to_owned())
        .collect::<Vec<_>>();
    actual.sort();
    expected.sort();
    assert_eq!(actual, expected);
    assert!(received.try_recv().is_err(), "coalesce repeated changes");
}

fn tail(last_id: u64) -> EventsDto {
    EventsDto {
        events: Vec::new(),
        last_id,
        reset: None,
    }
}

fn cursor(last_id: u64) -> ResourceChanges {
    ResourceChanges {
        last_id,
        reset: None,
    }
}

fn changed(last_id: u64) -> EventsDto {
    EventsDto {
        events: vec![
            EventDto {
                id: last_id - 1,
                event: "zone.state".into(),
                data: json!({"room": "Room / &", "volume": 42}),
            },
            EventDto {
                id: last_id,
                event: "dj.status".into(),
                data: json!({"zone": "Room / &@S2", "running": true}),
            },
        ],
        ..tail(last_id)
    }
}

struct NoIo;

impl Transport for NoIo {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("resource notifications must not contact local speakers")
    }
}

struct Request {
    path: String,
    reply: mpsc::Sender<Result<Value, Failure>>,
    thread: std::thread::ThreadId,
}

impl Request {
    fn answer(self, answer: EventsDto) {
        self.reply
            .send(Ok(serde_json::to_value(answer).unwrap()))
            .unwrap();
    }
}

struct HeldDaemon(mpsc::Sender<Request>);

impl Daemon for HeldDaemon {
    fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Option<Result<Value, Failure>> {
        assert_eq!(method, "GET");
        assert!(body.is_none());
        let (reply, received) = mpsc::channel();
        let request = Request {
            path: path.into(),
            reply,
            thread: thread::current().id(),
        };
        let answer = if self.0.send(request).is_ok() {
            received.recv_timeout(WAIT).unwrap_or_else(|_| {
                Err(Failure::new(
                    ErrorCode::NotReady,
                    "test daemon disconnected",
                ))
            })
        } else {
            Err(Failure::new(ErrorCode::NotReady, "test daemon closed"))
        };
        Some(answer)
    }
}

fn event_backend() -> (Arc<Backend>, mpsc::Receiver<Request>) {
    let (send, requests) = mpsc::channel();
    let backend = Backend::new(
        Box::new(NoIo),
        Box::new(|_| panic!("resource subscriptions must not survey the LAN")),
        Policy::default(),
        Client::McpStdio,
        Box::new(SystemClock),
    )
    .with_daemon(Arc::new(HeldDaemon(send)));
    (Arc::new(backend), requests)
}

fn request(requests: &mpsc::Receiver<Request>, path: &str) -> Request {
    let request = requests.recv_timeout(WAIT).expect("daemon event read");
    assert_eq!(request.path, path);
    assert_ne!(request.thread, thread::current().id());
    request
}

fn finished(worker: JoinHandle<()>) {
    let deadline = Instant::now() + WAIT;
    while !worker.is_finished() {
        assert!(
            Instant::now() < deadline,
            "closed server retained its worker"
        );
        thread::sleep(Duration::from_millis(10));
    }
    worker.join().unwrap();
}

#[test]
fn an_idle_subscription_follows_the_selected_daemon_and_stops_after_unsubscribe() {
    let server = server();
    let updates = server.resource_updates();
    let (backend, requests) = event_backend();
    let worker = spawn(updates.clone(), move |since, wait| {
        backend.resource_changes_answer(since, wait)
    })
    .unwrap();
    assert!(
        requests.recv_timeout(IDLE * 2).is_err(),
        "no subscriber means no I/O"
    );

    let (listen, received) = selected(&server, &[ZONES, ROOM, DJ]);
    request(&requests, "/events?wait=0").answer(tail(7));
    take(&received, &[ZONES, ROOM, DJ]);
    request(&requests, "/events?wait=2&since=7").answer(changed(9));
    take(&received, &[ZONES, ROOM, DJ]);

    let pending = request(&requests, "/events?wait=2&since=9");
    drop(listen);
    pending.answer(changed(11));
    assert!(received.recv_timeout(IDLE * 2).is_err());
    assert!(
        requests.recv_timeout(IDLE * 2).is_err(),
        "unsubscribed streams stop polling"
    );
    drop(server);
    finished(worker);
    assert!(updates.is_closed());
}

#[test]
fn closing_a_server_during_a_daemon_read_cannot_publish_or_start_another_read() {
    let server = server();
    let updates = server.resource_updates();
    let (_listen, received) = selected(&server, &[ZONES]);
    let (backend, requests) = event_backend();
    let worker = spawn(updates.clone(), move |since, wait| {
        backend.resource_changes_answer(since, wait)
    })
    .unwrap();
    let pending = request(&requests, "/events?wait=0");
    drop(server);
    assert!(
        updates.is_closed(),
        "a retained listen handle cannot retain the server"
    );
    pending.answer(changed(3));
    finished(worker);
    assert!(received.try_recv().is_err());
    assert!(requests.try_recv().is_err());
}

#[test]
fn quiet_polls_stay_quiet_but_loss_restart_and_recovery_invalidate_all_resources() {
    let server = server();
    let updates = server.resource_updates();
    let (_listen, received) = selected(&server, &[ZONES, ROOM, DJ]);
    let mut feed = Feed::default();
    assert_eq!(feed.poll(&updates, &|_, _| Ok(cursor(7))), IDLE);
    take(&received, &[ZONES, ROOM, DJ]);
    feed.poll(&updates, &|since, wait| {
        assert_eq!((since, wait), (Some(7), WAIT_SECS));
        Ok(cursor(7))
    });
    assert!(received.try_recv().is_err());
    assert_eq!(
        feed.poll(&updates, &|_, _| Err(Failure::new(
            ErrorCode::NotReady,
            "offline"
        ))),
        RETRY,
    );
    assert!(received.try_recv().is_err());
    assert_eq!(feed.since, Some(7));

    // Recovery with no new event still invalidates potentially stale documents.
    feed.poll(&updates, &|_, _| Ok(cursor(7)));
    take(&received, &[ZONES, ROOM, DJ]);
    feed.poll(&updates, &|_, _| {
        Ok(ResourceChanges {
            reset: Some(9),
            ..cursor(8)
        })
    });
    take(&received, &[ZONES, ROOM, DJ]);
    // Also handle a restarted remote daemon that omitted its reset marker.
    feed.poll(&updates, &|_, _| Ok(cursor(1)));
    take(&received, &[ZONES, ROOM, DJ]);
    assert_eq!(feed.since, Some(1));
    feed.poll(&updates, &|_, _| Ok(cursor(3)));
    take(&received, &[ZONES, ROOM, DJ]);
}

#[test]
fn a_subscription_joining_during_a_long_poll_gets_its_exact_qualified_uri() {
    let server = server();
    let updates = server.resource_updates();
    let (_zones, zones) = selected(&server, &[ZONES]);
    let mut feed = Feed::default();
    feed.poll(&updates, &|_, _| Ok(cursor(4)));
    take(&zones, &[ZONES]);
    let joined = RefCell::new(None);
    feed.poll(&updates, &|_, _| {
        *joined.borrow_mut() = Some(listen(&server, 2, vec![ROOM.into()]));
        Ok(cursor(4))
    });
    let (_room, room) = joined.into_inner().unwrap();
    take(&zones, &[ZONES]);
    take(&room, &[ROOM]);
}

#[test]
fn invalid_or_unrelated_subscriptions_do_not_poll_or_poison_valid_updates() {
    let server = server();
    let updates = server.resource_updates();
    let invalid = vec![
        "file:///unrelated".into(),
        "sonos://zones/unescaped room".into(),
        format!(
            "sonos://zones/{}",
            "x".repeat(MAX_RESOURCE_UPDATE_BATCH_BYTES)
        ),
    ];
    let (bad, bad_received) = listen(&server, 1, invalid);
    let mut feed = Feed::default();
    feed.poll(&updates, &|_, _| panic!("no valid Sonos subscription"));
    let (_good, good_received) = listen(&server, 2, vec![ROOM.into()]);
    feed.poll(&updates, &|_, _| Ok(cursor(4)));
    take(&good_received, &[ROOM]);
    assert!(bad_received.try_recv().is_err());
    drop(bad);
}

#[test]
fn large_subscription_sets_fit_both_publication_budgets_without_losing_rooms() {
    let server = server();
    let updates = server.resource_updates();
    let mut uris = (0..MAX_RESOURCE_UPDATE_BATCH + 3)
        .map(|i| format!("sonos://zones/Room{i}%40S2"))
        .collect::<Vec<_>>();
    uris.extend((0..3).map(|i| format!("sonos://zones/{i}{}%40S2", "x".repeat(32 * 1024))));
    let (_listen, received) = listen(&server, 1, uris.clone());
    let mut feed = Feed::default();
    feed.poll(&updates, &|_, _| Ok(cursor(3)));
    take(
        &received,
        &uris.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    assert!(
        !feed.dirty,
        "every batch must satisfy count and byte limits"
    );
}
