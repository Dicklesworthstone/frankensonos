//! `service.state`: whether `fsonos serve` runs as a service of this user
//! (`fsonos service`), so it starts at login and comes back after a
//! restart. Not installed is not a fault (a daemon run by hand, or none, is
//! a choice); a file `fsonos service install` did not write, or a service
//! installed but not running, is worth a warning.

use fsonos_core::doctor::{Check, CheckContext, CheckId, CheckResult};
use serde_json::json;

use crate::service_cmd::{self, Manager};

pub const STATE: CheckId = CheckId("service.state");

pub struct ServiceCheck;

impl Check for ServiceCheck {
    fn id(&self) -> CheckId {
        STATE
    }

    fn title(&self) -> &'static str {
        "Service"
    }

    fn run(&self, _: &CheckContext) -> CheckResult {
        let Ok(manager) = Manager::here() else {
            return CheckResult::skip("no service manager `fsonos service` knows here");
        };
        let Some(path) = service_cmd::service_path(manager) else {
            return CheckResult::skip("HOME is unset, so there is no service file to look for");
        };
        verdict(
            &path.display().to_string(),
            service_cmd::ours(&path),
            || service_cmd::manager_state(manager),
        )
    }
}

/// The verdict for the service file at `path` (`ours`: whether `install`
/// wrote it, `None` when there is none) and the manager's word on it.
fn verdict(
    path: &str,
    ours: anyhow::Result<Option<bool>>,
    state: impl FnOnce() -> anyhow::Result<(Option<bool>, Option<u32>)>,
) -> CheckResult {
    match ours {
        Err(e) => CheckResult::warn(
            format!("could not read {path}"),
            "Check its permissions, or run `fsonos service status`.",
        )
        .with_detail(format!("{e:#}")),
        Ok(None) => CheckResult::skip(format!(
            "not installed ({path}): the daemon does not come back after a restart on its own"
        ))
        .with_remedy("`fsonos service install --load` runs `fsonos serve` as your login service."),
        Ok(Some(false)) => CheckResult::warn(
            format!("{path} is there, but `fsonos service install` did not write it"),
            "Move it away, then `fsonos service install --load`.",
        ),
        Ok(Some(true)) => match state() {
            Ok((Some(true), pid)) => CheckResult::pass(match pid {
                Some(pid) => format!("installed and running (pid {pid})"),
                None => "installed and running".to_owned(),
            })
            .with_evidence(json!({ "path": path, "pid": pid })),
            Ok((Some(false), _)) => CheckResult::warn(
                format!("installed ({path}) but not running"),
                "Start it with `fsonos service start`; its log says why it stopped.",
            ),
            Ok((None, _)) => CheckResult::pass(format!("installed ({path})")),
            Err(e) => CheckResult::pass(format!("installed ({path})"))
                .with_detail(format!("could not ask the service manager: {e:#}")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsonos_core::doctor::Status;

    fn ok(
        state: (Option<bool>, Option<u32>),
    ) -> impl FnOnce() -> anyhow::Result<(Option<bool>, Option<u32>)> {
        move || Ok(state)
    }

    #[test]
    fn the_verdict_follows_the_file_and_the_manager() {
        let path = "/home/u/.config/systemd/user/fsonos.service";
        let none = verdict(path, Ok(None), ok((None, None)));
        assert_eq!(none.status, Status::Skip, "not installed is no fault");
        assert!(
            none.remedy
                .as_deref()
                .unwrap()
                .contains("fsonos service install")
        );
        assert_eq!(
            verdict(path, Ok(Some(false)), ok((None, None))).status,
            Status::Warn
        );
        let running = verdict(path, Ok(Some(true)), ok((Some(true), Some(4242))));
        assert_eq!(running.status, Status::Pass);
        assert!(running.summary.contains("pid 4242"), "{}", running.summary);
        assert_eq!(
            verdict(path, Ok(Some(true)), ok((Some(false), None))).status,
            Status::Warn
        );
        let unknown = verdict(path, Ok(Some(true)), || anyhow::bail!("no user bus"));
        assert_eq!(unknown.status, Status::Pass);
        assert!(unknown.detail.as_deref().unwrap().contains("no user bus"));
        assert_eq!(
            verdict(path, Err(anyhow::anyhow!("denied")), ok((None, None))).status,
            Status::Warn
        );
    }
}
