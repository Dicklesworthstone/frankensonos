//! Drive the DJ from live events, with bounded maintenance for quiet queues.

use super::Surface;
use fsonos_core::live::{LiveEvent, Snapshot};
use fsonos_core::reconcile::Health;
use fsonos_types::PlayerId;
use std::collections::HashSet;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

/// Retry from cached state even if the speaker sends no further delta.
/// This is not a speaker poll: only an active feed that needs work does I/O.
const MAINTENANCE: Duration = Duration::from_secs(30);

pub(super) fn follow(surface: &Arc<Surface>) {
    let (Some(live), true) = (surface.live(), surface.dj.is_some()) else {
        return;
    };
    let changes = live.subscribe();
    let surface = Arc::downgrade(surface);
    if let Err(error) = std::thread::Builder::new()
        .name("fsonos-dj".into())
        .spawn(move || {
            drive(&changes, MAINTENANCE, |event| {
                handle(&surface, event.as_ref())
            });
        })
    {
        tracing::error!(%error, "could not start the DJ queue follower");
    }
}

fn handle(surface: &Weak<Surface>, event: Option<&LiveEvent>) -> bool {
    let Some(surface) = surface.upgrade() else {
        return false;
    };
    let Some(live) = surface.live() else {
        return false;
    };
    // An event queued before a regroup must not feed the former coordinator.
    // Use one current topology/health/playback snapshot, never the old event's
    // implied ownership or a direct household survey.
    let snapshot = live.snapshot();
    drop(live);
    // A spontaneous election can transfer the queue without a move command.
    // Reconcile ownership before filtering targets by the feeds they own.
    if let Some(dj) = &surface.dj
        && let Err(error) = surface.with_dj_store(|store| {
            dj.on_topology(&*surface.transport, &snapshot, store);
            Ok(())
        })
    {
        tracing::warn!(detail = %error.detail, "DJ topology reconciliation could not access the store");
    }
    // Whoever started a session the engine carried over still holds its caps.
    for (from, to) in surface.dj.iter().flat_map(|dj| dj.take_handovers()) {
        surface.dj_starters.moved(&from, &to);
    }
    let feeds = |id: &PlayerId| surface.dj.as_ref().is_some_and(|dj| dj.feeds(id));
    for player in targets(&snapshot, event, feeds) {
        // This rechecks ownership: a concurrent stop or move must not restart
        // a session that ended while the recovery pass was being planned.
        surface.dj_playback(&player);
    }
    true
}

fn targets(
    snapshot: &Snapshot,
    event: Option<&LiveEvent>,
    feeds: impl Fn(&PlayerId) -> bool,
) -> Vec<PlayerId> {
    let only = match event {
        Some(
            LiveEvent::Playback { player, .. }
            | LiveEvent::Health {
                player,
                health: Health::Healthy,
            },
        ) => Some(player),
        // After an upstream gap, reconcile every currently owned feed from
        // the snapshot, not the identities of whatever deltas survived.
        Some(LiveEvent::Topology | LiveEvent::Resync) | None => None,
        Some(LiveEvent::Health { .. }) => return Vec::new(),
    };
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for household in &snapshot.households {
        for group in &household.groups {
            let player = &group.coordinator;
            if only.is_some_and(|id| id != player)
                || household.player(player).is_none()
                || snapshot
                    .health
                    .of(player)
                    .is_some_and(|h| h.health == Health::Offline)
                || snapshot
                    .playback
                    .of(player)
                    .is_none_or(|p| p.transport.is_none())
                || !feeds(player)
                || !seen.insert(player.clone())
            {
                continue;
            }
            out.push(player.clone());
        }
    }
    out
}

/// `None` asks for maintenance; `Some` delivers an event. Returning false
/// stops the worker. Run once immediately, then at most once per interval,
/// including while a busy event stream never lets recv_timeout time out.
fn drive<T>(events: &Receiver<T>, interval: Duration, mut handle: impl FnMut(Option<T>) -> bool) {
    if !handle(None) {
        return;
    }
    let mut due = Instant::now() + interval;
    loop {
        match events.recv_timeout(due.saturating_duration_since(Instant::now())) {
            Ok(event) => {
                if !handle(Some(event)) {
                    return;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        if Instant::now() >= due {
            if !handle(None) {
                return;
            }
            // Do not replay missed ticks after a slow network operation or
            // host sleep; one pass over current state is sufficient.
            due = Instant::now() + interval;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsonos_core::playback::{Changes, EventSource};
    use fsonos_proto::gena::Notify;
    use std::sync::mpsc;

    #[test]
    fn starts_from_cached_state_without_waiting_for_an_event() {
        let (_send, events) = mpsc::channel::<()>();
        let mut calls = 0;
        drive(&events, Duration::from_secs(60), |event| {
            assert!(event.is_none());
            calls += 1;
            false
        });
        assert_eq!(calls, 1);
    }

    #[test]
    fn delivers_events_in_order_and_exits_when_the_model_stops() {
        let (send, events) = mpsc::channel();
        send.send(1).unwrap();
        send.send(2).unwrap();
        drop(send);
        let mut delivered = Vec::new();
        drive(&events, Duration::from_secs(60), |event| {
            delivered.push(event);
            true
        });
        assert_eq!(delivered, [None, Some(1), Some(2)]);
    }

    #[test]
    fn an_idle_stream_keeps_retrying_without_new_events() {
        let (_send, events) = mpsc::channel::<()>();
        let (observed, receive) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut calls = 0;
            drive(&events, Duration::from_millis(5), |event| {
                assert!(event.is_none());
                calls += 1;
                observed.send(calls).unwrap();
                calls < 3
            });
        });
        for expected in 1..=3 {
            assert_eq!(
                receive.recv_timeout(Duration::from_secs(2)).unwrap(),
                expected
            );
        }
        worker.join().unwrap();
    }

    #[test]
    fn unrelated_event_traffic_cannot_starve_maintenance() {
        let (send, events) = mpsc::channel();
        for n in 0..1000 {
            send.send(n).unwrap();
        }
        let mut events_seen = 0;
        let mut maintenance = 0;
        drive(&events, Duration::from_millis(5), |event| {
            if event.is_some() {
                events_seen += 1;
                std::thread::sleep(Duration::from_millis(1));
            } else {
                maintenance += 1;
            }
            maintenance < 2
        });
        assert_eq!(maintenance, 2);
        assert!(events_seen > 0 && events_seen < 1000);
    }

    #[test]
    fn dropping_the_surface_can_stop_the_worker_on_an_event() {
        let (send, events) = mpsc::channel();
        send.send(1).unwrap();
        send.send(2).unwrap();
        let mut delivered = Vec::new();
        drive(&events, Duration::from_secs(60), |event| {
            delivered.push(event);
            event.is_none()
        });
        assert_eq!(delivered, [None, Some(1)]);
    }

    fn snapshot() -> (Snapshot, Vec<PlayerId>) {
        let mut snapshot = Snapshot {
            households: crate::zones::fixtures::households(),
            ..Snapshot::default()
        };
        let coordinators: Vec<PlayerId> = snapshot
            .households
            .iter()
            .flat_map(|h| h.groups.iter().map(|g| g.coordinator.clone()))
            .collect();
        let event = Notify {
            sid: "uuid:test".into(),
            seq: 0,
            path: "/AVTransport".into(),
            properties: vec![(
                "LastChange".into(),
                "<Event><InstanceID val=\"0\"><TransportState val=\"PAUSED_PLAYBACK\"/>\
                 </InstanceID></Event>"
                    .into(),
            )],
        };
        for player in &coordinators {
            snapshot
                .playback
                .apply(player, EventSource::AvTransport, &event, Instant::now())
                .unwrap();
        }
        (snapshot, coordinators)
    }

    #[test]
    fn maintenance_skips_offline_inactive_and_incomplete_state() {
        let (mut snapshot, ids) = snapshot();
        assert!(ids.len() >= 4);
        snapshot.health.missing(&ids[0], "offline");
        snapshot.playback.remove(&ids[1]);
        // A RenderingControl event can precede the initial AVTransport state.
        let rendering = Notify {
            sid: "uuid:rendering".into(),
            seq: 0,
            path: "/RenderingControl".into(),
            properties: vec![(
                "LastChange".into(),
                "<Event><InstanceID val=\"0\"><Volume channel=\"Master\" val=\"12\"/>\
                 </InstanceID></Event>"
                    .into(),
            )],
        };
        snapshot
            .playback
            .apply(
                &ids[1],
                EventSource::RenderingControl,
                &rendering,
                Instant::now(),
            )
            .unwrap();
        assert_eq!(snapshot.playback.of(&ids[1]).unwrap().volume, Some(12));
        snapshot.households.push(snapshot.households[0].clone());
        assert_eq!(targets(&snapshot, None, |id| *id != ids[2]), ids[3..]);
        assert!(targets(&snapshot, None, |_| false).is_empty());
    }

    #[test]
    fn delayed_playback_cannot_target_a_former_or_unaddressable_coordinator() {
        let (mut snapshot, ids) = snapshot();
        let event = LiveEvent::Playback {
            player: ids[0].clone(),
            changes: Changes::default(),
        };
        assert_eq!(targets(&snapshot, Some(&event), |_| true), ids[..1]);
        let before = snapshot.households.clone();
        for household in &mut snapshot.households {
            household.groups.retain(|g| g.coordinator != ids[0]);
        }
        assert!(targets(&snapshot, Some(&event), |_| true).is_empty());
        snapshot.households = before;
        for household in &mut snapshot.households {
            household.players.retain(|p| p.id != ids[0]);
        }
        assert!(targets(&snapshot, Some(&event), |_| true).is_empty());
    }

    #[test]
    fn resync_revisits_active_feeds_from_current_state_only() {
        let (mut snapshot, ids) = snapshot();
        assert!(ids.len() >= 4);
        snapshot.health.missing(&ids[0], "offline now");
        snapshot.playback.remove(&ids[1]);
        let feeds = |id: &PlayerId| *id != ids[2];
        assert_eq!(
            targets(&snapshot, Some(&LiveEvent::Resync), feeds),
            ids[3..]
        );
        // A reset must never start a stopped feed or recreate a lost group.
        assert!(targets(&snapshot, Some(&LiveEvent::Resync), |_| false).is_empty());
        snapshot.households.clear();
        assert!(targets(&snapshot, Some(&LiveEvent::Resync), |_| true).is_empty());
    }

    #[test]
    fn topology_and_health_recovery_revisit_only_current_active_feeds() {
        let (mut snapshot, ids) = snapshot();
        let feeds = |id: &PlayerId| *id == ids[0];
        assert_eq!(
            targets(&snapshot, Some(&LiveEvent::Topology), feeds),
            ids[..1]
        );
        for health in [Health::Degraded, Health::Offline] {
            let event = LiveEvent::Health {
                player: ids[0].clone(),
                health,
            };
            assert!(targets(&snapshot, Some(&event), feeds).is_empty());
        }
        let recovered = LiveEvent::Health {
            player: ids[0].clone(),
            health: Health::Healthy,
        };
        snapshot.health.missing(&ids[0], "newer than the event");
        assert!(targets(&snapshot, Some(&recovered), feeds).is_empty());
        snapshot.health.ok(&ids[0], Instant::now());
        assert_eq!(targets(&snapshot, Some(&recovered), feeds), ids[..1]);
    }
}

#[cfg(test)]
mod simulator_tests {
    use super::*;
    use crate::dj::{DjEngine, DjMoodsDto, DjSpeakers, DjStatusDto, DjSteer};
    use crate::plan::DjAction;
    use crate::{Failure, OutcomeDto};
    use fsonos_core::clock::{Clock, SystemClock};
    use fsonos_core::live::{Live, LiveConfig};
    use fsonos_core::playback::PlayerPlayback;
    use fsonos_core::policy::{Client, Policy};
    use fsonos_core::store::{MemStore, Store};
    use fsonos_proto::net::Lan;
    use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
    use fsonos_types::PlayerId;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{self, Sender};

    struct Probe {
        active: PlayerId,
        seen: Sender<Option<u8>>,
        topology_seen: AtomicBool,
        /// A feed it reports carried over, once.
        handover: Mutex<Option<(PlayerId, PlayerId)>>,
    }

    impl DjEngine for Probe {
        fn act(
            &self,
            _: DjSpeakers<'_>,
            _: &mut dyn Store,
            _: DjAction,
            _: &dyn Clock,
        ) -> Result<OutcomeDto, Failure> {
            unreachable!("maintenance must not issue a DJ command")
        }

        fn steer(
            &self,
            _: DjSpeakers<'_>,
            _: &mut dyn Store,
            _: &DjSteer,
            _: &dyn Clock,
        ) -> Result<OutcomeDto, Failure> {
            unreachable!()
        }

        fn status(
            &self,
            _: DjSpeakers<'_>,
            _: &dyn Store,
            _: Option<u32>,
            _: &dyn Clock,
        ) -> Result<DjStatusDto, Failure> {
            Err(Failure::invalid("probe has no status"))
        }

        fn moods(
            &self,
            _: Option<DjSpeakers<'_>>,
            _: &dyn Store,
            _: &dyn Clock,
        ) -> Result<DjMoodsDto, Failure> {
            unreachable!()
        }

        fn feeds(&self, coordinator: &PlayerId) -> bool {
            assert!(
                self.topology_seen.load(Ordering::SeqCst),
                "ownership must reconcile before selecting feed targets"
            );
            *coordinator == self.active
        }

        fn on_topology(
            &self,
            _: &dyn fsonos_proto::Transport,
            snapshot: &Snapshot,
            _: &mut dyn Store,
        ) {
            assert!(!snapshot.households.is_empty());
            self.topology_seen.store(true, Ordering::SeqCst);
        }

        fn take_handovers(&self) -> Vec<(PlayerId, PlayerId)> {
            self.handover.lock().unwrap().take().into_iter().collect()
        }

        fn on_playback(
            &self,
            at: DjSpeakers<'_>,
            _: &mut dyn Store,
            playback: &PlayerPlayback,
            _: &dyn Clock,
        ) {
            assert_eq!(*at.coordinator, self.active, "inactive feed was visited");
            let _ = self.seen.send(playback.volume);
        }
    }

    fn household() -> (SimHandle, Arc<Live>, PlayerId) {
        let sim = SimHousehold::builder()
            .s2([
                SimPlayerSpec::new("Den", SimModel::One),
                SimPlayerSpec::new("Office", SimModel::One),
            ])
            .spawn()
            .unwrap();
        let lan = Arc::new(
            Lan::start()
                .unwrap()
                .with_routes(sim.players().iter().map(|p| (p.ip, p.addr)).collect()),
        );
        let live = Arc::new(Live::start_with(
            Arc::new(sim.lan()),
            lan,
            LiveConfig::new(sim.players().iter().map(|p| p.ip).collect()),
        ));
        assert!(live.wait_ready(Duration::from_secs(10)));
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            if sim.players().iter().all(|p| {
                live.player(&PlayerId(p.uuid.clone())).is_some_and(|p| {
                    p.transport.is_some() && p.volume.is_some() && p.group_volume.is_some()
                })
            }) {
                break;
            }
            assert!(Instant::now() < until, "initial GENA state did not arrive");
            std::thread::sleep(Duration::from_millis(10));
        }
        let id = PlayerId(sim.player("Den").unwrap().uuid.clone());
        (sim, live, id)
    }

    fn surface(
        sim: &SimHandle,
        live: &Arc<Live>,
        active: PlayerId,
    ) -> (Arc<Surface>, Receiver<Option<u8>>) {
        surface_handing(sim, live, active, None)
    }

    /// [`surface`], its engine reporting `handover` once.
    fn surface_handing(
        sim: &SimHandle,
        live: &Arc<Live>,
        active: PlayerId,
        handover: Option<(PlayerId, PlayerId)>,
    ) -> (Arc<Surface>, Receiver<Option<u8>>) {
        let (seen, received) = mpsc::channel();
        let surface = Surface::new(
            Box::new(sim.lan()),
            Box::new(|_| panic!("DJ maintenance must not survey the speakers")),
            Policy::default(),
            Box::new(SystemClock),
        )
        .with_live(live)
        .with_dj(Box::new(Probe {
            active,
            seen,
            topology_seen: AtomicBool::new(false),
            handover: Mutex::new(handover),
        }))
        .with_action_log(Box::new(MemStore::default()), "test");
        (Arc::new(surface), received)
    }

    /// A coordinator re-elected under the DJ: the engine carries the feed,
    /// and who started the session goes with it, its caps still holding.
    #[test]
    fn a_handover_the_engine_made_carries_the_sessions_starter() {
        let (sim, live, active) = household();
        let office = PlayerId(sim.player("Office").unwrap().uuid.clone());
        let handover = Some((active.clone(), office.clone()));
        let (surface, _received) = surface_handing(&sim, &live, active.clone(), handover);
        surface.dj_starters.started(&active, &Client::McpStdio);
        assert!(handle(&Arc::downgrade(&surface), None));
        assert_eq!(surface.dj_starters.of(&office), Some(Client::McpStdio));
        assert_eq!(surface.dj_starters.of(&active), None);
        drop(surface);
        drop(live);
        sim.shutdown();
    }

    #[test]
    fn follower_bootstraps_active_feeds_after_initial_events_already_arrived() {
        let (sim, live, active) = household();
        let expected = live.player(&active).unwrap().volume;
        let (surface, received) = surface(&sim, &live, active);
        // The real follower subscribes after Live already has every initial
        // state. No new playback change is needed to visit a restored feed.
        follow(&surface);
        assert_eq!(
            received.recv_timeout(Duration::from_secs(2)).unwrap(),
            expected
        );
        drop(surface);
        drop(live);
        sim.shutdown();
    }

    #[test]
    fn idle_maintenance_uses_current_live_state_and_stops_with_the_surface() {
        let (sim, live, active) = household();
        let original = live.player(&active).unwrap().volume;
        let (surface, received) = surface(&sim, &live, active);
        let weak = Arc::downgrade(&surface);
        // Deliberately no events on this channel: only the timer can reach
        // the production handler, while Live receives real simulator GENA.
        let (_send, events) = mpsc::channel::<LiveEvent>();
        let worker = std::thread::spawn(move || {
            drive(&events, Duration::from_millis(10), |event| {
                handle(&weak, event.as_ref())
            });
        });
        assert_eq!(
            received.recv_timeout(Duration::from_secs(2)).unwrap(),
            original
        );
        let changed = if original == Some(37) { 38 } else { 37 };
        fsonos_proto::control::set_volume(&sim.lan(), sim.player("Den").unwrap().ip, changed)
            .unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            let remaining = until.saturating_duration_since(Instant::now());
            let seen = received.recv_timeout(remaining).unwrap();
            if seen == Some(changed) {
                break;
            }
        }
        drop(surface);
        worker.join().unwrap();
        drop(live);
        sim.shutdown();
    }

    #[test]
    fn regrouped_player_is_not_fed_by_a_queued_event_or_timer() {
        use fsonos_core::playback::Changes;

        let (sim, live, active) = household();
        let (surface, received) = surface(&sim, &live, active.clone());
        let weak = Arc::downgrade(&surface);
        assert!(handle(&weak, Some(&LiveEvent::Topology)));
        received.recv_timeout(Duration::from_secs(2)).unwrap();
        let office = PlayerId(sim.player("Office").unwrap().uuid.clone());
        fsonos_proto::control::join_group(&sim.lan(), sim.player("Den").unwrap().ip, &office)
            .unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        while live
            .households()
            .iter()
            .any(|h| h.groups.iter().any(|g| g.coordinator == active))
        {
            assert!(Instant::now() < until, "regroup topology did not arrive");
            std::thread::sleep(Duration::from_millis(10));
        }
        let stale = LiveEvent::Playback {
            player: active,
            changes: Changes::default(),
        };
        assert!(handle(&weak, Some(&stale)));
        assert!(handle(&weak, Some(&LiveEvent::Resync)));
        assert!(handle(&weak, None));
        assert!(matches!(
            received.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        drop(surface);
        drop(live);
        sim.shutdown();
    }
}
