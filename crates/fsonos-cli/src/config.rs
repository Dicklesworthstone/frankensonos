//! `fsonos serve` configuration: listener addresses, local paths, and the bind
//! guard.
//!
//! Every setting can come from a flag or an `FSONOS_*` environment variable
//! (launchd passes the environment form; see `docs/DEPLOY.md`). The HTTP API
//! and the MCP server have no authentication of their own, so the bind guard
//! keeps them off wildcard and public addresses unless explicitly overridden.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};

/// Settings for the long-lived daemon.
#[derive(Debug, Clone, clap::Args)]
pub struct ServeArgs {
    /// HTTP API bind address. Keep it on loopback (or the tailnet address).
    #[arg(long, env = "FSONOS_HTTP_ADDR", default_value = "127.0.0.1:8099")]
    pub http: SocketAddr,

    /// MCP streamable-HTTP bind address (endpoint path `/mcp`).
    #[arg(long, env = "FSONOS_MCP_HTTP_ADDR", default_value = "127.0.0.1:8098")]
    pub mcp_http: SocketAddr,

    /// Data directory for the store database and the Spotify token cache
    /// [default: the OS per-user data directory, under `fsonos`].
    #[arg(long, env = "FSONOS_DATA_DIR")]
    pub data_dir: Option<PathBuf>,

    /// Direct-seed list (TOML of player IPs) for networks where SSDP
    /// multicast is unreliable. Discovery still runs.
    #[arg(long, env = "FSONOS_SEEDS")]
    pub seeds: Option<PathBuf>,

    /// Spotify app client id (PKCE: identifies the app, not a secret).
    #[arg(long, env = "FSONOS_SPOTIFY_CLIENT_ID")]
    pub spotify_client_id: Option<String>,

    /// Spotify OAuth redirect URI; must be registered with the Spotify app.
    #[arg(
        long,
        env = "FSONOS_SPOTIFY_REDIRECT_URI",
        default_value = "http://127.0.0.1:8099/auth/spotify/callback"
    )]
    pub spotify_redirect_uri: String,

    /// Allow binding the API or MCP server to a wildcard or public address.
    /// Neither has authentication: anyone who can reach it controls the
    /// speakers.
    #[arg(long)]
    pub allow_unsafe_bind: bool,
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
    let [a, b, ..] = ip.octets();
    if ip.is_unspecified() {
        BindScope::Wildcard
    } else if ip.is_loopback() {
        BindScope::Loopback
    } else if a == 100 && (64..128).contains(&b) {
        BindScope::Tailnet
    } else if ip.is_private() || ip.is_link_local() {
        BindScope::Lan
    } else {
        BindScope::Public
    }
}

fn v6_scope(ip: Ipv6Addr) -> BindScope {
    const TAILNET: [u16; 3] = [0xfd7a, 0x115c, 0xa1e0];
    if ip.is_unspecified() {
        BindScope::Wildcard
    } else if ip.is_loopback() {
        BindScope::Loopback
    } else if ip.segments()[..3] == TAILNET {
        BindScope::Tailnet
    } else if ip.is_unique_local() || ip.is_unicast_link_local() {
        BindScope::Lan
    } else {
        BindScope::Public
    }
}

/// Vet a control-surface (API/MCP) bind address. `Ok(None)`: fine.
/// `Ok(Some(warning))`: allowed, but log the warning. `Err(reason)`: refuse
/// to start.
pub fn check_control_bind(
    listener: &str,
    addr: SocketAddr,
    allow_unsafe: bool,
) -> Result<Option<String>, String> {
    let unauthenticated =
        "it has no authentication, so anyone who can reach it controls the speakers";
    match bind_scope(addr.ip()) {
        BindScope::Loopback | BindScope::Tailnet => Ok(None),
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
                Err(format!(
                    "{message}. Bind 127.0.0.1 or the tailnet address instead, \
                     or pass --allow-unsafe-bind"
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

impl ServeArgs {
    /// The configured data directory, or the per-user default for this OS.
    #[must_use]
    pub fn data_dir(&self) -> Option<PathBuf> {
        self.data_dir.clone().or_else(|| {
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
            check_control_bind("api", at("127.0.0.1:8099"), false),
            Ok(None)
        );
        assert_eq!(
            check_control_bind("api", at("100.70.1.2:8099"), false),
            Ok(None)
        );
        let lan = check_control_bind("api", at("192.168.1.9:8099"), false).unwrap();
        assert!(lan.unwrap().contains("LAN address"));
        let wild = check_control_bind("mcp", at("0.0.0.0:8098"), false).unwrap_err();
        assert!(wild.contains("every interface") && wild.contains("--allow-unsafe-bind"));
        let public = check_control_bind("mcp", at("[2001:db8::1]:8098"), false).unwrap_err();
        assert!(public.contains("public address"));
        let forced = check_control_bind("mcp", at("0.0.0.0:8098"), true).unwrap();
        assert!(forced.unwrap().contains("no authentication"));
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
        assert_eq!(bind_scope(h.serve.http.ip()), BindScope::Loopback);
        assert_eq!(bind_scope(h.serve.mcp_http.ip()), BindScope::Loopback);
        assert!(!h.serve.allow_unsafe_bind);
        let h = Harness::try_parse_from(["fsonos", "--http", "100.70.1.2:9000"]).unwrap();
        assert_eq!(h.serve.http, "100.70.1.2:9000".parse().unwrap());
        assert!(Harness::try_parse_from(["fsonos", "--http", "not-an-addr"]).is_err());
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
        let cmd = ServeArgs::augment_args(clap::Command::new("serve"));
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
