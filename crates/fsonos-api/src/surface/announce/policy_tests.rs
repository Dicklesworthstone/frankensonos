//! A queued clip uses the caps in force when its household can play it.

use super::*;
use chrono::{DateTime, FixedOffset, TimeDelta};
use fsonos_core::announce::Announcer;
use fsonos_core::clock::{Clock, FakeClock};
use fsonos_core::policy::Policy;
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHousehold, SimLan, SimModel, SimPlayerSpec};
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(10);

struct SharedClock(Arc<FakeClock>);

impl Clock for SharedClock {
    fn now(&self) -> DateTime<FixedOffset> {
        self.0.now()
    }
}

/// Hold the first clip after it starts, so a second call really waits in
/// the household queue. Finish each clip after observing it playing.
struct QueuedClips {
    lan: SimLan,
    in_clip: AtomicBool,
    reads: AtomicUsize,
    first: Mutex<Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>>,
}

impl Transport for QueuedClips {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if action.contains("#GetTransportInfo")
            && self.in_clip.load(Ordering::Acquire)
            && self.reads.fetch_add(1, Ordering::AcqRel) == 1
        {
            soap::stop(&self.lan, host)?;
        }
        let result = self.lan.soap_post(host, path, action, body);
        if result.is_ok() && action.contains("#SetAVTransportURI") {
            let clip = body.contains("/media/");
            self.in_clip.store(clip, Ordering::Release);
            if clip {
                self.reads.store(0, Ordering::Release);
            }
        }
        if result.is_ok()
            && action.trim_matches('"').rsplit('#').next() == Some("Play")
            && self.in_clip.load(Ordering::Acquire)
        {
            let first = self.first.lock().unwrap().take();
            if let Some((entered, release)) = first {
                entered.send(()).unwrap();
                release.recv_timeout(WAIT).unwrap();
            }
        }
        result
    }
}

fn queued_cap(s2: bool, change_policy: bool) {
    let spec = SimPlayerSpec::new(
        "Kitchen",
        if s2 {
            SimModel::One
        } else {
            SimModel::Play5Gen1
        },
    );
    let builder = SimHousehold::builder();
    let sim = if s2 {
        builder.s2([spec])
    } else {
        builder.s1([spec])
    }
    .spawn()
    .unwrap();
    let host = sim.player("Kitchen").unwrap().ip;
    let mut house = HouseholdState::default();
    house.apply_topology(&get_zone_group_state(&sim.lan(), host).unwrap());
    let (entered, playing) = mpsc::channel();
    let (release, waiting) = mpsc::channel();
    let clock = Arc::new(FakeClock::new(
        DateTime::parse_from_rfc3339("2026-10-07T21:59:00-04:00").unwrap(),
    ));
    let initial = "[defaults]\nmax_volume = 70\n\
        [quiet_hours]\nstart = \"22:00\"\nend = \"07:00\"\nmax_volume = 20\n";
    let data = tempfile::tempdir().unwrap();
    std::fs::write(data.path().join("policy.toml"), initial).unwrap();
    let mut announcements = Announcements::new(
        MediaStore::new(data.path()),
        Box::new(|| Some("http://192.0.2.250:3400".into())),
    );
    announcements.announcer =
        Announcer::with_timing(Duration::from_millis(5), Duration::from_millis(100));
    let surface = Surface::new(
        Box::new(QueuedClips {
            lan: sim.lan(),
            in_clip: AtomicBool::new(false),
            reads: AtomicUsize::new(0),
            first: Mutex::new(Some((entered, waiting))),
        }),
        Box::new(move |_| Ok(vec![house.clone()])),
        Policy::from_toml(initial).unwrap(),
        Box::new(SharedClock(Arc::clone(&clock))),
    )
    .with_policy_file(data.path().to_path_buf())
    .with_announcements(announcements);
    let request = AnnounceRequest {
        chime: Some("beep".into()),
        rooms: vec!["Kitchen".into()],
        volume: Some(60),
        ..AnnounceRequest::default()
    };
    std::thread::scope(|scope| {
        let first = scope.spawn(|| surface.announce(&Client::LoopbackHttp, &request));
        playing.recv_timeout(WAIT).unwrap();
        let second = scope.spawn(|| surface.announce(&Client::LoopbackHttp, &request));
        let deadline = Instant::now() + WAIT;
        while surface.announcements_pending() != 2 {
            assert!(
                Instant::now() < deadline,
                "second announcement did not queue"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let expected = if change_policy {
            std::fs::write(
                data.path().join("policy.toml"),
                "[defaults]\nmax_volume = 70\n[rooms.Kitchen]\nmax_volume = 12\n",
            )
            .unwrap();
            12
        } else {
            clock.advance(TimeDelta::minutes(2));
            20
        };
        release.send(()).unwrap();
        let first = first.join().unwrap().unwrap();
        let second = second.join().unwrap().unwrap();
        assert_eq!(first.households[0].levels[0].volume, 60, "{first:?}");
        assert_eq!(
            second.households[0].levels[0].volume, expected,
            "{second:?}"
        );
        assert_eq!(second.households[0].outcome, "finished", "{second:?}");
        assert_eq!(surface.announcements_pending(), 0);
    });
    sim.shutdown();
}

#[test]
fn queued_announcements_obey_quiet_hours_when_their_turn_starts() {
    for s2 in [false, true] {
        queued_cap(s2, false);
    }
}

#[test]
fn queued_announcements_use_the_latest_room_caps() {
    for s2 in [false, true] {
        queued_cap(s2, true);
    }
}
