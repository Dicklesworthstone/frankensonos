//! `fsonos service` on Linux, run as a user would, with a fake `systemctl`
//! first on PATH that logs each call: install writes the user unit (and
//! starts it only with `--load`, never over a running daemon), status reads
//! it, and uninstall stops and removes only a file install wrote.
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

/// One test: the fake is written once, before any command runs, so no
/// other thread forks while it is open for writing (ETXTBSY).
#[test]
fn install_status_load_and_uninstall_a_user_unit() {
    let home = Home::new();
    let unit = home.unit();

    // A dry run prints the unit and writes nothing.
    let dry = home.fsonos(&["service", "install", "--dry-run", "--tailscale-serve"]);
    assert!(dry.status.success());
    let printed = stdout(&dry);
    assert!(
        printed.contains(&format!("# would write {}", unit.display())),
        "{printed}"
    );
    assert!(printed.contains("\"serve\""), "{printed}");
    assert!(!unit.exists());

    // Install writes it with this binary, the data directory and the flags.
    let installed = home.fsonos(&["service", "install", "--tailscale-serve"]);
    assert!(installed.status.success());
    let text = fs::read_to_string(&unit).unwrap();
    let binary = fs::canonicalize(env!("CARGO_BIN_EXE_fsonos")).unwrap();
    assert!(
        text.contains("Written by `fsonos service install`"),
        "{text}"
    );
    assert!(
        text.contains(&format!("ExecStart=\"{}\" \"serve\"", binary.display())),
        "{text}"
    );
    assert!(
        text.contains("Environment=\"FSONOS_TAILSCALE_SERVE=true\""),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "Environment=\"FSONOS_DATA_DIR={}\"",
            home.data().display()
        )),
        "{text}"
    );
    assert_eq!(home.calls(), "", "install alone starts nothing");

    // Status reads the file and asks systemd.
    let status = home.fsonos(&["--json", "service", "status"]);
    assert!(status.status.success());
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["manager"], "systemd");
    assert_eq!(status["installed"], true);
    assert_eq!(status["running"], true);
    assert_eq!(status["pid"], 4242);
    assert_eq!(status["daemon_answers"], false);

    // --load reloads systemd and enables the unit now, but never while a
    // daemon holds the data directory.
    fs::create_dir_all(home.data()).unwrap();
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(home.data().join("daemon.lock"))
        .unwrap();
    lock.try_lock().unwrap();
    let busy = home.fsonos(&["service", "install", "--load"]);
    assert!(!busy.status.success());
    assert!(String::from_utf8_lossy(&busy.stderr).contains("already serves"));
    assert!(!home.calls().contains("enable"), "{}", home.calls());
    drop(lock);
    let loaded = home.fsonos(&["service", "install", "--load"]);
    assert!(loaded.status.success());
    assert_eq!(
        home.calls(),
        "--user show fsonos.service -p ActiveState -p MainPID\n\
         --user daemon-reload\n--user enable --now fsonos.service\n"
    );

    // Uninstall stops it and removes the file.
    let removed = home.fsonos(&["service", "uninstall"]);
    assert!(removed.status.success());
    assert!(!unit.exists());
    assert!(
        home.calls()
            .ends_with("--user disable --now fsonos.service\n--user daemon-reload\n")
    );

    // A unit install did not write is left alone by both.
    fs::write(&unit, "[Service]\nExecStart=/bin/true\n").unwrap();
    let refused = home.fsonos(&["service", "install"]);
    assert!(!refused.status.success());
    let kept = home.fsonos(&["service", "uninstall"]);
    assert!(!kept.status.success());
    assert!(String::from_utf8_lossy(&kept.stderr).contains("leaving it alone"));
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
