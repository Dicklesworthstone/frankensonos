//! The daemon's Tailscale Serve origin, kept current while it runs.
//!
//! A daemon that started before Tailscale (or before Serve was set up) did
//! not know its MagicDNS name, so a page served through Serve could read the
//! API but not control it: its POSTs carry `Origin: https://<name>`, which
//! is not among the listener's own names. Every [`INTERVAL`] this asks
//! Tailscale (`tailscale status` and `tailscale serve status`) and admits
//! exactly `https://<this host's MagicDNS name>` while Serve proxies that
//! name's port 443 to the HTTP API, and only while Funnel is off for it
//! ([`ServeOrigin`]); otherwise none. Never a wildcard, never a name taken
//! from a request.

use fsonos_api::web::ServeOrigin;
use fsonos_tailscale::serve::{self, Mapping, ServeConfig};
use fsonos_tailscale::{Probe, TailnetStatus};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::tailscale_cmd::API_PORT;

/// How often Tailscale is asked.
pub const INTERVAL: Duration = Duration::from_secs(30);

/// The MagicDNS name whose Serve origin to admit: this host's, when Serve
/// proxies its port 443 to `target` (the HTTP API) and Funnel is off there.
pub fn serve_name(status: &TailnetStatus, config: &ServeConfig, target: &str) -> Option<String> {
    let name = status
        .running()?
        .magic_dns_name
        .as_deref()?
        .trim_end_matches('.')
        .to_ascii_lowercase();
    let api = Mapping {
        label: "HTTP API".into(),
        https_port: API_PORT,
        target: target.to_owned(),
    };
    let ours = serve::status(config, &[api]).pop()?;
    let served = config
        .web
        .keys()
        .any(|host_port| host_port.eq_ignore_ascii_case(&format!("{name}:{API_PORT}")));
    (ours.active && !ours.funnel && served).then_some(name)
}

/// Admit `name`'s Serve origin (or none); a line on stderr when that
/// changes what is admitted.
pub fn admit(origin: &ServeOrigin, name: Option<&str>) {
    let before = origin.get();
    if let Err(e) = origin.set(name) {
        tracing::warn!("not admitting a Tailscale Serve origin: {e}");
        let _ = origin.set(None);
    }
    let after = origin.get();
    if before != after {
        match after {
            Some(admitted) => {
                eprintln!("fsonos serve: pages from {admitted} (Tailscale Serve) may use the API");
            }
            None => eprintln!(
                "fsonos serve: Tailscale Serve no longer fronts the API; its pages may not use it"
            ),
        }
    }
}

/// Keep `origin` current, on a thread of its own, until `stop`. `target`
/// is what Serve proxies the API's port 443 to.
pub fn watch(target: String, origin: ServeOrigin, stop: Arc<AtomicBool>) {
    let spawned = thread::Builder::new()
        .name("fsonos-serve-origin".into())
        .spawn(move || {
            let probe = Probe::default();
            while !stop.load(Ordering::Acquire) {
                let name = probe
                    .serve_config()
                    .ok()
                    .and_then(|config| serve_name(&probe.detect(), &config, &target));
                admit(&origin, name.as_deref());
                let until = Instant::now() + INTERVAL;
                while Instant::now() < until && !stop.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(200));
                }
            }
        });
    if let Err(e) = spawned {
        tracing::warn!("not watching Tailscale Serve: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsonos_tailscale::{Source, Tailnet, Unavailable};

    /// Placeholders: no real tailnet.
    const NAME: &str = "sonos-host.example-tailnet.ts.net";
    const TARGET: &str = "http://127.0.0.1:8099";

    fn up(name: Option<&str>) -> TailnetStatus {
        TailnetStatus::Available(Tailnet {
            source: Source::Cli,
            backend_state: Some("Running".into()),
            running: true,
            logged_in: true,
            ipv4: vec!["100.64.0.10".parse().unwrap()],
            ipv6: Vec::new(),
            magic_dns_name: name.map(str::to_owned),
            tailnet: None,
        })
    }

    fn config(json: &str) -> ServeConfig {
        serve::parse_serve_config(json.as_bytes()).unwrap()
    }

    fn proxy(host: &str, port: u16, target: &str, funnel: bool) -> ServeConfig {
        let funnel = if funnel {
            format!(r#","AllowFunnel":{{"{host}:{port}":true}}"#)
        } else {
            String::new()
        };
        config(&format!(
            r#"{{"TCP":{{"{port}":{{"HTTPS":true}}}},"Web":{{"{host}:{port}":{{"Handlers":{{"/":{{"Proxy":"{target}"}}}}}}}}{funnel}}}"#
        ))
    }

    #[test]
    fn only_this_hosts_serve_of_the_api_is_admitted() {
        assert_eq!(
            serve_name(&up(Some(NAME)), &proxy(NAME, 443, TARGET, false), TARGET).as_deref(),
            Some(NAME)
        );
        assert_eq!(
            serve_name(
                &up(Some("Sonos-Host.example-tailnet.ts.net.")),
                &proxy(NAME, 443, &format!("{TARGET}/"), false),
                TARGET
            )
            .as_deref(),
            Some(NAME)
        );
        for (why, status, config) in [
            (
                "Funnel publishes it",
                up(Some(NAME)),
                proxy(NAME, 443, TARGET, true),
            ),
            (
                "another listener",
                up(Some(NAME)),
                proxy(NAME, 443, "http://127.0.0.1:3000", false),
            ),
            (
                "not port 443",
                up(Some(NAME)),
                proxy(NAME, 8443, TARGET, false),
            ),
            (
                "another name",
                up(Some(NAME)),
                proxy("other.example-tailnet.ts.net", 443, TARGET, false),
            ),
            ("nothing served", up(Some(NAME)), config("{}")),
            ("no MagicDNS", up(None), proxy(NAME, 443, TARGET, false)),
            (
                "Tailscale off",
                TailnetStatus::Unavailable(Unavailable::Disabled),
                proxy(NAME, 443, TARGET, false),
            ),
        ] {
            assert_eq!(serve_name(&status, &config, TARGET), None, "{why}");
        }
    }

    #[test]
    fn admitting_follows_serve() {
        let origin = ServeOrigin::default();
        admit(&origin, Some(NAME));
        assert_eq!(origin.get(), Some(format!("https://{NAME}")));
        // Never a name that is not one; nothing stays admitted.
        admit(&origin, Some("*.ts.net"));
        assert_eq!(origin.get(), None);
        admit(&origin, Some(NAME));
        admit(&origin, None);
        assert_eq!(origin.get(), None);
    }
}
