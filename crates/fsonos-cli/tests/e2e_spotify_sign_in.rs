//! The Spotify sign-in through `fsonos serve`, against the virtual
//! households, with no Spotify on the network. A daemon without an app
//! client id answers NOT_IMPLEMENTED. With one, `POST /auth/spotify/begin`
//! gives Spotify's consent page for this app (PKCE S256,
//! user-library-read), the daemon holds the waiting sign-in, and a forged
//! redirect to its own callback route is refused on a plain HTML page before
//! anything leaves this host. The sign-in keeps waiting for the real one.

mod e2e;

use e2e::{Daemon, Scenario, http};
use fsonos_sim::SimHousehold;
use serde_json::{Value, json};
use std::time::Duration;

const CLIENT_ID: &str = "0123456789abcdef0123456789abcdef";

/// `key=value` out of a daemon line.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|word| word.strip_prefix(key)?.strip_prefix('='))
}

/// `fsonos serve` up, with `extra` arguments: the daemon and its API.
fn serve(s: &mut Scenario, step: &str, extra: &[&str]) -> (Daemon, String) {
    let mut args = vec![
        "serve",
        "--http",
        "127.0.0.1:0",
        "--mcp-http",
        "127.0.0.1:0",
    ];
    args.extend_from_slice(extra);
    let mut daemon = s.spawn(step, &args);
    let ready = daemon
        .wait_line("fsonos serve: ready", Duration::from_secs(20))
        .unwrap_or_default();
    let api = field(&ready, "http")
        .and_then(|u| u.strip_prefix("http://"))
        .unwrap_or("")
        .to_string();
    s.check(
        step,
        "daemon",
        "serve is up",
        !api.is_empty(),
        daemon.seen.join("\n"),
    );
    (daemon, api)
}

/// `(status, headers, body)` of `method path`.
fn call(
    api: &str,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> (u16, Vec<(String, String)>, String) {
    let text = body.map(Value::to_string).unwrap_or_default();
    let headers: &[(&str, &str)] = if body.is_some() {
        &[("Content-Type", "application/json")]
    } else {
        &[]
    };
    http(api, method, path, headers, &text).unwrap_or((0, Vec::new(), String::new()))
}

fn json_of(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or(Value::Null)
}

#[test]
fn the_daemon_holds_the_sign_in_and_refuses_a_forged_redirect() {
    let mut s = Scenario::start("spotify-sign-in");
    s.sim(SimHousehold::standard());

    let (mut daemon, api) = serve(&mut s, "serve-no-app", &[]);
    let (status, _, body) = call(&api, "GET", "/auth/spotify", None);
    s.check(
        "no-app",
        "http",
        "without an app client id the sign-in is NOT_IMPLEMENTED, naming the setting",
        status == 501
            && json_of(&body)["hint"]
                .as_str()
                .is_some_and(|h| h.contains("FSONOS_SPOTIFY_CLIENT_ID")),
        &body,
    );
    let code = daemon.interrupt(Duration::from_secs(10));
    s.check(
        "stop",
        "daemon",
        "serve stops",
        code == Some(0),
        format!("{code:?}"),
    );

    let (mut daemon, api) = serve(&mut s, "serve", &["--spotify-client-id", CLIENT_ID]);
    let (status, _, body) = call(&api, "GET", "/auth/spotify", None);
    s.check(
        "status",
        "http",
        "not signed in, and nothing waits",
        status == 200 && json_of(&body)["signed_in"] == false && json_of(&body)["pending"] == false,
        &body,
    );

    let (status, _, body) = call(&api, "POST", "/auth/spotify/begin", Some(&json!({})));
    let url = json_of(&body)["url"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    s.check(
        "begin",
        "http",
        "begin gives Spotify's consent page for this app: PKCE S256, user-library-read",
        status == 200
            && url.starts_with("https://accounts.spotify.com/authorize?")
            && url.contains(&format!("client_id={CLIENT_ID}"))
            && url.contains("code_challenge_method=S256")
            && url.contains("user-library-read"),
        &body,
    );
    let (_, _, body) = call(&api, "GET", "/auth/spotify", None);
    s.check(
        "pending",
        "http",
        "the daemon now holds a waiting sign-in",
        json_of(&body)["pending"] == true,
        &body,
    );

    let (status, headers, page) = call(
        &api,
        "GET",
        "/auth/spotify/callback?code=c&state=forged",
        None,
    );
    let csp = headers
        .iter()
        .find(|(n, _)| n == "content-security-policy")
        .map(|(_, v)| v.clone());
    s.check(
        "forged",
        "http",
        "a forged redirect is refused on a plain HTML page, before anything leaves this host",
        status == 409
            && page.contains("did not finish")
            && csp.as_deref() == Some("default-src 'none'"),
        format!("{status} {headers:?}\n{page}"),
    );
    let (_, _, body) = call(&api, "GET", "/auth/spotify", None);
    s.check(
        "still-pending",
        "http",
        "the sign-in still waits for the real redirect",
        json_of(&body)["pending"] == true && json_of(&body)["signed_in"] == false,
        &body,
    );

    let code = daemon.interrupt(Duration::from_secs(10));
    s.check(
        "exit",
        "daemon",
        "serve stops",
        code == Some(0),
        format!("{code:?}"),
    );
    s.finish();
}
