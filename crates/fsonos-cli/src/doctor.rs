//! `fsonos doctor`: what is wrong with this setup, and how to fix it.
//!
//! The report comes from the shared surface (the core's checks), plus the
//! checks only this layer can make ([`checks`], which `fsonos setup` and the
//! stdio MCP server's `doctor` tool run too):
//!
//! * `store.disk`, `store.open` and `lan.*` ([`lan_checks`]): the free
//!   space on the store's volume, and the core's LAN checks over this host's
//!   LAN as `--routes` and the seeds shape it (the store, SSDP, the seeds,
//!   reading each player, the households, a GENA round trip, the topology);
//! * `daemon.bind`: the bind guard's verdict on the HTTP and MCP addresses;
//! * `daemon.health`: whether a daemon answers on the HTTP address, and
//!   which version;
//! * `dj.taste`: whether the cached Spotify grant carries the taste
//!   scopes, so the DJ learns beyond the owner's saved library;
//! * `dj.sign_in` ([`sign_in`]): whether that grant still works (one liked
//!   track read, refreshing the token as serve would);
//! * `speech.backend`: which local speech executable announcements select;
//! * `selftest.last` ([`selftest_last`]): how the last full `fsonos selftest`
//!   on this host went, and when;
//! * `tailscale.*` ([`tailscale`]): whether the daemon is reachable over the
//!   tailnet, with the connect URLs, or why not.
//!
//! Exit codes: 0 all passed, 6 warnings only, 7 something failed (outside
//! the CLI's 1-5 error codes and clap's 2).

use fsonos_api::Failure;
use fsonos_api::surface::DoctorChecks;
use fsonos_core::announce::speech::SpeechConfig;
use fsonos_core::doctor::lan::LanProbe;
use fsonos_core::doctor::{Check, CheckContext, CheckId, CheckResult, Report, Runner, Status};
use fsonos_core::policy::Client;
use fsonos_spotify::client::TokenCache;
use serde_json::json;
use std::io::{Read as _, Write as _};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use crate::config::{self, GlobalArgs, ServeArgs};
use crate::direct::Direct;

mod remote;
pub mod selftest_last;
pub mod service;
pub mod sign_in;
pub mod tailscale;
pub mod warnings;

/// `fsonos doctor` arguments.
#[derive(Debug, Clone, clap::Args)]
pub struct DoctorArgs {
    /// The daemon settings to check (the same flags and env as `serve`).
    /// While a daemon serves this data directory and answers, the settings
    /// it runs with are checked instead. With --daemon or --daemon-url,
    /// all checks run on that daemon using its own settings.
    #[command(flatten)]
    pub serve: ServeArgs,
    /// Only checks whose id starts with this (e.g. `spotify`, `daemon.`).
    #[arg(long, value_name = "PREFIX")]
    pub only: Option<String>,
}

const BIND: CheckId = CheckId("daemon.bind");
const HEALTH: CheckId = CheckId("daemon.health");
const TASTE: CheckId = CheckId("dj.taste");
const SPEECH: CheckId = CheckId("speech.backend");

/// This host's own tailnet addresses, asked only when a listener is bound
/// in the tailnet range.
type OwnTailnet = Box<dyn Fn() -> Vec<IpAddr> + Send + Sync>;

/// The bind guard's verdict for both control listeners.
struct BindCheck {
    http: SocketAddr,
    mcp: SocketAddr,
    allow_unsafe: bool,
    tailnet: OwnTailnet,
}

impl Check for BindCheck {
    fn id(&self) -> CheckId {
        BIND
    }

    fn title(&self) -> &'static str {
        "Listener addresses"
    }

    fn run(&self, _: &CheckContext) -> CheckResult {
        let evidence = json!({ "http": self.http.to_string(), "mcp": self.mcp.to_string() });
        let in_range = [self.http, self.mcp]
            .iter()
            .any(|a| config::bind_scope(a.ip()) == config::BindScope::Tailnet);
        let ours = if in_range {
            (self.tailnet)()
        } else {
            Vec::new()
        };
        let mut warnings = Vec::new();
        for (listener, addr) in [("HTTP API", self.http), ("MCP server", self.mcp)] {
            match config::check_control_bind(listener, addr, self.allow_unsafe, &ours) {
                Ok(None) => {}
                Ok(Some(warning)) => warnings.push(warning),
                Err(refusal) => {
                    return CheckResult::fail(refusal.detail, refusal.hint).with_evidence(evidence);
                }
            }
        }
        if warnings.is_empty() {
            CheckResult::pass(format!(
                "HTTP {} and MCP {} are loopback or tailnet addresses",
                self.http, self.mcp
            ))
            .with_evidence(evidence)
        } else {
            CheckResult::warn(
                warnings.join("; "),
                "Bind 127.0.0.1 and front it with Tailscale Serve (docs/DEPLOY.md).",
            )
            .with_evidence(evidence)
        }
    }
}

/// `GET /health` on `addr`: the daemon's version, or why there is none.
fn probe_health(addr: SocketAddr, timeout: Duration) -> Result<String, String> {
    probe_health_json(addr, timeout)
        .map(|health| health["version"].as_str().unwrap_or_default().to_owned())
}

/// `GET /health` on `addr`: its answer, or why there is none.
fn probe_health_json(addr: SocketAddr, timeout: Duration) -> Result<serde_json::Value, String> {
    let mut stream = TcpStream::connect_timeout(&addr, timeout).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;
    let request = format!("GET /health HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .map_err(|e| e.to_string())?;
    let mut answer = String::new();
    stream
        .read_to_string(&mut answer)
        .map_err(|e| e.to_string())?;
    let body = answer.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    let health: serde_json::Value = serde_json::from_str(body)
        .map_err(|_| format!("not a FrankenSonos answer: {answer:.80}"))?;
    if health["version"].as_str().is_none() {
        return Err("no version in /health".to_string());
    }
    Ok(health)
}

/// The verdict on a daemon that answered `/health` on `http`: answering is
/// not enough, it must see the speakers. A blind one (no player found) is
/// what a daemon the operating system keeps off the LAN looks like, while
/// this terminal reaches the speakers fine. A daemon too old to say passes
/// on its answer alone.
fn health_result(health: &serde_json::Value, http: SocketAddr) -> CheckResult {
    let version = health["version"].as_str().unwrap_or_default();
    let live = &health["live"];
    let players = live["players"].as_u64().unwrap_or_default();
    let evidence = json!({ "version": version, "live": live, "store": health["store"] });
    let result = match live["verdict"].as_str() {
        Some("blind") => CheckResult::fail(
            format!("fsonos serve {version} answers on {http} but sees no players"),
            "The daemon can't reach the speakers. On macOS allow it in System Settings > \
             Privacy & Security > Local Network (a service needs this once per binary); else \
             check the network it is on, and FSONOS_SEEDS.",
        ),
        Some("starting") => CheckResult::warn(
            format!("fsonos serve {version} answers on {http} and is still finding the speakers"),
            "Run the doctor again in a few seconds.",
        ),
        Some("degraded") => CheckResult::warn(
            format!(
                "fsonos serve {version} sees {players} player(s), {} offline, with {} event \
                 subscription(s)",
                live["offline"], live["subscriptions"]
            ),
            "See the daemon's own report (`GET /doctor`, daemon.live) for which players and why.",
        ),
        _ => CheckResult::pass(if live.is_null() {
            format!("fsonos serve {version} answers on {http}")
        } else {
            format!("fsonos serve {version} answers on {http} and sees {players} player(s)")
        }),
    };
    // A store that fails while the speakers answer is worse than either
    // alone looks: actions run, unlogged and not undoable (vg3s.8).
    let result = match store_result(&health["store"], version) {
        Some(store) if store.status == Status::Fail || result.status == Status::Pass => store,
        _ => result,
    };
    result.with_evidence(evidence)
}

/// How long a store fault in `/health` fails `daemon.health`; older ones
/// warn.
const STORE_FAULT_RECENT_SECS: u64 = 15 * 60;

/// The store faults the daemon reported (`/health`'s `store`), judged: a
/// recent one fails, an older one warns.
fn store_result(store: &serde_json::Value, version: &str) -> Option<CheckResult> {
    let failures = store["failures"].as_u64()?;
    let ago = store["last_secs_ago"].as_u64().unwrap_or_default();
    let kind = store["kind"].as_str().unwrap_or("io");
    let summary = format!(
        "fsonos serve {version}'s store failed {failures} time(s) ({kind}), the last {ago} s ago"
    );
    let remedy = match kind {
        "disk_full" => {
            "Make room on the data directory's volume (`fsonos doctor --only store` shows it): \
             until the store can write, actions go unlogged and cannot be undone."
        }
        "corrupt" => {
            "Stop the daemon, keep a copy of fsonos.db and its -wal file, and read the daemon \
             log: the store is a cache plus history, and fsonos deletes nothing itself."
        }
        _ => {
            "Read the daemon log for the store's I/O errors, and check the data directory's volume."
        }
    };
    Some(if ago <= STORE_FAULT_RECENT_SECS {
        CheckResult::fail(summary, remedy)
    } else {
        CheckResult::warn(summary, remedy)
    })
}

/// Whether `fsonos serve` answers `/health` on the HTTP address.
struct HealthCheck {
    http: SocketAddr,
}

impl Check for HealthCheck {
    fn id(&self) -> CheckId {
        HEALTH
    }

    fn title(&self) -> &'static str {
        "Daemon"
    }

    fn run(&self, ctx: &CheckContext) -> CheckResult {
        if self.http.port() == 0 {
            return CheckResult::skip("the HTTP address has no fixed port to probe");
        }
        let timeout = ctx.remaining().min(Duration::from_secs(3));
        match probe_health_json(self.http, timeout) {
            Ok(health) => health_result(&health, self.http),
            Err(why) => CheckResult::warn(
                format!("no daemon answers on {}", self.http),
                "Start it with `fsonos serve`, or under launchd (docs/DEPLOY.md).",
            )
            .with_detail(why),
        }
    }
}

/// Whether the cached Spotify grant carries the taste scopes, so the DJ can
/// learn beyond the owner's saved library (followed and top artists, top
/// tracks, recent plays, playlists). Without them the DJ runs library-only,
/// which this warns about; it never fails.
struct TasteScopesCheck {
    data_dir: PathBuf,
}

impl Check for TasteScopesCheck {
    fn id(&self) -> CheckId {
        TASTE
    }

    fn title(&self) -> &'static str {
        "Spotify taste scopes"
    }

    fn run(&self, _: &CheckContext) -> CheckResult {
        match TokenCache::in_data_dir(&self.data_dir).load() {
            Ok(None) => CheckResult::skip("not signed in to Spotify yet (run `fsonos setup`)"),
            Ok(Some(token)) => {
                let missing = token.missing_taste_scopes();
                if missing.is_empty() {
                    CheckResult::pass(
                        "the grant carries every taste scope; the DJ learns from your \
                         follows, top artists and tracks, recent plays and playlists",
                    )
                } else {
                    CheckResult::warn(
                        format!(
                            "the DJ's taste is library-only: the Spotify grant is missing {}",
                            missing.join(", ")
                        ),
                        "Re-run `fsonos setup` (or sign in again) to grant the taste scopes.",
                    )
                    .with_evidence(json!({ "missing_scopes": missing }))
                }
            }
            Err(e) => CheckResult::warn(
                "could not read the Spotify token cache".to_owned(),
                "Check the data directory, or re-run `fsonos setup`.",
            )
            .with_detail(e.to_string()),
        }
    }
}

/// Discover the configured speech command without running it or generating audio.
struct SpeechBackendCheck {
    data_dir: PathBuf,
}

impl Check for SpeechBackendCheck {
    fn id(&self) -> CheckId {
        SPEECH
    }

    fn title(&self) -> &'static str {
        "Local speech backend"
    }

    fn run(&self, _: &CheckContext) -> CheckResult {
        let config_path = self.data_dir.join("speech.toml");
        let config = match SpeechConfig::load(&self.data_dir) {
            Ok(config) => config,
            Err(error) => {
                return CheckResult::fail(
                    "could not load the speech configuration",
                    format!("Correct the configuration in {}.", config_path.display()),
                )
                .with_detail(error.to_string())
                .with_evidence(json!({ "config_path": config_path.display().to_string() }));
            }
        };
        match config.discover(None) {
            Ok(backend) => {
                let detail = if backend.name() == "frankentts" {
                    "FrankenTTS model readiness and speech synthesis were not tested. \
                     Run `ftts pull` once to install the local models before the first announcement."
                } else {
                    "The executable is available; speech synthesis was not run."
                };
                CheckResult::pass(format!(
                    "{} executable available at {}",
                    backend.name(),
                    backend.executable().display()
                ))
                .with_detail(detail)
                .with_evidence(json!({
                    "backend": backend.name(),
                    "executable": backend.executable().display().to_string(),
                    "config_path": config_path.display().to_string(),
                    "synthesis_tested": false,
                }))
            }
            Err(error) => CheckResult::warn(
                "no usable local speech backend was found",
                format!(
                    "Install FrankenTTS (`ftts`) and run `ftts pull`, or install espeak-ng; \
                     configure another local engine in {}.",
                    config_path.display()
                ),
            )
            .with_detail(error.to_string())
            .with_evidence(json!({
                "config_path": config_path.display().to_string(),
                "synthesis_tested": false,
            })),
        }
    }
}

/// Register the checks this layer owns for `serve`'s settings.
pub fn register(runner: &mut Runner, serve: &ServeArgs, data_dir: &Path) {
    let settings = serve.clone();
    runner.register(BindCheck {
        http: serve.http_local(),
        mcp: serve.mcp_local(),
        allow_unsafe: serve.allow_unsafe_bind,
        tailnet: Box::new(move || config::own_tailnet_addrs(&settings.tailnet())),
    });
    runner.register(HealthCheck {
        http: serve.http_local(),
    });
    runner.register(warnings::WarningsCheck {
        http: serve.http_local(),
    });
    runner.register(TasteScopesCheck {
        data_dir: data_dir.to_path_buf(),
    });
    runner.register(sign_in::SignInCheck::new(serve, data_dir));
    runner.register(SpeechBackendCheck {
        data_dir: data_dir.to_path_buf(),
    });
    runner.register(selftest_last::LastSelftest {
        data_dir: data_dir.to_path_buf(),
    });
    runner.register(service::ServiceCheck);
    tailscale::register(runner, serve);
}

/// Keep only the checks whose id starts with `prefix`.
#[must_use]
pub fn only(report: Report, prefix: Option<&str>) -> Report {
    match prefix {
        None => report,
        Some(prefix) => Report {
            entries: report
                .entries
                .into_iter()
                .filter(|e| e.id.0.starts_with(prefix))
                .collect(),
        },
    }
}

/// The core's LAN checks over this host's LAN, as `--routes`, the seeds and
/// `--wait` shape it, with the store and free-space checks for the data
/// directory. Each run takes a fresh probe (it discovers once and caches
/// what it saw).
pub fn lan_checks(
    global: &GlobalArgs,
) -> Result<impl Fn(&mut Runner) + Send + Sync + 'static, Failure> {
    let lan = global.network()?.lan;
    let (seeds, wait) = (global.seed_addrs()?, global.wait());
    let data_dir = crate::daemon::data_dir(global)?;
    Ok(move |runner: &mut Runner| {
        let probe = Arc::new(LanProbe::new(Arc::clone(&lan), seeds.clone()).with_ssdp_wait(wait));
        fsonos_core::doctor::disk::register(runner, data_dir.clone());
        fsonos_core::doctor::lan::register(runner, data_dir.clone(), &probe);
        let aliases = data_dir.join(fsonos_api::surface::ALIASES_FILE);
        fsonos_core::doctor::rooms::register(runner, aliases, &probe);
    })
}

/// Every check this layer adds for `serve`'s settings: [`lan_checks`], then
/// [`register`]'s. While a daemon serves this data directory, it is judged
/// by the settings it runs with ([`serving_with`], at each run).
pub fn checks(global: &GlobalArgs, serve: ServeArgs) -> Result<DoctorChecks, Failure> {
    let lan = lan_checks(global)?;
    let data_dir = crate::daemon::data_dir(global)?;
    Ok(Box::new(move |runner| {
        lan(runner);
        let serve = serving_with(&serve, &data_dir, &|addr| {
            probe_health(addr, Duration::from_secs(1))
        });
        register(runner, &serve, &data_dir);
    }))
}

/// The settings to check: while a daemon serving `data_dir` answers, the
/// ones it recorded in daemon.json (this command's environment need not be
/// serve's: a daemon started by launchd or a script sets its own), else
/// `serve` as given.
fn serving_with(
    serve: &ServeArgs,
    data_dir: &Path,
    probe: &dyn Fn(SocketAddr) -> Result<String, String>,
) -> ServeArgs {
    let mut effective = serve.clone();
    if let Some(file) = crate::remote::read_daemon_file(data_dir)
        && let Some(settings) = file.serve
        && probe(file.http).is_ok()
    {
        effective.http = settings.http;
        effective.mcp_http = settings.mcp_http;
        effective.tailscale_serve = settings.tailscale_serve;
        effective.allow_unsafe_bind = settings.allow_unsafe_bind;
        effective.tailscale = if settings.tailscale_off {
            config::TailscaleMode::Off
        } else {
            config::TailscaleMode::Auto
        };
    }
    effective
}

/// Run `fsonos doctor` and print the report; the exit code is the report's.
pub fn run(global: &GlobalArgs, args: &DoctorArgs) -> anyhow::Result<ExitCode> {
    if let Some(report) = remote::fetch(global, args.only.as_deref())? {
        if global.json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            print!("{}", report.render_table());
        }
        return Ok(ExitCode::from(report.exit_code()));
    }
    let direct = Direct::open(global, Some(checks(global, args.serve.clone())?))?;
    let report = only(direct.doctor(&Client::Cli)?, args.only.as_deref());
    if global.json {
        println!("{}", serde_json::to_string_pretty(&report.to_json())?);
    } else {
        print!("{}", report.render_table());
    }
    Ok(ExitCode::from(report.exit_code()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsonos_core::doctor::Status;

    /// Run one check the way `fsonos doctor` does, through a runner.
    fn run_one(check: impl Check + 'static) -> CheckResult {
        let id = check.id();
        let mut runner = Runner::new();
        runner.register(check);
        runner.run().unwrap().get(id).cloned().unwrap()
    }

    /// This host's synthetic tailnet address in the bind tests.
    const OURS: [u8; 4] = [100, 64, 0, 7];

    fn bind_at(http: SocketAddr, mcp: SocketAddr) -> CheckResult {
        run_one(BindCheck {
            http,
            mcp,
            allow_unsafe: false,
            tailnet: Box::new(|| vec![IpAddr::from(OURS)]),
        })
    }

    fn bind(http: &str, mcp: &str) -> CheckResult {
        bind_at(http.parse().unwrap(), mcp.parse().unwrap())
    }

    #[test]
    fn a_tailnet_range_bind_passes_only_on_this_hosts_own_address() {
        let local: SocketAddr = "127.0.0.1:8098".parse().unwrap();
        assert_eq!(
            bind_at(SocketAddr::from((OURS, 8099)), local).status,
            Status::Pass
        );
        let stray = bind_at(SocketAddr::from(([100, 64, 0, 8], 8099)), local);
        assert_eq!(stray.status, Status::Warn);
        assert!(stray.summary.contains("carrier-grade NAT"), "{stray:?}");
    }

    #[test]
    fn bind_verdicts_follow_the_guard() {
        assert_eq!(
            bind("127.0.0.1:8099", "127.0.0.1:8098").status,
            Status::Pass
        );
        assert_eq!(
            bind("192.168.1.9:8099", "127.0.0.1:8098").status,
            Status::Warn
        );
        let wild = bind("0.0.0.0:8099", "127.0.0.1:8098");
        assert_eq!(wild.status, Status::Fail);
        assert!(wild.remedy.unwrap().contains("--allow-unsafe-bind"));
    }

    #[test]
    fn a_missing_daemon_is_a_warning_with_a_remedy() {
        // Nothing listens on a fresh ephemeral port once its listener is dropped.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let r = run_one(HealthCheck {
            http: SocketAddr::from(([127, 0, 0, 1], port)),
        });
        assert_eq!(r.status, Status::Warn);
        assert!(r.remedy.unwrap().contains("fsonos serve"));
        let skipped = run_one(HealthCheck {
            http: "127.0.0.1:0".parse().unwrap(),
        });
        assert_eq!(skipped.status, Status::Skip);
    }

    #[test]
    fn a_daemon_that_answers_but_sees_no_players_fails() {
        let http: SocketAddr = "127.0.0.1:8099".parse().unwrap();
        let health =
            |live: serde_json::Value| json!({ "status": "ok", "version": "0.1.0", "live": live });
        let blind = health_result(&health(json!({"verdict": "blind", "players": 0})), http);
        assert_eq!(blind.status, Status::Fail, "{blind:?}");
        assert!(blind.remedy.unwrap().contains("Local Network"));
        let degraded = health_result(
            &health(
                json!({"verdict": "degraded", "players": 13, "offline": 1, "subscriptions": 28}),
            ),
            http,
        );
        assert_eq!(degraded.status, Status::Warn, "{degraded:?}");
        let ok = health_result(&health(json!({"verdict": "ok", "players": 13})), http);
        assert_eq!(ok.status, Status::Pass);
        assert!(ok.summary.contains("13 player"), "{ok:?}");
        let starting = health_result(&health(json!({"verdict": "starting", "players": 0})), http);
        assert_eq!(starting.status, Status::Warn, "{starting:?}");
        let older = health_result(&json!({ "status": "ok", "version": "0.1.0" }), http);
        assert_eq!(older.status, Status::Pass);
    }

    /// A daemon whose store fails while the speakers answer fails the check;
    /// one that failed long ago warns (vg3s.8).
    #[test]
    fn a_daemon_whose_store_fails_is_not_healthy() {
        let http: SocketAddr = "127.0.0.1:8099".parse().unwrap();
        let health = |ago: u64| {
            json!({
                "status": "ok",
                "version": "0.1.0",
                "live": {"verdict": "ok", "players": 13},
                "store": {"kind": "disk_full", "failures": 12, "last_secs_ago": ago},
            })
        };
        let failing = health_result(&health(30), http);
        assert_eq!(failing.status, Status::Fail, "{failing:?}");
        assert!(
            failing.summary.contains("12 time(s) (disk_full)"),
            "{failing:?}"
        );
        assert!(failing.remedy.unwrap().contains("Make room"));
        let earlier = health_result(&health(STORE_FAULT_RECENT_SECS + 1), http);
        assert_eq!(earlier.status, Status::Warn, "{earlier:?}");
        // A blind daemon stays a failure about the speakers.
        let mut blind = health(STORE_FAULT_RECENT_SECS + 1);
        blind["live"] = json!({"verdict": "blind", "players": 0});
        let blind = health_result(&blind, http);
        assert_eq!(blind.status, Status::Fail);
        assert!(blind.summary.contains("sees no players"), "{blind:?}");
    }

    #[test]
    fn only_keeps_a_prefix() {
        let mut runner = Runner::new();
        register(
            &mut runner,
            &ServeArgs {
                http: Some("127.0.0.1:0".parse().unwrap()),
                mcp_http: Some("127.0.0.1:0".parse().unwrap()),
                spotify_client_id: None,
                spotify_redirect_uri: String::new(),
                events_port: 0,
                allow_unsafe_bind: false,
                bridge_compat: None,
                tailscale: crate::config::TailscaleMode::Auto,
                tailscale_serve: false,
            },
            &std::env::temp_dir(),
        );
        let report = only(runner.run().unwrap(), Some("daemon.b"));
        assert_eq!(report.entries.len(), 1);
        assert_eq!(report.entries[0].id, BIND);
    }

    #[test]
    fn taste_scopes_skip_when_not_signed_in() {
        let dir =
            std::env::temp_dir().join(format!("fsonos-doctor-taste-{}-absent", std::process::id()));
        assert_eq!(
            run_one(TasteScopesCheck { data_dir: dir }).status,
            Status::Skip
        );
    }

    fn speech_data_dir(label: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "fsonos-doctor-speech-{}-{nonce}-{label}",
            std::process::id()
        ));
        std::fs::create_dir(&dir).unwrap();
        dir
    }

    fn configure_speech_command(dir: &Path, executable: &Path) {
        let config = json!({
            "backend": "command",
            "command": [executable, "{output}"],
        });
        std::fs::write(dir.join("speech.toml"), toml::to_string(&config).unwrap()).unwrap();
    }

    #[test]
    fn malformed_speech_config_is_a_failure_with_the_path_to_fix() {
        let dir = speech_data_dir("malformed");
        std::fs::write(dir.join("speech.toml"), "backend = [").unwrap();
        let r = run_one(SpeechBackendCheck { data_dir: dir });
        assert_eq!(r.status, Status::Fail);
        assert!(r.detail.is_some());
        assert!(r.remedy.unwrap().contains("speech.toml"));
    }

    // macOS file systems (APFS, HFS+) refuse names that are not UTF-8
    // (EILSEQ, "Illegal byte sequence"), so no data directory there is one.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn speech_evidence_handles_a_non_utf8_data_directory() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt as _;

        let parent = speech_data_dir("non-utf8");
        let dir = parent.join(OsString::from_vec(b"speech-\xff".to_vec()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("speech.toml");
        let expected_path = path.display().to_string();
        for (contents, status) in [
            ("backend = [", Status::Fail),
            (
                "backend = 'command'\ncommand = ['/dev/null/missing-engine', '{output}']",
                Status::Warn,
            ),
            (
                "backend = 'command'\ncommand = ['/bin/sh', '{output}']",
                Status::Pass,
            ),
        ] {
            std::fs::write(&path, contents).unwrap();
            let result = run_one(SpeechBackendCheck {
                data_dir: dir.clone(),
            });
            assert_eq!(result.status, status);
            assert_eq!(result.evidence["config_path"], expected_path);
        }
    }

    #[test]
    fn missing_speech_command_warns_with_local_engine_setup() {
        let dir = speech_data_dir("missing");
        configure_speech_command(&dir, &dir.join("missing-engine"));
        let r = run_one(SpeechBackendCheck { data_dir: dir });
        assert_eq!(r.status, Status::Warn);
        assert_eq!(r.evidence["synthesis_tested"], false);
        let remedy = r.remedy.unwrap();
        assert!(remedy.contains("ftts pull"));
        assert!(remedy.contains("speech.toml"));
    }

    #[cfg(unix)]
    #[test]
    fn speech_discovery_does_not_execute_the_configured_command() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = speech_data_dir("read-only");
        let executable = dir.join("speech-engine");
        // It is discoverable, but executing it would fail: doctor must only
        // inspect availability, never start synthesis or an arbitrary command.
        std::fs::write(&executable, "#!/nonexistent-fsonos-doctor-interpreter\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        configure_speech_command(&dir, &executable);
        let r = run_one(SpeechBackendCheck { data_dir: dir });
        assert_eq!(r.status, Status::Pass);
        assert_eq!(r.evidence["backend"], "command");
        assert_eq!(r.evidence["executable"], json!(executable));
        assert_eq!(r.evidence["synthesis_tested"], false);
        assert!(r.detail.unwrap().contains("not run"));
    }

    #[test]
    fn a_running_daemon_is_checked_by_the_settings_it_runs_with() {
        let dir =
            std::env::temp_dir().join(format!("fsonos-doctor-serving-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mine = ServeArgs {
            http: None,
            mcp_http: None,
            spotify_client_id: None,
            spotify_redirect_uri: String::new(),
            events_port: 0,
            allow_unsafe_bind: false,
            bridge_compat: None,
            tailscale: crate::config::TailscaleMode::Auto,
            tailscale_serve: false,
        };
        let loopback: SocketAddr = "127.0.0.1:8099".parse().unwrap();
        let answers = |_: SocketAddr| Ok("0.1.0".to_string());
        let silent = |_: SocketAddr| Err("refused".to_string());
        assert_eq!(
            serving_with(&mine, &dir, &answers).http,
            None,
            "no daemon.json"
        );

        let mut file = crate::remote::DaemonFile {
            http: loopback,
            cli_token: "0123456789abcdef0123456789abcdef".into(),
            pid: 1,
            routes: None,
            serve: None,
        };
        crate::remote::write_daemon_file(&dir, &file).unwrap();
        let older = serving_with(&mine, &dir, &answers);
        assert!(
            !older.tailscale_serve && older.http.is_none(),
            "an older daemon"
        );

        file.serve = Some(crate::remote::ServeSettings {
            http: Some(loopback),
            mcp_http: Some("127.0.0.1:8098".parse().unwrap()),
            tailscale_serve: true,
            allow_unsafe_bind: true,
            tailscale_off: true,
        });
        crate::remote::write_daemon_file(&dir, &file).unwrap();
        let theirs = serving_with(&mine, &dir, &answers);
        assert!(theirs.tailscale_serve);
        assert_eq!(theirs.http, Some(loopback));
        assert_eq!(theirs.mcp_http, Some("127.0.0.1:8098".parse().unwrap()));
        assert!(theirs.allow_unsafe_bind);
        assert_eq!(theirs.tailscale, crate::config::TailscaleMode::Off);
        assert!(
            !serving_with(&mine, &dir, &silent).tailscale_serve,
            "a daemon.json left by a daemon that no longer answers is not believed"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
