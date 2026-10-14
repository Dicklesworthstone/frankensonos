//! Drive the CLI's shared playback-order actions on virtual S1/S2 households.

mod e2e;

use e2e::Scenario;
use fsonos_api::surface::queue::{PlayModeDto, PlayModeSetDto};
use fsonos_proto::content::{QUEUE, browse_snapshot};
use fsonos_proto::control as soap;
use fsonos_proto::play_mode::get_play_mode;
use fsonos_sim::{SimHousehold, SimModel, SimPlayerSpec};
use fsonos_types::{PlayMode, PlayerId};
use std::time::Duration;

fn setup(name: &str, s2: bool) -> Scenario {
    let mut s = Scenario::start(name);
    let model = if s2 {
        SimModel::One
    } else {
        SimModel::Play5Gen1
    };
    let players = [
        SimPlayerSpec::new("Lead", model),
        SimPlayerSpec::new("Member", model),
    ];
    let builder = SimHousehold::builder();
    s.sim(if s2 {
        builder.s2(players)
    } else {
        builder.s1(players)
    });
    let lan = s.lan();
    let lead = s.ip("Lead");
    let uuid = s.sim_handle().unwrap().player("Lead").unwrap().uuid.clone();
    for title in ["a", "b", "c"] {
        soap::enqueue_uri(
            &lan,
            lead,
            &format!("x-file-cifs://example.invalid/{title}.flac"),
            "",
            0,
            false,
        )
        .unwrap();
    }
    soap::set_av_transport_uri(&lan, s.ip("Member"), &format!("x-rincon:{uuid}"), "").unwrap();
    soap::play_from_queue(&lan, lead, &PlayerId(uuid)).unwrap();
    soap::seek_track(&lan, lead, 2).unwrap();
    soap::play(&lan, lead).unwrap();
    soap::pause(&lan, lead).unwrap();
    s
}

fn modes(s: &mut Scenario, route: &str, room: &str) {
    let lan = s.lan();
    let host = s.ip("Lead");
    let position = soap::get_position_info(&lan, host).unwrap();
    let source = soap::get_media_info(&lan, host).unwrap();
    let queue = browse_snapshot(&lan, host, QUEUE).unwrap();
    let transport = soap::get_transport_info(&lan, host).unwrap();
    let mut previous = get_play_mode(&lan, host).unwrap();

    for mode in PlayMode::ALL {
        let read = s.cli("mode-read", &[route, "--json", "play-mode", room]);
        assert!(read.ok(), "{}", read.stderr);
        let observed: PlayModeDto = serde_json::from_str(&read.stdout).unwrap();
        assert_eq!(observed.mode, previous.as_str());
        assert!(observed.queue_active);
        let changed = s.cli(
            "mode-set",
            &[
                route,
                "--json",
                "play-mode",
                room,
                mode.as_str(),
                "--expected-mode",
                previous.as_str(),
            ],
        );
        assert!(changed.ok(), "{}", changed.stderr);
        let result: PlayModeSetDto = serde_json::from_str(&changed.stdout).unwrap();
        s.check(
            "mode-set",
            "cli",
            "the shared mode changes on the coordinator without changing playback",
            result.state.mode == mode.as_str()
                && result.changed == (mode != previous)
                && result.state.queue_active
                && get_play_mode(&lan, host).unwrap() == mode
                && soap::get_position_info(&lan, host).unwrap() == position
                && soap::get_media_info(&lan, host).unwrap() == source
                && soap::get_transport_info(&lan, host).unwrap() == transport
                && browse_snapshot(&lan, host, QUEUE).unwrap() == queue,
            &changed.stdout,
        );
        previous = mode;
    }

    // Last mode is not NORMAL: a stale guard must not normalize the queue.
    let stale = s.cli(
        "stale-mode",
        &[
            route,
            "play-mode",
            room,
            "NORMAL",
            "--expected-mode",
            "NORMAL",
        ],
    );
    s.check(
        "stale-mode",
        "cli",
        "a stale expected mode is propagated without direct retry",
        !stale.ok() && get_play_mode(&lan, host).unwrap() == previous,
        &stale.stderr,
    );
    let radio = "x-rincon-mp3radio://example.invalid/live.mp3";
    soap::set_av_transport_uri(&lan, host, radio, "").unwrap();
    let refused = s.cli("non-queue", &[route, "play-mode", room, "REPEAT_ALL"]);
    s.check(
        "non-queue",
        "cli",
        "mode writes never select the queue to make another source writable",
        !refused.ok() && soap::get_media_info(&lan, host).unwrap().uri == radio,
        &refused.stderr,
    );
}

#[test]
fn direct_modes_preserve_paused_playback_on_both_generations() {
    for s2 in [false, true] {
        let mut s = setup(
            if s2 {
                "play-mode-direct-s2"
            } else {
                "play-mode-direct-s1"
            },
            s2,
        );
        modes(
            &mut s,
            "--direct",
            if s2 { "Member@S2" } else { "Member@S1" },
        );
        s.finish();
    }
}

#[test]
fn daemon_modes_use_the_same_actions_and_log() {
    let mut s = setup("play-mode-daemon", true);
    let mut daemon = s.spawn("serve", &["serve", "--http", "127.0.0.1:0"]);
    assert!(
        daemon
            .wait_line("fsonos serve: live", Duration::from_secs(20))
            .is_some()
    );
    modes(&mut s, "--daemon", "Member@S2");
    let log = s.cli("mode-log", &["--daemon", "--json", "log", "--limit", "20"]);
    s.check(
        "mode-log",
        "cli",
        "the daemon owns the playback-order action log",
        log.ok() && log.stdout.contains("set_play_mode"),
        format!("{} {}", log.stdout, log.stderr),
    );
    drop(daemon);
    s.finish();
}
