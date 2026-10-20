//! `fsonos` configuration: the global options every command takes (seeds,
//! SSDP wait, JSON output), the `serve` settings (listener addresses, local
//! paths), and the bind guard.
//!
//! Every setting can come from a flag or an `FSONOS_*` environment variable
//! (launchd passes the environment form; see `docs/DEPLOY.md`). The HTTP API
//! and the MCP server have no authentication of their own, so the bind guard
//! keeps them off wildcard and public addresses unless explicitly overridden.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use fsonos_api::Failure;
use fsonos_proto::Transport;
use fsonos_proto::net::Lan;

use crate::confine::Confined;

/// The seed file in the data directory (see [`GlobalArgs::seed_addrs`]).
pub const SEEDS_FILE: &str = "seeds.toml";

/// Options every command accepts (and `serve` uses for discovery).
#[derive(Debug, Clone, clap::Args)]
pub struct GlobalArgs {
    /// Seed file of player addresses for networks where SSDP multicast is
    /// unreliable; every IP address in it is tried (e.g. TOML
    /// `players = ["192.0.2.10"]`, or one per line). Discovery still runs.
    /// [default: `seeds.toml` in the data directory, when there is one]
    #[arg(long, env = "FSONOS_SEEDS", global = true)]
    pub seeds: Option<PathBuf>,

    /// A player address to try directly, in addition to SSDP (repeatable).
    #[arg(long = "seed", value_name = "IP", global = true)]
    pub seed: Vec<IpAddr>,

    /// Routes file mapping player addresses to the sockets that serve them,
    /// and an SSDP target to search instead of multicast. `fsonos sim` writes
    /// one for its virtual players; real players need none. While one is
    /// set, nothing the file does not name is contacted.
    #[arg(long, env = "FSONOS_ROUTES", global = true)]
    pub routes: Option<PathBuf>,

    /// Seconds to wait for SSDP replies.
    #[arg(long, value_name = "SECS", default_value_t = 2, global = true)]
    pub wait: u64,

    /// Print JSON instead of text.
    #[arg(long, global = true)]
    pub json: bool,

    /// Never through a running daemon (`fsonos serve`): find the
    /// speakers directly.
    #[arg(long, global = true)]
    pub direct: bool,

    /// Only through a running daemon: fail when none answers.
    #[arg(long, global = true, conflicts_with = "direct")]
    pub daemon: bool,

    /// The daemon to send commands to from another machine on the tailnet:
    /// its URL, e.g. `https://<mac>.<tailnet>.ts.net` behind Tailscale Serve
    /// or `http://<tailnet address>:8099`. Implies --daemon.
    #[arg(long, env = "FSONOS_DAEMON_URL", value_name = "URL", global = true)]
    pub daemon_url: Option<String>,

    /// Data directory for the store database, the Spotify token cache, and
    /// `policy.toml` [default: the OS per-user data directory, under
    /// `fsonos`; with --routes, `data` beside the routes file].
    #[arg(long, env = "FSONOS_DATA_DIR", global = true)]
    pub data_dir: Option<PathBuf>,
}

impl GlobalArgs {
    /// The SSDP listening window.
    #[must_use]
    pub fn wait(&self) -> Duration {
        Duration::from_secs(self.wait)
    }

    /// Every seed address: `--seed` flags, then the seed file's (`--seeds`,
    /// else the data directory's [`SEEDS_FILE`], which `fsonos setup`
    /// writes), without duplicates.
    pub fn seed_addrs(&self) -> Result<Vec<IpAddr>, Failure> {
        let mut addrs = self.seed.clone();
        let file = self.seeds.clone().or_else(|| {
            self.data_dir()
                .map(|d| d.join(SEEDS_FILE))
                .filter(|p| p.exists())
        });
        if let Some(path) = &file {
            let text = std::fs::read_to_string(path).map_err(|e| {
                Failure::invalid(format!("cannot read seed file {}: {e}", path.display()))
                    .with_hint("Fix FSONOS_SEEDS / --seeds, or drop it to rely on SSDP.")
            })?;
            addrs.extend(ip_addresses(&text));
        }
        let mut unique = Vec::new();
        for addr in addrs {
            if !unique.contains(&addr) {
                unique.push(addr);
            }
        }
        Ok(unique)
    }

    /// The `--routes` file's contents; empty when none is given.
    pub fn routes(&self) -> Result<Routes, Failure> {
        let Some(path) = &self.routes else {
            return Ok(Routes::default());
        };
        let hint = "Fix FSONOS_ROUTES / --routes (fsonos sim writes a valid one), or drop it.";
        let text = std::fs::read_to_string(path).map_err(|e| {
            Failure::invalid(format!("cannot read routes file {}: {e}", path.display()))
                .with_hint(hint)
        })?;
        Routes::parse(&text).map_err(|e| {
            Failure::invalid(format!("routes file {}: {e}", path.display())).with_hint(hint)
        })
    }

    /// The transport commands survey and control through: the LAN, or with
    /// `--routes` the LAN redirected to, and confined to, what the file names
    /// (see [`crate::confine`]).
    pub fn lan(&self) -> Result<Box<dyn Transport + Send + Sync>, Failure> {
        Ok(Box::new(self.network()?.transport))
    }

    /// The LAN (redirected by any `--routes`) and the transport over it; see
    /// [`Network`].
    pub fn network(&self) -> Result<Network, Failure> {
        let lan = Lan::start().map_err(|e| Failure::from(fsonos_core::CoreError::from(e)))?;
        if self.routes.is_none() {
            let lan = Arc::new(lan);
            return Ok(Network {
                transport: Arc::clone(&lan) as _,
                lan,
            });
        }
        let routes = self.routes()?;
        let mut lan = lan.with_routes(routes.players.clone());
        if let Some(target) = routes.ssdp {
            lan = lan.with_ssdp_target(target);
        }
        let lan = Arc::new(lan);
        Ok(Network {
            transport: Arc::new(Confined::new(Arc::clone(&lan), &routes)),
            lan,
        })
    }
}

/// How the daemon reaches the speakers.
pub struct Network {
    /// The LAN itself, for GENA: event subscriptions and the event listener.
    pub lan: Arc<Lan>,
    /// Surveys, reads and control: the LAN, confined under `--routes`.
    pub transport: Arc<dyn Transport + Send + Sync>,
}

/// Where to reach players that do not answer on `ip:1400` (a simulator's
/// loopback sockets), and where to send the SSDP search instead of multicast.
///
/// ```toml
/// ssdp = "127.0.0.1:53000"
/// [routes]
/// "192.0.2.10" = "127.0.0.1:53211"
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Routes {
    pub players: Vec<(IpAddr, SocketAddr)>,
    pub ssdp: Option<SocketAddr>,
}

impl Routes {
    /// Parse a routes file.
    pub fn parse(text: &str) -> Result<Self, String> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct File {
            ssdp: Option<SocketAddr>,
            #[serde(default)]
            routes: BTreeMap<IpAddr, SocketAddr>,
        }
        let file: File = toml::from_str(text).map_err(|e| e.to_string().trim().to_owned())?;
        Ok(Self {
            players: file.routes.into_iter().collect(),
            ssdp: file.ssdp,
        })
    }
}

/// Every IP address written in `text`, in order: tolerant of TOML arrays,
/// one-per-line lists, quotes, commas and `#` comments.
#[must_use]
pub fn ip_addresses(text: &str) -> Vec<IpAddr> {
    text.lines()
        .map(|line| line.split('#').next().unwrap_or_default())
        .flat_map(|line| line.split(|c: char| !(c.is_ascii_hexdigit() || c == '.' || c == ':')))
        .filter_map(|token| token.trim_matches(':').parse().ok())
        .collect()
}

/// Settings for the long-lived daemon.
#[derive(Debug, Clone, clap::Args)]
pub struct ServeArgs {
    /// HTTP API bind address [default: 127.0.0.1:8099 plus this host's
    /// tailnet addresses when Tailscale is up]. Setting it binds exactly this
    /// address (loopback behind Tailscale Serve, or the tailnet address).
    #[arg(long, env = "FSONOS_HTTP_ADDR")]
    pub http: Option<SocketAddr>,

    /// MCP streamable-HTTP bind address, endpoint path `/mcp` [default:
    /// 127.0.0.1:8098]. Loopback unless set: reach it from the tailnet
    /// through Tailscale Serve.
    #[arg(long, env = "FSONOS_MCP_HTTP_ADDR")]
    pub mcp_http: Option<SocketAddr>,

    /// Look for Tailscale. `off` keeps fsonos off the tailnet: unconfigured
    /// listeners bind loopback only, and the doctor skips its Tailscale
    /// checks.
    #[arg(long, env = "FSONOS_TAILSCALE", value_enum, default_value_t = TailscaleMode::Auto)]
    pub tailscale: TailscaleMode,

    /// Put Tailscale Serve in front of the daemon when it starts, as
    /// `fsonos tailscale setup` does: HTTPS on 443 (the API) and 8443 (MCP).
    /// Never Funnel. A refusal or failure is logged; the daemon serves on.
    /// The variable takes true/false, 1/0, yes/no or on/off.
    #[arg(
        long,
        env = "FSONOS_TAILSCALE_SERVE",
        value_parser = clap::builder::BoolishValueParser::new()
    )]
    pub tailscale_serve: bool,

    /// Spotify app client id (PKCE: identifies the app, not a secret).
    #[arg(long, env = "FSONOS_SPOTIFY_CLIENT_ID")]
    pub spotify_client_id: Option<String>,

    /// Spotify OAuth redirect URI; must be registered with the Spotify app.
    #[arg(long, env = "FSONOS_SPOTIFY_REDIRECT_URI", default_value = SPOTIFY_REDIRECT_URI)]
    pub spotify_redirect_uri: String,

    /// Port the players deliver their state-change events (GENA) to, on the
    /// address facing them. Fixed so a firewall rule can name it; 0 picks
    /// any free port.
    #[arg(long, env = "FSONOS_EVENTS_PORT", default_value_t = EVENTS_PORT)]
    pub events_port: u16,

    /// Allow binding the API or MCP server to a wildcard or public address.
    /// Neither has authentication: anyone who can reach it controls the
    /// speakers.
    #[arg(long)]
    pub allow_unsafe_bind: bool,
}

/// Whether `fsonos` looks for Tailscale (`--tailscale`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum TailscaleMode {
    /// Detect this host's tailnet.
    Auto,
    /// Never look: no tailnet.
    Off,
}

/// The HTTP API's port when no address is configured.
pub const HTTP_PORT: u16 = 8099;
/// `serve`'s Spotify redirect URI when none is configured.
pub const SPOTIFY_REDIRECT_URI: &str = "http://127.0.0.1:8099/auth/spotify/callback";
/// The port the players deliver events to when none is configured.
pub const EVENTS_PORT: u16 = 8097;
/// The MCP server's port when no address is configured.
pub const MCP_PORT: u16 = 8098;

impl ServeArgs {
    /// The settings `serve` takes from its environment alone (no flags), for
    /// a command that checks the daemon without taking its flags.
    pub fn from_env() -> Result<Self, clap::Error> {
        use clap::{Args as _, FromArgMatches as _};
        let matches =
            Self::augment_args(clap::Command::new("serve")).try_get_matches_from(["serve"])?;
        Self::from_arg_matches(&matches)
    }

    /// This host's tailnet, unless `--tailscale off`.
    #[must_use]
    pub fn tailnet(&self) -> fsonos_tailscale::TailnetStatus {
        match self.tailscale {
            TailscaleMode::Auto => fsonos_tailscale::detect(),
            TailscaleMode::Off => fsonos_tailscale::TailnetStatus::Unavailable(
                fsonos_tailscale::Unavailable::Disabled,
            ),
        }
    }

    /// Where this machine reaches the HTTP API: the configured address, or
    /// loopback on the default port (always among the bound addresses).
    #[must_use]
    pub fn http_local(&self) -> SocketAddr {
        self.http
            .unwrap_or(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), HTTP_PORT))
    }

    /// Where this machine reaches the MCP server (see [`Self::http_local`]).
    #[must_use]
    pub fn mcp_local(&self) -> SocketAddr {
        self.mcp_http
            .unwrap_or(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), MCP_PORT))
    }

    /// Where the HTTP API binds: the configured address, or loopback plus
    /// the tailnet when Tailscale is up.
    #[must_use]
    pub fn http_plan(
        &self,
        tailnet: &fsonos_tailscale::TailnetStatus,
    ) -> fsonos_tailscale::BindPlan {
        fsonos_tailscale::bind_plan(tailnet, HTTP_PORT, self.http)
    }

    /// Where the MCP server binds: the configured address, else loopback.
    /// Not the tailnet automatically: one MCP backend serves every MCP
    /// listener with a single caller identity, so a tailnet listener would
    /// share loopback's (full) rights. Tailscale Serve, or a configured
    /// tailnet address (whose callers are `unknown`), reaches it from the
    /// tailnet instead.
    #[must_use]
    pub fn mcp_plan(
        &self,
        tailnet: &fsonos_tailscale::TailnetStatus,
    ) -> fsonos_tailscale::BindPlan {
        if self.mcp_http.is_some() {
            return fsonos_tailscale::bind_plan(tailnet, MCP_PORT, self.mcp_http);
        }
        let loopback = self.mcp_local();
        fsonos_tailscale::BindPlan {
            addrs: vec![loopback],
            reason: fsonos_tailscale::BindReason::LoopbackOnly,
            note: format!(
                "MCP on loopback only ({loopback}); front it with `tailscale serve` to reach it \
                 from the tailnet"
            ),
        }
    }
}

/// Who can reach a listener bound to an address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindScope {
    /// This machine only.
    Loopback,
    /// The Tailscale tailnet (`100.64.0.0/10`, `fd7a:115c:a1e0::/48`).
    Tailnet,
    /// The local network (RFC 1918, link-local, IPv6 unique-local).
    Lan,
    /// Every interface (`0.0.0.0`, `::`).
    Wildcard,
    /// A publicly routable address.
    Public,
}

/// Classify the reachability of `ip`. IPv4-mapped IPv6 addresses are judged
/// as the IPv4 address they carry.
#[must_use]
pub fn bind_scope(ip: IpAddr) -> BindScope {
    match ip {
        IpAddr::V4(v4) => v4_scope(v4),
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or_else(|| v6_scope(v6), v4_scope),
    }
}

fn v4_scope(ip: Ipv4Addr) -> BindScope {
    if ip.is_unspecified() {
        BindScope::Wildcard
    } else if ip.is_loopback() {
        BindScope::Loopback
    } else if fsonos_tailscale::is_tailnet_v4(ip) {
        BindScope::Tailnet
    } else if ip.is_private() || ip.is_link_local() {
        BindScope::Lan
    } else {
        BindScope::Public
    }
}

fn v6_scope(ip: Ipv6Addr) -> BindScope {
    if ip.is_unspecified() {
        BindScope::Wildcard
    } else if ip.is_loopback() {
        BindScope::Loopback
    } else if fsonos_tailscale::is_tailnet_v6(ip) {
        BindScope::Tailnet
    } else if ip.is_unique_local() || ip.is_unicast_link_local() {
        BindScope::Lan
    } else {
        BindScope::Public
    }
}

/// This host's own tailnet addresses in `status`, IPv4 first, as the
/// Tailscale CLI reported them. None when no tailnet was found, or when it
/// was only inferred from addresses in the tailnet ranges on this host's
/// interfaces: a carrier-grade NAT address there looks the same.
#[must_use]
pub fn own_tailnet_addrs(status: &fsonos_tailscale::TailnetStatus) -> Vec<IpAddr> {
    match status {
        fsonos_tailscale::TailnetStatus::Available(t)
            if t.source == fsonos_tailscale::Source::Cli =>
        {
            let v4 = t.ipv4.iter().copied().map(IpAddr::V4);
            v4.chain(t.ipv6.iter().copied().map(IpAddr::V6)).collect()
        }
        fsonos_tailscale::TailnetStatus::Available(_)
        | fsonos_tailscale::TailnetStatus::Unavailable(_) => Vec::new(),
    }
}

/// Vet a control-surface (API/MCP) bind address; `ours` are this host's own
/// tailnet addresses ([`own_tailnet_addrs`]). `Ok(None)`: fine.
/// `Ok(Some(warning))`: allowed, but log the warning. `Err(failure)`: refuse
/// to start (`INVALID_ARGUMENT`, exit 2).
pub fn check_control_bind(
    listener: &str,
    addr: SocketAddr,
    allow_unsafe: bool,
    ours: &[IpAddr],
) -> Result<Option<String>, Failure> {
    let unauthenticated =
        "it has no authentication, so anyone who can reach it controls the speakers";
    match bind_scope(addr.ip()) {
        BindScope::Loopback => Ok(None),
        BindScope::Tailnet if ours.contains(&addr.ip().to_canonical()) => Ok(None),
        // Carrier-grade NAT (a phone hotspot, some ISPs) hands out addresses
        // from the same 100.64.0.0/10 block.
        BindScope::Tailnet => Ok(Some(format!(
            "{listener} is bound to {addr}, in Tailscale's range but not one of this \
             machine's tailnet addresses ({}); carrier-grade NAT uses the same range, and \
             {unauthenticated}",
            if ours.is_empty() {
                "Tailscale reported none".to_string()
            } else {
                ours.iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        ))),
        BindScope::Lan => Ok(Some(format!(
            "{listener} is bound to LAN address {addr}; {unauthenticated} \
             (prefer loopback behind Tailscale Serve)"
        ))),
        scope @ (BindScope::Wildcard | BindScope::Public) => {
            let what = if scope == BindScope::Wildcard {
                "every interface"
            } else {
                "a public address"
            };
            let message = format!("{listener} would bind {what} ({addr}); {unauthenticated}");
            if allow_unsafe {
                Ok(Some(message))
            } else {
                Err(Failure::invalid(message).with_hint(
                    "Bind 127.0.0.1 (behind Tailscale Serve) or the tailnet address, \
                     or pass --allow-unsafe-bind.",
                ))
            }
        }
    }
}

/// The per-user data directory `fsonos` uses when none is configured:
/// `~/Library/Application Support/fsonos` on macOS, else
/// `$XDG_DATA_HOME/fsonos` or `~/.local/share/fsonos`. `None` when no home
/// directory is known.
#[must_use]
pub fn default_data_dir(
    macos: bool,
    home: Option<&Path>,
    xdg_data_home: Option<&Path>,
) -> Option<PathBuf> {
    if macos {
        return home.map(|h| h.join("Library/Application Support/fsonos"));
    }
    match xdg_data_home.filter(|p| p.is_absolute()) {
        Some(xdg) => Some(xdg.join("fsonos")),
        None => home.map(|h| h.join(".local/share/fsonos")),
    }
}

impl GlobalArgs {
    /// The configured data directory, or the per-user default for this OS.
    /// A command confined to a simulator (`--routes`) keeps its own, `data`
    /// beside the routes file, so nothing it saves (a schedule, a scene, an
    /// action to undo) reaches the real house's store.
    #[must_use]
    pub fn data_dir(&self) -> Option<PathBuf> {
        self.data_dir.clone().or_else(|| {
            if let Some(routes) = &self.routes {
                return Some(routes.parent().unwrap_or(Path::new(".")).join("data"));
            }
            let home = std::env::var_os("HOME").map(PathBuf::from);
            let xdg = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from);
            default_data_dir(cfg!(target_os = "macos"), home.as_deref(), xdg.as_deref())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{Args, Parser};

    #[derive(Parser)]
    struct Harness {
        #[command(flatten)]
        serve: ServeArgs,
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn classifies_bind_scopes() {
        for (addr, want) in [
            ("127.0.0.1", BindScope::Loopback),
            ("127.8.9.10", BindScope::Loopback),
            ("::1", BindScope::Loopback),
            ("::ffff:127.0.0.1", BindScope::Loopback),
            ("100.64.0.1", BindScope::Tailnet),
            ("100.127.255.254", BindScope::Tailnet),
            ("fd7a:115c:a1e0::1", BindScope::Tailnet),
            ("192.168.1.20", BindScope::Lan),
            ("10.0.0.5", BindScope::Lan),
            ("172.31.0.1", BindScope::Lan),
            ("169.254.3.4", BindScope::Lan),
            ("fd00::1", BindScope::Lan),
            ("fe80::1", BindScope::Lan),
            ("0.0.0.0", BindScope::Wildcard),
            ("::", BindScope::Wildcard),
            ("100.63.255.255", BindScope::Public),
            ("100.128.0.0", BindScope::Public),
            ("172.32.0.1", BindScope::Public),
            ("8.8.8.8", BindScope::Public),
            ("2001:db8::1", BindScope::Public),
        ] {
            assert_eq!(bind_scope(ip(addr)), want, "{addr}");
        }
    }

    #[test]
    fn bind_guard_refuses_wildcard_and_public_by_default() {
        let at = |s: &str| s.parse::<SocketAddr>().unwrap();
        assert_eq!(
            check_control_bind("api", at("127.0.0.1:8099"), false, &[]),
            Ok(None)
        );
        let lan = check_control_bind("api", at("192.168.1.9:8099"), false, &[]).unwrap();
        assert!(lan.unwrap().contains("LAN address"));
        let wild = check_control_bind("mcp", at("0.0.0.0:8098"), false, &[]).unwrap_err();
        assert!(wild.detail.contains("every interface"), "{wild}");
        assert!(wild.hint.contains("--allow-unsafe-bind"));
        assert_eq!(wild.exit_code(), 2);
        let public = check_control_bind("mcp", at("[2001:db8::1]:8098"), false, &[]).unwrap_err();
        assert!(public.detail.contains("public address"));
        let forced = check_control_bind("mcp", at("0.0.0.0:8098"), true, &[]).unwrap();
        assert!(forced.unwrap().contains("no authentication"));
    }

    #[test]
    fn a_tailnet_range_address_passes_only_when_it_is_this_hosts_own() {
        // Synthetic addresses from the 100.64.0.0/10 block.
        let ours = IpAddr::from([100, 64, 0, 7]);
        let other = SocketAddr::from(([100, 64, 0, 8], 8099));
        let own = SocketAddr::new(ours, 8099);
        assert_eq!(check_control_bind("api", own, false, &[ours]), Ok(None));
        // Its IPv4-mapped IPv6 form is the same address.
        let mapped = SocketAddr::new(
            IpAddr::V6(Ipv4Addr::from([100, 64, 0, 7]).to_ipv6_mapped()),
            8099,
        );
        assert_eq!(check_control_bind("api", mapped, false, &[ours]), Ok(None));
        // Another address in the block, or any with no tailnet found: allowed,
        // with a warning that names the host's own.
        let stray = check_control_bind("api", other, false, &[ours])
            .unwrap()
            .unwrap();
        assert!(
            stray.contains("not one of this machine's tailnet addresses"),
            "{stray}"
        );
        assert!(stray.contains(&ours.to_string()), "{stray}");
        assert!(stray.contains("carrier-grade NAT"), "{stray}");
        let unknown = check_control_bind("mcp", own, false, &[]).unwrap().unwrap();
        assert!(unknown.contains("(Tailscale reported none)"), "{unknown}");
    }

    #[test]
    fn own_tailnet_addrs_are_the_ones_the_tailscale_cli_reported() {
        use fsonos_tailscale::{Source, Tailnet, TailnetStatus, Unavailable};
        let v4 = Ipv4Addr::from([100, 64, 0, 7]);
        let v6: Ipv6Addr = "fd7a:115c:a1e0::7".parse().unwrap();
        let up = |source| {
            TailnetStatus::Available(Tailnet {
                source,
                backend_state: None,
                running: true,
                logged_in: true,
                ipv4: vec![v4],
                ipv6: vec![v6],
                magic_dns_name: None,
                tailnet: None,
            })
        };
        assert_eq!(
            own_tailnet_addrs(&up(Source::Cli)),
            [IpAddr::V4(v4), IpAddr::V6(v6)]
        );
        // Found only on an interface: a carrier-grade NAT address looks the
        // same, so it is not counted as this host's tailnet address.
        assert!(own_tailnet_addrs(&up(Source::Interfaces)).is_empty());
        let off = TailnetStatus::Unavailable(Unavailable::Disabled);
        assert!(own_tailnet_addrs(&off).is_empty());
    }

    #[test]
    fn default_data_dir_follows_the_platform() {
        let home = Path::new("/home/u");
        assert_eq!(
            default_data_dir(true, Some(home), None).unwrap(),
            Path::new("/home/u/Library/Application Support/fsonos")
        );
        assert_eq!(
            default_data_dir(false, Some(home), Some(Path::new("/xdg"))).unwrap(),
            Path::new("/xdg/fsonos")
        );
        // A relative XDG_DATA_HOME is invalid per the spec and ignored.
        assert_eq!(
            default_data_dir(false, Some(home), Some(Path::new("rel"))).unwrap(),
            Path::new("/home/u/.local/share/fsonos")
        );
        assert_eq!(default_data_dir(false, None, None), None);
    }

    #[test]
    fn defaults_are_loopback() {
        let h = Harness::try_parse_from(["fsonos"]).unwrap();
        assert_eq!((h.serve.http, h.serve.mcp_http), (None, None));
        assert_eq!(h.serve.http_local(), "127.0.0.1:8099".parse().unwrap());
        assert_eq!(h.serve.mcp_local(), "127.0.0.1:8098".parse().unwrap());
        assert_eq!(bind_scope(h.serve.http_local().ip()), BindScope::Loopback);
        assert!(!h.serve.allow_unsafe_bind);
        // Unconfigured: loopback alone off a tailnet, plus the tailnet on one.
        let off = fsonos_tailscale::TailnetStatus::Unavailable(
            fsonos_tailscale::Unavailable::NotInstalled,
        );
        assert_eq!(
            h.serve.http_plan(&off).addrs,
            ["127.0.0.1:8099".parse::<SocketAddr>().unwrap()]
        );
        let on = fsonos_tailscale::from_addresses(["100.70.1.2".parse().unwrap()]).unwrap();
        let on = fsonos_tailscale::TailnetStatus::Available(on);
        assert_eq!(
            h.serve.http_plan(&on).addrs,
            [
                "127.0.0.1:8099".parse::<SocketAddr>().unwrap(),
                "100.70.1.2:8099".parse().unwrap()
            ]
        );
        // MCP never joins the tailnet on its own (one shared caller identity).
        assert_eq!(
            h.serve.mcp_plan(&on).addrs,
            ["127.0.0.1:8098".parse::<SocketAddr>().unwrap()]
        );
        let h = Harness::try_parse_from(["fsonos", "--http", "100.70.1.2:9000"]).unwrap();
        assert_eq!(h.serve.http, Some("100.70.1.2:9000".parse().unwrap()));
        // A configured address is bound alone, even on a tailnet.
        assert_eq!(
            h.serve.http_plan(&on).addrs,
            ["100.70.1.2:9000".parse::<SocketAddr>().unwrap()]
        );
        assert!(Harness::try_parse_from(["fsonos", "--http", "not-an-addr"]).is_err());
    }

    #[test]
    fn tailscale_off_means_no_tailnet() {
        let auto = Harness::try_parse_from(["fsonos"]).unwrap();
        assert_eq!(auto.serve.tailscale, TailscaleMode::Auto);
        let h = Harness::try_parse_from(["fsonos", "--tailscale", "off"]).unwrap();
        assert_eq!(h.serve.tailscale, TailscaleMode::Off);
        let status = h.serve.tailnet();
        assert_eq!(
            status,
            fsonos_tailscale::TailnetStatus::Unavailable(fsonos_tailscale::Unavailable::Disabled)
        );
        let plan = h.serve.http_plan(&status);
        assert_eq!(plan.reason, fsonos_tailscale::BindReason::LoopbackOnly);
        assert!(plan.note.contains("FSONOS_TAILSCALE=off"), "{}", plan.note);
        assert!(Harness::try_parse_from(["fsonos", "--tailscale", "maybe"]).is_err());
    }

    #[test]
    fn seed_files_yield_every_address() {
        let toml =
            "# seeds\nplayers = [\"192.0.2.10\", \"192.0.2.11\"] # two\nv6 = [\"fd00::1\"]\n";
        assert_eq!(
            ip_addresses(toml),
            ["192.0.2.10", "192.0.2.11", "fd00::1"].map(|a| a.parse::<IpAddr>().unwrap())
        );
        let lines = "192.0.2.20\n\n  192.0.2.21  # den\nnot-an-ip\n";
        assert_eq!(ip_addresses(lines).len(), 2);
        assert_eq!(ip_addresses("players = []"), Vec::<IpAddr>::new());
    }

    #[test]
    fn seeds_merge_flags_and_file_without_duplicates() {
        let dir = std::env::temp_dir().join(format!("fsonos-seeds-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("seeds.toml");
        std::fs::write(&file, "players = [\"192.0.2.10\", \"192.0.2.12\"]").unwrap();
        let g = GlobalHarness::try_parse_from([
            "fsonos",
            "--seed",
            "192.0.2.10",
            "--seeds",
            file.to_str().unwrap(),
        ])
        .unwrap()
        .global;
        assert_eq!(
            g.seed_addrs().unwrap(),
            ["192.0.2.10", "192.0.2.12"].map(|a| a.parse::<IpAddr>().unwrap())
        );
        let missing = GlobalHarness::try_parse_from(["fsonos", "--seeds", "/nonexistent/seeds"])
            .unwrap()
            .global
            .seed_addrs()
            .unwrap_err();
        assert_eq!(missing.exit_code(), 2);
        assert!(missing.detail.contains("cannot read seed file"));
    }

    #[test]
    fn a_simulator_command_keeps_its_own_data_directory() {
        let routes = std::env::temp_dir()
            .join("fsonos-sim-dd")
            .join("routes.toml");
        let routes = routes.to_str().unwrap();
        let confined = GlobalHarness::try_parse_from(["fsonos", "--routes", routes])
            .unwrap()
            .global;
        assert_eq!(
            confined.data_dir(),
            Some(std::env::temp_dir().join("fsonos-sim-dd").join("data"))
        );
        let named = GlobalHarness::try_parse_from([
            "fsonos",
            "--routes",
            routes,
            "--data-dir",
            "/srv/fsonos",
        ])
        .unwrap()
        .global;
        assert_eq!(named.data_dir(), Some(PathBuf::from("/srv/fsonos")));
    }

    #[test]
    fn the_seed_file_setup_writes_is_read_unless_another_is_named() {
        let dir = std::env::temp_dir().join(format!("fsonos-seeds-dd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let data = dir.to_str().unwrap();
        let none = GlobalHarness::try_parse_from(["fsonos", "--data-dir", data])
            .unwrap()
            .global;
        assert_eq!(none.seed_addrs().unwrap(), Vec::<IpAddr>::new());
        std::fs::write(dir.join(SEEDS_FILE), "players = [\"192.0.2.10\"]").unwrap();
        let addrs = |g: &GlobalArgs| g.seed_addrs().unwrap();
        assert_eq!(addrs(&none), ["192.0.2.10".parse::<IpAddr>().unwrap()]);
        let other = dir.join("other.toml");
        std::fs::write(&other, "players = [\"192.0.2.12\"]").unwrap();
        let named = GlobalHarness::try_parse_from([
            "fsonos",
            "--data-dir",
            data,
            "--seeds",
            other.to_str().unwrap(),
        ])
        .unwrap()
        .global;
        assert_eq!(addrs(&named), ["192.0.2.12".parse::<IpAddr>().unwrap()]);
    }

    #[test]
    fn routes_files_parse_and_reject_typos() {
        let routes = Routes::parse(
            "# sim\nssdp = \"127.0.0.1:53000\"\n\n[routes]\n\
             \"192.0.2.11\" = \"127.0.0.1:53212\"\n\"192.0.2.10\" = \"127.0.0.1:53211\"\n",
        )
        .unwrap();
        assert_eq!(routes.ssdp, Some("127.0.0.1:53000".parse().unwrap()));
        assert_eq!(
            routes.players,
            [
                (
                    "192.0.2.10".parse().unwrap(),
                    "127.0.0.1:53211".parse().unwrap()
                ),
                (
                    "192.0.2.11".parse().unwrap(),
                    "127.0.0.1:53212".parse().unwrap()
                ),
            ]
        );
        assert_eq!(Routes::parse("").unwrap(), Routes::default());
        assert!(Routes::parse("[routes]\n\"192.0.2.10\" = \"nowhere\"\n").is_err());
        assert!(Routes::parse("sdp = \"127.0.0.1:1\"\n").is_err());

        let missing = GlobalHarness::try_parse_from(["fsonos", "--routes", "/nonexistent/routes"])
            .unwrap()
            .global
            .routes()
            .unwrap_err();
        assert_eq!(missing.exit_code(), 2);
        assert!(missing.detail.contains("cannot read routes file"));
        let none = GlobalHarness::try_parse_from(["fsonos"]).unwrap().global;
        assert_eq!(none.routes().unwrap(), Routes::default());
    }

    #[derive(Parser)]
    struct GlobalHarness {
        #[command(flatten)]
        global: GlobalArgs,
    }

    /// Every `FSONOS_*` token in `text`.
    fn fsonos_vars(text: &str) -> Vec<String> {
        let mut vars = Vec::new();
        let mut rest = text;
        while let Some(at) = rest.find("FSONOS_") {
            let tail = &rest[at..];
            let len = tail
                .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
                .unwrap_or(tail.len());
            vars.push(tail[..len].to_string());
            rest = &tail[len..];
        }
        vars
    }

    /// The deploy docs, the launchd template and `.env.example` must name only
    /// settings `serve` actually reads, and `DEPLOY.md` must document all of
    /// them.
    #[test]
    fn deploy_docs_match_the_declared_settings() {
        let cmd = GlobalArgs::augment_args(ServeArgs::augment_args(clap::Command::new("serve")));
        let declared: Vec<String> = cmd
            .get_arguments()
            .filter_map(clap::Arg::get_env)
            .map(|e| e.to_string_lossy().into_owned())
            .collect();
        let deploy = include_str!("../../../docs/DEPLOY.md");
        for (file, text) in [
            ("docs/DEPLOY.md", deploy),
            (
                "docs/launchd/io.github.dicklesworthstone.fsonos.plist",
                include_str!("../../../docs/launchd/io.github.dicklesworthstone.fsonos.plist"),
            ),
            (".env.example", include_str!("../../../.env.example")),
        ] {
            for var in fsonos_vars(text) {
                assert!(
                    declared.contains(&var),
                    "{file} names {var}, which `fsonos serve` does not read"
                );
            }
        }
        for var in &declared {
            assert!(
                deploy.contains(var.as_str()),
                "docs/DEPLOY.md does not document {var}"
            );
        }
    }
}
