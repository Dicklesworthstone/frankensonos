//! The Spotify sign-in routes over a real loopback socket, with a stand-in
//! engine (fsonos-spotify's flow is tested in its own crate). Covered: the
//! status, begin, the browser's callback page (signed in, or why not, with no
//! script and no referrer), the pasted address, the answer without a Spotify
//! app, and the house policy's deny.

use asupersync::Cx;
use asupersync::http::Client as HttpClient;
use asupersync::runtime::{Runtime, RuntimeBuilder, reactor::create_reactor};
use fastapi::{ServerConfig, TcpServer};
use fsonos_api::surface::spotify_auth::{SignInDto, SignInStartDto, SpotifyAuth};
use fsonos_api::{Failure, Identity, Surface, WebPolicy};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_proto::{ProtoError, Transport};
use serde_json::{Value, json};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

const REDIRECT_URI: &str = "http://127.0.0.1:8099/auth/spotify/callback";

fn runtime() -> Runtime {
    RuntimeBuilder::current_thread()
        .with_reactor(create_reactor().expect("reactor"))
        .blocking_threads(0, 4)
        .build()
        .expect("runtime")
}

/// No speakers: the sign-in never reaches one.
struct NoSpeakers;

impl Transport for NoSpeakers {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        Err(ProtoError::Network {
            target: "no speakers".into(),
            detail: "none here".into(),
        })
    }
}

/// Signs in when the redirect carries `state=good`.
#[derive(Default)]
struct FakeAuth {
    signed_in: Mutex<bool>,
    pending: Mutex<bool>,
}

impl SpotifyAuth for FakeAuth {
    fn status(&self) -> SignInDto {
        SignInDto {
            signed_in: *self.signed_in.lock().unwrap(),
            pending: *self.pending.lock().unwrap(),
            redirect_uri: REDIRECT_URI.into(),
        }
    }

    fn begin(&self) -> Result<SignInStartDto, Failure> {
        *self.pending.lock().unwrap() = true;
        Ok(SignInStartDto {
            url: "https://accounts.example.invalid/authorize?state=good".into(),
            redirect_uri: REDIRECT_URI.into(),
        })
    }

    fn complete(&self, redirect: &str) -> Result<(), Failure> {
        if !*self.pending.lock().unwrap() {
            return Err(Failure::invalid("no Spotify sign-in is waiting"));
        }
        if !redirect.contains("state=good") {
            return Err(Failure::invalid(
                "the redirect's state does not match <script>",
            ));
        }
        *self.pending.lock().unwrap() = false;
        *self.signed_in.lock().unwrap() = true;
        Ok(())
    }
}

/// The API on an ephemeral loopback port over `surface`.
fn serve(surface: Surface) -> SocketAddr {
    let web = WebPolicy::for_listener("127.0.0.1:0".parse().unwrap(), &[]);
    let app = Arc::new(fsonos_api::app(
        &Arc::new(surface),
        &Identity::fixed(Client::LoopbackHttp),
        &web,
    ));
    let config = ServerConfig::new("127.0.0.1:0").with_allowed_hosts(web.hosts().to_vec());
    let server = Arc::new(TcpServer::new(config));
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        runtime().block_on(async move {
            let cx = Cx::current().expect("ambient Cx");
            let listener = asupersync::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind loopback");
            tx.send(listener.local_addr().expect("local addr"))
                .expect("report addr");
            let _ = server.serve_on_app(&cx, listener, app).await;
        });
    });
    rx.recv_timeout(Duration::from_secs(10))
        .expect("server ready")
}

fn surface(policy: Policy) -> Surface {
    Surface::new(
        Box::new(NoSpeakers),
        Box::new(|_| Ok(Vec::new())),
        policy,
        Box::new(SystemClock),
    )
}

/// `(status, headers lowercased, body)`.
fn call(
    addr: SocketAddr,
    path: &str,
    body: Option<&Value>,
) -> (u16, Vec<(String, String)>, String) {
    let url = format!("http://{addr}{path}");
    let body = body.map(Value::to_string);
    runtime().block_on(async move {
        let cx = Cx::current().expect("ambient Cx");
        let http = HttpClient::default_for_runtime(&cx);
        let request = match body {
            Some(body) => http
                .post(url)
                .header("Content-Type", "application/json")
                .body(body),
            None => http.get(url),
        };
        let resp = request.send(&cx).await.expect("HTTP round trip");
        let headers = resp
            .headers
            .iter()
            .map(|(n, v)| (n.to_ascii_lowercase(), v.clone()))
            .collect();
        (
            resp.status,
            headers,
            String::from_utf8_lossy(&resp.body).into_owned(),
        )
    })
}

fn json_of(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or(Value::Null)
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

#[test]
fn without_a_spotify_app_the_sign_in_is_not_implemented() {
    let addr = serve(surface(Policy::default()));
    let (status, _, body) = call(addr, "/auth/spotify", None);
    assert_eq!(status, 501, "{body}");
    assert_eq!(json_of(&body)["code"], "NOT_IMPLEMENTED");
    assert!(
        json_of(&body)["hint"]
            .as_str()
            .unwrap_or_default()
            .contains("FSONOS_SPOTIFY_CLIENT_ID")
    );
}

#[test]
fn the_browser_comes_back_to_the_daemon_and_signs_in() {
    let addr = serve(surface(Policy::default()).with_spotify_auth(Box::new(FakeAuth::default())));
    let (status, _, body) = call(addr, "/auth/spotify", None);
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        json_of(&body),
        json!({ "signed_in": false, "pending": false, "redirect_uri": REDIRECT_URI })
    );

    let (status, _, body) = call(addr, "/auth/spotify/begin", Some(&json!({})));
    assert_eq!(status, 200, "{body}");
    assert!(
        json_of(&body)["url"]
            .as_str()
            .unwrap_or_default()
            .contains("state=good")
    );
    assert_eq!(
        json_of(&call(addr, "/auth/spotify", None).2)["pending"],
        true
    );

    // A forged redirect: an HTML page that says why (escaped), and the
    // sign-in still waits.
    let (status, headers, page) = call(addr, "/auth/spotify/callback?code=c&state=forged", None);
    assert_eq!(status, 422, "{page}");
    assert_eq!(
        header(&headers, "content-type"),
        Some("text/html; charset=utf-8")
    );
    assert!(
        page.contains("did not finish") && page.contains("&lt;script&gt;"),
        "{page}"
    );
    assert!(!page.contains("<script>"), "{page}");
    assert_eq!(
        json_of(&call(addr, "/auth/spotify", None).2)["pending"],
        true
    );

    // The real one: signed in, on a page with no script and no referrer.
    let (status, headers, page) = call(addr, "/auth/spotify/callback?code=c&state=good", None);
    assert_eq!(status, 200, "{page}");
    assert!(page.contains("Signed in to Spotify"), "{page}");
    assert_eq!(
        header(&headers, "content-security-policy"),
        Some("default-src 'none'")
    );
    assert_eq!(header(&headers, "referrer-policy"), Some("no-referrer"));
    assert_eq!(header(&headers, "cache-control"), Some("no-store"));
    assert_eq!(
        json_of(&call(addr, "/auth/spotify", None).2),
        json!({ "signed_in": true, "pending": false, "redirect_uri": REDIRECT_URI })
    );
}

#[test]
fn an_address_pasted_from_another_browser_finishes_the_sign_in() {
    let addr = serve(surface(Policy::default()).with_spotify_auth(Box::new(FakeAuth::default())));
    let complete = |redirect: &str| {
        call(
            addr,
            "/auth/spotify/complete",
            Some(&json!({ "redirect": redirect })),
        )
    };
    let (status, _, body) =
        complete("http://127.0.0.1:8099/auth/spotify/callback?code=c&state=good");
    assert_eq!(status, 422, "nothing waits yet: {body}");
    let _ = call(addr, "/auth/spotify/begin", Some(&json!({})));
    let (status, _, body) =
        complete("http://127.0.0.1:8099/auth/spotify/callback?code=c&state=good");
    assert_eq!(status, 200, "{body}");
    assert_eq!(json_of(&body)["signed_in"], true);
    let (status, _, body) = call(addr, "/auth/spotify/complete", Some(&json!({ "url": "x" })));
    assert_eq!(status, 422, "unknown fields are refused: {body}");
}

#[test]
fn the_house_policy_can_deny_the_sign_in() {
    let policy =
        Policy::from_toml("[clients.\"loopback-http\"]\ndeny = [\"spotify_sign_in\"]\n").unwrap();
    let addr = serve(surface(policy).with_spotify_auth(Box::new(FakeAuth::default())));
    let (status, _, body) = call(addr, "/auth/spotify/begin", Some(&json!({})));
    assert_eq!(status, 403, "{body}");
    assert_eq!(json_of(&body)["code"], "POLICY_DENIED");
}
