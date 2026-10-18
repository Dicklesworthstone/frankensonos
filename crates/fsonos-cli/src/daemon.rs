//! `fsonos serve`: the long-lived daemon.
//!
//! One shared [`Surface`] (the LAN, the house policy, and the live model)
//! backs both control surfaces: the HTTP API and the MCP server over
//! streamable HTTP (`/mcp`). Each listens on its own thread and runtime.
//! Once both are bound, a ready line on stderr names the actual addresses (so
//! ephemeral `:0` ports are usable), then the daemon runs until SIGINT or
//! SIGTERM. It keeps running when discovery finds nothing: calls answer
//! `NOT_READY` until the speakers do (TN3179: macOS shows the Local Network
//! prompt only to a process that stays alive).
//!
//! The live model ([`Live`]) surveys in the background and keeps the zones
//! current from the players' GENA events, which arrive on the events port
//! (`--events-port`). A second line, `fsonos serve: live ...`, says when its
//! first survey found the households and where events arrive. Stopping the
//! daemon ends every subscription.
//!
//! Unconfigured, the HTTP API listens on loopback plus this host's tailnet
//! addresses (ts-autobind) and MCP on loopback; a third line,
//! `fsonos serve: listening (...)`, and the connect URLs follow the ready
//! line.
//!
//! Callers are identified per listener. A loopback listener's callers are
//! local processes (`loopback-http`, which is how Tailscale Serve arrives);
//! on any other bind (the tailnet), Tailscale names each caller by its
//! address (WhoIs: a login, `tag:<name>` or node name, which the house
//! policy's `[clients]` tables apply to), and a caller it can't name is
//! `unknown`, which the default policy keeps read-only.
//!
//! Each HTTP listener admits browser requests only from its own origins
//! (its bound address and names); loopback ones also from the Tailscale
//! Serve origin the daemon learns while it runs ([`serve_origin`]).

use anyhow::Context as _;
use fastapi::{ServerConfig, TcpServer};
use fsonos_api::surface::schedules::Scheduler;
use fsonos_api::web::ServeOrigin;
use fsonos_api::{Failure, Identity, Surface, WebPolicy};
use fsonos_core::clock::SystemClock;
use fsonos_core::live::{Live, LiveConfig, LiveEvent};
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::SqliteStore;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::{self, GlobalArgs, ServeArgs};
use crate::dj::sync::LibrarySync;
use crate::schedule_cmd::SchedulerArgs;

mod spotify_auth;

mod serve_origin;

/// The data directory, or `INVALID_ARGUMENT` when none can be determined.
pub fn data_dir(global: &GlobalArgs) -> Result<PathBuf, Failure> {
    global.data_dir().ok_or_else(|| {
        Failure::invalid("no data directory is configured and HOME is unset")
            .with_hint("Set FSONOS_DATA_DIR (or HOME) and start again.")
    })
}

/// Take up the sleep timers of the daemon's last run, where the speakers
/// still keep their own, once the house is known: until the live model's
/// first survey finds it every group looks gone, and the timers would be
/// dropped. Off the main thread.
fn take_up_sleep_timers(surface: &Arc<Surface>) {
    let surface = Arc::clone(surface);
    thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline
            && !surface
                .households()
                .is_ok_and(|houses| houses.iter().any(|h| !h.rooms.is_empty()))
        {
            thread::sleep(Duration::from_millis(500));
        }
        let adopted = surface.adopt_sleep_timers();
        if adopted > 0 {
            tracing::info!("took up {adopted} sleep timer(s) from before the restart");
        }
    });
}

/// The house policy from `policy.toml` in `data_dir` (defaults when absent).
pub fn policy(data_dir: &Path) -> Result<Policy, Failure> {
    Policy::load(data_dir).map_err(|e| {
        Failure::invalid(e.to_string()).with_hint("Fix policy.toml in the data directory.")
    })
}

/// The surface the control servers share: the LAN (with any `--routes`),
/// surveyed with the global seeds and SSDP wait.
pub fn surface(global: &GlobalArgs, policy: Policy) -> Result<Surface, Failure> {
    let seeds = global.seed_addrs()?;
    let data_dir = global.data_dir();
    let remembered = move || crate::known_players::remembered(data_dir.as_deref());
    let wait = global.wait();
    let survey: fsonos_api::surface::Survey = Box::new(move |transport| {
        let found =
            fsonos_core::inventory::survey_or_remembered(transport, &seeds, &remembered, wait)?;
        Ok(found.households)
    });
    Ok(
        Surface::new(global.lan()?, survey, policy, Box::new(SystemClock)).with_dj(Box::new(
            crate::dj::SpotifyDj::new(global.data_dir().map(|d| d.join(crate::dj::MOODS_FILE))),
        )),
    )
}

/// The daemon's surface over a live model of the speakers: surveys and
/// reads through the (confined) transport, events through the LAN on
/// `events_port`. The caller owns the model: dropping the returned `Arc`
/// ends every subscription. With `library`, the DJ can refresh its
/// library from Spotify (`dj_sync`).
pub fn live_surface(
    global: &GlobalArgs,
    events_port: u16,
    policy: Policy,
    library: Option<Arc<LibrarySync>>,
) -> Result<(Surface, Arc<Live>), Failure> {
    let seeds = global.seed_addrs()?;
    let data_dir = global.data_dir();
    let remembered = move || crate::known_players::remembered(data_dir.as_deref());
    let wait = global.wait();
    let network = global.network()?;
    // The players fetch announcement clips from the event listener.
    let (media, files) = crate::announce_cmd::media(global.data_dir().as_deref());
    let config = LiveConfig {
        callback_port: events_port,
        media: Some(files),
        remembered: remembered(),
        // Each survey's players are remembered for the next start.
        surveyed: crate::known_players::remembering(global),
        ..LiveConfig::new(seeds.clone())
    };
    let live = Arc::new(Live::start_with(
        Arc::clone(&network.transport),
        network.lan,
        config,
    ));
    // Right after a regroup the surface surveys directly; see
    // `fsonos_api::surface::SETTLE`.
    let survey: fsonos_api::surface::Survey = Box::new(move |transport| {
        let found =
            fsonos_core::inventory::survey_or_remembered(transport, &seeds, &remembered, wait)?;
        Ok(found.households)
    });
    let surface = Surface::new(
        Box::new(network.transport),
        survey,
        policy,
        Box::new(SystemClock),
    )
    .with_live(&live)
    .with_dj(Box::new(
        crate::dj::SpotifyDj::new(global.data_dir().map(|d| d.join(crate::dj::MOODS_FILE)))
            .with_library(library)
            .with_saved_feeds(
                global
                    .data_dir()
                    .map(|d| d.join(crate::dj::saved::FEEDS_FILE)),
            ),
    ));
    let listener = Arc::downgrade(&live);
    let surface = surface.with_announcements(fsonos_api::surface::announce::Announcements::new(
        media,
        Box::new(move || listener.upgrade()?.snapshot().callback),
    ));
    Ok((surface, live))
}

/// Print `fsonos serve: live ...` once the model's first survey has found
/// the households (at once if it already has). Ends with the model.
fn announce_live(live: &Arc<Live>) {
    // Subscribe before looking, so a survey finishing in between is seen.
    let changes = live.subscribe();
    if announce_if_found(live) {
        return;
    }
    let live: Weak<Live> = Arc::downgrade(live);
    let _ = thread::Builder::new()
        .name("fsonos-live-ready".into())
        .spawn(move || {
            while let Ok(change) = changes.recv() {
                if change != LiveEvent::Topology {
                    continue;
                }
                let Some(live) = live.upgrade() else { return };
                if announce_if_found(&live) {
                    return;
                }
            }
        });
}

/// The live line, if the model has found players; whether it was printed.
fn announce_if_found(live: &Live) -> bool {
    let snapshot = live.snapshot();
    let players: usize = snapshot.households.iter().map(|h| h.players.len()).sum();
    if players > 0 {
        eprintln!(
            "fsonos serve: live households={} players={players} events={}",
            snapshot.households.len(),
            snapshot.callback.as_deref().unwrap_or("none"),
        );
    }
    players > 0
}

/// The store's file in the data directory (the name core's store uses).
pub const DB_FILE: &str = "fsonos.db";

/// `surface` with the action log kept in the data directory's store
/// (`fsonos.db`), recorded as `label`, and the owner's room aliases and house
/// policy from the same directory (`aliases.toml`, and `policy.toml` re-read
/// when it changes). A store that cannot open is a warning, not a refusal:
/// control still works, only undo and the log do not.
#[must_use]
pub fn with_action_log(surface: Surface, data_dir: &Path, label: &str) -> Surface {
    let surface = surface
        .with_aliases_file(data_dir.join(fsonos_api::surface::ALIASES_FILE))
        .with_policy_file(data_dir.to_path_buf());
    let opened = std::fs::create_dir_all(data_dir)
        .map_err(|e| e.to_string())
        .and_then(|()| SqliteStore::open(&data_dir.join(DB_FILE)).map_err(|e| e.to_string()));
    match opened {
        Ok(store) => surface.with_action_log(Box::new(store), label),
        Err(e) => {
            tracing::warn!(
                "no action log (undo unavailable): cannot open the store in {}: {e}",
                data_dir.display()
            );
            surface
        }
    }
}

/// Who the callers of a listener bound to `addr` are, for the house policy.
#[must_use]
pub fn listener_client(addr: SocketAddr) -> Client {
    if addr.ip().is_loopback() {
        Client::LoopbackHttp
    } else {
        Client::Unknown
    }
}

/// The data directory and its daemon lock, held for as long as the daemon
/// runs: a second daemon there would share its store and take over
/// daemon.json while this one still holds the event port, so the CLI would
/// talk to a daemon whose model is stale.
fn unshared_data_dir(global: &GlobalArgs) -> anyhow::Result<(PathBuf, std::fs::File)> {
    let data_dir = data_dir(global)?;
    let lock = crate::remote::lock_data_dir(&data_dir)
        .with_context(|| format!("lock the data directory {}", data_dir.display()))?;
    match lock {
        crate::remote::DirLock::Held(lock) => Ok((data_dir, lock)),
        crate::remote::DirLock::Busy(other) => anyhow::bail!(
            "fsonos serve already runs for this data directory{}: stop it first, or give \
             this one its own --data-dir and --events-port",
            other.map_or_else(String::new, |f| format!(
                " (pid {}, http://{})",
                f.pid, f.http
            ))
        ),
    }
}

/// Run the bind guard over the HTTP API's and the MCP server's planned
/// addresses: warn about the ones it allows with a warning, refuse to start
/// on one it refuses.
fn vet_binds(
    args: &ServeArgs,
    tailnet: &fsonos_tailscale::TailnetStatus,
    [http, mcp]: [&fsonos_tailscale::BindPlan; 2],
) -> anyhow::Result<()> {
    let ours = config::own_tailnet_addrs(tailnet);
    for (listener, plan) in [("HTTP API", http), ("MCP server", mcp)] {
        for &addr in &plan.addrs {
            match config::check_control_bind(listener, addr, args.allow_unsafe_bind, &ours) {
                Ok(None) => {}
                Ok(Some(warning)) => tracing::warn!("{warning}"),
                Err(refusal) => return Err(refusal.into()),
            }
        }
    }
    Ok(())
}

/// Vet the configuration, start both servers, report readiness, and run
/// until SIGINT / SIGTERM.
pub fn run(global: &GlobalArgs, args: &ServeArgs, scheduler: &SchedulerArgs) -> anyhow::Result<()> {
    // Unconfigured listeners bind loopback plus the tailnet (ts-autobind).
    let tailnet = args.tailnet();
    let http_plan = args.http_plan(&tailnet);
    let mcp_plan = args.mcp_plan(&tailnet);
    vet_binds(args, &tailnet, [&http_plan, &mcp_plan])?;
    let (data_dir, _lock) = unshared_data_dir(global)?;
    // GET /doctor and the MCP doctor run what `fsonos doctor` runs: the
    // store, the LAN (discovery, each player, a GENA round trip) and the
    // aliases, then serve's own checks.
    let checks = crate::doctor::checks(global, args.clone())?;
    let library = LibrarySync::for_serve(args, &data_dir, scheduler.clock());
    let house = policy(&data_dir)?;
    let (surface, live) = live_surface(global, args.events_port, house, library.clone())?;
    let surface = surface
        .with_clock(scheduler.clock())
        .with_sleep(scheduler.sleep(&data_dir));
    // The owner's Spotify sign-in comes back to this daemon's own port.
    let surface = spotify_auth::install(surface, args, &data_dir);
    let surface = Arc::new(with_action_log(surface, &data_dir, "serve").with_doctor_checks(checks));
    // The live model's playback keeps each DJ queue topped up.
    fsonos_api::surface::follow(&surface);

    let stop = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(&stop))
            .context("install the SIGINT/SIGTERM handler")?;
    }
    let ticking = start_scheduler(&surface, &stop)?;
    // The DJ's library: refreshed when due at start, then daily.
    crate::dj::sync::watch(library, Arc::clone(&stop));

    // The local CLI's proof that it is the CLI (see crate::remote).
    let cli_token = fsonos_core::announce::clip::clip_id().context("make the CLI token")?;
    let names = tailnet_names(&tailnet);
    let serve = ServeOrigin::default();
    let http = start_all("HTTP API", &http_plan, |addr| {
        start_http(
            &surface,
            addr,
            &names,
            args.tailscale_serve,
            &cli_token,
            &serve,
        )
    })?;
    let mcp = start_all("MCP server", &mcp_plan, |addr| {
        start_mcp(&surface, addr, &names)
    })?;
    // One value per key: the e2e harness and scripts parse this line.
    eprintln!(
        "fsonos serve: ready http=http://{} mcp=http://{}/mcp data={}",
        http[0].1,
        mcp[0].1,
        data_dir.display()
    );
    if let Some(&(_, loopback)) = http.iter().find(|(_, a)| a.ip().is_loopback()) {
        let file = crate::remote::DaemonFile {
            http: loopback,
            cli_token: cli_token.clone(),
            pid: std::process::id(),
            routes: global.routes.as_deref().map(crate::remote::canonical),
            serve: Some(crate::remote::ServeSettings {
                http: args.http,
                mcp_http: args.mcp_http,
                tailscale_serve: args.tailscale_serve,
                allow_unsafe_bind: args.allow_unsafe_bind,
                tailscale_off: args.tailscale == config::TailscaleMode::Off,
            }),
        };
        if let Err(e) = crate::remote::write_daemon_file(&data_dir, &file) {
            tracing::warn!("the CLI will not find this daemon: {e}");
        }
    }
    announce_tailnet(&tailnet, &http_plan, &http, &mcp, args.tailscale_serve);
    if !matches!(
        tailnet,
        fsonos_tailscale::TailnetStatus::Unavailable(fsonos_tailscale::Unavailable::Disabled)
    ) {
        let target = crate::tailscale_cmd::mappings(args)[0].target.clone();
        serve_origin::watch(target, serve, Arc::clone(&stop));
    }
    if args.tailscale_serve {
        // A first HTTPS certificate can take a while: not on the main thread.
        let (serve, tailnet) = (args.clone(), tailnet.clone());
        thread::spawn(
            move || match crate::tailscale_cmd::setup_at_startup(&serve, &tailnet) {
                Ok(done) => eprintln!("fsonos serve: {done}"),
                Err(e) => eprintln!("fsonos serve: Tailscale Serve not set up: {e:#}"),
            },
        );
    }
    announce_live(&live);

    while !stop.load(Ordering::Acquire) {
        thread::sleep(Duration::from_millis(100));
    }
    eprintln!("fsonos serve: stopping");
    crate::remote::remove_daemon_file(&data_dir, std::process::id());
    // Fades in flight put their volumes back; runs in flight finish.
    if ticking.join().is_err() {
        tracing::warn!("the scheduler stopped with a panic");
    }
    stop_http(&http);
    stop_mcp(mcp);
    // Announcements in flight put the music back before the process ends.
    drain_announcements(|| surface.announcements_pending(), ANNOUNCE_DRAIN);
    // The surfaces hold the model weakly: this ends every subscription.
    drop(live);
    Ok(())
}

/// How long a stopping daemon waits for announcements in flight to put the
/// music back (a clip plays for at most five minutes; most take seconds).
const ANNOUNCE_DRAIN: Duration = Duration::from_secs(60);

/// Wait until no announcement is pending (`pending` says how many), at most
/// `wait`. Answers whether they all finished.
fn drain_announcements(pending: impl Fn() -> u64, wait: Duration) -> bool {
    let deadline = Instant::now() + wait;
    loop {
        if pending() == 0 {
            return true;
        }
        if Instant::now() >= deadline {
            tracing::warn!("stopping with an announcement still in flight");
            return false;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// Shut the HTTP API listeners down.
fn stop_http(http: &[(Arc<TcpServer>, SocketAddr)]) {
    for (server, addr) in http {
        server.shutdown();
        // Wake the accept loop so it sees the shutdown. Briefly: a host may
        // not reach its own tailnet IPv6 address (Tailscale on macOS), and
        // the loop also polls for the shutdown.
        drop(std::net::TcpStream::connect_timeout(
            addr,
            Duration::from_millis(250),
        ));
    }
}

/// How long a stopping daemon waits for its MCP listeners to close.
const MCP_STOP: Duration = Duration::from_secs(5);

/// Shut the MCP server's listeners down: cancel each one's context, which
/// its accept loop checks between accepts, and wait up to [`MCP_STOP`] for
/// them to close.
fn stop_mcp(mcp: Vec<(McpStop, SocketAddr)>) {
    for (listener, _) in &mcp {
        listener.cx.cancel_with(
            asupersync::types::CancelKind::Shutdown,
            Some("fsonos serve is stopping"),
        );
    }
    let deadline = Instant::now() + MCP_STOP;
    for (listener, addr) in mcp {
        while !listener.thread.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        if !listener.thread.is_finished() {
            tracing::warn!("the MCP server on {addr} did not stop within {MCP_STOP:?}");
        }
    }
}

/// Run the sleep timers and the stored schedules on `surface`: a tick a
/// second until `stop`, then a graceful stop (see [`Scheduler`]).
fn start_scheduler(
    surface: &Arc<Surface>,
    stop: &Arc<AtomicBool>,
) -> anyhow::Result<thread::JoinHandle<()>> {
    take_up_sleep_timers(surface);
    Arc::new(Scheduler::new(surface))
        .start(Arc::clone(stop))
        .context("start the scheduler")
}

/// Bind every address of `plan`. Loopback and a configured address must
/// bind; a tailnet address that fails in the automatic plan (Tailscale still
/// coming up, say) is logged and skipped rather than stopping the daemon.
fn start_all<T>(
    listener: &str,
    plan: &fsonos_tailscale::BindPlan,
    mut start: impl FnMut(SocketAddr) -> anyhow::Result<T>,
) -> anyhow::Result<Vec<T>> {
    let mut started = Vec::new();
    for &addr in &plan.addrs {
        match start(addr) {
            Ok(bound) => started.push(bound),
            Err(e)
                if plan.reason == fsonos_tailscale::BindReason::Tailnet
                    && !addr.ip().is_loopback() =>
            {
                tracing::warn!("{listener}: not listening on {addr}: {e:#}");
            }
            Err(e) => return Err(e),
        }
    }
    anyhow::ensure!(!started.is_empty(), "{listener}: nothing to listen on");
    Ok(started)
}

/// Say where the listeners are and how tailnet devices reach them (the
/// connect URLs and the `claude mcp add` line), after the ready line.
fn announce_tailnet(
    tailnet: &fsonos_tailscale::TailnetStatus,
    plan: &fsonos_tailscale::BindPlan,
    http: &[(Arc<TcpServer>, SocketAddr)],
    mcp: &[(McpStop, SocketAddr)],
    fronted: bool,
) {
    eprintln!("fsonos serve: listening ({})", plan.note);
    // Describe each listener by its tailnet address when it has one.
    let pick = |addrs: Vec<SocketAddr>| {
        addrs
            .iter()
            .copied()
            .find(|a| fsonos_tailscale::is_tailnet_ip(a.ip()))
            .unwrap_or(addrs[0])
    };
    let listeners = [
        fsonos_tailscale::Listener {
            label: "HTTP API",
            bind: pick(http.iter().map(|(_, a)| *a).collect()),
            path: "",
            fronted,
        },
        fsonos_tailscale::Listener {
            label: "MCP server",
            bind: pick(mcp.iter().map(|(_, a)| *a).collect()),
            path: "/mcp",
            fronted,
        },
    ];
    for line in fsonos_tailscale::describe(tailnet, &listeners).lines() {
        eprintln!("fsonos serve: {line}");
    }
}

/// The names this host has on its tailnet (MagicDNS name and addresses),
/// which tailnet clients and Tailscale Serve send as the Host. Empty off a
/// tailnet.
fn tailnet_names(status: &fsonos_tailscale::TailnetStatus) -> Vec<String> {
    let Some(tailnet) = status.running() else {
        return Vec::new();
    };
    tailnet
        .magic_dns_name
        .iter()
        .cloned()
        .chain(tailnet.ipv4.iter().map(ToString::to_string))
        .chain(tailnet.ipv6.iter().map(ToString::to_string))
        .collect()
}

/// Bind the HTTP API on its own thread; returns the server and the bound
/// address once it listens. Only the listener's own Host names are admitted
/// (DNS-rebinding defense), and every route applies the browser rules for
/// the address it bound (so an ephemeral `:0` admits its own origin); a
/// loopback listener also admits what `serve` learns.
fn start_http(
    surface: &Arc<Surface>,
    addr: SocketAddr,
    names: &[String],
    behind_serve: bool,
    cli_token: &str,
    serve: &ServeOrigin,
) -> anyhow::Result<(Arc<TcpServer>, SocketAddr)> {
    // Host names do not depend on the port.
    let hosts = WebPolicy::for_listener(addr, names).hosts().to_vec();
    // Behind Tailscale Serve, its login header names the tailnet user.
    let identity = if behind_serve {
        Identity::behind_serve(listener_client(addr))
    } else {
        Identity::fixed(listener_client(addr))
    }
    .with_cli_token(cli_token.to_string());
    // On the tailnet, Tailscale names each caller (see the module docs);
    // each listener keeps its own short WhoIs cache.
    let identity = if addr.ip().is_loopback() {
        identity
    } else {
        let whois = fsonos_tailscale::WhoIs::default();
        // A process on this host reaching its tailnet address is local.
        let own = names
            .iter()
            .filter_map(|n| n.parse().ok())
            .chain([addr.ip()]);
        identity
            .with_tailnet(move |peer| whois.resolve(peer).map(|id| id.principal()))
            .with_own_addresses(own)
    };
    let (surface, names, serve) = (Arc::clone(surface), names.to_vec(), serve.clone());
    let mut config = ServerConfig::new(addr.to_string())
        .with_allowed_hosts(hosts)
        .with_max_connections(MAX_HTTP_CONNECTIONS)
        .with_request_timeout_secs(fsonos_api::surface::announce::ANNOUNCE_TIMEOUT_SECS);
    // The request parser also counts HTTP headers; the app enforces the
    // smaller body limit before deserializing a WAV upload.
    config.parse_limits.max_request_size = fsonos_api::surface::announce::MAX_ANNOUNCE_REQUEST_BYTES
        + config.parse_limits.max_headers_size;
    let server = Arc::new(TcpServer::new(config));
    let (bound_tx, bound_rx) = mpsc::channel();
    let serving = Arc::clone(&server);
    thread::Builder::new()
        .name("fsonos-http".into())
        .spawn(move || {
            let ready_tx = bound_tx.clone();
            let result = listener_runtime().and_then(|rt| {
                rt.block_on(async move {
                    let cx = asupersync::Cx::current().context("ambient Cx")?;
                    let listener = asupersync::net::TcpListener::bind(addr)
                        .await
                        .with_context(|| format!("bind the HTTP API on {addr}"))?;
                    let local = listener.local_addr().context("HTTP API address")?;
                    let mut web = WebPolicy::for_listener(local, &names);
                    if local.ip().is_loopback() {
                        web = web.with_serve_origin(&serve);
                    }
                    let app = Arc::new(fsonos_api::app(&surface, &identity, &web));
                    let _ = ready_tx.send(Ok(local));
                    // One task per connection: an idle keep-alive client or
                    // an open GET /events stream must not hold up the rest.
                    serving
                        .serve_on_app_concurrent(&cx, listener, app)
                        .await
                        .map_err(|e| anyhow::anyhow!("HTTP API: {e}"))
                })
            });
            if let Err(e) = result {
                let _ = bound_tx.send(Err(e));
            }
        })
        .context("start the HTTP API thread")?;
    let bound = bound_rx
        .recv_timeout(Duration::from_secs(10))
        .context("the HTTP API did not start within 10 s")??;
    Ok((server, bound))
}

/// The most connections the HTTP API holds open at once: an event stream or
/// a long poll keeps a thread of its own for as long as it is open, so a
/// caller opening them without end could exhaust the host otherwise.
const MAX_HTTP_CONNECTIONS: usize = 256;

/// How many threads each listener serves on.
const LISTENER_WORKERS: usize = 4;

/// The runtime a listener serves on. Its workers only move bytes: each HTTP
/// route, and each MCP tool that can take long, runs on a thread of its own
/// (`fsonos_api::off_thread`), so a long call (an announcement that plays, a
/// fade, a DJ start that asks Spotify) holds none of them.
fn listener_runtime() -> anyhow::Result<asupersync::runtime::Runtime> {
    asupersync::runtime::RuntimeBuilder::multi_thread()
        .worker_threads(LISTENER_WORKERS)
        .with_reactor(asupersync::runtime::reactor::create_reactor().context("create I/O reactor")?)
        .blocking_threads(0, 16)
        .build()
        .context("build the listener runtime")
}

/// How [`stop_mcp`] stops one MCP listener.
struct McpStop {
    /// The listener's context: its accept loop ends once this is cancelled.
    cx: asupersync::Cx,
    /// The listener's thread, finished once the listener has closed.
    thread: thread::JoinHandle<()>,
}

/// Bind the MCP server (streamable HTTP) on its own thread; returns the bound
/// address once it listens, and how to stop it. Like the HTTP API, it admits
/// only its own Host names (DNS-rebinding defense).
fn start_mcp(
    surface: &Arc<Surface>,
    addr: SocketAddr,
    names: &[String],
) -> anyhow::Result<(McpStop, SocketAddr)> {
    let hosts = WebPolicy::for_listener(addr, names).hosts().to_vec();
    let backend = fsonos_mcp::tools::Backend::shared(Arc::clone(surface), listener_client(addr));
    if !fsonos_mcp::tools::install(backend) {
        anyhow::bail!("the MCP backend is already installed");
    }
    let (bound_tx, bound_rx) = mpsc::channel();
    let thread = thread::Builder::new()
        .name("fsonos-mcp".into())
        .spawn(move || {
            let ready_tx = bound_tx.clone();
            let result = listener_runtime().and_then(|rt| {
                rt.block_on(async move {
                    let cx = asupersync::Cx::current().context("ambient Cx")?;
                    let bound = fsonos_mcp::server_answering(hosts)
                        .bind_http(&cx, addr.to_string())
                        .await
                        .map_err(|e| anyhow::anyhow!("bind the MCP server on {addr}: {e}"))?;
                    let local = bound
                        .local_addr()
                        .map_err(|e| anyhow::anyhow!("MCP address: {e}"))?;
                    let _ = ready_tx.send(Ok((local, cx.clone())));
                    bound
                        .serve(&cx)
                        .await
                        .map(drop)
                        .map_err(|e| anyhow::anyhow!("MCP server: {e}"))
                })
            });
            if let Err(e) = result {
                let _ = bound_tx.send(Err(e));
            }
        })
        .context("start the MCP thread")?;
    let (local, cx) = bound_rx
        .recv_timeout(Duration::from_secs(10))
        .context("the MCP server did not start within 10 s")??;
    Ok((McpStop { cx, thread }, local))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    /// launchd sends SIGKILL `ExitTimeOut` seconds after SIGTERM. The job
    /// template must leave the announcement drain, and the unsubscribes after
    /// it, time to finish, or a restart cuts an announcement off before it
    /// puts the music back.
    #[test]
    fn the_launchd_job_waits_out_a_stopping_daemon() {
        let plist = include_str!("../../../docs/launchd/io.github.dicklesworthstone.fsonos.plist");
        let secs: u64 = plist
            .split("<key>ExitTimeOut</key>")
            .nth(1)
            .and_then(|rest| rest.split("<integer>").nth(1))
            .and_then(|rest| rest.split("</integer>").next())
            .and_then(|n| n.trim().parse().ok())
            .expect("the template sets ExitTimeOut");
        assert!(
            secs >= ANNOUNCE_DRAIN.as_secs() + 20,
            "ExitTimeOut {secs} s leaves no room after the {} s drain",
            ANNOUNCE_DRAIN.as_secs()
        );
    }

    #[test]
    fn a_stopping_daemon_waits_for_announcements_but_not_forever() {
        // Two in flight, done one after the other.
        let checks = AtomicU64::new(0);
        let finishing = || 2u64.saturating_sub(checks.fetch_add(1, Ordering::SeqCst));
        assert!(drain_announcements(finishing, Duration::from_secs(5)));
        assert_eq!(checks.load(Ordering::SeqCst), 3);
        // One that never ends is given up on.
        let started = Instant::now();
        assert!(!drain_announcements(|| 1, Duration::from_millis(250)));
        assert!(started.elapsed() >= Duration::from_millis(250));
        // Nothing in flight: no wait.
        assert!(drain_announcements(|| 0, Duration::ZERO));
    }

    #[test]
    fn loopback_listeners_serve_local_callers() {
        let at = |s: &str| s.parse::<SocketAddr>().unwrap();
        assert_eq!(listener_client(at("127.0.0.1:8099")), Client::LoopbackHttp);
        assert_eq!(listener_client(at("[::1]:8099")), Client::LoopbackHttp);
        assert_eq!(listener_client(at("100.70.1.2:8099")), Client::Unknown);
    }
}
