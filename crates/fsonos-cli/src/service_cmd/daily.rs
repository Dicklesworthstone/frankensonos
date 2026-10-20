//! The daily self-test (`fsonos service install --selftest-daily HH:MM`): a
//! second job of the same service manager that runs the inaudible
//! `fsonos selftest` once a day at a local time, with the service's binary,
//! environment and home. Each run keeps its report in the data directory,
//! where `fsonos doctor` reads the newest (`selftest.last`) and warns when
//! the daily run has not happened for two days.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::{Job, MARKER, Manager, ours, plist_head, run_step, uid, unit_head};

/// The launchd label.
pub const LABEL: &str = "io.github.dicklesworthstone.fsonos.selftest";
/// The systemd user unit that runs it once.
pub const UNIT: &str = "fsonos-selftest.service";
/// The systemd user timer that starts [`UNIT`] daily.
pub const TIMER: &str = "fsonos-selftest.timer";

/// A local time of day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct At {
    pub hour: u8,
    pub minute: u8,
}

/// `HH:MM`, 24-hour (`--selftest-daily`).
///
/// # Errors
/// Anything else, saying what is wrong.
pub fn parse_at(s: &str) -> Result<At, String> {
    let bad = || format!("{s:?} is not a time: give HH:MM, 24-hour (04:30, say)");
    let (hour, minute) = s.split_once(':').ok_or_else(bad)?;
    if hour.len() != 2 || minute.len() != 2 {
        return Err(bad());
    }
    match (hour.parse::<u8>(), minute.parse::<u8>()) {
        (Ok(hour @ 0..24), Ok(minute @ 0..60)) => Ok(At { hour, minute }),
        _ => Err(bad()),
    }
}

/// Where `manager` keeps the daily self-test's files, beside the service's
/// own (`service`): the job (launchd), or the run and its timer (systemd).
#[must_use]
pub fn paths(manager: Manager, service: &Path) -> Vec<PathBuf> {
    let dir = service.parent().unwrap_or(Path::new("."));
    match manager {
        Manager::Launchd => vec![dir.join(format!("{LABEL}.plist"))],
        Manager::Systemd => vec![dir.join(UNIT), dir.join(TIMER)],
    }
}

/// The daily self-test's files, in [`paths`] order: `fsonos selftest` with
/// the service `job`'s binary, seeds, environment and home, at `at`.
#[must_use]
pub fn files(manager: Manager, job: &Job, at: At) -> Vec<String> {
    let mut program = vec![job.program[0].clone(), "selftest".to_owned()];
    // The service's `--seed` players, so the self-test finds the house the
    // daemon does.
    let mut args = job.program.iter().skip(2);
    while let Some(arg) = args.next() {
        if arg == "--seed" {
            program.push(arg.clone());
            program.extend(args.next().cloned());
        }
    }
    let run = Job {
        program,
        log: job.log.with_file_name("selftest.log"),
        ..job.clone()
    };
    match manager {
        Manager::Launchd => vec![plist(&run, at)],
        Manager::Systemd => vec![
            unit_head(&run, "FrankenSonos daily self-test (fsonos selftest)") + "Type=oneshot\n",
            timer(at),
        ],
    }
}

/// The LaunchAgent: started at `at` each day (or at the next wake when the
/// Mac slept through it).
fn plist(job: &Job, at: At) -> String {
    let log = format!(
        "<string>{}</string>",
        super::xml(&job.log.to_string_lossy())
    );
    let mut out = plist_head(LABEL, job);
    let _ = write!(
        out,
        "\t<key>StartCalendarInterval</key>\n\t<dict>\n\t\t<key>Hour</key>\n\t\t<integer>{}</integer>\n\
         \t\t<key>Minute</key>\n\t\t<integer>{}</integer>\n\t</dict>\n\
         \t<key>ProcessType</key>\n\t<string>Standard</string>\n\
         \t<key>StandardOutPath</key>\n\t{log}\n\t<key>StandardErrorPath</key>\n\t{log}\n\
         </dict>\n</plist>\n",
        at.hour, at.minute
    );
    out
}

/// The systemd timer: [`UNIT`] at `at` each day.
fn timer(at: At) -> String {
    format!(
        "# {MARKER}; `fsonos service uninstall` removes it. Edit by installing again.\n\
         [Unit]\nDescription=Run the FrankenSonos self-test daily\n\n\
         [Timer]\nOnCalendar=*-*-* {:02}:{:02}:00\n\n[Install]\nWantedBy=timers.target\n",
        at.hour, at.minute
    )
}

/// Whether `install` wrote the daily self-test at `paths`.
#[must_use]
pub fn installed(paths: &[PathBuf]) -> bool {
    paths
        .first()
        .is_some_and(|path| matches!(ours(path), Ok(Some(true))))
}

/// The commands that start (or restart) the daily self-test's schedule.
pub(super) fn start(manager: Manager, paths: &[PathBuf]) -> anyhow::Result<()> {
    match manager {
        Manager::Launchd => {
            let domain = format!("gui/{}", uid()?);
            let target = format!("{domain}/{LABEL}");
            run_step(&["launchctl", "bootout", target.as_str()], true)?;
            // A run in flight is stopped first, and bootstrap fails until
            // launchd has let go of it.
            let deadline = Instant::now() + Duration::from_secs(30);
            while super::launchd_has(&target) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(250));
            }
            let file = super::text(&paths[0])?;
            run_step(
                &["launchctl", "bootstrap", domain.as_str(), file.as_str()],
                false,
            )
        }
        Manager::Systemd => {
            run_step(&["systemctl", "--user", "enable", TIMER], false)?;
            run_step(&["systemctl", "--user", "restart", TIMER], false)
        }
    }
}

/// The commands that stop the daily self-test's schedule and forget it.
pub(super) fn stop_steps(manager: Manager) -> anyhow::Result<Vec<Vec<String>>> {
    Ok(match manager {
        Manager::Launchd => vec![vec![
            "launchctl".into(),
            "bootout".into(),
            format!("gui/{}/{LABEL}", uid()?),
        ]],
        Manager::Systemd => vec![vec![
            "systemctl".into(),
            "--user".into(),
            "disable".into(),
            "--now".into(),
            TIMER.into(),
        ]],
    })
}

/// Stop the daily self-test (best effort) and remove its files.
pub(super) fn remove(manager: Manager, paths: &[PathBuf]) -> anyhow::Result<()> {
    for step in stop_steps(manager)? {
        run_step(step.as_slice(), true)?;
    }
    remove_files(paths)?;
    if manager == Manager::Systemd {
        run_step(&["systemctl", "--user", "daemon-reload"], true)?;
    }
    Ok(())
}

/// Remove the daily self-test's files (those that are there).
pub(super) fn remove_files(paths: &[PathBuf]) -> anyhow::Result<()> {
    for path in paths {
        if let Err(e) = std::fs::remove_file(path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(anyhow::Error::new(e).context(format!("removing {}", path.display())));
        }
    }
    Ok(())
}
