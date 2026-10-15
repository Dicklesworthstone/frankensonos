//! Read-only recovery of playback when GENA did not deliver a transition.

use super::{Engine, LiveEvent, Model};
use crate::playback::PlaybackObservation;
use crate::reconcile::Health;
use fsonos_proto::control::{get_media_info, get_position_info, get_transport_info};
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::PlayerId;
use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::atomic::Ordering;
use std::time::Instant;

/// Current coordinators only. Never ask an offline room, or mistake a
/// member's transport state for that of the group it follows.
pub(super) fn coordinators(model: &Model) -> Vec<(PlayerId, IpAddr)> {
    let mut seen = HashSet::new();
    let mut players = Vec::new();
    for household in &model.rec.households {
        for group in &household.groups {
            let Some(player) = household.player(&group.coordinator) else {
                continue;
            };
            if model
                .rec
                .health
                .of(&player.id)
                .is_some_and(|h| h.health == Health::Offline)
                || !seen.insert(player.id.clone())
            {
                continue;
            }
            players.push((player.id.clone(), player.ip));
        }
    }
    players
}

/// UPnP offers separate reads, not a transactional playback snapshot.
/// Bracket the sample with transport and source reads and reject observed
/// transitions rather than joining a new clock to an old play/pause state.
/// Nothing is applied until all reads succeed; stop is checked between calls.
pub(super) fn read<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
    stopping: impl Fn() -> bool,
) -> Result<Option<(PlaybackObservation, Instant)>, ProtoError> {
    if stopping() {
        return Ok(None);
    }
    let before = get_transport_info(transport, host)?;
    if stopping() {
        return Ok(None);
    }
    let media = get_media_info(transport, host)?;
    if stopping() {
        return Ok(None);
    }
    let position = get_position_info(transport, host)?;
    let observed = Instant::now();
    if stopping() {
        return Ok(None);
    }
    let current_media = get_media_info(transport, host)?;
    if stopping() {
        return Ok(None);
    }
    let after = get_transport_info(transport, host)?;
    if stopping() {
        return Ok(None);
    }
    if before != after
        || media.uri != current_media.uri
        || media.tracks != current_media.tracks
        || (media.uri.starts_with("x-rincon-queue:") && position.track > media.tracks)
    {
        return Err(ProtoError::Malformed(
            "playback changed during read-back; keeping the previous observation".into(),
        ));
    }
    Ok(Some((
        PlaybackObservation {
            transport: after,
            position,
            media,
        },
        observed,
    )))
}

/// The existing low-frequency survey also repairs lost pause, resume, stop
/// and queue notifications, publishing changes for the DJ and API consumers.
pub(super) fn survey(engine: &Engine, model: &mut Model) -> Vec<LiveEvent> {
    let mut events = Vec::new();
    for (player, host) in coordinators(model) {
        let read = read(&engine.via(), host, || engine.stop.load(Ordering::Acquire));
        match read {
            Ok(Some((observation, observed))) => {
                let changes = model
                    .playback
                    .apply_observation(&player, &observation, observed);
                if !changes.is_empty() {
                    events.push(LiveEvent::Playback { player, changes });
                }
            }
            Ok(None) => break,
            Err(error) => tracing::debug!(%error, "playback read-back failed; retaining live state"),
        }
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HouseholdState;
    use crate::live::{LiveConfig, Snapshot};
    use crate::playback::Playback;
    use crate::reconcile::Reconciler;
    use fsonos_proto::control::{pause, play, set_av_transport_uri, stop};
    use fsonos_proto::net::Lan;
    use fsonos_proto::topology::get_zone_group_state;
    use fsonos_sim::{SimHousehold, SimModel, SimPlayerSpec};
    use fsonos_types::TransportState;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, SystemTime};

    #[test]
    fn survey_recovers_unreported_transport_changes_on_both_generations() {
        for generation in [1, 2] {
            let builder = SimHousehold::builder();
            let sim = match generation {
                1 => builder.s1([SimPlayerSpec::new("Room", SimModel::Play5Gen1)]),
                _ => builder.s2([SimPlayerSpec::new("Room", SimModel::One)]),
            }
            .spawn()
            .unwrap();
            let host = sim.player("Room").unwrap().ip;
            let lan = Arc::new(
                Lan::start()
                    .unwrap()
                    .with_routes(sim.players().iter().map(|p| (p.ip, p.addr)).collect()),
            );
            let mut house = HouseholdState::default();
            house.apply_topology(&get_zone_group_state(&*lan, host).unwrap());
            let player = house.groups[0].coordinator.clone();
            let now = Instant::now();
            let mut rec = Reconciler::new(Duration::from_secs(300), Duration::from_secs(300), now);
            rec.households.push(house);
            let engine = Engine {
                transport: lan.clone(),
                lan: lan.clone(),
                config: LiveConfig::new(vec![host]),
                snapshot: Arc::new(Mutex::new(Snapshot::default())),
                subscribers: Arc::new(Mutex::new(Vec::new())),
                wake: Arc::new(AtomicBool::new(false)),
                stop: Arc::new(AtomicBool::new(false)),
            };
            let mut model = Model {
                ticked: (now, SystemTime::now()),
                remembered: Vec::new(),
                rec,
                playback: Playback::default(),
                progress: super::super::progress::Watchdog::default(),
                sink: None,
                surveyed_at: Some(now),
                last_error: None,
            };
            set_av_transport_uri(&*lan, host, "test:radio", "").unwrap();
            play(&*lan, host).unwrap();
            let events = survey(&engine, &mut model);
            engine.publish(&model, events);
            let snapshot = engine.snapshot.lock().unwrap();
            let state = snapshot.playback.of(&player).unwrap();
            assert_eq!(state.transport, Some(TransportState::Playing));
            drop(snapshot);
            // No event listener exists here: these state changes must be
            // recovered through real SOAP reads, not a synthetic NOTIFY.
            pause(&*lan, host).unwrap();
            let events = survey(&engine, &mut model);
            let expected = Some((Some(TransportState::Playing), TransportState::Paused));
            assert!(events.iter().any(|event| {
                matches!(event, LiveEvent::Playback { changes, .. } if changes.transport == expected)
            }));
            let state = model.playback.of(&player).unwrap();
            assert_eq!(state.transport, Some(TransportState::Paused));
            assert!(survey(&engine, &mut model).is_empty());
            play(&*lan, host).unwrap();
            assert!(!survey(&engine, &mut model).is_empty());
            stop(&*lan, host).unwrap();
            assert!(!survey(&engine, &mut model).is_empty());
            let state = model.playback.of(&player).unwrap();
            assert_eq!(state.transport, Some(TransportState::Stopped));
            engine.stop.store(true, Ordering::Release);
            assert!(survey(&engine, &mut model).is_empty());
            sim.shutdown();
        }
    }

    struct ReadFault<'a, T: ?Sized> {
        inner: &'a T,
        fail_at: usize,
        calls: AtomicUsize,
        change_final_state: bool,
        change_final_source: bool,
    }

    impl<T: Transport + ?Sized> Transport for ReadFault<'_, T> {
        fn soap_post(
            &self,
            host: IpAddr,
            path: &str,
            action: &str,
            body: &str,
        ) -> Result<String, ProtoError> {
            let call = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
            if call == self.fail_at {
                return Err(ProtoError::NotWired("read fault"));
            }
            let response = self.inner.soap_post(host, path, action, body)?;
            if self.change_final_state && call == 5 {
                Ok(response.replace("STOPPED", "PLAYING"))
            } else if self.change_final_source && call == 4 {
                Ok(response.replace("test:radio", "test:other"))
            } else {
                Ok(response)
            }
        }
    }

    #[test]
    fn interrupted_partial_and_inconsistent_reads_never_publish_a_sample() {
        let sim = SimHousehold::builder()
            .s2([SimPlayerSpec::new("Room", SimModel::One)])
            .spawn()
            .unwrap();
        let host = sim.player("Room").unwrap().ip;
        let lan = sim.lan();
        set_av_transport_uri(&lan, host, "test:radio", "").unwrap();
        let fault = |fail_at, change_final_state, change_final_source| ReadFault {
            inner: &lan,
            fail_at,
            calls: AtomicUsize::new(0),
            change_final_state,
            change_final_source,
        };
        for fail_at in 1..=5 {
            let transport = fault(fail_at, false, false);
            assert!(read(&transport, host, || false).is_err());
            assert_eq!(transport.calls.load(Ordering::Relaxed), fail_at);
        }
        for changed in [fault(0, true, false), fault(0, false, true)] {
            assert!(read(&changed, host, || false).is_err());
        }
        for stop_after in 0..=5 {
            let transport = fault(0, false, false);
            let result = read(&transport, host, || {
                transport.calls.load(Ordering::Relaxed) >= stop_after
            });
            assert!(result.unwrap().is_none());
            assert_eq!(transport.calls.load(Ordering::Relaxed), stop_after);
        }
        sim.shutdown();
    }
}
