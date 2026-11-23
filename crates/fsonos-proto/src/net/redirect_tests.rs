//! Endpoint identity and control must not be borrowed through HTTP redirects.
//! Real localhost servers also prove that artwork retains its redirect support.

use super::*;

const ART: &[u8] = &[0x89, b'P', b'N', b'G', 0xff, 0x00, 0xfe];

struct Server {
    addr: SocketAddr,
    received: Arc<Mutex<Vec<(String, String)>>>,
    shutdown: ShutdownSignal,
    worker: Option<JoinHandle<()>>,
}

impl Server {
    fn start(status: u16, location: Option<String>) -> Self {
        let received = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&received);
        let (ready_tx, ready_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let runtime = new_runtime().unwrap();
            let handle = runtime.handle();
            runtime.block_on(async move {
                let config = Http1ListenerConfig::default().http_config(
                    Http1Config::default()
                        .host_policy(HostPolicy::allow_list(vec!["127.0.0.1".into()])),
                );
                let listener = Http1Listener::bind_with_config(
                    "127.0.0.1:0",
                    move |request: Request| {
                        let observed = Arc::clone(&observed);
                        let location = location.clone();
                        async move {
                            observed
                                .lock()
                                .unwrap()
                                .push((request.method.as_str().into(), request.uri));
                            let response = Response::new(status, "Response", ART.to_vec())
                                .with_header("Content-Type", "image/png")
                                .with_header("SID", "uuid:redirect-target")
                                .with_header("TIMEOUT", "Second-300");
                            match location {
                                Some(location) => response.with_header("Location", location),
                                None => response,
                            }
                        }
                    },
                    config,
                )
                .await
                .unwrap();
                ready_tx
                    .send((listener.local_addr().unwrap(), listener.shutdown_signal()))
                    .unwrap();
                listener.run(&handle).await.unwrap();
            });
        });
        let (addr, shutdown) = ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        Self {
            addr,
            received,
            shutdown,
            worker: Some(worker),
        }
    }

    fn requests(&self) -> Vec<(String, String)> {
        self.received.lock().unwrap().clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown.trigger_immediate();
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

fn refused<T: std::fmt::Debug>(result: Result<T, ProtoError>, status: u16) {
    match result {
        Err(ProtoError::Network { detail, .. }) => {
            assert!(detail.contains(&format!("HTTP {status}")), "{detail}");
        }
        other => panic!("expected HTTP {status} refusal, got {other:?}"),
    }
}

fn routed(server: &Server) -> (Lan, IpAddr) {
    let player = "192.0.2.10".parse().unwrap();
    let lan = Lan::with_timeout(Duration::from_secs(5))
        .unwrap()
        .with_routes(vec![(player, server.addr)]);
    (lan, player)
}

#[test]
fn device_descriptions_and_batches_never_follow_redirects() {
    for status in [301, 302, 303, 307, 308] {
        let target = Server::start(200, None);
        let original = Server::start(
            status,
            Some(format!("http://{}/xml/device_description.xml", target.addr)),
        );
        let (lan, player) = routed(&original);
        let url = ssdp::description_url(player);
        refused(lan.http_get(&url), status);
        let mut batch = lan.http_get_many(std::slice::from_ref(&url), &|| false);
        assert_eq!(batch.len(), 1);
        refused(batch.remove(0), status);
        assert_eq!(
            original.requests(),
            vec![
                ("GET".into(), "/xml/device_description.xml".into()),
                ("GET".into(), "/xml/device_description.xml".into()),
            ]
        );
        assert!(target.requests().is_empty(), "borrowed another description");
    }
}

#[test]
fn soap_and_gena_never_redirect_reads_writes_or_subscription_authority() {
    for status in [301, 302, 303, 307, 308] {
        let target = Server::start(200, None);
        let original = Server::start(status, Some(format!("http://{}/borrowed", target.addr)));
        let (lan, player) = routed(&original);
        for action in ["GetZoneGroupState", "SetVolume"] {
            refused(
                lan.soap_post(player, "/control", action, "<envelope/>"),
                status,
            );
        }
        refused(
            lan.subscribe(player, "/events", "http://127.0.0.1:9999/callback", 300),
            status,
        );
        refused(lan.renew(player, "/events", "uuid:original", 300), status);
        refused(lan.unsubscribe(player, "/events", "uuid:original"), status);
        assert_eq!(
            original.requests(),
            vec![
                ("POST".into(), "/control".into()),
                ("POST".into(), "/control".into()),
                ("SUBSCRIBE".into(), "/events".into()),
                ("SUBSCRIBE".into(), "/events".into()),
                ("UNSUBSCRIBE".into(), "/events".into()),
            ]
        );
        assert!(
            target.requests().is_empty(),
            "redirect received speaker control or subscription authority"
        );
    }
}

#[test]
fn artwork_can_redirect_without_weakening_later_description_reads() {
    let target = Server::start(200, None);
    let original = Server::start(302, Some(format!("http://{}/cover", target.addr)));
    let (lan, player) = routed(&original);
    let art = lan
        .http_get_bytes(&format!("http://{player}:1400/getaa"))
        .unwrap();
    assert_eq!(art.body, ART);
    assert_eq!(art.content_type.as_deref(), Some("image/png"));
    refused(lan.http_get(&ssdp::description_url(player)), 302);
    assert_eq!(
        original.requests(),
        vec![
            ("GET".into(), "/getaa".into()),
            ("GET".into(), "/xml/device_description.xml".into()),
        ]
    );
    assert_eq!(target.requests(), vec![("GET".into(), "/cover".into())]);
}
