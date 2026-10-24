//! Restart from cached addresses with no multicast and no explicit seeds.
//! The live worker, event listener, SOAP reads and notifications are real.

use fsonos_core::live::{Live, LiveConfig, Snapshot};
use fsonos_core::resolve_room;
use fsonos_proto::control::set_volume;
use fsonos_proto::net::Lan;
use fsonos_proto::ssdp::{Advert, description_url};
use fsonos_proto::topology::host_of_location;
use fsonos_proto::{Discovered, ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use fsonos_types::PlayerId;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct DiscoveryTransport {
    lan: Arc<Lan>,
    visible: Mutex<Vec<IpAddr>>,
    gets: Mutex<Vec<IpAddr>>,
    fail: bool,
}

impl Transport for DiscoveryTransport {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        self.lan.soap_post(host, path, action, body)
    }

    fn http_get(&self, url: &str) -> Result<String, ProtoError> {
        self.gets
            .lock()
            .unwrap()
            .push(host_of_location(url).unwrap());
        self.lan.http_get(url)
    }

    fn discover(&self, _: u8, _: Duration) -> Discovered {
        let unavailable = || ProtoError::Network {
            target: "test-discovery".into(),
            detail: "multicast unavailable".into(),
        };
        if self.fail {
            return Discovered {
                ssdp: Err(unavailable()),
                mdns: Err(unavailable()),
            };
        }
        Discovered {
            ssdp: Ok(self
                .visible
                .lock()
                .unwrap()
                .iter()
                .map(|&ip| Advert {
                    location: description_url(ip),
                    st: "urn:schemas-upnp-org:device:ZonePlayer:1".into(),
                    usn: None,
                    household: None,
                    boot_seq: None,
                })
                .collect()),
            mdns: Ok(Vec::new()),
        }
    }
}

fn fixture(fail: bool) -> (SimHandle, Arc<DiscoveryTransport>, LiveConfig) {
    let sim = SimHousehold::builder()
        .s1([
            SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1),
            SimPlayerSpec::new("Office", SimModel::Play5Gen1),
        ])
        .s2([SimPlayerSpec::new("Living Room", SimModel::One)])
        .spawn()
        .unwrap();
    let lan = Arc::new(
        Lan::start()
            .unwrap()
            .with_routes(sim.players().iter().map(|p| (p.ip, p.addr)).collect()),
    );
    let transport = Arc::new(DiscoveryTransport {
        lan,
        visible: Mutex::new(Vec::new()),
        gets: Mutex::new(Vec::new()),
        fail,
    });
    let mut config = LiveConfig::new(Vec::new());
    config.remembered = sim.players().iter().map(|p| p.ip).collect();
    // Tests request each further survey explicitly rather than racing a timer.
    config.interval = Duration::from_secs(3600);
    config.max_backoff = Duration::from_secs(3600);
    (sim, transport, config)
}

/// These tests drive real sockets and a live worker; running all four at once
/// on a loaded box starves the survey past the settle deadline, so they take
/// turns inside this binary.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn wait_for(live: &Live, predicate: impl Fn(&Snapshot) -> bool) -> Snapshot {
    let until = Instant::now() + Duration::from_secs(120);
    loop {
        let snapshot = live.snapshot();
        if predicate(&snapshot) {
            return snapshot;
        }
        assert!(
            Instant::now() < until,
            "live model did not settle: {snapshot:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn refresh(live: &Live) -> Snapshot {
    let before = live.snapshot().surveyed_at;
    live.refresh_soon();
    wait_for(live, |s| s.surveyed_at.is_some() && s.surveyed_at != before)
}

fn start(transport: &Arc<DiscoveryTransport>, config: LiveConfig) -> Live {
    let reads: Arc<dyn Transport + Send + Sync> = Arc::clone(transport) as _;
    Live::start_with(reads, Arc::clone(&transport.lan), config)
}

fn proves_event_updates(live: &Live, sim: &SimHandle, transport: &DiscoveryTransport) {
    let player = sim.player("Living Room").unwrap();
    let id = PlayerId(player.uuid.clone());
    wait_for(live, |s| {
        s.playback.of(&id).is_some_and(|p| p.volume.is_some())
    });
    set_volume(&*transport.lan, player.ip, 27).unwrap();
    wait_for(live, |s| {
        s.playback.of(&id).is_some_and(|p| p.volume == Some(27))
    });
}

#[test]
fn cached_only_startup_finds_both_households_and_receives_live_events() {
    let _serial = serial();
    for fail in [false, true] {
        let (sim, transport, config) = fixture(fail);
        assert!(config.seeds.is_empty());
        let live = start(&transport, config);
        // The live controller subscribes in the background once the survey
        // has published the households: wait for both, not just the first.
        let snapshot = wait_for(&live, |s| s.households.len() == 2 && s.subscriptions > 0);
        assert!(snapshot.callback.is_some());
        assert!(snapshot.last_error.is_none());
        proves_event_updates(&live, &sim, &transport);
        live.stop();
    }
}

#[test]
fn an_initially_offline_cached_household_is_not_lost_after_partial_startup() {
    let _serial = serial();
    let (sim, transport, config) = fixture(false);
    sim.set_offline("Living Room", true).unwrap();
    let live = start(&transport, config);
    wait_for(&live, |s| s.households.len() == 1);
    for _ in 0..3 {
        let snapshot = refresh(&live);
        assert_eq!(snapshot.households.len(), 1);
        assert!(resolve_room(&snapshot.households, "Living Room").is_err());
    }
    sim.set_offline("Living Room", false).unwrap();
    let snapshot = refresh(&live);
    assert_eq!(snapshot.households.len(), 2);
    proves_event_updates(&live, &sim, &transport);
    live.stop();
}

#[test]
fn warm_discovery_does_not_unconditionally_probe_every_cached_player() {
    let _serial = serial();
    let (sim, transport, config) = fixture(false);
    let live = start(&transport, config);
    wait_for(&live, |s| s.households.len() == 2);
    let kitchen = sim.player("Kitchen").unwrap().ip;
    let living = sim.player("Living Room").unwrap().ip;
    *transport.visible.lock().unwrap() = vec![kitchen, living];
    transport.gets.lock().unwrap().clear();
    let snapshot = refresh(&live);
    assert_eq!(snapshot.households.len(), 2);
    assert_eq!(*transport.gets.lock().unwrap(), [kitchen, living]);
    live.stop();
}

#[test]
fn startup_with_all_cached_players_offline_recovers_without_restart() {
    let _serial = serial();
    let (sim, transport, config) = fixture(true);
    for room in ["Kitchen", "Office", "Living Room"] {
        sim.set_offline(room, true).unwrap();
    }
    let live = start(&transport, config);
    let waiting = wait_for(&live, |s| s.last_error.is_some());
    assert!(waiting.households.is_empty());
    assert!(
        waiting.callback.is_some(),
        "the listener bootstrapped from cache"
    );
    sim.set_offline("Living Room", false).unwrap();
    let snapshot = refresh(&live);
    assert_eq!(snapshot.households.len(), 1);
    assert!(resolve_room(&snapshot.households, "Living Room").is_ok());
    proves_event_updates(&live, &sim, &transport);
    live.stop();
}
