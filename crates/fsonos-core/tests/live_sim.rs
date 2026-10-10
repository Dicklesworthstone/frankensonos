//! The live model against `fsonos-sim` over real sockets: it surveys,
//! subscribes and folds events on its own thread; changes made elsewhere
//! arrive through events; a player that drops off is marked offline and
//! comes back; a reboot is followed; stopping ends every subscription.

use fsonos_core::events::{Service, wanted};
use fsonos_core::live::{Live, LiveConfig, LiveEvent, STOP_WAIT};
use fsonos_core::reconcile::Health;
use fsonos_core::{control, grouping, resolve_room};
use fsonos_proto::net::Lan;
use fsonos_proto::ssdp::Advert;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{GenaEvent, SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use fsonos_types::PlayerId;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
fn a_seek_made_elsewhere_is_reflected_on_the_next_survey() {
    // No track-change event follows an external seek (the sim sends none),
    // so only the survey's position sweep can learn it: after a daemon
    // restart the position is populated by the first survey, and a seek
    // shows up on a running model on the next one.
    let sim = sim();
    let lan = routed(&sim);
    let live = Live::start(Arc::clone(&lan), LiveConfig::new(Vec::new()));
    assert!(
        live.wait_ready(Duration::from_secs(10)),
        "{:?}",
        live.snapshot().last_error
    );
    let kitchen_id = id_of(&live, "Kitchen");
    let kitchen_addr = sim.player("Kitchen").unwrap().ip;
    // A stream playing: positions interpolate from the last survey read.
    let stream = "x-rincon-mp3radio://example.invalid/seek-test.mp3";
    control::play_uri(&*lan, &live.households(), &kitchen_id, stream, "").unwrap();
    assert!(eventually(Duration::from_secs(5), || live
        .player(&kitchen_id)
        .is_some_and(|p| p.track_uri.as_deref() == Some(stream))));
    let pos_of =
        |live: &Live, id: &PlayerId| live.player(id).and_then(|p| p.position_at(Instant::now()));
    let near =
        |want: u32| move |got: Option<u32>| got.is_some_and(|p| (want..want + 15).contains(&p));

    // External seek: reaches the running model on the next survey (the sim
    // emits no track-change for it; its clock only moves when advanced, so
    // the sim's position stays put until then).
    fsonos_proto::control::seek_position(&*lan, kitchen_addr, 95).unwrap();
    live.refresh_soon();
    assert!(
        eventually(Duration::from_secs(10), || near(95)(pos_of(
            &live,
            &kitchen_id
        ))),
        "position after the external seek: {pos_of:?}",
        pos_of = pos_of(&live, &kitchen_id)
    );

    // Daemon-restart case: a fresh model's first survey reports it, before
    // any track-change event could have.
    live.stop();
    let live2 = Live::start(Arc::clone(&lan), LiveConfig::new(Vec::new()));
    assert!(
        live2.wait_ready(Duration::from_secs(10)),
        "{:?}",
        live2.snapshot().last_error
    );
    let kitchen2 = id_of(&live2, "Kitchen");
    assert!(
        near(95)(pos_of(&live2, &kitchen2)),
        "position after daemon restart: {pos_of:?}",
        pos_of = pos_of(&live2, &kitchen2)
    );
    live2.stop();
    sim.shutdown();
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
    // Its room is known to be off, not unknown (any case, spacing).
    assert!(
        eventually(Duration::from_secs(5), || live
            .snapshot()
            .vanished_room("  office ")
            .is_some_and(|v| v.uuid == *office)),
        "{:?}",
        live.snapshot().vanished
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
    assert!(
        eventually(Duration::from_secs(5), || live
            .snapshot()
            .vanished_room("Office")
            .is_none()),
        "back on: no longer listed as vanished"
    );

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

#[test]
fn grouping_changes_resubscribe_without_waiting_for_a_survey() {
    let sim = sim();
    let lan = routed(&sim);
    let live = Live::start(Arc::clone(&lan), LiveConfig::new(Vec::new()));
    assert!(live.wait_ready(Duration::from_secs(10)));
    assert!(eventually(Duration::from_secs(5), || live
        .snapshot()
        .subscriptions
        > 0));
    let all = live.snapshot().subscriptions;
    let surveyed = live.snapshot().surveyed_at;

    // The Office joins the Kitchen: as a member it needs no AVTransport or
    // group-volume subscription of its own.
    let houses = live.households();
    let kitchen = resolve_room(&houses, "Kitchen").unwrap();
    let office = resolve_room(&houses, "Office").unwrap();
    assert!(grouping::group(&*lan, &houses, &kitchen, &[office]).is_complete());
    assert!(
        eventually(Duration::from_secs(5), || live.snapshot().subscriptions
            == all - 2),
        "{} of {all}",
        live.snapshot().subscriptions
    );

    // On its own again, it is a coordinator and gets them back.
    let houses = live.households();
    let office = resolve_room(&houses, "Office").unwrap();
    assert!(grouping::ungroup(&*lan, &houses, &[office]).is_complete());
    assert!(
        eventually(Duration::from_secs(5), || live.snapshot().subscriptions
            == all),
        "{} of {all}",
        live.snapshot().subscriptions
    );
    assert_eq!(
        live.snapshot().surveyed_at,
        surveyed,
        "no survey was needed"
    );
}

/// Forwards to the LAN, but refuses SSDP once `dead` is set, as a network
/// that stops passing multicast does.
struct Flaky {
    inner: Arc<Lan>,
    dead: AtomicBool,
}

impl Transport for Flaky {
    fn soap_post(
        &self,
        host: IpAddr,
        control_path: &str,
        soap_action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        self.inner.soap_post(host, control_path, soap_action, body)
    }

    fn http_get(&self, url: &str) -> Result<String, ProtoError> {
        self.inner.http_get(url)
    }

    fn ssdp_search(&self, mx_secs: u8, wait: Duration) -> Result<Vec<Advert>, ProtoError> {
        if self.dead.load(Ordering::Acquire) {
            return Err(ProtoError::Network {
                target: "ssdp".into(),
                detail: "no multicast".into(),
            });
        }
        self.inner.ssdp_search(mx_secs, wait)
    }
}

#[test]
fn known_players_are_surveyed_again_when_ssdp_stops_answering() {
    let sim = sim();
    let lan = routed(&sim);
    let flaky = Arc::new(Flaky {
        inner: Arc::clone(&lan),
        dead: AtomicBool::new(false),
    });
    let transport: Arc<dyn Transport + Send + Sync> = Arc::clone(&flaky) as _;
    // No seeds: the first survey finds everyone over SSDP.
    let live = Live::start_with(transport, Arc::clone(&lan), LiveConfig::new(Vec::new()));
    assert!(live.wait_ready(Duration::from_secs(10)));
    let first = live.snapshot().surveyed_at.unwrap();

    flaky.dead.store(true, Ordering::Release);
    live.refresh_soon();
    assert!(
        eventually(Duration::from_secs(10), || live
            .snapshot()
            .surveyed_at
            .is_some_and(|t| t > first)),
        "{:?}",
        live.snapshot().last_error
    );
    let snap = live.snapshot();
    assert_eq!(snap.last_error, None);
    assert_eq!(
        snap.households
            .iter()
            .map(|h| h.players.len())
            .sum::<usize>(),
        3
    );
}

#[test]
fn stop_returns_promptly_even_when_a_player_is_slow() {
    let sim = sim();
    let lan = routed(&sim);
    let live = Live::start(Arc::clone(&lan), LiveConfig::new(Vec::new()));
    assert!(live.wait_ready(Duration::from_secs(10)));
    // Every request to the Kitchen now takes 4 s, so a survey stalls on it
    // (and ending its subscriptions would, one UNSUBSCRIBE at a time).
    sim.set_latency("Kitchen", Duration::from_secs(4)).unwrap();
    live.refresh_soon();
    std::thread::sleep(Duration::from_millis(500));
    let started = Instant::now();
    live.stop();
    let took = started.elapsed();
    assert!(
        took < STOP_WAIT + Duration::from_secs(1),
        "stop took {took:?}"
    );
    sim.set_latency("Kitchen", Duration::ZERO).unwrap();
}

#[test]
fn the_players_fetch_clips_from_the_event_listener() {
    use fsonos_core::announce::clip::{Chime, MediaStore};
    let sim = sim();
    let lan = routed(&sim);
    let data = std::env::temp_dir().join(format!("fsonos-live-media-{}", std::process::id()));
    let media = MediaStore::new(&data);
    let clip = media.put(&Chime::Bell.wav()).unwrap();
    let files = media.clone();
    let config = LiveConfig {
        media: Some(Arc::new(move |name: &str| files.path(name))),
        ..LiveConfig::new(Vec::new())
    };
    let live = Live::start(Arc::clone(&lan), config);
    assert!(
        live.wait_ready(Duration::from_secs(10)),
        "{:?}",
        live.snapshot().last_error
    );
    let snap = live.snapshot();
    let base = snap.callback.clone().expect("listening");
    let url = clip.url(&base);
    let kitchen = resolve_room(&snap.households, "Kitchen")
        .unwrap()
        .player
        .id
        .clone();
    control::play_uri(&*lan, &snap.households, &kitchen, &url, "").unwrap();
    let fetched = || sim.fetch_log().into_iter().find(|f| f.url == url);
    assert!(eventually(Duration::from_secs(5), || fetched().is_some()));
    let fetch = fetched().unwrap();
    assert_eq!(fetch.result, Ok(200), "{fetch:?}");
    assert!(fetch.wav_duration_ms.is_some_and(|ms| ms > 0), "{fetch:?}");

    // Only clips the store holds are served.
    let unknown = format!("{base}/media/{}.wav", "0".repeat(32));
    control::play_uri(&*lan, &snap.households, &kitchen, &unknown, "").unwrap();
    assert!(eventually(Duration::from_secs(5), || sim
        .fetch_log()
        .iter()
        .any(|f| f.url == unknown && f.result == Ok(404))));
    live.stop();
    sim.shutdown();
    let _ = std::fs::remove_dir_all(&data);
}
