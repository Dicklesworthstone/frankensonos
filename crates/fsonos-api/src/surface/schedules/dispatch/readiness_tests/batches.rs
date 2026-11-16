//! Scheduled ordinary controls must not report partial delivery as success.

use super::*;
use fsonos_core::policy::Client;
use fsonos_core::store::ActionFilter;

#[test]
fn a_partial_scheduled_pause_or_volume_reports_failure_without_reopening_the_occurrence() {
    for s2 in [false, true] {
        for (action, verb) in [
            (
                ScheduleAction::Pause {
                    room: "all".into(),
                    fade_secs: 0,
                },
                "Pause",
            ),
            (
                ScheduleAction::Volume {
                    room: "all".into(),
                    level: 10,
                },
                "SetVolume",
            ),
        ] {
            let h = House::new(s2, &action);
            for room in ["Room0", "Room1"] {
                let ip = h.sim.player(room).unwrap().ip;
                soap::set_volume(&h.sim.lan(), ip, 25).unwrap();
                soap::set_av_transport_uri(
                    &h.sim.lan(),
                    ip,
                    "x-rincon-mp3radio://stream.example.invalid/schedule.mp3",
                    "",
                )
                .unwrap();
                soap::play(&h.sim.lan(), ip).unwrap();
            }
            h.sim.upnp_fault("Room1", verb, 701).unwrap();
            let schedule = h.surface.load_schedules().unwrap().remove(0);
            let prepared = ready(&h.surface, &schedule).unwrap();
            assert_eq!(
                claim_current(&h.surface, &schedule, h.at, || Admission::Run).unwrap(),
                Admission::Run
            );
            let before = h.sim.soap_log().len();
            let failure = fire(&h.surface, &schedule, None, prepared).unwrap_err();
            assert_eq!(failure.code, ErrorCode::OutcomeUncertain);
            assert!(failure.detail.contains("incomplete for 1 target(s)"));
            assert!(failure.detail.contains("Room1@"));
            assert!(failure.detail.contains("UPNP_FAULT"));
            assert!(failure.cli_text().contains("will not be retried"));
            assert_eq!(h.recorded(), Some(h.at));
            let sent: Vec<_> = h.sim.soap_log()[before..]
                .iter()
                .filter(|call| call.action == verb)
                .map(|call| call.room.clone())
                .collect();
            assert_eq!(sent.len(), 2);
            assert!(sent.contains(&"Room0".to_owned()));
            assert!(sent.contains(&"Room1".to_owned()));
            let ip = h.sim.player("Room0").unwrap().ip;
            if verb == "Pause" {
                assert_eq!(
                    soap::get_transport_info(&h.sim.lan(), ip).unwrap().state,
                    TransportState::Paused
                );
            } else {
                assert_eq!(soap::get_volume(&h.sim.lan(), ip).unwrap(), 10);
            }
            let logged = h
                .surface
                .recent_actions(&Client::Cli, &ActionFilter::default())
                .unwrap();
            assert_eq!(logged.len(), 1);
            assert!(logged[0].action.result.contains("Room1@"));
            assert!(logged[0].action.result.contains("UPNP_FAULT"));
            let after = h.sim.soap_log().len();
            let scheduler = Scheduler::new(&h.surface);
            for minute in 1..4 {
                assert!(
                    h.tick(&scheduler, h.at + chrono::TimeDelta::minutes(minute))
                        .is_empty()
                );
            }
            assert_eq!(h.sim.soap_log().len(), after);
        }
    }
}
