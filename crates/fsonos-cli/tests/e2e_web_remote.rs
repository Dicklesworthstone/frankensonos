//! The web remote against the simulator through `fsonos serve` (scenario
//! `web_remote`): the page and its assets are served, the page's own
//! control POSTs (from the listener's real origin) are admitted, the album
//! art of what a room plays comes from its player, and the browser rules
//! hold: a caller-supplied art URL is refused, another site's page gets no
//! art, and a foreign ts.net page cannot control.

mod e2e;

use e2e::{Scenario, http};
use fsonos_sim::SimHousehold;
use serde_json::json;
use std::time::Duration;

/// `key=value` out of the ready line.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|word| word.strip_prefix(key)?.strip_prefix('='))
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

/// The page, as a browser loads it.
fn check_page(s: &mut Scenario, api: &str) {
    let page = http(api, "GET", "/", &[], "");
    s.check(
        "page",
        "http",
        "GET / is the remote's page, kept to itself (CSP, nosniff)",
        page.as_ref().is_ok_and(|(code, headers, body)| {
            *code == 200
                && header(headers, "content-type") == Some("text/html; charset=utf-8")
                && header(headers, "content-security-policy")
                    .is_some_and(|csp| csp.contains("default-src 'none'"))
                && header(headers, "x-content-type-options") == Some("nosniff")
                && body.contains("/remote.js")
        }),
        format!("{page:?}"),
    );
    for (step, path, kind) in [
        ("script", "/remote.js", "text/javascript"),
        ("style", "/remote.css", "text/css"),
    ] {
        let asset = http(api, "GET", path, &[], "");
        s.check(
            step,
            "http",
            &format!("GET {path} is served as {kind}"),
            asset.as_ref().is_ok_and(|(code, headers, _)| {
                *code == 200 && header(headers, "content-type").is_some_and(|t| t.starts_with(kind))
            }),
            format!("{:?}", asset.as_ref().map(|(c, h, _)| (c, h))),
        );
    }
}

/// The page's own POST (Origin: the listener's real address, as a browser
/// sends it), then the art of what Kitchen plays.
fn check_play_and_art(s: &mut Scenario, api: &str) {
    let origin = format!("http://{api}");
    let body = json!({ "zone": "Kitchen", "favorite": "Sim Symphonies" }).to_string();
    let played = http(
        api,
        "POST",
        "/play/favorite",
        &[
            ("Origin", origin.as_str()),
            ("Content-Type", "application/json"),
        ],
        &body,
    );
    s.check(
        "play",
        "http",
        "the page's POST from its own origin is admitted: Kitchen plays an album",
        played.as_ref().is_ok_and(|(code, _, _)| *code == 200),
        format!("{played:?}"),
    );
    let art = http(
        api,
        "GET",
        "/art?zone=Kitchen&v=t1",
        &[("Sec-Fetch-Site", "same-origin")],
        "",
    );
    s.check(
        "art",
        "http",
        "GET /art is the image Kitchen's player serves for its track",
        art.as_ref().is_ok_and(|(code, headers, body)| {
            *code == 200
                && header(headers, "content-type") == Some("image/png")
                && header(headers, "cross-origin-resource-policy") == Some("same-origin")
                && body.contains("PNG")
        }),
        format!("{:?}", art.as_ref().map(|(c, h, _)| (c, h))),
    );
}

fn check_refusals(s: &mut Scenario, api: &str) {
    let supplied = http(
        api,
        "GET",
        "/art?zone=Kitchen&url=http%3A%2F%2F127.0.0.1%3A9%2Fsecret",
        &[],
        "",
    );
    s.check(
        "art-url-refused",
        "http",
        "a caller-supplied art URL is 422 INVALID_ARGUMENT (nothing is fetched)",
        supplied
            .as_ref()
            .is_ok_and(|(code, _, body)| *code == 422 && body.contains("INVALID_ARGUMENT")),
        format!("{supplied:?}"),
    );
    let embedded = http(
        api,
        "GET",
        "/art?zone=Kitchen",
        &[("Sec-Fetch-Site", "cross-site")],
        "",
    );
    s.check(
        "art-cross-site",
        "http",
        "another site's page gets no art (403 UNTRUSTED_ORIGIN)",
        embedded
            .as_ref()
            .is_ok_and(|(code, _, body)| *code == 403 && body.contains("UNTRUSTED_ORIGIN")),
        format!("{embedded:?}"),
    );
    let foreign = http(
        api,
        "POST",
        "/pause",
        &[
            ("Origin", "https://attacker.other-tailnet.ts.net"),
            ("Content-Type", "application/json"),
        ],
        &json!({ "zone": "Kitchen" }).to_string(),
    );
    s.check(
        "foreign-origin",
        "http",
        "a page on another ts.net name cannot control (403 UNTRUSTED_ORIGIN)",
        foreign
            .as_ref()
            .is_ok_and(|(code, _, body)| *code == 403 && body.contains("UNTRUSTED_ORIGIN")),
        format!("{foreign:?}"),
    );
}

#[test]
fn the_web_remote_drives_the_simulator() {
    let mut s = Scenario::start("web_remote");
    s.sim(SimHousehold::standard());
    let mut daemon = s.spawn(
        "serve",
        &[
            "serve",
            "--http",
            "127.0.0.1:0",
            "--mcp-http",
            "127.0.0.1:0",
        ],
    );
    let ready = daemon
        .wait_line("fsonos serve: ready", Duration::from_secs(20))
        .unwrap_or_default();
    let api = field(&ready, "http")
        .and_then(|u| u.strip_prefix("http://"))
        .unwrap_or("")
        .to_owned();
    let live = daemon.wait_line("fsonos serve: live", Duration::from_secs(20));
    s.check(
        "ready",
        "daemon",
        "serve is up on loopback and found the households",
        api.starts_with("127.0.0.1:") && live.is_some(),
        daemon.seen.join("\n"),
    );

    check_page(&mut s, &api);
    check_play_and_art(&mut s, &api);
    check_refusals(&mut s, &api);

    let code = daemon.interrupt(Duration::from_secs(10));
    s.check(
        "stop",
        "daemon",
        "SIGINT stops serve cleanly (exit 0)",
        code == Some(0),
        format!("exit {code:?}"),
    );
    s.finish();
}
