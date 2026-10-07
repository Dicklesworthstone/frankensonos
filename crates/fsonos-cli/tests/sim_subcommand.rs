//! `fsonos sim` end to end: start the virtual house with the real binary,
//! drive `discover` and `zones` against it from separate processes through the
//! seeds and routes files it writes, then stop it with SIGINT.
#![cfg(all(feature = "sim", unix))]

use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_fsonos");

/// A clean environment: no seeds, routes or data dir from the caller's shell.
fn fsonos(dir: &Path) -> Command {
    let mut cmd = Command::new(BIN);
    for var in ["FSONOS_SEEDS", "FSONOS_ROUTES", "FSONOS_DATA_DIR"] {
        cmd.env_remove(var);
    }
    cmd.env("FSONOS_DATA_DIR", dir.join("data"))
        .env("RUST_LOG", "warn");
    cmd
}

struct Sim {
    child: Child,
    lines: Receiver<String>,
    banner: Vec<String>,
}

impl Sim {
    fn start(dir: &Path, args: &[&str]) -> Self {
        let mut child = fsonos(dir)
            .arg("sim")
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn fsonos sim");
        let stdout = child.stdout.take().expect("stdout");
        let (tx, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut sim = Self {
            child,
            lines,
            banner: Vec::new(),
        };
        let deadline = Instant::now() + Duration::from_secs(30);
        while !sim.banner.iter().any(|l| l.starts_with("Ctrl-C")) {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = sim
                .lines
                .recv_timeout(left)
                .unwrap_or_else(|_| panic!("no ready banner; got {:?}", sim.banner));
            sim.banner.push(line);
        }
        assert!(
            sim.banner[0].starts_with("fsonos sim: ready"),
            "{:?}",
            sim.banner
        );
        sim
    }

    fn path(&self, label: &str) -> PathBuf {
        let line = self
            .banner
            .iter()
            .find_map(|l| l.strip_prefix(label))
            .unwrap_or_else(|| panic!("no {label:?} line in {:?}", self.banner));
        PathBuf::from(line.trim())
    }

    /// SIGINT, then wait (bounded) for a clean exit.
    fn interrupt(mut self) -> ExitStatus {
        let pid = self.child.id().to_string();
        let sent = Command::new("kill").args(["-INT", &pid]).status().unwrap();
        assert!(sent.success(), "kill -INT {pid}");
        wait_until_exit(&mut self.child, Duration::from_secs(20))
    }
}

impl Drop for Sim {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn wait_until_exit(child: &mut Child, limit: Duration) -> ExitStatus {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "process did not exit within {limit:?}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// Run `fsonos <args>` to completion within `limit`.
fn run(dir: &Path, args: &[&str], limit: Duration) -> Output {
    let mut child = fsonos(dir)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn fsonos");
    wait_until_exit(&mut child, limit);
    child.wait_with_output().unwrap()
}

fn json(out: &Output) -> Value {
    assert!(
        out.status.success(),
        "exit {:?}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("JSON on stdout")
}

/// A fresh scratch directory for one test (removed at the end).
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fsonos-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn discover_and_zones_reach_the_virtual_house() {
    let dir = scratch("sim-e2e");
    let seeds = dir.join("seeds.toml");
    let sim = Sim::start(&dir, &["--seeds-out", seeds.to_str().unwrap()]);
    let routes = sim.path("routes: ");
    assert_eq!(sim.path("seeds:  "), seeds);
    assert_eq!(routes, dir.join("routes.toml"));

    // Every route leads to loopback, one per player.
    let text = std::fs::read_to_string(&routes).unwrap();
    let targets: Vec<SocketAddr> = text
        .lines()
        .filter(|l| l.starts_with("\"192.0.2."))
        .map(|l| l.rsplit('"').nth(1).unwrap().parse().unwrap())
        .collect();
    assert_eq!(targets.len(), 5, "{text}");
    assert!(targets.iter().all(|a| a.ip().is_loopback()), "{text}");

    let flags = [
        "--seeds",
        seeds.to_str().unwrap(),
        "--routes",
        routes.to_str().unwrap(),
        "--json",
    ];
    let limit = Duration::from_secs(60);
    let found = json(&run(&dir, &[&flags[..], &["discover"]].concat(), limit));
    let text = found.to_string();
    for room in ["Kitchen", "Office", "Living Room", "Bedroom"] {
        assert!(text.contains(room), "{room} missing from discover: {text}");
    }
    let zones = json(&run(&dir, &[&flags[..], &["zones"]].concat(), limit));
    let text = zones.to_string();
    assert!(
        text.contains("Kitchen") && text.contains("Bedroom"),
        "{text}"
    );

    let status = sim.interrupt();
    assert!(status.success(), "fsonos sim exited {status:?}");
    // Files the caller asked for are kept.
    assert!(seeds.exists() && routes.exists());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn default_files_are_removed_on_ctrl_c() {
    let dir = scratch("sim-tmp");
    let sim = Sim::start(&dir, &["--scenario", "s2"]);
    assert!(
        sim.banner[0].contains("2 virtual players in 1 household"),
        "{:?}",
        sim.banner
    );
    let seeds = sim.path("seeds:  ");
    let routes = sim.path("routes: ");
    assert!(seeds.exists() && routes.exists());
    let status = sim.interrupt();
    assert!(status.success(), "fsonos sim exited {status:?}");
    assert!(
        !seeds.exists() && !routes.exists(),
        "temporary files left behind"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}
