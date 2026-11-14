use super::*;
use crate::daemon::{
    HttpStop, McpStop, bridge_compat, start_http, start_mcp, stop_http, stop_mcp, worker_failure,
};
use fsonos_api::web::ServeOrigin;
use fsonos_core::clock::{Clock, FakeClock, SystemClock};
use fsonos_core::live::Live;
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
    background: Option<BackgroundWorkers>,
    http: Vec<(HttpStop, SocketAddr)>,
    bridge: Vec<(HttpStop, SocketAddr)>,
    mcp: Vec<(McpStop, SocketAddr)>,
}

impl Running {
    fn new(surface: &Arc<Surface>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        Self {
            scheduler: Some(SchedulerWorker::start(surface, Arc::clone(&stop)).unwrap()),
            background: Some(BackgroundWorkers::new(Arc::clone(&stop))),
            stop,
            http: Vec::new(),
            bridge: Vec::new(),
            mcp: Vec::new(),
        }
    }

    fn failure(&mut self) -> anyhow::Error {
        self.failure_with_live(None)
    }

    fn failure_with_live(&mut self, live: Option<&Live>) -> anyhow::Error {
        let stop = Arc::clone(&self.stop);
        observe(&stop, || {
            worker_failure(
                &mut self.http,
                &mut self.bridge,
                &mut self.mcp,
                self.scheduler.as_mut().unwrap(),
                self.background.as_mut().unwrap(),
                live,
            )
        })
    }

    fn shutdown(&mut self) -> anyhow::Result<()> {
        self.stop.store(true, Ordering::Release);
        let background = self
            .background
            .take()
            .map_or(Ok(()), BackgroundWorkers::shutdown);
        let result = self
            .scheduler
            .take()
            .map_or(Ok(()), SchedulerWorker::shutdown);
        stop_http(&self.http);
        stop_http(&self.bridge);
        stop_mcp(std::mem::take(&mut self.mcp));
        background.and(result)
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

#[test]
fn a_real_live_worker_panic_stops_the_daemon_before_it_serves_stale_state() {
    use fsonos_core::live::LiveConfig;
    use fsonos_proto::net::Lan;
    use fsonos_sim::{SimHousehold, SimModel, SimPlayerSpec};

    let sim = SimHousehold::builder()
        .s2([SimPlayerSpec::new("Room", SimModel::One)])
        .spawn()
        .unwrap();
    let lan = Arc::new(
        Lan::start()
            .unwrap()
            .with_routes(sim.players().iter().map(|p| (p.ip, p.addr)).collect()),
    );
    let fail = Arc::new(AtomicBool::new(false));
    let panic_now = Arc::clone(&fail);
    let mut config = LiveConfig::new(sim.players().iter().map(|p| p.ip).collect());
    config.surveyed = Some(Arc::new(move |_| {
        assert!(
            !panic_now.load(Ordering::Acquire),
            "injected live worker failure"
        );
    }));
    let live = Live::start_with(Arc::new(sim.lan()), lan, config);
    assert!(live.wait_ready(Duration::from_secs(10)));
    assert!(!live.is_finished());
    let surface = surface();
    let mut running = Running::new(&surface);

    fail.store(true, Ordering::Release);
    live.refresh_soon();
    let error = format!("{:#}", running.failure_with_live(Some(&live)));
    assert!(
        error.contains("live reconciliation worker stopped unexpectedly"),
        "{error}"
    );
    assert!(running.stop.load(Ordering::Acquire));
    assert!(live.is_finished());
    assert!(!live.wait_ready(Duration::ZERO));
    running.shutdown().unwrap();
    live.stop();
    sim.shutdown();
}

#[test]
fn losing_a_playback_follower_stops_the_daemon_and_names_the_worker() {
    for panics in [false, true] {
        let surface = surface();
        let mut running = Running::new(&surface);
        let name = if panics { "fsonos-dj" } else { "fsonos-heard" };
        let worker = thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                assert!(!panics, "injected follower failure");
            })
            .unwrap();
        running.background.as_mut().unwrap().push(worker);
        let error = format!("{:#}", running.failure());
        assert!(
            error.contains(&format!("{name} stopped unexpectedly")),
            "{error}"
        );
        if panics {
            assert!(error.contains("injected follower failure"), "{error}");
        }
        assert!(running.stop.load(Ordering::Acquire));
        running
            .shutdown()
            .expect("the failed handle was already joined");
    }
}

#[test]
fn background_shutdown_has_one_deadline_for_all_workers() {
    use std::sync::mpsc;

    let stop = Arc::new(AtomicBool::new(false));
    let mut workers = BackgroundWorkers::new(Arc::clone(&stop));
    let mut releases = Vec::new();
    let mut completions = Vec::new();
    for name in ["first blocked worker", "second blocked worker"] {
        let (release, held) = mpsc::channel();
        let (completed, done) = mpsc::channel();
        workers.push(
            thread::Builder::new()
                .name(name.into())
                .spawn(move || {
                    held.recv_timeout(Duration::from_secs(5)).unwrap();
                    completed.send(()).unwrap();
                })
                .unwrap(),
        );
        releases.push(release);
        completions.push(done);
    }
    let began = Instant::now();
    let result = workers.shutdown_with(Duration::from_millis(50));
    let elapsed = began.elapsed();
    for release in releases {
        release.send(()).unwrap();
    }
    for done in completions {
        done.recv_timeout(Duration::from_secs(2)).unwrap();
    }
    assert!(stop.load(Ordering::Acquire));
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("first blocked worker")
    );
    assert!(elapsed < Duration::from_secs(1), "shutdown waited {elapsed:?}");
}

#[test]
fn abandoning_startup_stops_already_started_background_workers() {
    use std::sync::mpsc;

    let stop = Arc::new(AtomicBool::new(false));
    let stopped = Arc::clone(&stop);
    let (finished, done) = mpsc::channel();
    let mut workers = BackgroundWorkers::new(Arc::clone(&stop));
    workers.push(thread::spawn(move || {
        while !stopped.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(10));
        }
        finished.send(()).unwrap();
    }));
    drop(workers);
    done.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(stop.load(Ordering::Acquire));
}

struct LibraryWatchClock {
    visited: Arc<AtomicBool>,
    panics: bool,
}

impl Clock for LibraryWatchClock {
    fn now(&self) -> chrono::DateTime<chrono::FixedOffset> {
        SystemClock.now()
    }

    fn elapsed_secs(&self) -> i64 {
        self.visited.store(true, Ordering::Release);
        assert!(!self.panics, "injected library watcher failure");
        0
    }
}

fn library_watch(
    directory: &std::path::Path,
    visited: Arc<AtomicBool>,
    panics: bool,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    use crate::dj::sync::{LibrarySync, watch};
    use fsonos_spotify::client::SpotifyConfig;

    let sync = LibrarySync::new(
        SpotifyConfig {
            client_id: "0123456789abcdef0123456789abcdef".into(),
            redirect_uri: "http://127.0.0.1:8099/auth/spotify/callback".into(),
        },
        directory.to_path_buf(),
    )
    .unwrap()
    .with_clock(Box::new(LibraryWatchClock { visited, panics }));
    watch(Some(Arc::new(sync)), stop).unwrap().unwrap()
}

#[test]
fn a_real_library_watcher_panic_reaches_daemon_supervision() {
    let directory = tempfile::tempdir().unwrap();
    let visited = Arc::new(AtomicBool::new(false));
    let surface = surface();
    let mut running = Running::new(&surface);
    let worker = library_watch(
        directory.path(),
        Arc::clone(&visited),
        true,
        Arc::clone(&running.stop),
    );
    running.background.as_mut().unwrap().push(worker);
    let error = format!("{:#}", running.failure());
    assert!(visited.load(Ordering::Acquire));
    assert!(
        error.contains("fsonos-library-watch stopped unexpectedly"),
        "{error}"
    );
    assert!(error.contains("injected library watcher failure"), "{error}");
    running.shutdown().unwrap();
}

#[test]
fn an_unconfigured_library_starts_no_worker_and_a_signed_out_watch_stops_cleanly() {
    let stop = Arc::new(AtomicBool::new(false));
    assert!(
        crate::dj::sync::watch(None, Arc::clone(&stop))
            .unwrap()
            .is_none()
    );
    let directory = tempfile::tempdir().unwrap();
    let visited = Arc::new(AtomicBool::new(false));
    let mut workers = BackgroundWorkers::new(Arc::clone(&stop));
    workers.push(library_watch(
        directory.path(),
        Arc::clone(&visited),
        false,
        Arc::clone(&stop),
    ));
    let deadline = Instant::now() + Duration::from_secs(2);
    while !visited.load(Ordering::Acquire) {
        assert!(Instant::now() < deadline, "the library watcher never started");
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        workers.failure().is_none(),
        "being signed out is not a worker failure"
    );
    workers.shutdown().unwrap();
    assert!(stop.load(Ordering::Acquire));
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
