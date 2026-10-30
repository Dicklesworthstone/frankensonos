//! Terminal signals reach the controller before it shuts down owned capture.

use super::*;
use std::io::{BufRead as _, BufReader, Write as _};
use std::net::{TcpListener, TcpStream};
use std::os::unix::process::CommandExt as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const FRAME: &str = "fsonos-encoder-test-frame\n";
const DEADLINE: Duration = Duration::from_secs(5);

struct Owned(Child);

impl Owned {
    fn stop(&mut self) -> io::Result<()> {
        if self.0.try_wait()?.is_none() {
            if let Err(error) = self.0.kill()
                && self.0.try_wait()?.is_none()
            {
                return Err(error);
            }
            self.0.wait()?;
        }
        Ok(())
    }

    fn wait(&mut self) -> std::process::ExitStatus {
        let deadline = Instant::now() + DEADLINE;
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "owned fixture did not finish");
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn command(role: &str) -> Command {
    // Test names omit the crate name included by module_path!().
    let (_, module) = module_path!().split_once("::").unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--ignored",
            "--exact",
            &format!("{module}::process_fixture"),
            "--nocapture",
        ])
        .env("FSONOS_ENCODER_TEST_ROLE", role);
    command
}

fn await_frames(frames: &AtomicUsize, minimum: usize) {
    let deadline = Instant::now() + DEADLINE;
    while frames.load(Ordering::Acquire) < minimum {
        assert!(
            Instant::now() < deadline,
            "capture stopped producing frames"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn count_frames(child: &mut Child) -> (Arc<AtomicUsize>, JoinHandle<io::Result<()>>) {
    let output = child.stdout.take().unwrap();
    let frames = Arc::new(AtomicUsize::new(0));
    let delivered = Arc::clone(&frames);
    let reader = thread::spawn(move || {
        let mut output = BufReader::new(output);
        loop {
            let mut line = String::new();
            if output.read_line(&mut line)? == 0 {
                return Ok(());
            }
            if line == FRAME {
                delivered.fetch_add(1, Ordering::Release);
            }
        }
    });
    (frames, reader)
}

fn controller_fixture() {
    let stop = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(&stop)).unwrap();
    }
    let address = std::env::var("FSONOS_ENCODER_TEST_CONTROLLER").unwrap();
    let mut connection = TcpStream::connect(address).unwrap();
    connection.set_write_timeout(Some(DEADLINE)).unwrap();
    // This is the same spawn function used by streaming.rs::launch_encoder.
    let mut capture = Owned(spawn(&mut command("capture")).unwrap());
    let (frames, reader) = count_frames(&mut capture.0);
    await_frames(&frames, 3);
    connection.write_all(b"ready\n").unwrap();

    let deadline = Instant::now() + DEADLINE;
    while !stop.load(Ordering::Acquire) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        stop.load(Ordering::Acquire),
        "controller did not receive cancellation"
    );
    let before = frames.load(Ordering::Acquire);
    // Represent the supervisor's speaker-recovery work while capture remains
    // available. A real signal to the terminal group must not cut this audio.
    thread::sleep(Duration::from_millis(150));
    assert!(
        capture.0.try_wait().unwrap().is_none(),
        "terminal signal killed capture"
    );
    await_frames(&frames, before + 3);
    capture.stop().unwrap();
    assert!(capture.0.try_wait().unwrap().is_some());
    reader.join().unwrap().unwrap();
    connection
        .write_all(b"capture-lived-through-cancellation-and-was-reaped\n")
        .unwrap();
}

#[test]
#[ignore = "child process fixture; invoked only by the signal ownership regression"]
fn process_fixture() {
    match std::env::var("FSONOS_ENCODER_TEST_ROLE").as_deref() {
        Ok("capture") => {
            let mut output = io::stdout().lock();
            loop {
                output.write_all(FRAME.as_bytes()).unwrap();
                output.flush().unwrap();
                thread::sleep(Duration::from_millis(20));
            }
        }
        Ok("controller") => controller_fixture(),
        _ => {}
    }
}

fn controller_connection(listener: &TcpListener, controller: &mut Owned) -> TcpStream {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + DEADLINE;
    loop {
        match listener.accept() {
            Ok((connection, _)) => return connection,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => panic!("accept controller receipt: {error}"),
        }
        assert!(
            controller.0.try_wait().unwrap().is_none(),
            "controller exited before ready"
        );
        assert!(Instant::now() < deadline, "controller never connected");
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn foreground_group_signals_leave_capture_alive_until_the_supervisor_stops_it() {
    for signal in ["INT", "TERM"] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut launch = command("controller");
        launch
            .env(
                "FSONOS_ENCODER_TEST_CONTROLLER",
                listener.local_addr().unwrap().to_string(),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            // The signal target is this fixture's own isolated group only.
            .process_group(0);
        let mut controller = Owned(launch.spawn().unwrap());
        let connection = controller_connection(&listener, &mut controller);
        connection.set_read_timeout(Some(DEADLINE)).unwrap();
        let mut connection = BufReader::new(connection);
        let mut line = String::new();
        connection.read_line(&mut line).unwrap();
        assert_eq!(line, "ready\n");

        let sent = Command::new("/bin/kill")
            .args(["-s", signal, "--", &format!("-{}", controller.0.id())])
            .status();
        // The controller owns and cleans up capture even if the signal fails.
        let status = controller.wait();
        assert!(sent.unwrap().success());
        assert!(
            status.success(),
            "signal {signal} interrupted capture recovery"
        );
        line.clear();
        connection.read_line(&mut line).unwrap();
        assert_eq!(line, "capture-lived-through-cancellation-and-was-reaped\n");
    }
}

#[test]
fn encoder_spawn_failure_returns_the_original_process_error() {
    let error = spawn(&mut Command::new(
        "/definitely-not-present/fsonos-test-encoder",
    ))
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
}
