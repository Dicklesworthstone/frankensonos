//! Identity repair must survive the adapters used by higher-level commands.
//! Every mutation still goes through the real S1/S2 simulator; a lost reply
//! is injected only after the simulator acknowledges the requested write.

use super::*;
use fsonos_core::audio_inputs;
use fsonos_core::fade::Fader;
use fsonos_core::scenes::{self, ApplyOptions};
use fsonos_core::sleep::SleepTimers;
use fsonos_core::snapshot;
use fsonos_proto::content::{QUEUE, browse_snapshot};

struct LostReply<'a> {
    inner: &'a Evidence,
    verb: &'static str,
}

impl Transport for LostReply<'_> {
    fn http_get(&self, url: &str) -> Result<String, ProtoError> {
        self.inner.http_get(url)
    }

    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let response = self.inner.soap_post(host, path, action, body)?;
        let name = action
            .trim_matches('"')
            .rsplit('#')
            .next()
            .unwrap_or(action);
        if name != self.verb {
            return Ok(response);
        }
        parse_response(&response, name)?;
        Err(ProtoError::Network {
            target: "synthetic player".into(),
            detail: "the accepted command lost its reply".into(),
        })
    }
}

fn select(f: &Fixture, track: u32) {
    let lan = f.sim.lan();
    soap::play_from_queue(&lan, f.host, &f.target).unwrap();
    soap::seek_track(&lan, f.host, track).unwrap();
    soap::seek_position(&lan, f.host, 40).unwrap();
    soap::play(&lan, f.host).unwrap();
}

#[test]
fn navigation_recovers_a_lost_reply_with_stale_advertised_addresses() {
    for generation in [1, 2] {
        for next in [true, false] {
            let f = Fixture::new(generation);
            select(&f, if next { 1 } else { 2 });
            let evidence = f.evidence();
            let verb = if next { "Next" } else { "Previous" };
            let t = LostReply {
                inner: &evidence,
                verb,
            };
            let marker = f.sim.soap_log().len();
            if next {
                control::next(&t, &f.repaired(), &f.target).unwrap();
            } else {
                control::previous(&t, &f.repaired(), &f.target).unwrap();
            }
            assert_eq!(writes(&f.sim, marker), [("Target".into(), verb.into())]);
            let position = soap::get_position_info(&f.sim.lan(), f.host).unwrap();
            assert_eq!(position.track, if next { 2 } else { 1 });
            assert_eq!(
                soap::get_media_info(&f.sim.lan(), f.peer).unwrap().uri,
                ORIGINAL
            );
            assert!(evidence.descriptions.borrow().len() >= 3);
        }
    }
}

#[test]
fn absolute_and_relative_seek_recovery_keep_the_original_endpoint_proof() {
    for generation in [1, 2] {
        for seek in [control::Seek::At(60), control::Seek::By(20)] {
            let f = Fixture::new(generation);
            select(&f, 1);
            soap::pause(&f.sim.lan(), f.host).unwrap();
            let evidence = f.evidence();
            let t = LostReply {
                inner: &evidence,
                verb: "Seek",
            };
            let marker = f.sim.soap_log().len();
            assert_eq!(
                control::seek(&t, &f.repaired(), &f.target, seek).unwrap(),
                60
            );
            assert_eq!(writes(&f.sim, marker), [("Target".into(), "Seek".into())]);
            let position = soap::get_position_info(&f.sim.lan(), f.host).unwrap();
            assert_eq!((position.track, position.position_secs), (1, Some(60)));
            assert_eq!(
                soap::get_transport_info(&f.sim.lan(), f.host)
                    .unwrap()
                    .state,
                TransportState::Paused
            );
            assert!(evidence.descriptions.borrow().len() >= 3);
        }
    }
}

#[test]
fn lost_native_sleep_timer_reply_is_confirmed_without_replacing_it_twice() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let evidence = f.evidence();
        let t = LostReply {
            inner: &evidence,
            verb: "ConfigureSleepTimer",
        };
        let timers = SleepTimers::default();
        let now = chrono::DateTime::parse_from_rfc3339("2026-11-26T22:00:00Z")
            .unwrap()
            .to_utc();
        let marker = f.sim.soap_log().len();
        let timer = timers
            .start(
                &t,
                &f.repaired(),
                &Fader::default(),
                &f.target,
                Duration::from_secs(600),
                now,
            )
            .unwrap();
        assert_eq!(timer.ends, now + chrono::TimeDelta::seconds(600));
        assert_eq!(
            writes(&f.sim, marker),
            [("Target".into(), "ConfigureSleepTimer".into())]
        );
        assert!(
            soap::get_sleep_timer_info(&f.sim.lan(), f.host)
                .unwrap()
                .remaining_secs
                .is_some_and(|remaining| remaining >= 600)
        );
    }
}

#[test]
fn line_in_playback_keeps_identity_repair_and_rejects_a_later_invalid_description() {
    for generation in [1, 2] {
        for invalid in [false, true] {
            let f = Fixture::new(generation);
            let mut t = f.evidence();
            let houses = f.repaired();
            let prepared = audio_inputs::prepare(&t, &houses, &f.target, &f.peer_id).unwrap();
            let before = browse_snapshot(&f.sim.lan(), f.host, QUEUE).unwrap();
            if invalid {
                t.fault = DescriptionFault::DuplicateUdn;
            }
            let marker = f.sim.soap_log().len();
            let result = audio_inputs::play_prepared(&t, &houses, &f.target, &prepared);
            if invalid {
                assert!(result.is_err());
                assert!(writes(&f.sim, marker).is_empty());
                assert_eq!(
                    soap::get_media_info(&f.sim.lan(), f.host).unwrap().uri,
                    ORIGINAL
                );
            } else {
                result.unwrap();
                assert_eq!(
                    writes(&f.sim, marker),
                    [
                        ("Target".into(), "SetAVTransportURI".into()),
                        ("Target".into(), "Play".into())
                    ]
                );
                assert_eq!(
                    soap::get_media_info(&f.sim.lan(), f.host).unwrap().uri,
                    format!("x-rincon-stream:{}", f.peer_id.0)
                );
            }
            assert_eq!(
                browse_snapshot(&f.sim.lan(), f.host, QUEUE).unwrap(),
                before
            );
            assert_eq!(
                soap::get_media_info(&f.sim.lan(), f.peer).unwrap().uri,
                ORIGINAL
            );
        }
    }
}

#[test]
fn saved_queue_recall_and_reconstruction_work_through_nested_identity_adapters() {
    for generation in [1, 2] {
        for backup in [false, true] {
            let f = Fixture::new(generation);
            let lan = f.sim.lan();
            let houses = f.repaired();
            select(&f, 2);
            let scene = if backup {
                scenes::capture_with_queue_backups(&lan, &houses, "Saved queue", &[], 1)
            } else {
                scenes::capture(&lan, &houses, "Saved queue", &[], 1)
            }
            .unwrap();
            if backup {
                soap::remove_all_tracks_from_queue(&lan, f.host).unwrap();
                soap::add_uri_to_queue(&lan, f.host, WANTED, "", false).unwrap();
            }
            soap::set_av_transport_uri(&lan, f.host, ORIGINAL, "").unwrap();
            soap::play(&lan, f.host).unwrap();
            let snapshots: Vec<_> = scenes::zones(&scene, &houses)
                .iter()
                .map(|id| snapshot::capture(&lan, &houses, id, 2).unwrap())
                .collect();
            let plan = scenes::plan_apply(&scene, &houses, &snapshots).unwrap();
            let t = f.evidence();
            let marker = f.sim.soap_log().len();
            let report = scenes::apply(&t, &houses, &plan, &ApplyOptions::default());
            assert!(
                report.is_complete(),
                "S{generation}, backup={backup}: {report:?}"
            );
            let queue = control::queue_removal::read(&lan, f.host).unwrap();
            assert_eq!(queue.uris, TRACKS);
            let position = soap::get_position_info(&lan, f.host).unwrap();
            assert_eq!((position.track, position.position_secs), (2, Some(40)));
            assert_eq!(
                soap::get_transport_info(&lan, f.host).unwrap().state,
                TransportState::Playing
            );
            let changed = writes(&f.sim, marker);
            assert!(changed.iter().all(|(room, _)| room == "Target"));
            assert!(
                changed
                    .iter()
                    .all(|(_, verb)| verb != "RemoveAllTracksFromQueue")
            );
            assert_eq!(soap::get_media_info(&lan, f.peer).unwrap().uri, ORIGINAL);
        }
    }
}
