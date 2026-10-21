use super::*;
use clap::Parser;
use std::ffi::OsStr;
use std::time::Instant;

#[derive(Parser)]
struct Cli {
    #[command(flatten)]
    args: StreamArgs,
}

fn parse(values: &[&str]) -> StreamArgs {
    Cli::try_parse_from(values).unwrap().args
}

#[test]
fn ordinary_streaming_does_not_prepare_an_application() {
    let args = parse(&["stream", "Room"]);
    assert!(args.application.is_empty());
    assert!(Launch::prepare(&args).unwrap().is_none());
}

#[test]
fn application_requires_an_explicit_output_and_double_dash() {
    for values in [
        vec!["stream", "Room", "--", "player"],
        vec!["stream", "Room", "--virtual-output", "movie", "player"],
        vec![
            "stream", "Room", "--virtual-output", "movie", "--input", "other.monitor",
            "--", "player",
        ],
    ] {
        assert!(Cli::try_parse_from(values).is_err());
    }
    let args = parse(&[
        "stream", "Room", "--virtual-output", "movie", "--", "player",
        "--measure-latency", "track with spaces; $(not-a-shell)",
    ]);
    assert_eq!(args.application.len(), 3);
    assert_eq!(args.application[1], "--measure-latency");
    #[cfg(unix)]
    assert!(!args.sync.measure_latency);
}

#[cfg(unix)]
#[test]
fn existing_player_correction_and_measurement_cannot_launch_another_application() {
    for flags in [
        vec!["--measure-latency", "--sync-microphone", "test_mic"],
        vec!["--sync-mpv", "/not-opened", "--sync-input", "paired"],
    ] {
        let mut values = vec!["stream", "Room", "--virtual-output", "movie"];
        values.extend(flags);
        values.extend(["--", "not-launched"]);
        assert!(Cli::try_parse_from(values).is_err());
    }
}

#[test]
fn launch_preflight_is_linux_pulse_only_and_has_no_execution_side_effects() {
    for format in ["pulse", "alsa", "avfoundation"] {
        let args = parse(&[
            "stream", "Room", "--virtual-output", "movie", "--format", format,
            "--", "/definitely-not-present/fsonos-test-player",
        ]);
        let prepared = super::super::virtual_output::prepare(&args)
            .and_then(|args| Launch::prepare(&args));
        assert_eq!(prepared.is_ok(), cfg!(target_os = "linux") && format == "pulse");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn malformed_programmatic_launches_are_rejected_before_io() {
    let args = parse(&[
        "stream", "Room", "--virtual-output", "movie", "--", "not-launched",
    ]);
    let args = super::super::virtual_output::prepare(&args).unwrap();
    for program in ["", "player\0suffix"] {
        let mut invalid = args.clone();
        invalid.application[0] = program.into();
        assert!(Launch::prepare(&invalid).is_err());
    }
    let mut invalid = args.clone();
    invalid.application.push("arg\0suffix".into());
    assert!(Launch::prepare(&invalid).is_err());
    let mut invalid = args.clone();
    invalid.input = "another.monitor".into();
    assert!(Launch::prepare(&invalid).is_err());
    let mut invalid = args.clone();
    invalid.virtual_output = None;
    assert!(Launch::prepare(&invalid).is_err());
    let mut invalid = args;
    invalid.sync.measure_latency = true;
    assert!(Launch::prepare(&invalid).is_err());
}

#[test]
fn arguments_remain_literal_and_output_preference_is_child_local() {
    let parent_sink = std::env::var_os("PULSE_SINK");
    let launch = Launch {
        program: "player".into(),
        arguments: vec!["--option".into(), "a b; $(not-a-shell) 🎵".into()],
        sink: "movie".into(),
    };
    let command = launch.command();
    assert_eq!(command.get_program(), "player");
    assert_eq!(command.get_args().map(OsString::from).collect::<Vec<_>>(), launch.arguments);
    assert_eq!(
        command.get_envs().collect::<Vec<_>>(),
        vec![(OsStr::new("PULSE_SINK"), Some(OsStr::new("movie")))]
    );
    assert_eq!(std::env::var_os("PULSE_SINK"), parent_sink);
}

#[cfg(unix)]
#[test]
fn native_non_utf8_arguments_are_not_reencoded() {
    use std::os::unix::ffi::OsStringExt as _;
    let argument = OsString::from_vec(vec![b'f', 0xff, b' ']);
    let launch = Launch {
        program: "player".into(),
        arguments: vec![argument.clone()],
        sink: "movie".into(),
    };
    assert_eq!(launch.command().get_args().next(), Some(argument.as_os_str()));
}

struct Receipt(std::path::PathBuf);

impl Receipt {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "fsonos-app-test-{}",
            fsonos_core::announce::clip::clip_id().unwrap()
        ));
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        Self(path)
    }

    fn command(&self, role: &str) -> Command {
        let launch = Launch {
            program: std::env::current_exe().unwrap().into_os_string(),
            arguments: vec![
                "--exact".into(),
                "streaming::application::tests::child_fixture".into(),
                "--nocapture".into(),
            ],
            sink: "movie".into(),
        };
        let mut command = launch.command();
        command.env("FSONOS_APP_TEST_ROLE", role);
        command.env("FSONOS_APP_TEST_RECEIPT", &self.0);
        command
    }

    fn read(&self) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(value) = serde_json::from_slice(&std::fs::read(&self.0).unwrap()) {
                return value;
            }
            assert!(Instant::now() < deadline, "child did not write its receipt");
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Receipt {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn wait_for_worker(app: &Application) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !app.worker.as_ref().unwrap().is_finished() {
        assert!(Instant::now() < deadline, "application worker did not finish");
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn child_fixture() {
    use std::io::Read as _;
    let Ok(role) = std::env::var("FSONOS_APP_TEST_ROLE") else {
        return;
    };
    let path = std::env::var_os("FSONOS_APP_TEST_RECEIPT").unwrap();
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input).unwrap();
    std::fs::write(
        path,
        serde_json::to_vec(&serde_json::json!({
            "sink": std::env::var("PULSE_SINK").unwrap(),
            "stdin_bytes": input.len(),
            "role": role,
        }))
        .unwrap(),
    )
    .unwrap();
    match role.as_str() {
        "block" => thread::sleep(Duration::from_secs(30)),
        "failure" => std::process::exit(17),
        "success" => {}
        _ => panic!("unknown fixture role"),
    }
}

#[test]
fn a_real_child_inherits_the_sink_without_ending_the_stream_on_success() {
    let receipt = Receipt::new();
    let stream_stop = Arc::new(AtomicBool::new(false));
    let mut app = Application::from_command(receipt.command("success"), Arc::clone(&stream_stop))
        .unwrap();
    let result = receipt.read();
    assert_eq!(result["sink"], "movie");
    assert_eq!(result["stdin_bytes"], 0);
    wait_for_worker(&app);
    app.shutdown().unwrap();
    assert!(app.reaped());
    assert!(!stream_stop.load(Ordering::Acquire));
    app.shutdown().unwrap();
}

#[test]
fn a_failed_child_is_reported_once_without_restart_or_ending_buffered_audio() {
    let receipt = Receipt::new();
    let stream_stop = Arc::new(AtomicBool::new(false));
    let mut app = Application::from_command(receipt.command("failure"), Arc::clone(&stream_stop))
        .unwrap();
    assert_eq!(receipt.read()["role"], "failure");
    wait_for_worker(&app);
    assert!(app.shutdown().unwrap_err().to_string().contains("exited with"));
    assert!(app.reaped());
    assert!(!stream_stop.load(Ordering::Acquire));
    app.shutdown().unwrap();
}

#[test]
fn cancellation_before_launch_never_spawns_the_child() {
    let receipt = Receipt::new();
    let stop = Arc::new(AtomicBool::new(true));
    let result = Application::from_command(receipt.command("success"), stop);
    assert!(result.is_err());
    assert_eq!(std::fs::metadata(&receipt.0).unwrap().len(), 0);
}

#[test]
fn shutdown_stops_and_reaps_a_running_child_without_cancelling_stream_cleanup() {
    let receipt = Receipt::new();
    let stream_stop = Arc::new(AtomicBool::new(false));
    let mut app = Application::from_command(receipt.command("block"), Arc::clone(&stream_stop))
        .unwrap();
    receipt.read();
    let started = Instant::now();
    app.shutdown().unwrap();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(app.reaped());
    assert!(!stream_stop.load(Ordering::Acquire));
    app.shutdown().unwrap();
}

#[test]
fn stream_cancellation_and_drop_both_reap_the_owned_child() {
    for cancel in [false, true] {
        let receipt = Receipt::new();
        let stream_stop = Arc::new(AtomicBool::new(false));
        let app = Application::from_command(receipt.command("block"), Arc::clone(&stream_stop))
            .unwrap();
        receipt.read();
        let reaped = Arc::clone(&app.reaped);
        if cancel {
            stream_stop.store(true, Ordering::Release);
            wait_for_worker(&app);
        }
        drop(app);
        assert!(reaped.load(Ordering::Acquire));
    }
}

#[test]
fn a_spawn_failure_leaves_the_stream_flag_unchanged() {
    let stop = Arc::new(AtomicBool::new(false));
    let result = Application::from_command(
        Command::new("/definitely-not-present/fsonos-test-player"),
        Arc::clone(&stop),
    );
    assert!(result.is_err());
    assert!(!stop.load(Ordering::Acquire));
}
