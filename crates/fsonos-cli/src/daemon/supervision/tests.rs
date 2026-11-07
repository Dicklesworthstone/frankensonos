use super::*;
use crate::daemon::{
    HttpStop, McpStop, bridge_compat, start_http, start_mcp, stop_http, stop_mcp, worker_failure,
};
use fsonos_api::web::ServeOrigin;
use fsonos_core::clock::{Clock, FakeClock, SystemClock};
use fsonos_core::policy::{Client, Policy};
use std::net::{IpAddr, SocketAddr};
use std::time::Instant;

struct NoLan;

impl fsonos_proto::Transport for NoLan {
    fn soap_post(
        &self,
        _: IpAddr,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<String, fsonos_proto::ProtoError> {
        Err(fsonos_proto::ProtoError::NotWired("no LAN in this test"))
    }
}

fn surface() -> Arc<Surface> {
    Arc::new(Surface::new(
        Box::new(NoLan),
        Box::new(|_| Ok(Vec::new())),
        Policy::default(),
        Box::new(SystemClock),
    ))
}

/// Exercise the production wait loop, but make a missed failure fail the
/// test within ten seconds instead of leaving a daemon thread behind.
fn observe(stop: &AtomicBool, mut failure: impl FnMut() -> Option<anyhow::Error>) -> anyhow::Error {
    let deadline = Instant::now() + Duration::from_secs(10);
    wait(stop, || {
        if Instant::now() >= deadline {
            stop.store(true, Ordering::Release);
            return None;
        }
        failure()
    })
    .expect_err("the supervisor must notice the stopped worker")
}

struct Running {
    stop: Arc<AtomicBool>,
    scheduler: Option<SchedulerWorker>,
    http: Vec<(HttpStop, SocketAddr)>,
    bridge: Vec<(HttpStop, SocketAddr)>,
    mcp: Vec<(McpStop, SocketAddr)>,
}

impl Running {
    fn new(surface: &Arc<Surface>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        Self {
            scheduler: Some(SchedulerWorker::start(surface, Arc::clone(&stop)).unwrap()),
            stop,
            http: Vec::new(),
            bridge: Vec::new(),
            mcp: Vec::new(),
        }
    }

    fn failure(&mut self) -> anyhow::Error {
        let stop = Arc::clone(&self.stop);
        observe(&stop, || {
            worker_failure(
                &mut self.http,
                &mut self.bridge,
                &mut self.mcp,
                self.scheduler.as_mut().unwrap(),
            )
        })
    }

    fn shutdown(&mut self) -> anyhow::Result<()> {
        self.stop.store(true, Ordering::Release);
        let result = self
            .scheduler
            .take()
            .map_or(Ok(()), SchedulerWorker::shutdown);
        stop_http(&self.http);
        stop_http(&self.bridge);
        stop_mcp(std::mem::take(&mut self.mcp));
        result
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[test]
fn losing_an_http_or_configured_bridge_listener_stops_the_daemon() {
    for legacy in [false, true] {
        let surface = surface();
        let mut running = Running::new(&surface);
        let (addr, role) = if legacy {
            running.bridge =
                bridge_compat::start(&surface, Some("127.0.0.1:0".parse().unwrap())).unwrap();
            (running.bridge[0].1, "bridge-compatible API")
        } else {
            running.http.push(
                start_http(
                    &surface,
                    "127.0.0.1:0".parse().unwrap(),
                    &[],
                    false,
                    "test-cli-token",
                    &ServeOrigin::default(),
                )
                .unwrap(),
            );
            (running.http[0].1, "HTTP API")
        };
        std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(1))
            .expect("the real listener is accepting connections");
        // Only the listener stops: no signal has stopped the daemon.
        stop_http(if legacy {
            &running.bridge
        } else {
            &running.http
        });
        let error = format!("{:#}", running.failure());
        assert!(
            error.contains(&format!("{role} on {addr} stopped unexpectedly")),
            "{error}"
        );
        assert!(running.stop.load(Ordering::Acquire));
        running.shutdown().unwrap();
        std::net::TcpListener::bind(addr).expect("the failed listener released its port");
    }
}

#[test]
fn losing_an_mcp_listener_stops_the_daemon() {
    let mut running = Running::new(&surface());
    running
        .mcp
        .push(start_mcp("127.0.0.1:0".parse().unwrap(), &[], false).unwrap());
    let addr = running.mcp[0].1;
    std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(1))
        .expect("the real MCP listener is accepting connections");
    running.mcp[0].0.cx.cancel_with(
        asupersync::types::CancelKind::Shutdown,
        Some("test listener failure without a daemon shutdown signal"),
    );
    let error = format!("{:#}", running.failure());
    assert!(
        error.contains(&format!("MCP server on {addr} stopped unexpectedly")),
        "{error}"
    );
    assert!(running.stop.load(Ordering::Acquire));
    running.shutdown().unwrap();
    std::net::TcpListener::bind(addr).expect("the failed MCP listener released its port");
}

#[test]
fn listener_errors_retain_the_address_and_original_error_chain() {
    let stop = AtomicBool::new(false);
    let mut thread = Some(thread::spawn(|| {
        Err(anyhow::anyhow!("socket disappeared").context("accept failed"))
    }));
    let error = observe(&stop, || {
        listener_failure(&mut thread, "HTTP API on 127.0.0.1:8099")
    });
    let text = format!("{error:#}");
    assert!(
        text.contains("HTTP API on 127.0.0.1:8099 stopped unexpectedly"),
        "{text}"
    );
    assert!(text.contains("accept failed: socket disappeared"), "{text}");
    assert!(stop.load(Ordering::Acquire));
    assert!(thread.is_none(), "the dead worker is joined, not retried");
}

#[test]
fn worker_panics_are_reported_instead_of_leaving_the_daemon_running() {
    type Job = fn() -> anyhow::Result<()>;
    let jobs: [(&str, Job); 3] = [
        ("literal panic", || panic!("literal panic")),
        ("owned panic", || {
            std::panic::panic_any(String::from("owned panic"))
        }),
        ("non-string panic payload", || std::panic::panic_any(17_u8)),
    ];
    for (message, job) in jobs {
        let stop = AtomicBool::new(false);
        let mut thread = Some(thread::spawn(job));
        let error = observe(&stop, || {
            listener_failure(&mut thread, "MCP server on 127.0.0.1:8100")
        });
        let text = format!("{error:#}");
        assert!(
            text.contains("MCP server on 127.0.0.1:8100 stopped unexpectedly"),
            "{text}"
        );
        assert!(text.contains(message), "{text}");
        assert!(stop.load(Ordering::Acquire));
    }
}

#[test]
fn shutdown_signals_do_not_turn_expected_worker_exits_into_failures() {
    let stop = AtomicBool::new(true);
    wait(&stop, || {
        panic!("already stopping: workers must not be checked")
    })
    .unwrap();

    let stop = AtomicBool::new(false);
    wait(&stop, || {
        stop.store(true, Ordering::Release);
        Some(anyhow::anyhow!("the listener ended after the signal"))
    })
    .unwrap();

    let mut running = Running::new(&surface());
    running
        .shutdown()
        .expect("the real scheduler stops normally");
}

/// Fail only a scheduler tick, leaving its existing state available for
/// shutdown. Other workers and the cleanup still see the controlled clock.
struct FailingClock {
    clock: Arc<FakeClock>,
    fail: Arc<AtomicBool>,
}

impl Clock for FailingClock {
    fn now(&self) -> chrono::DateTime<chrono::FixedOffset> {
        let fail = thread::current().name() == Some("fsonos-scheduler")
            && self.fail.swap(false, Ordering::AcqRel);
        assert!(!fail, "injected scheduler clock failure");
        self.clock.now()
    }
}

#[test]
fn a_panicked_scheduler_restores_an_active_sleep_fade_before_exit() {
    use fsonos_api::surface::schedules::{Sleep, SleepRequest};
    use fsonos_core::HouseholdState;
    use fsonos_proto::control as soap;
    use fsonos_sim::{SimHousehold, SimModel, SimPlayerSpec};

    let sim = SimHousehold::builder()
        .s1([SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1)])
        .spawn()
        .unwrap();
    let lan = sim.lan();
    let host = sim.player("Kitchen").unwrap().ip;
    soap::set_volume(&lan, host, 40).unwrap();
    soap::set_av_transport_uri(
        &lan,
        host,
        "x-rincon-mp3radio://stream.example.invalid/music.mp3",
        "",
    )
    .unwrap();
    soap::play(&lan, host).unwrap();
    let clock = Arc::new(FakeClock::new(
        chrono::DateTime::parse_from_rfc3339("2026-10-22T12:00:00+00:00").unwrap(),
    ));
    let fail = Arc::new(AtomicBool::new(false));
    let surface = Arc::new(
        Surface::new(
            Box::new(sim.lan()),
            Box::new(move |transport| {
                let mut house = HouseholdState::default();
                house.apply_topology(
                    &fsonos_proto::topology::get_zone_group_state(transport, host)
                        .map_err(fsonos_core::CoreError::from)?,
                );
                Ok(vec![house])
            }),
            Policy::default(),
            Box::new(FailingClock {
                clock: Arc::clone(&clock),
                fail: Arc::clone(&fail),
            }),
        )
        .with_sleep(Sleep::with_fade(Duration::from_secs(30))),
    );
    surface
        .set_sleep_timer(
            &Client::Cli,
            &SleepRequest {
                zone: "Kitchen".into(),
                duration: Some("1m".into()),
                extend: false,
                cancel: false,
            },
        )
        .unwrap();
    clock.advance(chrono::TimeDelta::seconds(30));
    let mut running = Running::new(&surface);
    let deadline = Instant::now() + Duration::from_secs(10);
    while soap::get_volume(&lan, host).unwrap() == 40 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    let faded = soap::get_volume(&lan, host).unwrap();
    assert!(
        faded < 40,
        "a real sleep fade must be in flight before the failure: {faded}"
    );

    fail.store(true, Ordering::Release);
    let error = format!("{:#}", running.failure());
    assert!(error.contains("scheduler stopped unexpectedly"), "{error}");
    assert!(
        error.contains("injected scheduler clock failure"),
        "{error}"
    );
    assert!(running.stop.load(Ordering::Acquire));
    running
        .shutdown()
        .expect("the retained scheduler cleans up after its panic");
    assert_eq!(
        soap::get_volume(&lan, host).unwrap(),
        40,
        "the fade put the original volume back"
    );
    assert_eq!(
        soap::get_transport_info(&lan, host).unwrap().state,
        fsonos_types::TransportState::Playing,
        "shutdown leaves the speaker's backstop timer to pause the music later"
    );
}
