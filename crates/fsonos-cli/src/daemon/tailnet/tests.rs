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
        &addresses(&unavailable, config::HTTP_PORT),
        Vec::new(),
        || false,
        |_, _| panic!("no tailnet must not bind"),
    );
    let status = up();
    let desired = addresses(&status, config::HTTP_PORT);
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
                &addresses(status, config::HTTP_PORT),
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
                &addresses(status, config::HTTP_PORT),
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
    assert_eq!(
        addresses(&interfaces(), config::HTTP_PORT),
        [address(V4), address(V6)]
    );
    assert!(!accept(&interfaces(), true, &mut unanswered));
    let missing = TailnetStatus::Unavailable(Unavailable::TimedOut);
    while unanswered < UNANSWERED_LIMIT - 1 {
        assert!(!accept(&missing, true, &mut unanswered));
    }
    assert!(accept(&missing, true, &mut unanswered));
    assert!(
        addresses(&missing, config::HTTP_PORT).is_empty(),
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
        "127.0.0.1".parse::<std::net::Ipv4Addr>().unwrap(),
        "192.168.1.10".parse().unwrap(),
        "0.0.0.0".parse().unwrap(),
        "203.0.113.10".parse().unwrap(),
        "100.64.0.10".parse().unwrap(),
    ]);
    status.ipv6.extend([
        "::1".parse::<std::net::Ipv6Addr>().unwrap(),
        "::".parse().unwrap(),
        "2001:db8::10".parse().unwrap(),
    ]);
    assert_eq!(
        addresses(&TailnetStatus::Available(status), config::HTTP_PORT),
        [address(V4), address(V6)]
    );
}

#[test]
fn fixed_binds_keep_owner_watching_without_managing_their_listeners() {
    let mut args = ServeArgs {
        http: None,
        mcp_http: None,
        tailscale: TailscaleMode::Auto,
        tailscale_serve: false,
        spotify_client_id: None,
        spotify_redirect_uri: config::SPOTIFY_REDIRECT_URI.into(),
        events_port: config::EVENTS_PORT,
        allow_unsafe_bind: false,
        bridge_compat: None,
    };
    assert!(enabled(&args));
    assert!(automatic_http(&args));
    assert!(automatic_mcp(&args));
    args.tailscale = TailscaleMode::Off;
    assert!(!enabled(&args));
    assert!(!automatic_http(&args));
    assert!(!automatic_mcp(&args));
    args.tailscale = TailscaleMode::Auto;
    for configured in ["127.0.0.1:8099", V4, "0.0.0.0:8099"] {
        args.http = Some(address(configured));
        assert!(enabled(&args), "fixed binds still follow host ownership");
        assert!(
            !automatic_http(&args),
            "an explicit bind stays as configured"
        );
        assert!(automatic_mcp(&args), "HTTP configuration does not fix MCP");
    }
    args.tailscale_serve = true;
    assert!(enabled(&args), "Serve's callers need current owner rights");
    assert!(!automatic_http(&args));
    args.http = None;
    args.mcp_http = Some(address("100.64.0.10:8098"));
    assert!(
        automatic_http(&args),
        "MCP configuration does not change HTTP policy"
    );
    assert!(!automatic_mcp(&args));
    args.http = Some(address("127.0.0.1:8099"));
    args.mcp_http = Some(address("127.0.0.1:8098"));
    assert!(enabled(&args));
    assert!(!automatic_http(&args));
    assert!(!automatic_mcp(&args));
}

#[test]
fn mcp_late_login_rename_and_logout_follow_their_own_port() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let mut listeners = Listeners::new(Vec::new(), Vec::new());
    let status = up();
    let original = [
        address("100.64.0.10:8098"),
        address("[fd7a:115c:a1e0::10]:8098"),
    ];
    assert_eq!(addresses(&status, config::MCP_PORT), original);
    listeners.reconcile(
        &addresses(&status, config::MCP_PORT),
        super::super::tailnet_names(&status),
        || false,
        |addr, names| Ok(Listener::bind(addr, names, &events)),
    );
    assert_eq!(listeners.held.len(), 2);
    let TailnetStatus::Available(mut changed) = status else {
        unreachable!()
    };
    changed.ipv4 = vec!["100.64.0.11".parse().unwrap()];
    changed.ipv6.clear();
    changed.magic_dns_name = Some("new.example-tailnet.ts.net".into());
    let status = TailnetStatus::Available(changed.clone());
    events.borrow_mut().clear();
    listeners.reconcile(
        &addresses(&status, config::MCP_PORT),
        super::super::tailnet_names(&status),
        || false,
        |addr, names| Ok(Listener::bind(addr, names, &events)),
    );
    assert_eq!(listeners.held.len(), 1);
    assert_eq!(listeners.held[0].0, address("100.64.0.11:8098"));
    assert!(listeners.names.contains(&"new.example-tailnet.ts.net".into()));
    assert!(!listeners.names.contains(&NAME.into()));
    assert_eq!(
        &events.borrow()[..2],
        &[Event::Dropped(original[0]), Event::Dropped(original[1])]
    );
    changed.logged_in = false;
    changed.running = false;
    let status = TailnetStatus::Available(changed);
    listeners.reconcile(
        &addresses(&status, config::MCP_PORT),
        super::super::tailnet_names(&status),
        || false,
        |_, _| panic!("logout must only retire listeners"),
    );
    assert!(listeners.held.is_empty());
}

#[test]
fn an_mcp_bind_failure_does_not_disturb_healthy_http_listeners() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let mut http = Listeners::new(Vec::new(), Vec::new());
    let mut mcp = Listeners::new(Vec::new(), Vec::new());
    let status = up();
    let names = super::super::tailnet_names(&status);
    http.reconcile(
        &addresses(&status, config::HTTP_PORT),
        names.clone(),
        || false,
        |addr, names| Ok(Listener::bind(addr, names, &events)),
    );
    events.borrow_mut().clear();
    mcp.reconcile(
        &addresses(&status, config::MCP_PORT),
        names.clone(),
        || false,
        |_, _| anyhow::bail!("the MCP port is temporarily occupied"),
    );
    assert!(mcp.held.is_empty());
    assert_eq!(http.held.len(), 2);
    http.reconcile(
        &addresses(&status, config::HTTP_PORT),
        names.clone(),
        || false,
        |_, _| panic!("MCP failure must not restart HTTP"),
    );
    mcp.reconcile(
        &addresses(&status, config::MCP_PORT),
        names,
        || false,
        |addr, names| Ok(Listener::bind(addr, names, &events)),
    );
    assert_eq!(mcp.held.len(), 2);
    assert_eq!(events.borrow().len(), 2, "only new MCP listeners were bound");
}

#[test]
fn retiring_an_owned_mcp_server_releases_its_socket_and_keeps_a_fixed_one() {
    let fixed = super::super::start_mcp("127.0.0.1:0".parse().unwrap(), &[], false)
        .expect("the fixed MCP listener binds");
    let automatic = super::super::start_mcp("127.0.0.1:0".parse().unwrap(), &[], false)
        .expect("the automatic MCP listener binds");
    let automatic_address = automatic.1;
    let mut listeners = Listeners::new(vec![(automatic_address, Mcp(automatic))], Vec::new());
    listeners.reconcile(
        &[],
        Vec::new(),
        || false,
        |_, _| panic!("a disconnected tailnet does not bind"),
    );
    let rebound = std::net::TcpListener::bind(automatic_address)
        .expect("retirement waits for the owned MCP socket to close");
    let connected = std::net::TcpStream::connect_timeout(&fixed.1, Duration::from_secs(1))
        .expect("the independent fixed listener still accepts connections");
    assert_eq!(connected.peer_addr().unwrap(), fixed.1);
    drop(connected);
    drop(rebound);
    super::super::stop_mcp(&mut [fixed]);
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

fn owned_surface() -> Arc<Surface> {
    struct NoLan;
    impl fsonos_proto::Transport for NoLan {
        fn soap_post(
            &self,
            _: std::net::IpAddr,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<String, fsonos_proto::ProtoError> {
            panic!("owner lifecycle must not touch speakers");
        }
    }
    Arc::new(Surface::new(
        Box::new(NoLan),
        Box::new(|_| Ok(Vec::new())),
        fsonos_core::policy::Policy::default().with_owner(Some("owner@example.com".into())),
        Box::new(fsonos_core::clock::SystemClock),
    ))
}

#[test]
fn failed_or_panicked_owner_workers_cannot_leave_permanent_authority() {
    let surface = owned_surface();
    let lease = OwnerLease(Arc::clone(&surface));
    let unstarted = move || {
        let _lease = lease;
    };
    // A failed thread spawn drops its closure, including its captured guard.
    drop(unstarted);
    assert_eq!(surface.policy().owner(), None);

    surface.set_tailnet_owner(Some("owner@example.com".into()));
    let lease = OwnerLease(Arc::clone(&surface));
    let worker = thread::spawn(move || {
        let _lease = lease;
        panic!("synthetic watcher failure");
    });
    assert!(worker.join().is_err());
    assert_eq!(surface.policy().owner(), None);
}

#[test]
fn an_owner_only_watch_stops_without_retiring_configured_sockets() {
    let surface = owned_surface();
    let fixed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let stop = Arc::new(AtomicBool::new(true));
    let watcher = watch(
        InitialListeners {
            http: None,
            mcp: None,
        },
        up(),
        Arc::clone(&surface),
        "synthetic-token".into(),
        true,
        ServeOrigin::default(),
        stop,
    )
    .unwrap();
    watcher.join().unwrap();
    assert_eq!(surface.policy().owner(), None);
    let address = fixed.local_addr().unwrap();
    let connected = std::net::TcpStream::connect_timeout(&address, Duration::from_secs(1)).unwrap();
    assert_eq!(connected.peer_addr().unwrap(), address);
}
