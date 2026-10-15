#![forbid(unsafe_code)]

use asupersync::Cx;
use fastapi::{Method, Request, RequestContext, ResponseBody};
use fsonos_api::{Identity, Surface, WebPolicy};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::{HouseholdState, Room};
use fsonos_types::{Generation, Player, PlayerId, ZoneGroup};
use serde_json::Value;
use std::fmt::Write as _;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

const PLAYER: &str = "RINCON_ART_STUB";
struct Metadata(Option<String>);
impl fsonos_proto::Transport for Metadata {
    fn soap_post(
        &self,
        _: IpAddr,
        _: &str,
        action: &str,
        _: &str,
    ) -> Result<String, fsonos_proto::ProtoError> {
        let action = action.trim_matches('"').rsplit('#').next().unwrap();
        let art = self
            .0
            .as_ref()
            .map(|u| format!("<upnp:albumArtURI>{}</upnp:albumArtURI>", escape(u)))
            .unwrap_or_default();
        let didl = format!(
            "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\"><item id=\"FV:2/1\"><dc:title>Stub Album</dc:title><upnp:class>object.item.audioItem.musicTrack</upnp:class>{art}<res>spotify:track:stub</res></item></DIDL-Lite>"
        );
        let fields = match action {
            "GetTransportInfo" => "<CurrentTransportState>PLAYING</CurrentTransportState><CurrentTransportStatus>OK</CurrentTransportStatus><CurrentSpeed>1</CurrentSpeed>".into(),
            "GetPositionInfo" => format!("<Track>1</Track><TrackDuration>00:03:00</TrackDuration><TrackURI>spotify:track:stub</TrackURI><RelTime>00:00:04</RelTime><TrackMetaData>{}</TrackMetaData>",escape(&didl)),
            "GetVolume" => "<CurrentVolume>20</CurrentVolume>".into(),
            "GetTransportSettings" => "<PlayMode>NORMAL</PlayMode><RecQualityMode>NOT_IMPLEMENTED</RecQualityMode>".into(),
            "GetMute" => "<CurrentMute>0</CurrentMute>".into(),
            "Browse" => format!("<Result>{}</Result><NumberReturned>1</NumberReturned><TotalMatches>1</TotalMatches><UpdateID>1</UpdateID>",escape(&didl)),
            _ => panic!("unexpected action {action}"),
        };
        Ok(format!(
            "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><u:{action}Response xmlns:u=\"urn:x\">{fields}</u:{action}Response></s:Body></s:Envelope>"
        ))
    }
}
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
fn surface(ip: IpAddr, art: Option<&str>, client: Client, policy: Policy) -> fastapi::App {
    let id = PlayerId(PLAYER.into());
    let h = HouseholdState {
        players: vec![Player {
            id: id.clone(),
            room_name: "Art Room".into(),
            ip,
            model: String::new(),
            generation: Generation::S2,
        }],
        groups: vec![ZoneGroup {
            coordinator: id.clone(),
            members: vec![id.clone()],
        }],
        rooms: vec![Room {
            name: "Art Room".into(),
            primary: id.clone(),
            players: vec![id.clone()],
            missing: vec![],
            coordinator: id,
        }],
        ..Default::default()
    };
    let surface = Arc::new(Surface::new(
        Box::new(Metadata(art.map(str::to_string))),
        Box::new(move |_| Ok(vec![h.clone()])),
        policy,
        Box::new(SystemClock),
    ));
    fsonos_api::app(
        &surface,
        &Identity::fixed(client),
        &WebPolicy::for_listener("127.0.0.1:8099".parse().unwrap(), &[]),
    )
}
fn request(
    app: &fastapi::App,
    target: &str,
) -> (u16, std::collections::BTreeMap<String, Vec<u8>>, Vec<u8>) {
    fsonos_spotify::fake_spotify::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let ctx = RequestContext::new(cx, 0);
        let (path, query) = target
            .split_once('?')
            .map_or((target, None), |(p, q)| (p, Some(q.into())));
        let mut req = Request::new(Method::Get, path);
        req.set_query(query);
        let response = app.handle(&ctx, &mut req).await;
        let (status, headers, body) = response.into_parts();
        let ResponseBody::Bytes(bytes) = body else {
            panic!("bytes")
        };
        (status.as_u16(), headers.into_iter().collect(), bytes)
    })
}
fn get_json(app: &fastapi::App, target: &str) -> (u16, Value) {
    let (s, _, b) = request(app, target);
    (s, serde_json::from_slice(&b).unwrap())
}
fn art_target(path: &str) -> String {
    let encoded = path.bytes().fold(String::new(), |mut text, byte| {
        write!(text, "%{byte:02X}").unwrap();
        text
    });
    format!("/speaker-art?player={PLAYER}&u={encoded}")
}
#[test]
fn pure_a1_a2_a8_unknown_player_and_unsafe_paths() {
    let app = surface(
        "192.0.2.10".parse().unwrap(),
        None,
        Client::LoopbackHttp,
        Policy::default(),
    );
    let (status, body) = get_json(&app, "/speaker-art?player=missing&u=%2Fgetaa");
    assert_eq!(status, 404);
    assert_eq!(body["code"], "UNKNOWN_PLAYER");
    for path in [
        "/status",
        "//host/getaa",
        "/getaa/../status",
        "/getaa/%2e%2e/status",
        "/getaa/%252e%252e/status",
        "/getaa//host",
        "/getaa/@host",
        "http://host/getaa",
        "/getaa?u=http://host",
        "/getaa\\..\\status",
        "/getaa#fragment",
        "/getaax",
        "/getaa%00",
        "/getaa%2f%2fhost",
        "/getaa%40host",
    ] {
        let (status, body) = get_json(&app, &art_target(path));
        assert_eq!(status, 400, "{path}: {body}");
        assert_eq!(body["code"], "INVALID_ARGUMENT");
    }
    assert_eq!(
        get_json(&app, &art_target(&format!("/getaa?u={}", "a".repeat(1024)))).0,
        400
    );
    for target in [
        "/speaker-art",
        "/speaker-art?player=RINCON_ART_STUB",
        "/speaker-art?player=RINCON_ART_STUB&u=%zz",
    ] {
        assert_eq!(get_json(&app, target).0, 400);
    }
}
#[test]
fn pure_a6_track_and_favorite_art_normalization() {
    let ip = "192.0.2.10".parse().unwrap();
    for (uri, expected) in [
        (None, None),
        (
            Some("https://cdn.example.invalid/art.jpg"),
            Some("https://cdn.example.invalid/art.jpg".into()),
        ),
        (
            Some("/getaa?s=1&u=cover"),
            Some(format!(
                "/speaker-art?player={PLAYER}&u=%2Fgetaa%3Fs%3D1%26u%3Dcover"
            )),
        ),
        (
            Some("http://192.0.2.10:1400/getaa?s=1&u=cover"),
            Some(format!(
                "/speaker-art?player={PLAYER}&u=%2Fgetaa%3Fs%3D1%26u%3Dcover"
            )),
        ),
        (Some("http://192.0.2.11:1400/getaa"), None),
        (Some("http://192.0.2.10:9999/getaa"), None),
    ] {
        let app = surface(ip, uri, Client::LoopbackHttp, Policy::default());
        let state = get_json(&app, "/zones/Art%20Room/state").1;
        let favorites = get_json(&app, "/favorites?zone=Art%20Room").1;
        for field in [&state["track"], &favorites[0]] {
            let key = if field.get("uri").is_some() {
                "art_url"
            } else {
                "art_uri"
            };
            assert_eq!(
                field.get(key).and_then(Value::as_str),
                expected.as_deref(),
                "{state} {favorites}"
            );
        }
        if uri.is_none() {
            assert!(state["track"].get("art_url").is_none());
        }
    }
}
#[test]
fn pure_a7_policy_and_openapi_operation() {
    let app = surface(
        "192.0.2.10".parse().unwrap(),
        None,
        Client::Unknown,
        Policy::from_toml("[clients.unknown]\nallow = [\"spotify_status\"]").unwrap(),
    );
    let (status, body) = get_json(&app, &art_target("/getaa"));
    assert_eq!(status, 403);
    assert_eq!(body["code"], "POLICY_DENIED");
    assert!(body["detail"].as_str().unwrap().contains("get_speaker_art"));
    let app = surface(
        "192.0.2.10".parse().unwrap(),
        None,
        Client::LoopbackHttp,
        Policy::default(),
    );
    let spec = get_json(&app, "/openapi.json").1;
    assert_eq!(
        spec["paths"]["/speaker-art"]["get"]["operationId"],
        "get_speaker_art"
    );
    assert_eq!(
        spec["paths"]["/speaker-art"]["get"]["responses"]["200"]["content"]["image/*"]["schema"]["format"],
        "binary"
    );
}

#[derive(Clone)]
struct Reply {
    status: u16,
    kind: String,
    bytes: Vec<u8>,
    delay: Duration,
}
struct Speaker {
    state: Arc<Mutex<Reply>>,
    paths: Arc<Mutex<Vec<String>>>,
    stop: Box<dyn FnOnce()>,
    worker: thread::JoinHandle<()>,
}
impl Speaker {
    fn start() -> Self {
        use asupersync::http::h1::server::HostPolicy;
        use asupersync::http::h1::types::{Request, Response};
        use asupersync::http::h1::{Http1Config, Http1Listener, Http1ListenerConfig};
        let state = Arc::new(Mutex::new(Reply {
            status: 200,
            kind: "image/png".into(),
            bytes: vec![137, 80, 78, 71],
            delay: Duration::ZERO,
        }));
        let paths = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = mpsc::channel();
        let s = state.clone();
        let p = paths.clone();
        let worker = thread::spawn(move || {
            let rt = fsonos_spotify::fake_spotify::runtime();
            let handle = rt.handle();
            rt.block_on(async move {
                let listener = Http1Listener::bind_with_config(
                    "127.0.0.1:1400",
                    move |req: Request| {
                        p.lock().unwrap().push(req.uri);
                        let reply = s.lock().unwrap().clone();
                        async move {
                            asupersync::time::sleep(asupersync::time::wall_now(), reply.delay)
                                .await;
                            Response::new(reply.status, "", reply.bytes)
                                .with_header("Content-Type", reply.kind)
                                .with_header("Location", "http://127.0.0.1:1400/status")
                        }
                    },
                    Http1ListenerConfig::default().http_config(
                        Http1Config::default()
                            .host_policy(HostPolicy::allow_list(vec!["127.0.0.1".into()])),
                    ),
                )
                .await
                .unwrap();
                tx.send(listener.shutdown_signal()).unwrap();
                listener.run(&handle).await.unwrap();
            });
        });
        let signal = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        Self {
            state,
            paths,
            stop: Box::new(move || signal.trigger_immediate()),
            worker,
        }
    }
    fn stop(self) {
        (self.stop)();
        self.worker.join().unwrap();
    }
}
#[test]
fn socket_a3_a4_a5_images_limits_timeout_and_no_redirects() {
    let speaker = Speaker::start();
    let app = surface(
        "127.0.0.1".parse().unwrap(),
        None,
        Client::LoopbackHttp,
        Policy::default(),
    );
    let (status, headers, bytes) = request(&app, &art_target("/getaa?s=1&u=cover"));
    assert_eq!(status, 200);
    assert_eq!(bytes, vec![137, 80, 78, 71]);
    assert_eq!(
        headers.get("content-type").map(Vec::as_slice),
        Some(b"image/png".as_slice())
    );
    assert_eq!(
        headers.get("cache-control").map(Vec::as_slice),
        Some(b"public, max-age=86400".as_slice())
    );
    assert_eq!(*speaker.paths.lock().unwrap(), ["/getaa?s=1&u=cover"]);
    for path in [
        "/getaa/../status",
        "/getaa/%252e%252e/status",
        "/getaa/@host",
    ] {
        assert_eq!(get_json(&app, &art_target(path)).0, 400);
    }
    assert_eq!(speaker.paths.lock().unwrap().len(), 1);
    speaker.state.lock().unwrap().kind = "text/html".into();
    let (status, body) = get_json(&app, &art_target("/getaa"));
    assert_eq!(status, 502);
    assert_eq!(body["code"], "BAD_ART");
    {
        let mut s = speaker.state.lock().unwrap();
        s.kind = "image/png".into();
        s.bytes = vec![0; 4 * 1024 * 1024 + 1];
    }
    let (status, body) = get_json(&app, &art_target("/getaa"));
    assert_eq!(status, 502);
    assert_eq!(body["code"], "BAD_ART");
    {
        let mut s = speaker.state.lock().unwrap();
        s.bytes = vec![0; 4 * 1024 * 1024];
    }
    assert_eq!(
        request(&app, &art_target("/getaa")).2.len(),
        4 * 1024 * 1024
    );
    speaker.state.lock().unwrap().status = 404;
    let (status, headers, _) = request(&app, &art_target("/getaa"));
    assert_eq!(status, 404);
    assert!(!headers.contains_key("cache-control"));
    speaker.state.lock().unwrap().status = 302;
    let count = speaker.paths.lock().unwrap().len();
    assert_eq!(get_json(&app, &art_target("/getaa")).0, 502);
    assert_eq!(speaker.paths.lock().unwrap().len(), count + 1);
    {
        let mut s = speaker.state.lock().unwrap();
        s.status = 200;
        s.bytes = vec![1];
        s.delay = Duration::from_secs(6);
    }
    let started = Instant::now();
    let (status, body) = get_json(&app, &art_target("/getaa"));
    assert_eq!(status, 502);
    assert_eq!(body["retryable"], true);
    assert!(started.elapsed() < Duration::from_secs(6));
    speaker.stop();
    let (status, body) = get_json(&app, &art_target("/getaa"));
    assert_eq!(status, 502);
    assert_eq!(body["retryable"], true);
}

#[test]
fn pure_track_dto_carries_optional_art() {
    let position = fsonos_proto::control::PositionInfo {
        track: 1,
        duration_secs: None,
        position_secs: None,
        uri: "spotify:track:stub".into(),
        metadata: None,
    };
    assert!(
        fsonos_api::reads::TrackDto::from_position(&position)
            .unwrap()
            .art_url
            .is_none()
    );
}
