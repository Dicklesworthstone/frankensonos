//! An initial NOTIFY must contain the minimum state the live model consumes.
//! An empty but well-formed document is not a recovered subscription. Reject it
//! before applying anything so the reconciler requests fresh state and retains
//! its existing sequence/retry semantics. Ordinary deltas may omit fields.

use crate::HouseholdState;
use crate::events::{self, Service};
use crate::playback::{Changes, Playback};
use fsonos_proto::ProtoError;
use fsonos_proto::gena::Notify;
use fsonos_types::PlayerId;
use std::time::Instant;

pub(super) fn apply(
    households: &mut [HouseholdState],
    playback: &mut Playback,
    player: &PlayerId,
    service: Service,
    notify: &Notify,
    now: Instant,
) -> Result<Changes, ProtoError> {
    validate(service, notify)?;
    events::apply(households, playback, player, service, notify, now)
}

fn level(value: Option<&str>) -> bool {
    value
        .and_then(|value| value.trim().parse::<u8>().ok())
        .is_some_and(|volume| volume <= 100)
}

fn mute(value: Option<&str>) -> bool {
    value.is_some_and(|value| matches!(value.trim(), "0" | "1"))
}

fn validate(service: Service, notify: &Notify) -> Result<(), ProtoError> {
    if notify.seq != 0 {
        return Ok(());
    }
    let complete = match service {
        Service::AvTransport => notify.last_change()?.is_some_and(|change| {
            change
                .get("TransportState")
                .is_some_and(|state| !state.trim().is_empty())
                && change.get("CurrentTrackURI").is_some()
        }),
        Service::RenderingControl => notify
            .last_change()?
            .is_some_and(|change| level(change.get("Volume")) && mute(change.get("Mute"))),
        Service::GroupRenderingControl => {
            level(notify.property("GroupVolume")) && mute(notify.property("GroupMute"))
        }
        Service::ZoneGroupTopology => notify.property("ZoneGroupState").is_some(),
    };
    if complete {
        Ok(())
    } else {
        Err(ProtoError::Malformed(format!(
            "initial {} notification is missing usable state",
            service.tag()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{INITIAL_STATE_WAIT, RETRY, Subscriber, Want};
    use crate::reconcile::Reconciler;
    use fsonos_proto::gena::{Subscription, parse_propertyset};
    use std::cell::Cell;
    use std::time::Duration;

    fn notify(service: Service, seq: u32, properties: Vec<(String, String)>) -> Notify {
        Notify {
            sid: "uuid:initial-test-1".into(),
            seq,
            path: format!("/{}", service.tag()),
            properties,
        }
    }

    fn last_change(body: &str) -> Vec<(String, String)> {
        vec![(
            "LastChange".into(),
            format!("<Event><InstanceID val=\"0\">{body}</InstanceID></Event>"),
        )]
    }

    fn transport() -> Vec<(String, String)> {
        last_change("<TransportState val=\"STOPPED\"/><CurrentTrackURI val=\"\"/>")
    }

    fn rendering() -> Vec<(String, String)> {
        last_change("<Volume channel=\"Master\" val=\"17\"/><Mute channel=\"Master\" val=\"1\"/>")
    }

    #[test]
    fn initial_state_is_required_for_every_service_but_not_for_deltas() {
        for service in [
            Service::AvTransport,
            Service::RenderingControl,
            Service::GroupRenderingControl,
            Service::ZoneGroupTopology,
        ] {
            assert!(validate(service, &notify(service, 0, Vec::new())).is_err());
            assert!(validate(service, &notify(service, 1, Vec::new())).is_ok());
        }
    }

    #[test]
    fn empty_media_is_valid_but_an_empty_or_partial_last_change_is_not() {
        let service = Service::AvTransport;
        assert!(validate(service, &notify(service, 0, transport())).is_ok());
        for body in [
            "",
            "<TransportState val=\"PLAYING\"/>",
            "<CurrentTrackURI val=\"test:track\"/>",
            "<TransportState val=\"\"/><CurrentTrackURI val=\"\"/>",
        ] {
            let initial = notify(service, 0, last_change(body));
            assert!(validate(service, &initial).is_err());
        }
    }

    #[test]
    fn initial_rendering_needs_master_levels_while_volume_deltas_remain_valid() {
        let service = Service::RenderingControl;
        assert!(validate(service, &notify(service, 0, rendering())).is_ok());
        for body in [
            "<Volume channel=\"Master\" val=\"17\"/>",
            "<Mute channel=\"Master\" val=\"1\"/>",
            "<Volume channel=\"LF\" val=\"17\"/><Mute channel=\"RF\" val=\"1\"/>",
            "<Volume channel=\"Master\" val=\"101\"/><Mute channel=\"Master\" val=\"1\"/>",
            "<Volume channel=\"Master\" val=\"17\"/><Mute channel=\"Master\" val=\"?\"/>",
        ] {
            assert!(validate(service, &notify(service, 0, last_change(body))).is_err());
        }
        let values = last_change("<Volume channel=\"Master\" val=\"9\"/>");
        assert!(validate(service, &notify(service, 1, values)).is_ok());
    }

    #[test]
    fn group_initials_need_valid_levels_and_topology_needs_its_document() {
        let service = Service::GroupRenderingControl;
        for (volume, muted, valid) in [("17", "1", true), ("101", "1", false), ("17", "?", false)] {
            let event = notify(
                service,
                0,
                vec![
                    ("GroupVolume".into(), volume.into()),
                    ("GroupMute".into(), muted.into()),
                ],
            );
            assert_eq!(validate(service, &event).is_ok(), valid);
        }
        let service = Service::ZoneGroupTopology;
        let partial = vec![("AvailableSoftwareUpdate".into(), String::new())];
        assert!(validate(service, &notify(service, 0, partial.clone())).is_err());
        assert!(validate(service, &notify(service, 1, partial)).is_ok());
        let full = vec![("ZoneGroupState".into(), "<ZoneGroupState/>".into())];
        assert!(validate(service, &notify(service, 0, full)).is_ok());
    }

    #[test]
    fn captured_initial_notifications_keep_their_existing_contract() {
        let avt_s1 =
            include_str!("../../../fsonos-proto/tests/fixtures/gena_notify_avt_initial_s1.xml");
        let avt_s2 =
            include_str!("../../../fsonos-proto/tests/fixtures/gena_notify_avt_initial_s2.xml");
        let rendering =
            include_str!("../../../fsonos-proto/tests/fixtures/gena_notify_rcs_initial_s1.xml");
        let topology = include_str!("../../../fsonos-proto/tests/fixtures/gena_notify_zgt_s1.xml");
        for (service, body) in [
            (Service::AvTransport, avt_s1),
            (Service::AvTransport, avt_s2),
            (Service::RenderingControl, rendering),
            (Service::ZoneGroupTopology, topology),
        ] {
            let event = notify(service, 0, parse_propertyset(body).unwrap());
            validate(service, &event).unwrap();
        }
    }

    #[derive(Default)]
    struct SubscriberProbe {
        calls: Cell<u32>,
        refuse: Cell<bool>,
    }

    impl Subscriber for SubscriberProbe {
        fn subscribe(&self, _: &Want, _: &str) -> Result<Subscription, ProtoError> {
            self.calls.set(self.calls.get() + 1);
            if self.refuse.get() {
                return Err(ProtoError::Malformed("test subscription refusal".into()));
            }
            Ok(Subscription {
                sid: format!("uuid:initial-test-{}", self.calls.get()),
                timeout_secs: 300,
            })
        }

        fn renew(&self, _: &Want, _: &str) -> Result<Subscription, ProtoError> {
            panic!("incomplete initial state must resubscribe, not renew")
        }

        fn unsubscribe(&self, _: &Want, _: &str) -> Result<(), ProtoError> {
            Ok(())
        }
    }

    fn callback(service: Service) -> String {
        format!("http://127.0.0.1:3400/{}", service.tag())
    }

    #[test]
    fn incomplete_initial_recovery_preserves_state_sequence_and_retry_backoff() {
        let now = Instant::now();
        let service = Service::RenderingControl;
        let player = PlayerId("initial-test-player".into());
        let subscriber = SubscriberProbe::default();
        let mut rec = Reconciler::new(RETRY, RETRY, now);
        let want = Want {
            player: player.clone(),
            host: std::net::Ipv4Addr::LOCALHOST.into(),
            service,
        };
        rec.subscriptions.sync(&subscriber, &[want], callback, now);
        let mut playback = Playback::default();
        let good = notify(service, 0, rendering());
        events::apply(&mut [], &mut playback, &player, service, &good, now).unwrap();
        let before = playback.of(&player).unwrap().clone();
        let values = last_change("<Volume channel=\"Master\" val=\"55\"/>");
        let incomplete = notify(service, 0, values);
        subscriber.refuse.set(true);
        for seconds in 0..3 {
            let at = now + Duration::from_secs(seconds);
            assert!(
                rec.on_notify(&subscriber, &mut playback, &incomplete, callback, at)
                    .is_err()
            );
            assert_eq!(playback.of(&player), Some(&before));
            assert!(rec.subscriptions.route(&incomplete).is_some());
            assert_eq!(subscriber.calls.get(), 2);
            assert_eq!(rec.subscriptions.next_due(), Some(now + RETRY));
        }
        // A corrected same-SEQ event still applies while recovery is pending.
        rec.on_notify(&subscriber, &mut playback, &good, callback, now)
            .unwrap();
        assert!(rec.subscriptions.route(&good).is_none());
        subscriber.refuse.set(false);
        let recovered = rec
            .subscriptions
            .renew_due(&subscriber, callback, now + RETRY);
        assert_eq!(recovered.resubscribed, 1);
        assert_eq!(
            rec.subscriptions.next_due(),
            Some(now + RETRY + INITIAL_STATE_WAIT)
        );
    }

    #[test]
    fn real_initial_events_recover_after_incomplete_delivery_on_both_generations() {
        use fsonos_proto::control::{set_mute, set_volume};
        use fsonos_proto::net::{EventSink, Lan};
        use fsonos_proto::topology::get_zone_group_state;
        use fsonos_sim::{GenaEvent, SimHousehold, SimModel, SimPlayerSpec};
        use std::collections::HashSet;
        use std::net::SocketAddr;

        for generation in [1, 2] {
            let builder = SimHousehold::builder();
            let sim = match generation {
                1 => builder.s1([SimPlayerSpec::new("Room", SimModel::Play5Gen1)]),
                _ => builder.s2([SimPlayerSpec::new("Room", SimModel::One)]),
            }
            .spawn()
            .unwrap();
            let lan = Lan::start()
                .unwrap()
                .with_routes(sim.players().iter().map(|p| (p.ip, p.addr)).collect());
            let host = sim.player("Room").unwrap().ip;
            set_volume(&lan, host, 17).unwrap();
            set_mute(&lan, host, true).unwrap();
            let mut house = HouseholdState::default();
            house.apply_topology(&get_zone_group_state(&lan, host).unwrap());
            let now = Instant::now();
            let mut rec = Reconciler::new(RETRY, RETRY, now);
            rec.households.push(house);
            let wants = events::wanted(&rec.households);
            let player = wants[0].player.clone();
            let local = lan.local_address_toward(host).unwrap();
            let sink = EventSink::start(SocketAddr::new(local, 0)).unwrap();
            let callback = |service: Service| sink.callback_url(service.tag());
            let report = rec.subscriptions.sync(&lan, &wants, callback, now);
            assert_eq!(report.subscribed, 4);
            assert!(report.failed.is_empty());
            let mut playback = Playback::default();
            let mut accepted = HashSet::new();
            let mut rejected = false;
            let deadline = Instant::now() + Duration::from_secs(5);
            while accepted.len() < wants.len() {
                assert!(Instant::now() < deadline, "initial state did not recover");
                let Some(mut event) = sink.recv_timeout(Duration::from_millis(100)) else {
                    continue;
                };
                let Some((_, service)) = rec.subscriptions.route(&event) else {
                    continue;
                };
                if service == Service::RenderingControl && !rejected {
                    event.properties.clear();
                    assert!(
                        rec.on_notify(&lan, &mut playback, &event, callback, now)
                            .is_err()
                    );
                    assert!(rec.subscriptions.route(&event).is_none());
                    assert!(
                        playback
                            .of(&player)
                            .and_then(|state| state.volume)
                            .is_none()
                    );
                    rejected = true;
                    continue;
                }
                rec.on_notify(&lan, &mut playback, &event, callback, now)
                    .unwrap()
                    .unwrap();
                accepted.insert(service);
            }
            assert!(rejected);
            let state = playback.of(&player).unwrap();
            assert_eq!(state.volume, Some(17));
            assert_eq!(state.mute, Some(true));
            assert!(state.transport.is_some());
            assert!(state.group_volume.is_some());
            let subscriptions = sim
                .gena_log()
                .iter()
                .filter(|entry| matches!(entry.event, GenaEvent::Subscribed { .. }))
                .count();
            assert_eq!(subscriptions, 5, "only the incomplete service was replaced");
            rec.subscriptions.unsubscribe_all(&lan);
            sim.shutdown();
        }
    }

    #[test]
    fn another_instances_initial_or_delta_cannot_change_current_playback() {
        let now = Instant::now();
        let player = PlayerId("instance-test-player".into());
        for service in [Service::AvTransport, Service::RenderingControl] {
            let values = if service == Service::AvTransport {
                transport()
            } else {
                rendering()
            };
            let good = notify(service, 0, values);
            let mut playback = Playback::default();
            apply(&mut [], &mut playback, &player, service, &good, now).unwrap();
            let before = playback.of(&player).unwrap().clone();
            // Conflicting values would visibly alter state if instance seven
            // were accidentally applied. Repeating the current values would
            // not test that the delta is actually isolated.
            let conflicting = if service == Service::AvTransport {
                "<TransportState val=\"PLAYING\"/><CurrentTrackURI val=\"test:other\"/>"
            } else {
                "<Volume channel=\"Master\" val=\"55\"/><Mute channel=\"Master\" val=\"0\"/>"
            };
            let mut other = notify(service, 0, last_change(conflicting));
            let body = &mut other.properties[0].1;
            *body = body.replacen("val=\"0\"", "val=\"7\"", 1);
            assert!(apply(&mut [], &mut playback, &player, service, &other, now).is_err());
            assert_eq!(playback.of(&player), Some(&before));
            other.seq = 1;
            let changes = apply(&mut [], &mut playback, &player, service, &other, now).unwrap();
            assert!(changes.is_empty());
            assert_eq!(playback.of(&player), Some(&before));
        }
    }

    #[test]
    fn ambiguous_instance_deltas_preserve_state_and_accept_a_corrected_sequence() {
        let now = Instant::now();
        let service = Service::RenderingControl;
        let player = PlayerId("duplicate-instance-player".into());
        let subscriber = SubscriberProbe::default();
        let mut rec = Reconciler::new(RETRY, RETRY, now);
        let want = Want {
            player: player.clone(),
            host: std::net::Ipv4Addr::LOCALHOST.into(),
            service,
        };
        rec.subscriptions.sync(&subscriber, &[want], callback, now);
        let mut playback = Playback::default();
        let good = notify(service, 0, rendering());
        rec.on_notify(&subscriber, &mut playback, &good, callback, now)
            .unwrap();
        let before = playback.of(&player).unwrap().clone();
        let changed = "<Volume channel=\"Master\" val=\"55\"/>";
        let mut ambiguous = notify(service, 1, last_change(changed));
        let body = &mut ambiguous.properties[0].1;
        let duplicate = "<InstanceID val=\"0\"/></Event>";
        *body = body.replace("</Event>", duplicate);
        subscriber.refuse.set(true);
        assert!(
            rec.on_notify(&subscriber, &mut playback, &ambiguous, callback, now)
                .is_err()
        );
        assert_eq!(playback.of(&player), Some(&before));
        assert!(rec.subscriptions.route(&ambiguous).is_some());
        assert_eq!(rec.subscriptions.next_due(), Some(now + RETRY));
        let corrected = notify(service, 1, last_change(changed));
        let report = rec
            .on_notify(&subscriber, &mut playback, &corrected, callback, now)
            .unwrap()
            .unwrap();
        assert!(!report.changes.is_empty());
        assert_eq!(playback.of(&player).unwrap().volume, Some(55));
        assert_eq!(playback.of(&player).unwrap().mute, Some(true));
        assert!(rec.subscriptions.route(&ambiguous).is_none());
        assert_eq!(subscriber.calls.get(), 2, "no extra retry");
    }
}
