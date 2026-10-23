use super::*;
use fsonos_tailscale::{Source, Tailnet, Unavailable};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

const NAME: &str = "sonos-host.example-tailnet.ts.net";
const V4: &str = "100.64.0.10:8099";
const V6: &str = "[fd7a:115c:a1e0::10]:8099";

fn address(text: &str) -> SocketAddr {
    text.parse().unwrap()
}

fn up() -> TailnetStatus {
    TailnetStatus::Available(Tailnet {
        source: Source::Cli,
        backend_state: Some("Running".into()),
        running: true,
        logged_in: true,
        ipv4: vec!["100.64.0.10".parse().unwrap()],
        ipv6: vec!["fd7a:115c:a1e0::10".parse().unwrap()],
        magic_dns_name: Some(NAME.into()),
        tailnet: None,
    })
}

fn interfaces() -> TailnetStatus {
    let TailnetStatus::Available(mut found) = up() else {
        unreachable!()
    };
    found.source = Source::Interfaces;
    found.backend_state = None;
    found.magic_dns_name = None;
    TailnetStatus::Available(found)
}

#[derive(Debug, PartialEq, Eq)]
enum Event {
    Bound(SocketAddr, Vec<String>),
    Dropped(SocketAddr),
}

struct Listener {
    addr: SocketAddr,
    events: Rc<RefCell<Vec<Event>>>,
}

impl Listener {
    fn bind(addr: SocketAddr, names: &[String], events: &Rc<RefCell<Vec<Event>>>) -> Self {
        events.borrow_mut().push(Event::Bound(addr, names.to_vec()));
        Self {
            addr,
            events: Rc::clone(events),
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.events.borrow_mut().push(Event::Dropped(self.addr));
    }
}

#[test]
fn a_late_tailnet_binds_both_families_and_retries_only_failed_binds() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let mut listeners = Listeners::new(Vec::new(), Vec::new());
    let unavailable = TailnetStatus::Unavailable(Unavailable::NotInstalled);
    listeners.reconcile(
        &addresses(&unavailable),
        Vec::new(),
        || false,
        |_, _| panic!("no tailnet must not bind"),
    );
    let status = up();
    let desired = addresses(&status);
    let names = super::super::tailnet_names(&status);
    let attempts = RefCell::new(Vec::new());
    let fail_v6 = Cell::new(true);
    let bind = |addr, names: &[String]| {
        attempts.borrow_mut().push(addr);
        if addr == address(V6) && fail_v6.get() {
            anyhow::bail!("interface not ready");
        }
        Ok(Listener::bind(addr, names, &events))
    };
    listeners.reconcile(&desired, names.clone(), || false, bind);
    assert_eq!(listeners.held.len(), 1);
    assert_eq!(attempts.borrow().as_slice(), &[address(V4), address(V6)]);
    fail_v6.set(false);
    listeners.reconcile(&desired, names.clone(), || false, bind);
    assert_eq!(listeners.held.len(), 2);
    assert_eq!(
        attempts.borrow().as_slice(),
        &[address(V4), address(V6), address(V6)]
    );
    listeners.reconcile(&desired, names, || false, bind);
    assert_eq!(attempts.borrow().len(), 3, "healthy listeners stay bound");
    assert_eq!(events.borrow().len(), 2);
}

#[test]
fn a_new_address_or_magicdns_name_replaces_old_identity_rules() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let old = address(V4);
    let new = address("100.64.0.11:8099");
    let old_names = vec![NAME.to_string(), old.ip().to_string()];
    let initial = Listener::bind(old, &old_names, &events);
    let mut listeners = Listeners::new(vec![(old, initial)], old_names);
    events.borrow_mut().clear();
    let new_names = vec![
        "renamed.example-tailnet.ts.net".to_string(),
        new.ip().to_string(),
    ];
    listeners.reconcile(
        &[new],
        new_names.clone(),
        || false,
        |addr, names| Ok(Listener::bind(addr, names, &events)),
    );
    assert_eq!(
        events.borrow().as_slice(),
        &[
            Event::Dropped(old),
            Event::Bound(new, canonical_names(new_names)),
        ]
    );
    // A DNS rename on the same socket also needs new Host/origin rules.
    events.borrow_mut().clear();
    let names = vec![
        "again.example-tailnet.ts.net".to_string(),
        new.ip().to_string(),
    ];
    listeners.reconcile(
        &[new],
        names.clone(),
        || false,
        |addr, names| Ok(Listener::bind(addr, names, &events)),
    );
    assert_eq!(
        events.borrow().as_slice(),
        &[
            Event::Dropped(new),
            Event::Bound(new, canonical_names(names))
        ]
    );
}

#[test]
fn name_order_case_and_repeated_addresses_do_not_restart_listeners() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let addr = address(V4);
    let names = vec![NAME.to_string(), addr.ip().to_string()];
    let initial = Listener::bind(addr, &names, &events);
    let mut listeners = Listeners::new(vec![(addr, initial)], names);
    events.borrow_mut().clear();
    listeners.reconcile(
        &[addr, addr],
        vec![
            addr.ip().to_string(),
            format!("{}.", NAME.to_ascii_uppercase()),
            NAME.into(),
        ],
        || false,
        |_, _| panic!("the same names and addresses must keep the listener"),
    );
    assert!(events.borrow().is_empty());
    assert_eq!(listeners.held.len(), 1);
}

#[test]
fn an_answered_disconnect_retires_tailnet_listeners_and_reconnect_restores_them() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let mut listeners = Listeners::new(Vec::new(), Vec::new());
    let mut unanswered = 0;
    let mut observe = |status: &TailnetStatus| {
        if accept(status, !listeners.held.is_empty(), &mut unanswered) {
            listeners.reconcile(
                &addresses(status),
                super::super::tailnet_names(status),
                || false,
                |addr, names| Ok(Listener::bind(addr, names, &events)),
            );
        }
        listeners.held.len()
    };
    assert_eq!(observe(&up()), 2);
    let TailnetStatus::Available(mut down) = up() else {
        unreachable!()
    };
    down.running = false;
    down.backend_state = Some("Stopped".into());
    events.borrow_mut().clear();
    assert_eq!(observe(&TailnetStatus::Available(down)), 0);
    assert_eq!(
        events.borrow().as_slice(),
        &[Event::Dropped(address(V4)), Event::Dropped(address(V6))]
    );
    assert_eq!(observe(&up()), 2);
}

#[test]
fn a_brief_cli_failure_keeps_names_but_persistent_failure_expires_them() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let mut listeners = Listeners::new(Vec::new(), Vec::new());
    let mut unanswered = 0;
    let mut observe = |status: &TailnetStatus| {
        if accept(status, !listeners.held.is_empty(), &mut unanswered) {
            listeners.reconcile(
                &addresses(status),
                super::super::tailnet_names(status),
                || false,
                |addr, names| Ok(Listener::bind(addr, names, &events)),
            );
        }
        listeners.names.clone()
    };
    let named = observe(&up());
    events.borrow_mut().clear();
    for _ in 1..UNANSWERED_LIMIT {
        assert_eq!(observe(&interfaces()), named);
    }
    assert!(
        events.borrow().is_empty(),
        "one missed CLI must not flap sockets"
    );
    assert_eq!(observe(&up()), named, "an answer resets the missed count");
    for _ in 1..UNANSWERED_LIMIT {
        assert_eq!(observe(&interfaces()), named);
    }
    let fallback = observe(&interfaces());
    assert!(!fallback.iter().any(|name| name == NAME));
    assert_eq!(fallback.len(), 2, "interface addresses can still serve");
    assert_eq!(
        events.borrow().len(),
        4,
        "old names retired before rebinding"
    );
    assert_eq!(
        observe(&up()),
        named,
        "a fresh CLI answer restores the name"
    );
}

#[test]
fn interface_fallback_can_start_a_listener_without_waiting_for_the_cli() {
    let mut unanswered = 0;
    assert!(accept(&interfaces(), false, &mut unanswered));
    assert_eq!(addresses(&interfaces()), [address(V4), address(V6)]);
    assert!(!accept(&interfaces(), true, &mut unanswered));
    let missing = TailnetStatus::Unavailable(Unavailable::TimedOut);
    while unanswered < UNANSWERED_LIMIT - 1 {
        assert!(!accept(&missing, true, &mut unanswered));
    }
    assert!(accept(&missing, true, &mut unanswered));
    assert!(
        addresses(&missing).is_empty(),
        "unobserved addresses expire"
    );
}

#[test]
fn shutdown_during_reconciliation_starts_no_more_listeners_and_drops_owned_ones() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let stopping = Cell::new(false);
    let mut listeners = Listeners::new(Vec::new(), Vec::new());
    listeners.reconcile(
        &[address(V4), address(V6)],
        vec![NAME.into()],
        || stopping.get(),
        |addr, names| {
            stopping.set(true);
            Ok(Listener::bind(addr, names, &events))
        },
    );
    assert_eq!(listeners.held.len(), 1);
    drop(listeners);
    assert_eq!(
        events.borrow().as_slice(),
        &[
            Event::Bound(address(V4), vec![NAME.into()]),
            Event::Dropped(address(V4)),
        ]
    );
}

#[test]
fn automatic_addresses_never_include_loopback_lan_wildcard_or_public_ips() {
    let TailnetStatus::Available(mut status) = up() else {
        unreachable!()
    };
    status.ipv4.extend([
        "127.0.0.1".parse().unwrap(),
        "192.168.1.10".parse().unwrap(),
        "0.0.0.0".parse().unwrap(),
        "203.0.113.10".parse().unwrap(),
        "100.64.0.10".parse().unwrap(),
    ]);
    status.ipv6.extend([
        "::1".parse().unwrap(),
        "::".parse().unwrap(),
        "2001:db8::10".parse().unwrap(),
    ]);
    assert_eq!(
        addresses(&TailnetStatus::Available(status)),
        [address(V4), address(V6)]
    );
}

#[test]
fn configured_http_and_tailscale_off_do_not_start_the_worker() {
    let mut args = ServeArgs {
        http: None,
        mcp_http: None,
        tailscale: TailscaleMode::Auto,
        tailscale_serve: false,
        spotify_client_id: None,
        spotify_redirect_uri: config::SPOTIFY_REDIRECT_URI.into(),
        events_port: config::EVENTS_PORT,
        allow_unsafe_bind: false,
    };
    assert!(enabled(&args));
    args.tailscale = TailscaleMode::Off;
    assert!(!enabled(&args));
    args.tailscale = TailscaleMode::Auto;
    for configured in ["127.0.0.1:8099", V4, "0.0.0.0:8099"] {
        args.http = Some(address(configured));
        assert!(!enabled(&args), "an explicit bind stays as configured");
    }
    args.http = None;
    args.mcp_http = Some(address("100.64.0.10:8098"));
    assert!(
        enabled(&args),
        "MCP configuration does not change HTTP policy"
    );
}

#[test]
fn retirement_releases_owned_sockets_and_leaves_external_loopback_available() {
    let local = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let automatic = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let auto_addr = automatic.local_addr().unwrap();
    let mut listeners = Listeners::new(vec![(auto_addr, automatic)], Vec::new());
    listeners.reconcile(
        &[],
        Vec::new(),
        || false,
        |_, _| panic!("a disconnect must only retire listeners"),
    );
    let rebound = std::net::TcpListener::bind(auto_addr).unwrap();
    assert_eq!(rebound.local_addr().unwrap(), auto_addr);
    let connected = std::net::TcpStream::connect(local.local_addr().unwrap()).unwrap();
    assert_eq!(connected.peer_addr().unwrap(), local.local_addr().unwrap());
}
