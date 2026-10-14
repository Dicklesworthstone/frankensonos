//! `tailscale status --json` parsing against synthetic payloads covering the
//! backend states the daemon must tell apart. Fixture values are invented
//! (documentation-style tailnet addresses, `example` names) — never site data.

use fsonos_tailscale::{Source, Tailnet, parse_status};
use std::net::{Ipv4Addr, Ipv6Addr};

const RUNNING: &[u8] = include_bytes!("fixtures/status_running.json");
const STOPPED: &[u8] = include_bytes!("fixtures/status_stopped.json");
const LOGGED_OUT: &[u8] = include_bytes!("fixtures/status_logged_out.json");
const MAGICDNS_OFF: &[u8] = include_bytes!("fixtures/status_magicdns_off.json");

const V4: Ipv4Addr = Ipv4Addr::new(100, 101, 102, 103);

fn v6() -> Ipv6Addr {
    "fd7a:115c:a1e0::6501:6667".parse().unwrap()
}

#[test]
fn running() {
    assert_eq!(
        parse_status(RUNNING).unwrap(),
        Tailnet {
            source: Source::Cli,
            backend_state: Some("Running".into()),
            running: true,
            logged_in: true,
            ipv4: vec![V4],
            ipv6: vec![v6()],
            magic_dns_name: Some("sonos-host.example-tailnet.ts.net".into()),
            tailnet: Some("example.com".into()),
        }
    );
}

#[test]
fn stopped_is_logged_in_but_not_running() {
    let t = parse_status(STOPPED).unwrap();
    assert_eq!(t.backend_state.as_deref(), Some("Stopped"));
    assert!(!t.running);
    assert!(t.logged_in);
    assert_eq!(t.ipv4, [V4]);
    assert_eq!(
        t.magic_dns_name.as_deref(),
        Some("sonos-host.example-tailnet.ts.net")
    );
}

#[test]
fn logged_out_has_no_addresses_or_name() {
    let t = parse_status(LOGGED_OUT).unwrap();
    assert_eq!(t.backend_state.as_deref(), Some("NeedsLogin"));
    assert!(!t.running);
    assert!(!t.logged_in);
    assert_eq!(t.ipv4, [] as [Ipv4Addr; 0]);
    assert_eq!(t.ipv6, [] as [Ipv6Addr; 0]);
    assert_eq!(t.magic_dns_name, None);
    assert_eq!(t.tailnet, None);
}

#[test]
fn magicdns_off_drops_the_name() {
    let t = parse_status(MAGICDNS_OFF).unwrap();
    assert!(t.running);
    assert_eq!(t.ipv4, [V4]);
    assert_eq!(t.ipv6, [] as [Ipv6Addr; 0]);
    assert_eq!(t.magic_dns_name, None);
    assert_eq!(t.tailnet.as_deref(), Some("example.com"));
}

#[test]
fn unknown_state_falls_back_to_node_key() {
    let t = parse_status(br#"{"BackendState":"SomethingNew","HaveNodeKey":true}"#).unwrap();
    assert!(!t.running);
    assert!(t.logged_in);
    let t = parse_status(b"{}").unwrap();
    assert_eq!(t.backend_state, None);
    assert!(!t.running && !t.logged_in);
}

#[test]
fn non_json_is_an_error() {
    assert!(parse_status(b"failed to connect to local tailscaled").is_err());
}
