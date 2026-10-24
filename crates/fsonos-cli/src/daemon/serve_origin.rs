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
//! from a request. A look Tailscale does not answer changes nothing, for up
//! to [`UNANSWERED_LIMIT`] looks in a row.

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
/// How long each question may take: this thread waits for nobody.
const PATIENCE: Duration = Duration::from_secs(10);
/// Unanswered looks in a row before the origin goes anyway (2 minutes at
/// [`INTERVAL`]), so Serve turned off while Tailscale is silent is not
/// trusted for long.
pub const UNANSWERED_LIMIT: u32 = 4;

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

/// One look at Tailscale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Look {
    /// Tailscale answered both questions: the name whose Serve origin to
    /// admit, or none.
    Answered(Option<String>),
    /// It did not (the CLI failed or missed its deadline), so nothing is
    /// known about Serve.
    Unanswered,
}

/// Ask Tailscale (`tailscale serve status`, then `tailscale status`)
/// whether Serve fronts `target` on this host's MagicDNS name.
pub fn look(probe: &Probe, target: &str) -> Look {
    let Ok(config) = probe.serve_config() else {
        return Look::Unanswered;
    };
    let status = probe.detect();
    if status.answered() {
        Look::Answered(serve_name(&status, &config, target))
    } else {
        Look::Unanswered
    }
}

/// What to act on after `look`, counting unanswered looks in a row in
/// `unanswered`. One missed answer is not Serve going away (seen live: a
/// busy host's CLI misses its deadline, and the admitted origin flapped off
/// and on for 30 s at a time), so what is admitted stays, until
/// [`UNANSWERED_LIMIT`] looks in a row went unanswered: then nothing is.
pub fn settle(look: Look, unanswered: &mut u32) -> Look {
    match look {
        Look::Answered(_) => *unanswered = 0,
        Look::Unanswered => {
            *unanswered = unanswered.saturating_add(1);
            if *unanswered >= UNANSWERED_LIMIT {
                return Look::Answered(None);
            }
        }
    }
    look
}

/// Keep `origin` current, on a thread of its own, until `stop`. `target`
/// is what Serve proxies the API's port 443 to.
pub fn watch(target: String, origin: ServeOrigin, stop: Arc<AtomicBool>) {
    let spawned = thread::Builder::new()
        .name("fsonos-serve-origin".into())
        .spawn(move || {
            let probe = Probe {
                timeout: PATIENCE,
                ..Probe::default()
            };
            let mut unanswered = 0;
            while !stop.load(Ordering::Acquire) {
                match settle(look(&probe, &target), &mut unanswered) {
                    Look::Answered(name) => {
                        if unanswered == UNANSWERED_LIMIT && origin.get().is_some() {
                            tracing::warn!(
                                "Tailscale has not answered {UNANSWERED_LIMIT} times in a row; \
                                 not admitting Serve's origin until it does"
                            );
                        }
                        admit(&origin, name.as_deref());
                    }
                    Look::Unanswered => {
                        tracing::debug!(
                            unanswered,
                            "Tailscale did not answer; Serve's origin kept"
                        );
                    }
                }
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

    /// A missed answer keeps what is admitted; only a run of them, or an
    /// answer, changes it.
    #[test]
    fn a_missed_answer_is_not_serve_going_away() {
        let origin = ServeOrigin::default();
        let mut unanswered = 0;
        let mut step = |look: Look| {
            if let Look::Answered(name) = settle(look, &mut unanswered) {
                admit(&origin, name.as_deref());
            }
            origin.get()
        };
        let admitted = Some(format!("https://{NAME}"));
        assert_eq!(step(Look::Answered(Some(NAME.into()))), admitted);
        for _ in 1..UNANSWERED_LIMIT {
            assert_eq!(step(Look::Unanswered), admitted);
        }
        // An answer starts the count again.
        assert_eq!(step(Look::Answered(Some(NAME.into()))), admitted);
        for _ in 1..UNANSWERED_LIMIT {
            assert_eq!(step(Look::Unanswered), admitted);
        }
        assert_eq!(step(Look::Unanswered), None, "too long without an answer");
        assert_eq!(step(Look::Unanswered), None);
        assert_eq!(step(Look::Answered(Some(NAME.into()))), admitted);
        // An answer that Serve is gone counts at once.
        assert_eq!(step(Look::Answered(None)), None);
    }

    /// A fake `tailscale`: `serve status --json` prints this host's Serve of
    /// the API; `status --json` prints a running tailnet with `NAME`, after
    /// `delay` seconds.
    #[cfg(unix)]
    fn fake_cli(delay: u32) -> Probe {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::AtomicUsize;
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "fsonos-serve-origin-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let serve = format!(
            r#"{{"TCP":{{"443":{{"HTTPS":true}}}},"Web":{{"{NAME}:443":{{"Handlers":{{"/":{{"Proxy":"{TARGET}"}}}}}}}}}}"#
        );
        let status = format!(
            r#"{{"BackendState":"Running","HaveNodeKey":true,"TailscaleIPs":["100.64.0.10"],"Self":{{"DNSName":"{NAME}."}}}}"#
        );
        std::fs::write(dir.join("serve.json"), serve).unwrap();
        std::fs::write(dir.join("status.json"), status).unwrap();
        let script = dir.join("tailscale");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\ncd '{}' || exit 1\n\
                 if [ \"$*\" = 'serve status --json' ]; then cat serve.json; exit 0; fi\n\
                 if [ \"$*\" = 'status --json' ]; then sleep {delay}; cat status.json; exit 0; fi\n\
                 exit 1\n",
                dir.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        Probe {
            cli_candidates: vec![script],
            timeout: Duration::from_secs(5),
        }
    }

    /// Only Tailscale's own answer counts: a CLI past its deadline (the
    /// probe falls back to an interface scan, which has no MagicDNS name)
    /// is no answer, not "Serve is gone".
    #[cfg(unix)]
    #[test]
    fn a_slow_cli_is_no_answer() {
        assert_eq!(
            look(&fake_cli(0), TARGET),
            Look::Answered(Some(NAME.into()))
        );
        assert_eq!(look(&fake_cli(8), TARGET), Look::Unanswered);
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
