use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::fs::DirBuilderExt as _;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use serde_json::{Value, json};

use super::*;
use crate::streaming::synchronization::mpv::Controller;

struct Player {
    child: Child,
    directory: PathBuf,
    socket: PathBuf,
    receipt: PathBuf,
}

impl Player {
    fn start(role: &str) -> Self {
        let directory = std::env::temp_dir().join(format!(
            "fsonos-owned-mpv-{}",
            fsonos_core::announce::clip::clip_id().unwrap()
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .unwrap();
        let socket = directory.join("ipc");
        let receipt = directory.join("receipt.json");
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "streaming::synchronization::mpv::launch::tests::child_fixture",
                "--nocapture",
            ])
            .env("FSONOS_MPV_LAUNCH_FIXTURE_ROLE", role)
            .env("FSONOS_MPV_LAUNCH_FIXTURE_SOCKET", &socket)
            .env("FSONOS_MPV_LAUNCH_FIXTURE_RECEIPT", &receipt)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        Self {
            child,
            directory,
            socket,
            receipt,
        }
    }

    fn connect(&mut self, stop: Arc<AtomicBool>, timeout: Duration) -> anyhow::Result<Client> {
        Client::connect_with_budget(
            &self.socket,
            self.child.id(),
            "pulse/movie",
            stop,
            timeout,
            || {
                ensure!(self.child.try_wait()?.is_none(), "owned test player exited");
                Ok(())
            },
        )
    }

    fn receipt(&self) -> Value {
        serde_json::from_slice(&std::fs::read(&self.receipt).unwrap()).unwrap()
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_file(&self.receipt);
        let _ = std::fs::remove_dir(&self.directory);
    }
}

fn record_receipt(path: &Path, commands: u32, writes: &[f64]) {
    std::fs::write(
        path,
        serde_json::to_vec(&json!({"commands": commands, "connections": 1, "writes": writes}))
            .unwrap(),
    )
    .unwrap();
}

#[test]
fn child_fixture() {
    let Ok(role) = std::env::var("FSONOS_MPV_LAUNCH_FIXTURE_ROLE") else {
        return;
    };
    if role == "exit" {
        return;
    }
    if role == "missing" {
        thread::sleep(Duration::from_secs(10));
        return;
    }
    let path = std::env::var_os("FSONOS_MPV_LAUNCH_FIXTURE_SOCKET").unwrap();
    let receipt = PathBuf::from(std::env::var_os("FSONOS_MPV_LAUNCH_FIXTURE_RECEIPT").unwrap());
    let listener = UnixListener::bind(path).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    peer.set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = BufReader::new(peer.try_clone().unwrap());
    let mut delay = 0.1;
    let mut writes = Vec::new();
    let mut commands = 0;
    let mut ready_reads = 0;
    let mut watching = false;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        commands += 1;
        record_receipt(&receipt, commands, &writes);
        if role == "silent" {
            thread::sleep(Duration::from_secs(10));
            return;
        }
        if role == "malformed" {
            let _ = peer.write_all(b"not JSON\n");
            return;
        }
        let request: Value = serde_json::from_str(&line).unwrap();
        let command = request["command"].as_array().unwrap();
        let mut unavailable = false;
        let data = match command[0].as_str().unwrap() {
            "observe_property" => {
                watching = true;
                if role == "slow-watch" {
                    thread::sleep(Duration::from_millis(200));
                }
                Value::Null
            }
            "get_property" => match command[1].as_str().unwrap() {
                "pid" => json!(std::process::id() + u32::from(role == "wrong-pid")),
                "audio-device" => json!(if role == "wrong-output"
                    || (role == "switch-after-ready" && watching)
                {
                    "pulse/other"
                } else {
                    "pulse/movie"
                }),
                "core-idle" => {
                    ready_reads += 1;
                    json!(role == "idle" || ready_reads < 3)
                }
                "current-ao" if ready_reads < 3 => {
                    unavailable = true;
                    Value::Null
                }
                "current-ao" => json!(if role == "wrong-ao" { "alsa" } else { "pulse" }),
                "path" => json!("synthetic movie.mkv"),
                "audio-delay" => json!(delay),
                "pause" => json!(false),
                "speed" => json!(1.0),
                other => panic!("unexpected readiness property {other}"),
            },
            "set_property" => {
                assert_eq!(command[1], "audio-delay");
                delay = command[2].as_f64().unwrap();
                writes.push(delay);
                Value::Null
            }
            other => panic!("unexpected command {other}"),
        };
        record_receipt(&receipt, commands, &writes);
        let reply = json!({"request_id": request["request_id"], "error": if unavailable { "property unavailable" } else { "success" }, "data": data});
        if writeln!(peer, "{reply}").is_err() {
            break;
        }
    }
}

#[test]
fn an_owned_child_hands_off_to_real_correction_and_restoration_on_one_connection() {
    let mut player = Player::start("ready");
    let stop = Arc::new(AtomicBool::new(false));
    let client = player
        .connect(Arc::clone(&stop), Duration::from_secs(5))
        .unwrap();
    assert_eq!(player.receipt()["writes"], json!([]));
    let mut controller = Controller::new(client, 0.0).unwrap();
    assert!(controller.ready().unwrap());
    controller.finish_launch("pulse/movie").unwrap();
    let window = controller.begin_window().unwrap().unwrap();
    assert!(controller.apply(0.4, window).unwrap());
    // Startup cancellation is retired after handoff; ordinary shutdown can
    // still restore the previous delay after the stream's Ctrl-C flag is set.
    stop.store(true, Ordering::Release);
    controller.restore().unwrap();
    let receipt = player.receipt();
    assert_eq!(receipt["connections"], 1);
    assert_eq!(receipt["writes"], json!([-0.4, 0.1]));
    drop(controller);
}

#[test]
fn wrong_process_output_driver_and_malformed_ipc_never_authorize_correction() {
    for role in ["wrong-pid", "wrong-output", "wrong-ao", "malformed"] {
        let mut player = Player::start(role);
        assert!(
            player
                .connect(Arc::new(AtomicBool::new(false)), Duration::from_secs(3))
                .is_err(),
            "{role}"
        );
        let receipt = player.receipt();
        assert_eq!(receipt["writes"], json!([]));
        assert_eq!(receipt["connections"], 1);
    }
}

#[test]
fn a_source_output_changed_during_observer_setup_is_not_adopted() {
    let mut player = Player::start("switch-after-ready");
    let client = player
        .connect(Arc::new(AtomicBool::new(false)), Duration::from_secs(3))
        .unwrap();
    let mut controller = Controller::new(client, 0.0).unwrap();
    assert!(controller.finish_launch("pulse/movie").is_err());
    assert_eq!(player.receipt()["writes"], json!([]));
}

#[test]
fn absent_socket_unresponsive_peer_and_idle_playback_have_one_total_deadline() {
    for role in ["missing", "silent", "idle"] {
        let mut player = Player::start(role);
        let started = Instant::now();
        let result = player.connect(Arc::new(AtomicBool::new(false)), Duration::from_millis(300));
        assert!(result.is_err(), "{role}");
        assert!(started.elapsed() < Duration::from_secs(2), "{role}");
    }
}

#[test]
fn observer_setup_uses_the_same_startup_deadline() {
    let mut player = Player::start("slow-watch");
    let stop = Arc::new(AtomicBool::new(false));
    let started = Instant::now();
    let client = player.connect(stop, Duration::from_millis(600)).unwrap();
    assert!(Controller::new(client, 0.0).is_err());
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(player.receipt()["writes"], json!([]));
}

#[test]
fn cancellation_and_child_exit_stop_readiness_without_an_ipc_write() {
    let mut player = Player::start("exit");
    assert!(
        player
            .connect(Arc::new(AtomicBool::new(false)), Duration::from_secs(3))
            .is_err()
    );
    let mut player = Player::start("idle");
    let stop = Arc::new(AtomicBool::new(false));
    let cancel = Arc::clone(&stop);
    let worker = thread::spawn(move || {
        thread::sleep(Duration::from_millis(250));
        cancel.store(true, Ordering::Release);
    });
    let started = Instant::now();
    assert!(player.connect(stop, Duration::from_secs(3)).is_err());
    assert!(started.elapsed() < Duration::from_secs(2));
    worker.join().unwrap();
}
