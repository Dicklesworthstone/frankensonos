use super::*;
use crate::{Cli, Command};
use clap::{CommandFactory, Parser};

fn parsed(args: &[&str]) -> Result<Cli, clap::Error> {
    Cli::try_parse_from(
        ["fsonos", "play-mode"]
            .into_iter()
            .chain(args.iter().copied()),
    )
}

fn arguments(args: &[&str]) -> PlayModeArgs {
    let Command::PlayMode(args) = parsed(args).unwrap().command else {
        panic!("play-mode")
    };
    args
}

#[test]
fn a_room_without_a_mode_is_a_read_not_a_normalization() {
    let args = arguments(&["Room@S1"]);
    assert_eq!(args.zone, "Room@S1");
    assert!(args.request().is_none());
    assert_eq!(args.read_path(), "/zones/Room%40S1/play-mode");
}

#[test]
fn each_shared_mode_constructs_the_existing_request_without_an_implicit_toggle() {
    for mode in PlayMode::ALL {
        let request = arguments(&["Room@S2", mode.as_str()]).request().unwrap();
        assert_eq!(request.zone, "Room@S2");
        assert_eq!(request.mode, mode.as_str());
        assert_eq!(request.expected_mode, None);
    }
}

#[test]
fn every_requested_and_expected_mode_pair_is_forwarded_unchanged() {
    for wanted in PlayMode::ALL {
        for before in PlayMode::ALL {
            let request = arguments(&[
                "Member@S2",
                wanted.as_str(),
                "--expected-mode",
                before.as_str(),
            ])
            .request()
            .unwrap();
            assert_eq!(request.mode, wanted.as_str());
            assert_eq!(request.expected_mode.as_deref(), Some(before.as_str()));
        }
    }
}

#[test]
fn an_expected_mode_cannot_turn_a_read_into_an_unrequested_write() {
    assert!(parsed(&["Room", "--expected-mode", "NORMAL"]).is_err());
    assert!(parsed(&[]).is_err());
    assert!(parsed(&[""]).is_err());
    assert!(parsed(&[" \t"]).is_err());
}

#[test]
fn unknown_and_inexact_mode_names_do_not_contact_any_backend() {
    for value in [
        "", "normal", "shuffle", "SHUFFLE_NO_REPEAT", "OFF", "NOT_IMPLEMENTED", " NORMAL ",
    ] {
        assert!(parsed(&["Room", value]).is_err());
        assert!(parsed(&["Room", "NORMAL", "--expected-mode", value]).is_err());
    }
}

#[test]
fn the_read_path_cannot_split_the_callers_room_into_a_path_or_query() {
    let args = arguments(&["Room / O'Brien?x=1@S2"]);
    assert_eq!(
        args.read_path(),
        "/zones/Room%20%2F%20O%27Brien%3Fx%3D1%40S2/play-mode"
    );
}

#[test]
fn global_routing_and_json_options_remain_available() {
    let cli = parsed(&[
        "Room",
        "REPEAT_ONE",
        "--json",
        "--daemon-url",
        "http://127.0.0.1:9999",
    ])
    .unwrap();
    assert!(cli.global.json);
    assert_eq!(
        cli.global.daemon_url.as_deref(),
        Some("http://127.0.0.1:9999")
    );
    assert!(parsed(&["Room", "--direct"]).unwrap().global.direct);
    assert!(parsed(&["Room", "--daemon"]).unwrap().global.daemon);
}

#[test]
fn parser_shape_is_valid_and_help_lists_every_shared_mode() {
    Cli::command().debug_assert();
    let error = parsed(&["--help"]).err().unwrap();
    let help = error.to_string();
    for mode in PlayMode::ALL {
        assert!(help.contains(mode.as_str()), "missing {}", mode.as_str());
    }
}
