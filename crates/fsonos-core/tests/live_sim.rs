//! The live model against `fsonos-sim` over real sockets: it surveys,
//! subscribes and folds events on its own thread; changes made elsewhere
//! arrive through events; a player that drops off is marked offline and
//! comes back; a reboot is followed; stopping ends every subscription.

use fsonos_core::control;
use fsonos_core::events::{Service, wanted};
use fsonos_core::live::{Live, LiveConfig, LiveEvent};
use fsonos_core::reconcile::Health;
use fsonos_proto::net::Lan;
use fsonos_proto::ssdp::Advert;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{GenaEvent, SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use fsonos_types::PlayerId;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

fn sim() -> SimHandle {
    SimHousehold::builder()
        .s1([
            SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1),
            SimPlayerSpec::new("Office", SimModel::Play5Gen1),
        ])
        .s2([SimPlayerSpec::new("Living Room", SimModel::One)])
        .spawn()
        .unwrap()
}

fn routed(sim: &SimHandle) -> Arc<Lan> {
    Arc::new(
        Lan::start()
            .unwrap()
            .with_routes(sim.players().iter().map(|p| (p.ip, p.addr)).collect())
            .with_ssdp_target(sim.ssdp_addr()),
    )
}

fn eventually(within: Duration, ok: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    ok()
}

fn id_of(live: &Live, room: &str) -> PlayerId {
    live.households()
        .iter()
        .flat_map(|h| &h.players)
        .find(|p| p.room_name == room)
        .unwrap()
        .id
        .clone()
}

fn unsubscribes(sim: &SimHandle) -> usize {
    sim.gena_log()
        .iter()
        .filter(|e| matches!(e.event, GenaEvent::Unsubscribed { .. }))
        .count()
}

#[test]
fn the_live_model_follows_the_speakers() {
    let sim = sim();
    let lan = routed(&sim);
    let live = Live::start(Arc::clone(&lan), LiveConfig::new(Vec::new()));
    assert!(
        live.wait_ready(Duration::from_secs(10)),
        "{:?}",
        live.snapshot().last_error
    );
    let snap = live.snapshot();
    assert_eq!(snap.households.len(), 2);
    assert_eq!(
        snap.households
            .iter()
            .map(|h| h.players.len())
            .sum::<usize>(),
        3
    );
    assert!(
        snap.callback.is_some() && snap.last_error.is_none(),
        "{snap:?}"
    );
    let ids: Vec<PlayerId> = ["Kitchen", "Office", "Living Room"]
        .iter()
        .map(|room| id_of(&live, room))
        .collect();

    // Every player's initial NOTIFYs arrive.
    assert!(eventually(Duration::from_secs(5), || ids.iter().all(
        |id| live
            .player(id)
            .is_some_and(|p| p.volume.is_some() && p.transport.is_some())
    )));

    // A change made by someone else shows up through events.
    let kitchen = &ids[0];
    control::set_volume(&*lan, &live.households(), kitchen, 33).unwrap();
    assert!(eventually(Duration::from_secs(3), || live
        .player(kitchen)
        .and_then(|p| p.volume)
        == Some(33)));

    // A player that drops off is found missing by the survey asked for now.
    let office = &ids[1];
    sim.set_offline("Office", true).unwrap();
    live.refresh_soon();
    assert!(eventually(Duration::from_secs(10), || live
        .snapshot()
        .health
        .of(office)
        .is_some_and(|h| h.health == Health::Offline)));
    assert!(live.households().iter().all(|h| h.player(office).is_none()));
    assert!(
        live.player(office).is_none(),
        "its stale playback state is gone"
    );

    // It comes back, healthy and reporting events again.
    sim.set_offline("Office", false).unwrap();
    live.refresh_soon();
    assert!(eventually(Duration::from_secs(10), || {
        let snap = live.snapshot();
        snap.health
            .of(office)
            .is_some_and(|h| h.health == Health::Healthy)
            && live.player(office).is_some_and(|p| p.volume.is_some())
    }));

    // Stopping ends every active subscription.
    let active = live.snapshot().subscriptions;
    assert!(active > 0);
    let before = unsubscribes(&sim);
    live.stop();
    assert_eq!(unsubscribes(&sim) - before, active);
}

#[test]
fn a_reboot_is_followed_and_events_keep_flowing() {
    let sim = sim();
    let lan = routed(&sim);
    let live = Live::start(Arc::clone(&lan), LiveConfig::new(Vec::new()));
    assert!(live.wait_ready(Duration::from_secs(10)));
    let households = live.households();
    // Reboot the S1 player that does not carry the household's topology
    // subscription, so the topology event reports the reboot.
    let topology_hosts: Vec<PlayerId> = wanted(&households)
        .into_iter()
        .filter(|w| w.service == Service::ZoneGroupTopology)
        .map(|w| w.player)
        .collect();
    let victim = ["Kitchen", "Office"]
        .into_iter()
        .map(|room| (room, id_of(&live, room)))
        .find(|(_, id)| !topology_hosts.contains(id))
        .unwrap();
    assert!(eventually(Duration::from_secs(5), || live
        .player(&victim.1)
        .is_some_and(|p| p.volume.is_some())));

    sim.reboot(victim.0, Duration::ZERO).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    // Its old subscriptions are gone; only fresh ones carry this change.
    control::set_volume(&*lan, &live.households(), &victim.1, 44).unwrap();
    assert!(
        eventually(Duration::from_secs(5), || live
            .player(&victim.1)
            .and_then(|p| p.volume)
            == Some(44)),
        "{:?}",
        live.player(&victim.1)
    );
    assert_eq!(
        live.snapshot().health.of(&victim.1).map(|h| h.health),
        Some(Health::Healthy)
    );
}

#[test]
fn changes_are_pushed_to_subscribers() {
    let sim = sim();
    let lan = routed(&sim);
    let live = Live::start(Arc::clone(&lan), LiveConfig::new(Vec::new()));
    assert!(live.wait_ready(Duration::from_secs(10)));
    let kitchen = id_of(&live, "Kitchen");
    let office = id_of(&live, "Office");
    assert!(eventually(Duration::from_secs(5), || live
        .player(&kitchen)
        .is_some_and(|p| p.volume.is_some())));
    let events = live.subscribe();
    let quitter = live.subscribe();
    drop(quitter);

    // A change made by someone else is pushed within a second.
    control::set_volume(&*lan, &live.households(), &kitchen, 27).unwrap();
    let started = Instant::now();
    let pushed = loop {
        let left = Duration::from_secs(1).saturating_sub(started.elapsed());
        match events.recv_timeout(left) {
            Ok(LiveEvent::Playback { player, changes })
                if player == kitchen && changes.volume == Some(27) =>
            {
                break true;
            }
            Ok(_) => {}
            Err(_) => break false,
        }
    };
    assert!(pushed, "the volume change was not pushed within 1 s");

    // A player dropping off: its health and the topology change are pushed.
    sim.set_offline("Office", true).unwrap();
    live.refresh_soon();
    let (mut offline, mut topology) = (false, false);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !(offline && topology) && Instant::now() < deadline {
        match events.recv_timeout(Duration::from_millis(200)) {
            Ok(LiveEvent::Health { player, health }) if player == office => {
                offline |= health == Health::Offline;
            }
            Ok(LiveEvent::Topology) => topology = true,
            _ => {}
        }
    }
    assert!(
        offline && topology,
        "offline {offline}, topology {topology}"
    );
}

/// Counts the requests sent through it and refuses multicast SSDP, as a
/// transport confined to a routes file without a responder does.
struct Counting {
    inner: Arc<Lan>,
    calls: AtomicUsize,
}

impl Transport for Counting {
    fn soap_post(
        &self,
        host: IpAddr,
        control_path: &str,
        soap_action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.inner.soap_post(host, control_path, soap_action, body)
    }

    fn http_get(&self, url: &str) -> Result<String, ProtoError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.inner.http_get(url)
    }

    fn ssdp_search(&self, _mx_secs: u8, _wait: Duration) -> Result<Vec<Advert>, ProtoError> {
        Err(ProtoError::Network {
            target: "ssdp".into(),
            detail: "refused".into(),
        })
    }
}

#[test]
fn surveys_go_through_the_given_transport_and_events_through_the_lan() {
    let sim = sim();
    let lan = routed(&sim);
    let counting = Arc::new(Counting {
        inner: Arc::clone(&lan),
        calls: AtomicUsize::new(0),
    });
    let seeds: Vec<IpAddr> = sim.players().iter().map(|p| p.ip).collect();
    let transport: Arc<dyn Transport + Send + Sync> = Arc::clone(&counting) as _;
    let live = Live::start_with(transport, Arc::clone(&lan), LiveConfig::new(seeds));
    assert!(
        live.wait_ready(Duration::from_secs(10)),
        "{:?}",
        live.snapshot().last_error
    );
    // The LAN could have searched the simulator's responder; the survey went
    // through the given transport (SSDP refused, the seeds found everyone).
    assert!(counting.calls.load(Ordering::Relaxed) > 0);
    let ids: Vec<PlayerId> = ["Kitchen", "Office", "Living Room"]
        .iter()
        .map(|room| id_of(&live, room))
        .collect();
    assert!(eventually(Duration::from_secs(5), || ids
        .iter()
        .all(|id| live.player(id).is_some_and(|p| p.volume.is_some()))));
}
