//! An undo whose restore takes long (the speakers answer slowly) never holds
//! up the daemon's other calls on the store, and two undos at once never put
//! the same action back twice. Against `fsonos-sim` over real loopback
//! sockets, with a transport that takes its time to set a volume.

use fsonos_api::VolumeRequest;
use fsonos_api::plan::plan_volume;
use fsonos_api::surface::{Surface, Survey};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::{ActionFilter, MemStore};
use fsonos_proto::{ProtoError, Transport, mdns, ssdp};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// The simulator's LAN, but once `slow` a volume set takes two seconds,
/// and says when one has begun.
struct Sluggish {
    lan: SimLan,
    slow: Arc<AtomicBool>,
    setting: Arc<AtomicBool>,
}

impl Transport for Sluggish {
    fn soap_post(
        &self,
        host: IpAddr,
        control_path: &str,
        soap_action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if self.slow.load(Ordering::SeqCst) && soap_action.trim_matches('"').ends_with("#SetVolume")
        {
            self.setting.store(true, Ordering::SeqCst);
            std::thread::sleep(Duration::from_secs(2));
        }
        self.lan.soap_post(host, control_path, soap_action, body)
    }

    fn http_get(&self, url: &str) -> Result<String, ProtoError> {
        self.lan.http_get(url)
    }

    fn ssdp_search(&self, mx_secs: u8, wait: Duration) -> Result<Vec<ssdp::Advert>, ProtoError> {
        self.lan.ssdp_search(mx_secs, wait)
    }

    fn mdns_search(&self, wait: Duration) -> Result<Vec<mdns::SonosAdvert>, ProtoError> {
        self.lan.mdns_search(wait)
    }
}

/// A logged surface over the sim, and the switch that makes it slow, and the
/// flag that says a slow volume set began.
fn surface(sim: &SimHandle) -> (Arc<Surface>, Arc<AtomicBool>, Arc<AtomicBool>) {
    let (slow, setting) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    );
    let survey: Survey = Box::new(|t| {
        Ok(fsonos_core::inventory::survey(t, &[], Duration::from_millis(500))?.households)
    });
    let transport = Sluggish {
        lan: sim.lan(),
        slow: Arc::clone(&slow),
        setting: Arc::clone(&setting),
    };
    let s = Surface::new(
        Box::new(transport),
        survey,
        Policy::default(),
        Box::new(SystemClock),
    )
    .with_action_log(Box::new(MemStore::default()), "mcp");
    (Arc::new(s), slow, setting)
}

fn sim() -> SimHandle {
    SimHousehold::builder()
        .s1([
            SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1),
            SimPlayerSpec::new("Office", SimModel::Play5Gen1),
        ])
        .spawn()
        .unwrap()
}

/// Set `room`'s own volume as the CLI, logged and undoable.
fn set(s: &Surface, room: &str, level: i64) {
    let req = VolumeRequest {
        zone: room.into(),
        volume: Some(level),
        delta: None,
        group: false,
    };
    s.control(&Client::Cli, "set_volume", |h| plan_volume(h, &req))
        .unwrap();
}

#[test]
fn a_slow_undo_never_holds_up_the_log() {
    let sim = sim();
    let (s, slow, setting) = surface(&sim);
    set(&s, "Kitchen", 30);
    slow.store(true, Ordering::SeqCst);
    let undoing = {
        let s = Arc::clone(&s);
        std::thread::spawn(move || s.undo(&Client::Cli, false))
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    while !setting.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "the undo never set a volume");
        std::thread::sleep(Duration::from_millis(10));
    }
    // The restore is talking to the speakers now: the log still answers.
    let started = Instant::now();
    let log = s
        .recent_actions(&Client::Cli, &ActionFilter::default())
        .unwrap();
    let took = started.elapsed();
    let report = undoing.join().unwrap().unwrap().expect("an action to undo");
    assert!(
        took < Duration::from_secs(1),
        "the log answered in {took:?}, behind the undo"
    );
    assert!(log.iter().any(|a| a.id == report.undone), "{log:?}");
}

#[test]
fn two_undos_at_once_put_back_two_actions() {
    let sim = sim();
    let (s, slow, _) = surface(&sim);
    set(&s, "Kitchen", 30);
    set(&s, "Office", 40);
    slow.store(true, Ordering::SeqCst);
    let undo = || {
        let s = Arc::clone(&s);
        std::thread::spawn(move || s.undo(&Client::Cli, false))
    };
    let (first, second) = (undo(), undo());
    let undone: Vec<i64> = [first, second]
        .into_iter()
        .map(|h| {
            h.join()
                .unwrap()
                .unwrap()
                .expect("an action to undo")
                .undone
        })
        .collect();
    assert_ne!(
        undone[0], undone[1],
        "the same action was put back twice: {undone:?}"
    );
}
