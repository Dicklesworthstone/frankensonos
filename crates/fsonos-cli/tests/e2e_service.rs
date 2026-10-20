//! `fsonos service` on Linux, run as a user would, with a fake `systemctl`
//! first on PATH that logs each call: install writes the user unit, status
//! reads it, start (or install --load) starts it or restarts it but never
//! beside another daemon, and uninstall stops and removes only a file
//! install wrote.
#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::process::{Command, Output};

const FAKE_SYSTEMCTL: &str = "#!/bin/sh\n\
    echo \"$*\" >> \"$FAKE_LOG\"\n\
    case \"$*\" in *show*) printf 'ActiveState=active\\nMainPID=4242\\n' ;; esac\n\
    exit 0\n";

struct Home(PathBuf);

impl Home {
    fn new() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("fsonos-service-{}-{nanos}", std::process::id()));
        fs::create_dir_all(dir.join("bin")).unwrap();
        let fake = dir.join("bin/systemctl");
        fs::write(&fake, FAKE_SYSTEMCTL).unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        Self(dir)
    }

    fn unit(&self) -> PathBuf {
        self.0.join(".config/systemd/user/fsonos.service")
    }

    fn data(&self) -> PathBuf {
        self.0.join("data")
    }

    /// The systemctl calls so far, one a line.
    fn calls(&self) -> String {
        fs::read_to_string(self.0.join("systemctl.log")).unwrap_or_default()
    }

    fn fsonos(&self, args: &[&str]) -> Output {
        let data = self.data();
        let out = Command::new(env!("CARGO_BIN_EXE_fsonos"))
            .arg("--data-dir")
            .arg(&data)
            .args(args)
            .env_clear()
            .env("HOME", &self.0)
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.0.join("bin").display()),
            )
            .env("FAKE_LOG", self.0.join("systemctl.log"))
            .output()
            .unwrap();
        eprintln!(
            "$ fsonos {}\n[exit {:?}]\n{}{}",
            args.join(" "),
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// One test: the fake is written once, before any command runs, so no
/// other thread forks while it is open for writing (ETXTBSY).
#[test]
fn install_status_start_and_uninstall_a_user_unit() {
    let home = Home::new();
    install_writes_the_unit_and_starts_nothing(&home);
    start_restarts_its_own_daemon_and_no_other(&home);
    a_daily_self_test_comes_and_goes(&home);
    uninstall_removes_only_its_own_unit(&home);
}

/// A dry run prints the unit and writes nothing; install writes it with this
/// binary, the data directory, the flags and the shell's PATH; status reads
/// it and asks systemd.
fn install_writes_the_unit_and_starts_nothing(home: &Home) {
    let unit = home.unit();
    let dry = home.fsonos(&["service", "install", "--dry-run", "--tailscale-serve"]);
    assert!(dry.status.success());
    let printed = stdout(&dry);
    assert!(
        printed.contains(&format!("# would write {}", unit.display())),
        "{printed}"
    );
    assert!(printed.contains("\"serve\""), "{printed}");
    assert!(!unit.exists());

    let installed = home.fsonos(&["service", "install", "--tailscale-serve"]);
    assert!(installed.status.success());
    assert!(stdout(&installed).contains("`fsonos service start`"));
    let text = fs::read_to_string(&unit).unwrap();
    let binary = fs::canonicalize(env!("CARGO_BIN_EXE_fsonos")).unwrap();
    for line in [
        "# Written by `fsonos service install`".to_owned(),
        format!("ExecStart=\"{}\" \"serve\"", binary.display()),
        "Environment=\"FSONOS_TAILSCALE_SERVE=true\"".to_owned(),
        format!("Environment=\"FSONOS_DATA_DIR={}\"", home.data().display()),
        format!(
            "Environment=\"PATH={}:/usr/bin:/bin\"",
            home.0.join("bin").display()
        ),
    ] {
        assert!(text.contains(&line), "{line} in\n{text}");
    }
    assert_eq!(home.calls(), "", "install alone starts nothing");

    let status = home.fsonos(&["--json", "service", "status"]);
    assert!(status.status.success());
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["manager"], "systemd");
    assert_eq!(status["installed"], true);
    assert_eq!(status["running"], true);
    assert_eq!(status["pid"], 4242);
    assert_eq!(status["daemon_answers"], false);
}

/// Neither start nor install --load starts the unit while another daemon
/// holds the data directory, one without a daemon.json or one whose pid is
/// not the service's (a refused --load leaves the file as it was);
/// the service's own daemon (systemd's MainPID) is restarted, so it runs
/// with the file installed since.
fn start_restarts_its_own_daemon_and_no_other(home: &Home) {
    fs::create_dir_all(home.data()).unwrap();
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(home.data().join("daemon.lock"))
        .unwrap();
    lock.try_lock().unwrap();
    let before = fs::read_to_string(home.unit()).unwrap();
    let busy = home.fsonos(&["service", "start"]);
    assert!(!busy.status.success());
    assert!(
        stderr(&busy).contains("already serves"),
        "{}",
        stderr(&busy)
    );
    let busy = home.fsonos(&["service", "install", "--load"]);
    assert!(!busy.status.success());
    assert_eq!(fs::read_to_string(home.unit()).unwrap(), before);

    // A daemon that is not the service's (systemd's MainPID is 4242).
    let daemon = home.data().join("daemon.json");
    fs::write(
        &daemon,
        r#"{"http":"127.0.0.1:9","cli_token":"t","pid":999}"#,
    )
    .unwrap();
    let busy = home.fsonos(&["service", "start"]);
    assert!(!busy.status.success());
    assert!(stderr(&busy).contains("(pid 999)"), "{}", stderr(&busy));

    fs::write(
        &daemon,
        r#"{"http":"127.0.0.1:9","cli_token":"t","pid":4242}"#,
    )
    .unwrap();
    let replaced = home.fsonos(&["service", "install", "--load", "--events-port", "0"]);
    assert!(replaced.status.success());
    assert!(
        stdout(&replaced).contains("\nrestarted;"),
        "{}",
        stdout(&replaced)
    );
    assert!(
        fs::read_to_string(home.unit())
            .unwrap()
            .contains("Environment=\"FSONOS_EVENTS_PORT=0\"")
    );

    drop(lock);
    fs::remove_file(&daemon).unwrap();
    let started = home.fsonos(&["service", "start"]);
    assert!(started.status.success());
    assert!(
        stdout(&started).starts_with("started;"),
        "{}",
        stdout(&started)
    );

    let show = "--user show fsonos.service -p ActiveState -p MainPID\n";
    let load = "--user daemon-reload\n--user enable fsonos.service\n\
                --user restart fsonos.service\n";
    assert_eq!(
        home.calls(),
        format!("{show}{show}{show}{show}{show}{load}{load}")
    );
}

/// `--selftest-daily` writes the self-test's run and timer beside the
/// unit, start starts the timer too, and installing again without it
/// removes them.
fn a_daily_self_test_comes_and_goes(home: &Home) {
    let dir = home.unit().parent().unwrap().to_path_buf();
    let (run, timer) = (
        dir.join("fsonos-selftest.service"),
        dir.join("fsonos-selftest.timer"),
    );
    let bad = home.fsonos(&["service", "install", "--selftest-daily", "25:00"]);
    assert!(!bad.status.success());
    assert!(!timer.exists());

    let installed = home.fsonos(&["service", "install", "--selftest-daily", "04:30"]);
    assert!(installed.status.success());
    let unit = fs::read_to_string(&run).unwrap();
    assert!(
        unit.contains("\" \"selftest\"\n") && unit.contains("Type=oneshot"),
        "{unit}"
    );
    let schedule = fs::read_to_string(&timer).unwrap();
    assert!(schedule.contains("OnCalendar=*-*-* 04:30:00"), "{schedule}");
    let before = home.calls().len();
    let status = home.fsonos(&["--json", "service", "status"]);
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["selftest_daily"], true);
    let started = home.fsonos(&["service", "start"]);
    assert!(
        stdout(&started).contains("with its daily self-test"),
        "{}",
        stdout(&started)
    );
    assert_eq!(
        &home.calls()[before..],
        "--user show fsonos.service -p ActiveState -p MainPID\n\
         --user daemon-reload\n--user enable fsonos.service\n--user restart fsonos.service\n\
         --user enable fsonos-selftest.timer\n--user restart fsonos-selftest.timer\n"
    );

    let again = home.fsonos(&["service", "install"]);
    assert!(
        stdout(&again).contains("removed the daily self-test"),
        "{}",
        stdout(&again)
    );
    assert!(!run.exists() && !timer.exists());
    assert!(
        home.calls()
            .ends_with("--user disable --now fsonos-selftest.timer\n--user daemon-reload\n")
    );
}

/// Uninstall stops the unit and removes its file; a unit install did not
/// write is left alone by install, start and uninstall.
fn uninstall_removes_only_its_own_unit(home: &Home) {
    let unit = home.unit();
    let removed = home.fsonos(&["service", "uninstall"]);
    assert!(removed.status.success());
    assert!(!unit.exists());
    assert!(
        home.calls()
            .ends_with("--user disable --now fsonos.service\n--user daemon-reload\n")
    );
    let absent = home.fsonos(&["service", "start"]);
    assert!(!absent.status.success());
    assert!(
        stderr(&absent).contains("not installed"),
        "{}",
        stderr(&absent)
    );

    fs::write(&unit, "[Service]\nExecStart=/bin/true\n").unwrap();
    for refused in [
        home.fsonos(&["service", "install"]),
        home.fsonos(&["service", "start"]),
        home.fsonos(&["service", "uninstall"]),
    ] {
        assert!(!refused.status.success());
    }
    assert_eq!(
        fs::read_to_string(&unit).unwrap(),
        "[Service]\nExecStart=/bin/true\n"
    );
    let foreign = home.fsonos(&["--json", "service", "status"]);
    let foreign: serde_json::Value = serde_json::from_slice(&foreign.stdout).unwrap();
    assert_eq!(
        (foreign["installed"].clone(), foreign["foreign"].clone()),
        (false.into(), true.into())
    );
}
