use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixListener;

use clap::Parser;

use super::*;
use crate::streaming::StreamArgs;
#[cfg(target_os = "linux")]
use crate::streaming::{synchronization, virtual_output};

#[derive(Parser)]
struct Cli {
    #[command(flatten)]
    args: StreamArgs,
}

#[test]
fn managed_mpv_requires_a_command_output_and_explicit_capture() {
    for flags in [
        vec!["--sync-launched-mpv"],
        vec!["--sync-launched-mpv", "--sync-microphone", "mic"],
        vec![
            "--sync-launched-mpv",
            "--virtual-output",
            "movie",
            "--",
            "mpv",
            "movie.mkv",
        ],
        vec![
            "--sync-launched-mpv",
            "--sync-microphone",
            "mic",
            "--",
            "mpv",
            "movie.mkv",
        ],
        vec![
            "--sync-launched-mpv",
            "--virtual-output",
            "movie",
            "--sync-microphone",
            "mic",
        ],
        vec![
            "--sync-launched-mpv",
            "--virtual-output",
            "movie",
            "--sync-microphone",
            "mic",
            "--sync-mpv",
            "/unused",
            "--",
            "mpv",
            "movie.mkv",
        ],
        vec![
            "--sync-launched-mpv",
            "--virtual-output",
            "movie",
            "--sync-microphone",
            "mic",
            "--measure-latency",
            "--",
            "mpv",
            "movie.mkv",
        ],
    ] {
        let mut argv = vec!["stream", "Room"];
        argv.extend(flags);
        assert!(Cli::try_parse_from(argv).is_err());
    }
}

#[cfg(target_os = "linux")]
#[test]
fn one_command_prepares_output_application_probe_and_sync_without_starting_them() {
    for flags in [
        vec![],
        vec!["--sync-probe", "movie"],
        vec!["--sync-base-delay", "0.2"],
    ] {
        let mut argv = vec![
            "stream",
            "Room",
            "--virtual-output",
            "movie",
            "--sync-launched-mpv",
            "--sync-microphone",
            "room_mic",
        ];
        argv.extend(flags);
        argv.extend(["--", "/definitely-not-present/mpv", "a movie.mkv"]);
        let args = virtual_output::prepare(&Cli::try_parse_from(argv).unwrap().args).unwrap();
        let launch = Launch::prepare(&args).unwrap().unwrap();
        assert!(launch.managed_mpv);
        assert_eq!(launch.sink, "movie");
        assert!(
            synchronization::Prepared::from_args(&args)
                .unwrap()
                .is_some()
        );
    }
}

#[test]
fn managed_options_cannot_escape_the_private_ipc_or_output_contract() {
    for option in [
        "--input-ipc-server=/other",
        "--input-ipc-client=fd://1",
        "--input-commands=set audio-device pulse/other",
        "--input-commands-append=load-script replacement.lua",
        "--input-conf=commands.conf",
        "--input-file=/other-input",
        "--audio-device=pulse/other",
        "-ao=alsa",
        "--no-config",
        "--config=yes",
        "--config-dir=/somewhere",
        "--include=config",
        "--use-filedir-conf=yes",
        "--profile=automatic",
        "--script=replace.lua",
        "--scripts-append=replace.lua",
        "--idle=yes",
        "--terminal=yes",
        "--no-input-terminal",
        "--fork",
        "--{",
    ] {
        assert!(
            validate(&[option.into(), "movie.mkv".into()]).is_err(),
            "{option}"
        );
    }
    assert!(validate(&[]).is_err());
    assert!(validate(&["--fullscreen".into()]).is_err());
    assert!(validate(&["--fullscreen".into(), "a movie.mkv".into()]).is_ok());
    assert!(validate(&["--".into(), "--audio-device=literal-filename".into()]).is_ok());
}

#[test]
fn private_ipc_and_forced_pulse_output_preserve_literal_media_arguments() {
    let launch = Launch {
        program: "mpv".into(),
        arguments: vec!["--fullscreen".into(), "a movie; $(literal).mkv".into()],
        sink: "movie".into(),
        managed_mpv: true,
    };
    let ipc = Ipc::new(&launch.sink).unwrap();
    let directory = ipc.directory.clone();
    let path = ipc.path.clone();
    assert_eq!(
        std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let command = ipc.command(&launch);
    assert_eq!(command.get_program(), "mpv");
    let arguments: Vec<_> = command.get_args().collect();
    assert_eq!(arguments[0], "--no-config");
    assert!(arguments.contains(&OsStr::new("--audio-device=pulse/movie")));
    assert_eq!(
        &arguments[arguments.len() - 2..],
        [
            OsStr::new("--fullscreen"),
            OsStr::new("a movie; $(literal).mkv")
        ]
    );
    assert_eq!(
        command.get_envs().collect::<Vec<_>>(),
        [(OsStr::new("PULSE_SINK"), Some(OsStr::new("movie")))]
    );
    let socket = UnixListener::bind(&path).unwrap();
    drop(socket);
    drop(ipc);
    assert!(!path.exists());
    assert!(!directory.exists());
}

#[cfg(target_os = "linux")]
#[test]
fn programmatic_arguments_cannot_skip_owned_launch_preconditions() {
    let cli = Cli::try_parse_from([
        "stream",
        "Room",
        "--virtual-output",
        "movie",
        "--sync-launched-mpv",
        "--sync-microphone",
        "room_mic",
        "--",
        "mpv",
        "movie.mkv",
    ])
    .unwrap();
    let args = virtual_output::prepare(&cli.args).unwrap();
    for alter in [
        |args: &mut StreamArgs| args.application.clear(),
        |args: &mut StreamArgs| args.virtual_output = None,
        |args: &mut StreamArgs| args.sync.sync_mpv = Some("/unused".into()),
        |args: &mut StreamArgs| args.sync.measure_latency = true,
        |args: &mut StreamArgs| args.sync.sync_microphone = None,
        |args: &mut StreamArgs| args.sync.sync_base_delay = f64::NAN,
    ] {
        let mut invalid = args.clone();
        alter(&mut invalid);
        assert!(
            Launch::prepare(&invalid)
                .and_then(|_| synchronization::Prepared::from_args(&invalid))
                .is_err()
        );
    }
}
