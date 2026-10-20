//! `fsonos service`: run `fsonos serve` as a service of your user, from a
//! file this command writes, so the daemon starts when you log in and comes
//! back after a crash or a restart.
//!
//! * `install`: write the job (macOS: a LaunchAgent,
//!   `~/Library/LaunchAgents/io.github.dicklesworthstone.fsonos.plist`;
//!   Linux: a systemd user unit, `~/.config/systemd/user/fsonos.service`)
//!   with this binary's absolute path, the data directory, the log, and the
//!   flags and environment `serve` would get here. `--dry-run` prints it;
//!   `--load` starts it, unless another daemon serves the data directory.
//!   Installing again rewrites the file it wrote, with the flags given then.
//! * `start`: start the installed job as its file says, or restart it, so
//!   it picks up a file installed since.
//! * `status`: whether the job is installed and running, its pid, and
//!   whether a daemon answers.
//! * `uninstall`: stop the job and remove its file, only a file `install`
//!   wrote: it never touches another agent (the legacy bridge's among them).
//!
//! On macOS the agent runs in your login session, where the Local Network
//! prompt can appear: approve `fsonos` once (System Settings > Privacy &
//! Security > Local Network) and the daemon reaches the speakers. `fsonos
//! doctor` says when it cannot (docs/DEPLOY.md section 4).

use anyhow::{Context as _, bail};
use serde::Serialize;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::config::{EVENTS_PORT, GlobalArgs, SPOTIFY_REDIRECT_URI, ServeArgs, TailscaleMode};
use crate::remote::{DirLock, lock_data_dir, read_daemon_file};

#[cfg(test)]
mod tests;

/// The launchd label.
pub const LABEL: &str = "io.github.dicklesworthstone.fsonos";
/// The systemd user unit.
pub const UNIT: &str = "fsonos.service";
/// Every file `install` writes says so; `uninstall` removes only such a file.
pub const MARKER: &str = "Written by `fsonos service install`";

/// `fsonos service` arguments.
#[derive(Debug, Clone, clap::Args)]
pub struct ServiceArgs {
    #[command(subcommand)]
    pub action: Action,
}

/// What to do with the service.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum Action {
    /// Write the service file (a LaunchAgent on macOS, a systemd user unit
    /// on Linux) that runs `fsonos serve` with these settings.
    Install {
        /// The daemon's settings: the same flags and environment as `serve`.
        #[command(flatten)]
        serve: ServeArgs,
        /// Print the file and where it would go; change nothing.
        #[arg(long)]
        dry_run: bool,
        /// Start it now (or restart it), unless another daemon serves the
        /// data directory.
        #[arg(long)]
        load: bool,
    },
    /// Start the installed service as its file says, or restart it.
    Start,
    /// Whether the service is installed and running, and whether a daemon
    /// answers.
    Status,
    /// Stop the service and remove its file (only a file `install` wrote).
    Uninstall {
        /// Print what would be done; change nothing.
        #[arg(long)]
        dry_run: bool,
    },
}

/// The service manager the daemon runs under here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Manager {
    Launchd,
    Systemd,
}

impl Manager {
    /// This platform's.
    pub(crate) fn here() -> anyhow::Result<Self> {
        if cfg!(target_os = "macos") {
            Ok(Self::Launchd)
        } else if cfg!(target_os = "linux") {
            Ok(Self::Systemd)
        } else {
            bail!(
                "fsonos service knows launchd (macOS) and systemd (Linux); run `fsonos serve` \
                 under this platform's service manager instead"
            )
        }
    }

    /// Where its file goes for the user whose home is `home`.
    #[must_use]
    pub fn path(self, home: &Path, xdg_config_home: Option<&Path>) -> PathBuf {
        match self {
            Self::Launchd => home
                .join("Library/LaunchAgents")
                .join(format!("{LABEL}.plist")),
            Self::Systemd => xdg_config_home
                .filter(|p| p.is_absolute())
                .map_or_else(|| home.join(".config"), Path::to_path_buf)
                .join("systemd/user")
                .join(UNIT),
        }
    }

    /// The file for `job`.
    #[must_use]
    pub fn render(self, job: &Job) -> String {
        match self {
            Self::Launchd => plist(job),
            Self::Systemd => unit(job),
        }
    }
}

/// What the service runs, for either manager's file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    /// The binary's absolute path, then its arguments.
    pub program: Vec<String>,
    /// The environment, in order: launchd and systemd give a job none of
    /// the shell's.
    pub env: Vec<(String, String)>,
    pub home: PathBuf,
    /// Where stdout and stderr go (launchd; systemd keeps a journal).
    pub log: PathBuf,
    /// Seconds from SIGTERM to SIGKILL.
    pub stop_secs: u64,
}

/// The job that runs this binary's `serve` with `serve`'s settings.
fn job(global: &GlobalArgs, serve: &ServeArgs, home: &Path) -> anyhow::Result<Job> {
    let binary = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .context("finding this fsonos binary")?;
    let data_dir = std::path::absolute(crate::daemon::data_dir(global)?)?;
    let mut program = vec![text(&binary)?, "serve".to_owned()];
    if serve.allow_unsafe_bind {
        program.push("--allow-unsafe-bind".to_owned());
    }
    for seed in &global.seed {
        program.extend(["--seed".to_owned(), seed.to_string()]);
    }
    let mut env = vec![
        ("HOME".to_owned(), text(home)?),
        ("FSONOS_DATA_DIR".to_owned(), text(&data_dir)?),
        (
            "RUST_LOG".to_owned(),
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info,fsqlite=error".to_owned()),
        ),
    ];
    if let Some(seeds) = &global.seeds {
        env.push((
            "FSONOS_SEEDS".to_owned(),
            text(&std::path::absolute(seeds)?)?,
        ));
    }
    // launchd starts a job with PATH=/usr/bin:/bin:/usr/sbin:/sbin, and a
    // systemd user manager with its own: keep this shell's, so a program
    // serve finds by name (a speech engine) is found there too.
    if let Some(path) = search_path(&std::env::var("PATH").unwrap_or_default()) {
        env.push(("PATH".to_owned(), path));
    }
    for key in ["FSONOS_TTS_BACKEND", "FSONOS_TTS_COMMAND"] {
        if let Ok(value) = std::env::var(key) {
            env.push((key.to_owned(), value));
        }
    }
    env.extend(serve_env(serve));
    Ok(Job {
        program,
        env,
        home: home.to_path_buf(),
        log: home.join("Library/Logs/fsonos/fsonos.log"),
        stop_secs: crate::daemon::STOP_SECS,
    })
}

/// `serve`'s settings that differ from its defaults, as its variables.
#[must_use]
pub fn serve_env(serve: &ServeArgs) -> Vec<(String, String)> {
    let mut env = Vec::new();
    let mut set = |key: &str, value: String| env.push((key.to_owned(), value));
    if let Some(addr) = serve.http {
        set("FSONOS_HTTP_ADDR", addr.to_string());
    }
    if let Some(addr) = serve.mcp_http {
        set("FSONOS_MCP_HTTP_ADDR", addr.to_string());
    }
    if serve.tailscale == TailscaleMode::Off {
        set("FSONOS_TAILSCALE", "off".to_owned());
    }
    if serve.tailscale_serve {
        set("FSONOS_TAILSCALE_SERVE", "true".to_owned());
    }
    if let Some(id) = &serve.spotify_client_id {
        set("FSONOS_SPOTIFY_CLIENT_ID", id.clone());
    }
    if serve.spotify_redirect_uri != SPOTIFY_REDIRECT_URI {
        set(
            "FSONOS_SPOTIFY_REDIRECT_URI",
            serve.spotify_redirect_uri.clone(),
        );
    }
    if serve.events_port != EVENTS_PORT {
        set("FSONOS_EVENTS_PORT", serve.events_port.to_string());
    }
    env
}

/// `path`'s absolute directories, in order; `None` when it has none. A
/// relative one (`.`) would name a program in the job's working directory.
fn search_path(path: &str) -> Option<String> {
    let dirs: Vec<&str> = path.split(':').filter(|d| d.starts_with('/')).collect();
    (!dirs.is_empty()).then(|| dirs.join(":"))
}

fn text(path: &Path) -> anyhow::Result<String> {
    path.to_str()
        .map(str::to_owned)
        .with_context(|| format!("{} is not UTF-8", path.display()))
}

/// The LaunchAgent: a login-session job, kept alive, with as long to stop
/// as the daemon's drain takes.
#[must_use]
pub fn plist(job: &Job) -> String {
    let string = |s: &str| format!("<string>{}</string>", xml(s));
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD \
         PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
    );
    let _ = writeln!(
        out,
        "<!-- {MARKER}; `fsonos service uninstall` removes it. Edit by installing again. -->"
    );
    out.push_str("<plist version=\"1.0\">\n<dict>\n");
    let _ = writeln!(out, "\t<key>Label</key>\n\t{}", string(LABEL));
    out.push_str("\t<key>ProgramArguments</key>\n\t<array>\n");
    for arg in &job.program {
        let _ = writeln!(out, "\t\t{}", string(arg));
    }
    out.push_str("\t</array>\n\t<key>EnvironmentVariables</key>\n\t<dict>\n");
    for (key, value) in &job.env {
        let _ = writeln!(out, "\t\t<key>{}</key>\n\t\t{}", xml(key), string(value));
    }
    let home = job.home.to_string_lossy();
    let log = job.log.to_string_lossy();
    let _ = write!(
        out,
        "\t</dict>\n\t<key>WorkingDirectory</key>\n\t{home}\n\t<key>RunAtLoad</key>\n\t<true/>\n\
         \t<key>KeepAlive</key>\n\t<true/>\n\
         \t<key>ThrottleInterval</key>\n\t<integer>10</integer>\n\
         \t<key>ExitTimeOut</key>\n\t<integer>{stop}</integer>\n\t<key>ProcessType</key>\n\
         \t<string>Standard</string>\n\t<key>StandardOutPath</key>\n\t{log}\n\
         \t<key>StandardErrorPath</key>\n\t{log}\n</dict>\n</plist>\n",
        home = string(&home),
        stop = job.stop_secs,
        log = string(&log),
    );
    out
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The systemd user unit: restarted on any exit, with as long to stop as
/// the daemon's drain takes.
#[must_use]
pub fn unit(job: &Job) -> String {
    let exec: Vec<String> = job
        .program
        .iter()
        .map(|arg| quoted(&arg.replace('$', "$$")))
        .collect();
    let mut out = format!(
        "# {MARKER}; `fsonos service uninstall` removes it. Edit by installing again.\n\
         [Unit]\nDescription=FrankenSonos daemon (fsonos serve)\n\n[Service]\nExecStart={}\n\
         WorkingDirectory={}\n",
        exec.join(" "),
        job.home.to_string_lossy().replace('%', "%%"),
    );
    for (key, value) in &job.env {
        let _ = writeln!(out, "Environment={}", quoted(&format!("{key}={value}")));
    }
    let _ = write!(
        out,
        "Restart=always\nRestartSec=10\nTimeoutStopSec={}\n\n[Install]\nWantedBy=default.target\n",
        job.stop_secs
    );
    out
}

/// One systemd word: quoted, its quotes and backslashes escaped, and `%`
/// (a specifier) doubled.
fn quoted(s: &str) -> String {
    let escaped = s
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    format!("\"{escaped}\"")
}

/// This user's home, when HOME names one.
fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|h| h.is_absolute())
}

/// Where `manager` keeps this user's service file.
pub(crate) fn service_path(manager: Manager) -> Option<PathBuf> {
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    home().map(|home| manager.path(&home, xdg.as_deref()))
}

/// `fsonos service`.
pub fn run(global: &GlobalArgs, args: &ServiceArgs) -> anyhow::Result<()> {
    let manager = Manager::here()?;
    let home = home().context("HOME is unset or not absolute")?;
    let path = service_path(manager).context("HOME is unset or not absolute")?;
    match &args.action {
        Action::Install {
            serve,
            dry_run,
            load,
        } => {
            let job = job(global, serve, &home)?;
            install(global, manager, &path, &job, *dry_run, *load)
        }
        Action::Start => {
            match ours(&path)? {
                Some(true) => {}
                Some(false) => bail!(
                    "{} was not written by `fsonos service install`; leaving it alone",
                    path.display()
                ),
                None => bail!(
                    "not installed: no {}; `fsonos service install --load` writes and starts it",
                    path.display()
                ),
            }
            // The directory the service will serve: the one its file names.
            let file = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let data_dir = match installed_data_dir(&file) {
                Some(dir) => dir,
                None => crate::daemon::data_dir(global)?,
            };
            let replacing = serving(&data_dir, manager)?;
            start(manager, &path, replacing)
        }
        Action::Status => {
            let status = status(global, manager, &path);
            crate::emit(global.json, &status, status_text)
        }
        Action::Uninstall { dry_run } => uninstall(manager, &path, *dry_run),
    }
}

/// Whether `install` wrote the file at `path`; `None` when there is none.
pub(crate) fn ours(path: &Path) -> anyhow::Result<Option<bool>> {
    match std::fs::read_to_string(path) {
        Ok(found) => Ok(Some(found.contains(MARKER))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn install(
    global: &GlobalArgs,
    manager: Manager,
    path: &Path,
    job: &Job,
    dry_run: bool,
    load: bool,
) -> anyhow::Result<()> {
    let file = manager.render(job);
    if dry_run {
        println!("# would write {}", path.display());
        print!("{file}");
        return Ok(());
    }
    if ours(path)? == Some(false) {
        bail!(
            "{} exists and `fsonos service install` did not write it; move it away first",
            path.display()
        );
    }
    // A refused --load changes nothing.
    let replacing = if load {
        Some(serving(&crate::daemon::data_dir(global)?, manager)?)
    } else {
        None
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    // launchd opens the log but does not make its directory.
    if manager == Manager::Launchd
        && let Some(dir) = job.log.parent()
    {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(path, &file).with_context(|| format!("writing {}", path.display()))?;
    println!("wrote {}", path.display());
    if manager == Manager::Launchd
        && Path::new("/Library/LaunchDaemons")
            .join(format!("{LABEL}.plist"))
            .exists()
    {
        println!(
            "note: /Library/LaunchDaemons/{LABEL}.plist is installed too. Keep it unloaded (or \
             remove it) so only one daemon serves the data directory."
        );
    }
    if let Some(replacing) = replacing {
        start(manager, path, replacing)
    } else {
        println!("`fsonos service start` starts it with this file (or restarts it)");
        Ok(())
    }
}

/// Start the job at `path`, or restart it (`replacing`: the service runs).
fn start(manager: Manager, path: &Path, replacing: bool) -> anyhow::Result<()> {
    match manager {
        Manager::Launchd => {
            let domain = format!("gui/{}", uid()?);
            let target = format!("{domain}/{LABEL}");
            run_step(&["launchctl", "bootout", target.as_str()], true)?;
            // bootout can return while the old copy still drains, and
            // bootstrap fails until launchd has let go of it.
            let deadline = Instant::now() + Duration::from_secs(crate::daemon::STOP_SECS + 5);
            while launchd_has(&target) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(250));
            }
            let file = text(path)?;
            run_step(
                &["launchctl", "bootstrap", domain.as_str(), file.as_str()],
                false,
            )?;
        }
        Manager::Systemd => {
            run_step(&["systemctl", "--user", "daemon-reload"], false)?;
            run_step(&["systemctl", "--user", "enable", UNIT], false)?;
            // Restart, not start: a running unit picks up the new file.
            run_step(&["systemctl", "--user", "restart", UNIT], false)?;
        }
    }
    println!(
        "{}; `fsonos service status` shows it",
        if replacing { "restarted" } else { "started" }
    );
    Ok(())
}

/// Whether this user's service serves the data directory now; an error when
/// another daemon does (one started by hand, say), which would keep the
/// service's from starting.
fn serving(data_dir: &Path, manager: Manager) -> anyhow::Result<bool> {
    match lock_data_dir(data_dir).with_context(|| format!("checking {}", data_dir.display()))? {
        DirLock::Held(lock) => {
            drop(lock);
            Ok(false)
        }
        DirLock::Busy(daemon) => {
            let pid = daemon.map(|d| d.pid);
            let service = match manager_state(manager) {
                Ok((Some(true), pid)) => pid,
                _ => None,
            };
            if pid.is_some() && pid == service {
                return Ok(true);
            }
            bail!(
                "a daemon{} already serves {}, and the service's could not start beside it: \
                 stop that one first",
                pid.map(|pid| format!(" (pid {pid})")).unwrap_or_default(),
                data_dir.display()
            )
        }
    }
}

/// The data directory a service file `install` wrote names (its
/// `FSONOS_DATA_DIR`), as the plist or the unit escapes it.
fn installed_data_dir(file: &str) -> Option<PathBuf> {
    if let Some((_, rest)) = file.split_once("<key>FSONOS_DATA_DIR</key>") {
        let value = rest.split_once("<string>")?.1.split_once("</string>")?.0;
        let value = value
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&");
        return Some(PathBuf::from(value));
    }
    let quoted = file
        .lines()
        .find_map(|line| line.strip_prefix("Environment=\"FSONOS_DATA_DIR="))?
        .strip_suffix('"')?;
    // quoted() escaped `\` and `"` with a backslash and doubled `%`.
    let mut value = String::new();
    let mut chars = quoted.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' | '%' => value.extend(chars.next()),
            c => value.push(c),
        }
    }
    Some(PathBuf::from(value))
}

/// Whether launchd still has `target` (`gui/<uid>/<label>`).
fn launchd_has(target: &str) -> bool {
    Command::new("launchctl")
        .args(["print", target])
        .output()
        .is_ok_and(|out| out.status.success())
}

/// The commands that stop the job and forget it.
fn unload_steps(manager: Manager) -> anyhow::Result<Vec<Vec<String>>> {
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
            UNIT.into(),
        ]],
    })
}

fn uid() -> anyhow::Result<String> {
    let out = Command::new("id")
        .arg("-u")
        .output()
        .context("running id -u")?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Run one step. A failure is an error unless the step is `best_effort`
/// (stopping a job that isn't loaded fails, and that is fine).
fn run_step<S: AsRef<str>>(step: &[S], best_effort: bool) -> anyhow::Result<()> {
    let words: Vec<&str> = step.iter().map(AsRef::as_ref).collect();
    let (program, args) = words.split_first().context("an empty step")?;
    let out = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running {program}"))?;
    if !out.status.success() && !best_effort {
        bail!(
            "`{}` failed: {}",
            words.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn uninstall(manager: Manager, path: &Path, dry_run: bool) -> anyhow::Result<()> {
    match ours(path)? {
        None => {
            println!("not installed: no {}", path.display());
            return Ok(());
        }
        Some(false) => bail!(
            "{} was not written by `fsonos service install`; leaving it alone",
            path.display()
        ),
        Some(true) => {}
    }
    let steps = unload_steps(manager)?;
    if dry_run {
        for step in &steps {
            println!("would run: {}", step.join(" "));
        }
        println!("would remove {}", path.display());
        return Ok(());
    }
    // Stopping is best effort: a job that isn't loaded, or a session with
    // no user manager, has nothing to stop; the file goes either way.
    for step in &steps {
        run_step(step.as_slice(), true)?;
    }
    if manager == Manager::Systemd {
        run_step(&["systemctl", "--user", "daemon-reload"], true)?;
    }
    std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
    println!("stopped and removed {}", path.display());
    Ok(())
}

/// `fsonos service status`.
#[derive(Debug, Serialize)]
pub struct ServiceStatus {
    pub manager: Manager,
    pub path: PathBuf,
    /// The file exists and `install` wrote it.
    pub installed: bool,
    /// A file is there that `install` did not write.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub foreign: bool,
    /// The service manager runs it, when it could say.
    pub running: Option<bool>,
    pub pid: Option<u32>,
    /// The daemon that serves the data directory, if one does.
    pub daemon_pid: Option<u32>,
    /// It answered `GET /health`.
    pub daemon_answers: bool,
    /// What could not be checked.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

fn status(global: &GlobalArgs, manager: Manager, path: &Path) -> ServiceStatus {
    let mut notes = Vec::new();
    let found = ours(path).unwrap_or_else(|e| {
        notes.push(format!("{e:#}"));
        None
    });
    let (running, pid) = match manager_state(manager) {
        Ok(state) => state,
        Err(e) => {
            notes.push(format!("{e:#}"));
            (None, None)
        }
    };
    let daemon_pid = global
        .data_dir()
        .as_deref()
        .and_then(read_daemon_file)
        .map(|f| f.pid);
    let daemon_answers = matches!(crate::remote::Daemon::find(global, false), Ok(Some(_)));
    ServiceStatus {
        manager,
        path: path.to_path_buf(),
        installed: found == Some(true),
        foreign: found == Some(false),
        running,
        pid,
        daemon_pid,
        daemon_answers,
        notes,
    }
}

/// Whether the manager runs the job, and its pid.
pub(crate) fn manager_state(manager: Manager) -> anyhow::Result<(Option<bool>, Option<u32>)> {
    let field = |text: &str, key: &str| -> Option<String> {
        text.lines()
            .map(str::trim)
            .find_map(|line| line.strip_prefix(key))
            .map(|v| v.trim().to_owned())
    };
    match manager {
        Manager::Launchd => {
            let out = Command::new("launchctl")
                .args(["print", &format!("gui/{}/{LABEL}", uid()?)])
                .output()
                .context("running launchctl print")?;
            if !out.status.success() {
                return Ok((Some(false), None));
            }
            let text = String::from_utf8_lossy(&out.stdout);
            let pid = field(&text, "pid = ").and_then(|p| p.parse().ok());
            Ok((
                Some(field(&text, "state = ").as_deref() == Some("running")),
                pid,
            ))
        }
        Manager::Systemd => {
            let out = Command::new("systemctl")
                .args(["--user", "show", UNIT, "-p", "ActiveState", "-p", "MainPID"])
                .output()
                .context("running systemctl --user show")?;
            if !out.status.success() {
                bail!(
                    "systemctl --user show: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            let text = String::from_utf8_lossy(&out.stdout);
            let pid = field(&text, "MainPID=")
                .and_then(|p| p.parse().ok())
                .filter(|&p| p != 0);
            Ok((
                Some(field(&text, "ActiveState=").as_deref() == Some("active")),
                pid,
            ))
        }
    }
}

fn status_text(s: &ServiceStatus) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{}: {}",
        s.path.display(),
        if s.installed {
            "installed"
        } else if s.foreign {
            "present, but not written by `fsonos service install`"
        } else {
            "not installed"
        }
    );
    let running = match (s.running, s.pid) {
        (Some(true), Some(pid)) => format!("running (pid {pid})"),
        (Some(true), None) => "running".to_owned(),
        (Some(false), _) => "not running".to_owned(),
        (None, _) => "unknown".to_owned(),
    };
    let _ = writeln!(out, "service: {running}");
    let daemon = match (s.daemon_answers, s.daemon_pid) {
        (true, Some(pid)) if s.pid.is_some_and(|p| p != pid) => {
            format!("answers (pid {pid}, not the service's)")
        }
        (true, Some(pid)) => format!("answers (pid {pid})"),
        (true, None) => "answers".to_owned(),
        (false, _) => "none answers".to_owned(),
    };
    let _ = writeln!(out, "daemon: {daemon}");
    for note in &s.notes {
        let _ = writeln!(out, "note: {note}");
    }
    out
}
