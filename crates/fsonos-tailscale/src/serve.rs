//! Tailscale Serve in front of the daemon: `https://<host>.<tailnet>.ts.net/`
//! for the HTTP API and `https://<host>.<tailnet>.ts.net:8443/mcp` for MCP,
//! with certificates Tailscale manages, reachable from the tailnet only.
//!
//! This module decides; [`crate::Probe`] runs. It reads Serve's current
//! config (`tailscale serve status --json`, [`parse_serve_config`]), says what
//! each port does today ([`ServeConfig::port`]), plans a setup or teardown
//! ([`plan_setup`], [`plan_teardown`]) and builds each step's `tailscale`
//! argv ([`setup_argv`], [`teardown_argv`]).
//!
//! **Funnel is refused.** Funnel publishes a Serve port to the public
//! internet, and the daemon's API and MCP server have no authentication of
//! their own: tailnet-only is the whole point. Setup refuses while Funnel is
//! on for a port it would use, never turns Funnel on, and [`Mapping`] status
//! flags Funnel loudly wherever it finds it on the daemon's ports.
//!
//! Setup never replaces Serve config it did not make: a port already serving
//! something else is reported, not overwritten. Teardown removes only the
//! mappings that point at the daemon.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

/// The parts of Tailscale's Serve config (`ipn.ServeConfig`) that setup
/// reads. Unknown fields (services, foreground sessions) are ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ServeConfig {
    /// Listeners by port.
    #[serde(rename = "TCP", default)]
    pub tcp: BTreeMap<u16, TcpHandler>,
    /// HTTP(S) handlers by `host:port`.
    #[serde(rename = "Web", default)]
    pub web: BTreeMap<String, WebServer>,
    /// `host:port`s published to the internet with Funnel.
    #[serde(rename = "AllowFunnel", default)]
    pub allow_funnel: BTreeMap<String, bool>,
}

/// What a Serve port listens as.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct TcpHandler {
    #[serde(rename = "HTTPS", default)]
    pub https: bool,
    #[serde(rename = "HTTP", default)]
    pub http: bool,
    /// Raw TCP forwarding target, instead of HTTP handlers.
    #[serde(rename = "TCPForward", default)]
    pub tcp_forward: Option<String>,
}

/// The HTTP handlers of one `host:port`, by mount point.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct WebServer {
    #[serde(rename = "Handlers", default)]
    pub handlers: BTreeMap<String, Handler>,
}

/// One mount point: a reverse proxy, a file path, or static text.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Handler {
    #[serde(rename = "Proxy", default)]
    pub proxy: Option<String>,
    #[serde(rename = "Path", default)]
    pub path: Option<String>,
    #[serde(rename = "Text", default)]
    pub text: Option<String>,
}

/// Parse `tailscale serve status --json` (`{}` when nothing is served).
///
/// # Errors
/// When the output is not Serve's JSON.
pub fn parse_serve_config(json: &[u8]) -> Result<ServeConfig, serde_json::Error> {
    serde_json::from_slice(json)
}

/// What Serve does on one HTTPS port today.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PortState {
    /// Nothing.
    Free,
    /// HTTPS reverse proxy of the whole site (`/`) to `target`.
    Proxy { target: String },
    /// Anything else (plain HTTP, a TCP forward, files, several mounts).
    Other { what: String },
}

impl fmt::Display for PortState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Free => f.write_str("nothing"),
            Self::Proxy { target } => write!(f, "a proxy to {target}"),
            Self::Other { what } => f.write_str(what),
        }
    }
}

/// The port of a `host:port` key.
fn port_of(host_port: &str) -> Option<u16> {
    host_port.rsplit_once(':')?.1.parse().ok()
}

impl ServeConfig {
    /// What Serve does on `port` today.
    #[must_use]
    pub fn port(&self, port: u16) -> PortState {
        let tcp = self.tcp.get(&port);
        let web: Vec<&WebServer> = self
            .web
            .iter()
            .filter(|(host_port, _)| port_of(host_port) == Some(port))
            .map(|(_, server)| server)
            .collect();
        match (tcp, web.as_slice()) {
            (None, []) => PortState::Free,
            (Some(tcp), _) if tcp.tcp_forward.is_some() => PortState::Other {
                what: format!(
                    "a TCP forward to {}",
                    tcp.tcp_forward.as_deref().unwrap_or_default()
                ),
            },
            (Some(tcp), [server]) if tcp.https => root_proxy(server).map_or_else(
                || PortState::Other {
                    what: format!(
                        "HTTPS handlers on {}",
                        server
                            .handlers
                            .keys()
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                },
                |target| PortState::Proxy { target },
            ),
            (Some(tcp), _) if tcp.http => PortState::Other {
                what: "plain HTTP".into(),
            },
            _ => PortState::Other {
                what: "a Serve config fsonos does not recognize".into(),
            },
        }
    }

    /// Whether Funnel publishes `port` to the internet (for any host name).
    #[must_use]
    pub fn funnel(&self, port: u16) -> bool {
        self.allow_funnel
            .iter()
            .any(|(host_port, on)| *on && port_of(host_port) == Some(port))
    }
}

/// The target of a server whose only mount is a reverse proxy at `/`.
fn root_proxy(server: &WebServer) -> Option<String> {
    let mut handlers = server.handlers.iter();
    if let (Some((mount, handler)), None) = (handlers.next(), handlers.next())
        && mount == "/"
        && handler.path.is_none()
        && handler.text.is_none()
    {
        handler.proxy.clone()
    } else {
        None
    }
}

/// One HTTPS port of this host that Serve proxies to a local listener.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Mapping {
    /// What it fronts, for messages ("HTTP API", "MCP server").
    pub label: String,
    pub https_port: u16,
    /// Where Serve proxies to, e.g. `http://127.0.0.1:8099`.
    pub target: String,
}

/// Whether two proxy targets are the same listener (ignoring a trailing `/`).
fn same_target(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

/// `tailscale serve --bg --https=<port> <target>`.
#[must_use]
pub fn setup_argv(mapping: &Mapping) -> Vec<String> {
    vec![
        "serve".into(),
        "--bg".into(),
        format!("--https={}", mapping.https_port),
        mapping.target.clone(),
    ]
}

/// `tailscale serve --https=<port> off`.
#[must_use]
pub fn teardown_argv(port: u16) -> Vec<String> {
    vec!["serve".into(), format!("--https={port}"), "off".into()]
}

/// One step of a setup or teardown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Step {
    /// Already as wanted: nothing to run.
    Keep { mapping: Mapping },
    /// Run `tailscale <argv>`.
    Run { mapping: Mapping, argv: Vec<String> },
    /// Leave alone: not the daemon's (teardown only).
    Leave {
        mapping: Mapping,
        current: PortState,
    },
}

/// Why setup will not touch Serve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "refusal", rename_all = "snake_case")]
pub enum Refusal {
    /// Funnel is on for a port setup would use.
    Funnel { port: u16 },
    /// The port already serves something that is not the daemon.
    PortTaken {
        mapping: Mapping,
        current: PortState,
    },
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Funnel { port } => write!(
                f,
                "Funnel is ON for port {port}: it publishes that port to the public internet, \
                 and the FrankenSonos API and MCP server have no authentication of their own. \
                 FrankenSonos never uses Funnel. Turn it off first: \
                 `tailscale funnel --https={port} off`"
            ),
            Self::PortTaken { mapping, current } => write!(
                f,
                "port {} already serves {current}, not the {} ({}); setup does not replace \
                 Serve config it did not make. Free the port (`tailscale serve --https={} off`) \
                 or choose another",
                mapping.https_port, mapping.label, mapping.target, mapping.https_port
            ),
        }
    }
}

/// What setup would do to put `wanted` in place, or why it will not.
///
/// # Errors
/// A [`Refusal`] when Funnel is on for a wanted port, or a port already
/// serves something else.
pub fn plan_setup(config: &ServeConfig, wanted: &[Mapping]) -> Result<Vec<Step>, Refusal> {
    if let Some(m) = wanted.iter().find(|m| config.funnel(m.https_port)) {
        return Err(Refusal::Funnel { port: m.https_port });
    }
    wanted
        .iter()
        .map(|m| match config.port(m.https_port) {
            PortState::Free => Ok(Step::Run {
                argv: setup_argv(m),
                mapping: m.clone(),
            }),
            PortState::Proxy { target } if same_target(&target, &m.target) => {
                Ok(Step::Keep { mapping: m.clone() })
            }
            current => Err(Refusal::PortTaken {
                mapping: m.clone(),
                current,
            }),
        })
        .collect()
}

/// What teardown would remove: only the `ours` mappings Serve still has.
#[must_use]
pub fn plan_teardown(config: &ServeConfig, ours: &[Mapping]) -> Vec<Step> {
    ours.iter()
        .map(|m| match config.port(m.https_port) {
            PortState::Proxy { target } if same_target(&target, &m.target) => Step::Run {
                argv: teardown_argv(m.https_port),
                mapping: m.clone(),
            },
            PortState::Free => Step::Keep { mapping: m.clone() },
            current => Step::Leave {
                mapping: m.clone(),
                current,
            },
        })
        .collect()
}

/// One of the daemon's mappings as Serve has it now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MappingStatus {
    pub mapping: Mapping,
    pub current: PortState,
    /// In place: Serve proxies this port to the daemon.
    pub active: bool,
    /// Funnel is on for this port.
    pub funnel: bool,
}

/// How each of `ours` stands in `config`.
#[must_use]
pub fn status(config: &ServeConfig, ours: &[Mapping]) -> Vec<MappingStatus> {
    ours.iter()
        .map(|m| {
            let current = config.port(m.https_port);
            MappingStatus {
                active: matches!(&current, PortState::Proxy { target } if same_target(target, &m.target)),
                funnel: config.funnel(m.https_port),
                mapping: m.clone(),
                current,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST: &str = "sonos-host.example-tailnet.ts.net";

    fn api() -> Mapping {
        Mapping {
            label: "HTTP API".into(),
            https_port: 443,
            target: "http://127.0.0.1:8099".into(),
        }
    }

    fn mcp() -> Mapping {
        Mapping {
            label: "MCP server".into(),
            https_port: 8443,
            target: "http://127.0.0.1:8098".into(),
        }
    }

    fn config(json: &str) -> ServeConfig {
        parse_serve_config(json.replace("HOST", HOST).as_bytes()).unwrap()
    }

    /// Serve with the daemon's API mapping on 443 (and `extra` merged in).
    const OURS_443: &str = r#"{
        "TCP": {"443": {"HTTPS": true}},
        "Web": {"HOST:443": {"Handlers": {"/": {"Proxy": "http://127.0.0.1:8099"}}}}
    }"#;

    #[test]
    fn parses_what_serve_status_prints() {
        assert_eq!(config("{}"), ServeConfig::default());
        let c = config(
            r#"{
            "TCP": {"443": {"HTTPS": true}, "8443": {"HTTPS": true}, "2222": {"TCPForward": "127.0.0.1:22"}},
            "Web": {
                "HOST:443": {"Handlers": {"/": {"Proxy": "http://127.0.0.1:8099"}}},
                "HOST:8443": {"Handlers": {"/": {"Proxy": "http://127.0.0.1:8098"}, "/files": {"Path": "/tmp/x"}}}
            },
            "AllowFunnel": {"HOST:8443": true},
            "Foreground": {}, "Services": {}
        }"#,
        );
        assert_eq!(
            c.port(443),
            PortState::Proxy {
                target: "http://127.0.0.1:8099".into()
            }
        );
        assert!(
            matches!(c.port(8443), PortState::Other { .. }),
            "{:?}",
            c.port(8443)
        );
        assert_eq!(
            c.port(2222),
            PortState::Other {
                what: "a TCP forward to 127.0.0.1:22".into()
            }
        );
        assert_eq!(c.port(10000), PortState::Free);
        assert!(c.funnel(8443) && !c.funnel(443));
        assert!(parse_serve_config(b"not json").is_err());
    }

    #[test]
    fn setup_adds_what_is_missing_and_keeps_what_is_there() {
        let steps = plan_setup(&ServeConfig::default(), &[api(), mcp()]).unwrap();
        assert_eq!(
            steps,
            [
                Step::Run {
                    mapping: api(),
                    argv: vec![
                        "serve".into(),
                        "--bg".into(),
                        "--https=443".into(),
                        "http://127.0.0.1:8099".into()
                    ]
                },
                Step::Run {
                    mapping: mcp(),
                    argv: vec![
                        "serve".into(),
                        "--bg".into(),
                        "--https=8443".into(),
                        "http://127.0.0.1:8098".into()
                    ]
                },
            ]
        );
        // Idempotent: what is already in place is kept, not re-run.
        let steps = plan_setup(&config(OURS_443), &[api(), mcp()]).unwrap();
        assert_eq!(steps[0], Step::Keep { mapping: api() });
        assert!(matches!(&steps[1], Step::Run { mapping, .. } if *mapping == mcp()));
        // A trailing slash is the same target.
        let slash = config(&OURS_443.replace("8099\"", "8099/\""));
        assert_eq!(
            plan_setup(&slash, &[api()]).unwrap(),
            [Step::Keep { mapping: api() }]
        );
    }

    #[test]
    fn setup_refuses_funnel_loudly() {
        let funnel = config(r#"{"AllowFunnel": {"HOST:443": true}}"#);
        let refusal = plan_setup(&funnel, &[api(), mcp()]).unwrap_err();
        assert_eq!(refusal, Refusal::Funnel { port: 443 });
        let message = refusal.to_string();
        eprintln!("funnel: {message}");
        assert!(message.contains("public internet"), "{message}");
        assert!(
            message.contains("tailscale funnel --https=443 off"),
            "{message}"
        );
        // Funnel switched off in the config map is not Funnel.
        let off = config(r#"{"AllowFunnel": {"HOST:443": false}}"#);
        assert!(plan_setup(&off, &[api()]).is_ok());
    }

    #[test]
    fn setup_never_replaces_someone_elses_serve_config() {
        let other = config(&OURS_443.replace("127.0.0.1:8099", "127.0.0.1:3000"));
        let refusal = plan_setup(&other, &[api()]).unwrap_err();
        let message = refusal.to_string();
        eprintln!("taken: {message}");
        assert_eq!(
            refusal,
            Refusal::PortTaken {
                mapping: api(),
                current: PortState::Proxy {
                    target: "http://127.0.0.1:3000".into()
                }
            }
        );
        assert!(
            message.contains("tailscale serve --https=443 off"),
            "{message}"
        );
        let http = config(r#"{"TCP": {"443": {"HTTP": true}}}"#);
        assert!(matches!(
            plan_setup(&http, &[api()]),
            Err(Refusal::PortTaken { .. })
        ));
    }

    #[test]
    fn teardown_removes_only_the_daemons_mappings() {
        let both = config(&OURS_443.replace(
            r#""TCP": {"443": {"HTTPS": true}}"#,
            r#""TCP": {"443": {"HTTPS": true}, "8443": {"HTTPS": true}}"#,
        ).replace(
            r#""Web": {"#,
            r#""Web": {"HOST:8443": {"Handlers": {"/": {"Proxy": "http://127.0.0.1:3000"}}}, "#,
        ));
        let steps = plan_teardown(&both, &[api(), mcp()]);
        assert_eq!(
            steps[0],
            Step::Run {
                mapping: api(),
                argv: vec!["serve".into(), "--https=443".into(), "off".into()]
            }
        );
        assert!(matches!(&steps[1], Step::Leave { mapping, .. } if *mapping == mcp()));
        // Nothing there: nothing to do.
        assert_eq!(
            plan_teardown(&ServeConfig::default(), &[api()]),
            [Step::Keep { mapping: api() }]
        );
    }

    #[test]
    fn status_says_what_is_active_and_flags_funnel() {
        let funnel = config(&OURS_443.replace(
            "\n    }",
            r#", "AllowFunnel": {"HOST:443": true}
    }"#,
        ));
        let s = status(&funnel, &[api(), mcp()]);
        assert!(s[0].active && s[0].funnel, "{s:?}");
        assert!(!s[1].active && !s[1].funnel && s[1].current == PortState::Free);
        assert_eq!(
            serde_json::to_value(&s[1].current).unwrap(),
            serde_json::json!({ "state": "free" })
        );
    }

    /// A fake `tailscale`: `serve status --json` prints `state`; any other
    /// call is logged to `calls` and runs `then` (shell).
    #[cfg(unix)]
    fn fake_cli(dir: &std::path::Path, state: &str, then: &str) -> crate::Probe {
        use std::os::unix::fs::PermissionsExt;
        let state_file = dir.join("state.json");
        std::fs::write(&state_file, state.replace("HOST", HOST)).unwrap();
        let script = dir.join("tailscale");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nif [ \"$*\" = 'serve status --json' ]; then cat '{}'; exit 0; fi\n\
                 echo \"$*\" >> '{}'\n{then}\n",
                state_file.display(),
                dir.join("calls").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        crate::Probe {
            cli_candidates: vec![script],
            timeout: std::time::Duration::from_secs(2),
        }
    }

    #[cfg(unix)]
    fn tempdir() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "fsonos-serve-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    #[test]
    fn setup_runs_each_missing_step_through_the_cli() {
        let dir = tempdir();
        let probe = fake_cli(&dir, OURS_443, "exit 0");
        let config = probe.serve_config().unwrap();
        let steps = plan_setup(&config, &[api(), mcp()]).unwrap();
        for step in &steps {
            if let Step::Run { argv, .. } = step {
                probe.run_step(argv).unwrap();
            }
        }
        let calls = std::fs::read_to_string(dir.join("calls")).unwrap();
        // The API mapping was already there: only MCP's step ran.
        assert_eq!(calls, "serve --bg --https=8443 http://127.0.0.1:8098\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_step_waiting_for_https_reports_what_it_printed() {
        let dir = tempdir();
        let probe = fake_cli(
            &dir,
            "{}",
            "echo 'Serve is not enabled on your tailnet. To enable, visit: https://login.example/f/serve'; exec sleep 30",
        );
        let err = probe.run_step(&setup_argv(&api())).unwrap_err();
        assert_eq!(
            err,
            crate::StepError::TimedOut {
                output: "Serve is not enabled on your tailnet. To enable, visit: \
                         https://login.example/f/serve"
                    .into()
            }
        );
        let failing = fake_cli(&dir, "{}", "echo 'serve: permission denied' >&2; exit 1");
        assert_eq!(
            failing.run_step(&teardown_argv(443)).unwrap_err(),
            crate::StepError::Failed {
                detail: "serve: permission denied".into()
            }
        );
        let absent = crate::Probe {
            cli_candidates: vec![dir.join("no-such-tailscale")],
            timeout: std::time::Duration::from_secs(1),
        };
        assert_eq!(
            absent.run_step(&teardown_argv(443)).unwrap_err(),
            crate::StepError::NotInstalled
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
