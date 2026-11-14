//! Household-specific clip addresses through the shared delivery lifecycle.

use super::*;
use crate::surface::schedules::{ScheduleRequest, Scheduler};
use chrono::{DateTime, FixedOffset, TimeDelta, Utc};
use fsonos_core::clock::{Clock, FakeClock};
use fsonos_core::policy::Policy;
use fsonos_core::store::MemStore;
use fsonos_proto::control as soap;
use fsonos_proto::net::{EventSink, MediaFiles};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use fsonos_types::TransportState;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(10);
const ORIGINAL: &str = "x-rincon-mp3radio://stream.example.invalid/original.mp3";
const ROOMS: [&str; 4] = ["MemberOne", "LeadOne", "MemberTwo", "LeadTwo"];
const LEADS: [&str; 2] = ["LeadOne", "LeadTwo"];

struct SharedClock(Arc<FakeClock>);

impl Clock for SharedClock {
    fn now(&self) -> DateTime<FixedOffset> {
        self.0.now()
    }
}

struct House {
    sim: SimHandle,
    surface: Arc<Surface>,
    routes: Arc<Mutex<HashMap<PlayerId, String>>>,
    lookups: Arc<Mutex<Vec<PlayerId>>>,
    clock: Arc<FakeClock>,
    _sinks: Vec<EventSink>,
    _dir: tempfile::TempDir,
}

impl House {
    fn new() -> Self {
        let sim = SimHousehold::builder()
            .s1(ROOMS[..2]
                .iter()
                .map(|name| SimPlayerSpec::new(name, SimModel::Play5Gen1)))
            .s2(ROOMS[2..]
                .iter()
                .map(|name| SimPlayerSpec::new(name, SimModel::One)))
            .spawn()
            .unwrap();
        let lan = sim.lan();
        let households: Vec<_> = LEADS
            .iter()
            .map(|name| {
                let mut house = HouseholdState::default();
                house.apply_topology(
                    &get_zone_group_state(&lan, sim.player(name).unwrap().ip).unwrap(),
                );
                house
            })
            .collect();
        // Admission sees the earlier independent rooms. The announcer's fresh
        // topology must choose the actual coordinator, not each list's first room.
        for rooms in ROOMS.chunks_exact(2) {
            let lead = sim.player(rooms[1]).unwrap();
            let member = sim.player(rooms[0]).unwrap();
            soap::set_av_transport_uri(&lan, lead.ip, ORIGINAL, "").unwrap();
            soap::play(&lan, lead.ip).unwrap();
            soap::set_volume(&lan, lead.ip, 18).unwrap();
            soap::set_volume(&lan, member.ip, 12).unwrap();
            soap::set_av_transport_uri(&lan, member.ip, &format!("x-rincon:{}", lead.uuid), "")
                .unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        let media = MediaStore::new(dir.path());
        let files: MediaFiles = {
            let media = media.clone();
            Arc::new(move |name| media.path(name))
        };
        let sinks: Vec<_> = (0..2)
            .map(|_| {
                EventSink::start_serving("127.0.0.1:0".parse().unwrap(), Some(Arc::clone(&files)))
                    .unwrap()
            })
            .collect();
        let routes = Arc::new(Mutex::new(
            LEADS
                .iter()
                .zip(&sinks)
                .map(|(name, sink)| {
                    (
                        PlayerId(sim.player(name).unwrap().uuid.clone()),
                        format!("http://{}", sink.local_addr()),
                    )
                })
                .collect::<HashMap<_, _>>(),
        ));
        let lookups = Arc::new(Mutex::new(Vec::new()));
        let routing = Arc::clone(&routes);
        let looked_up = Arc::clone(&lookups);
        let mut announcements = Announcements::new(
            media,
            // This legacy readiness address must never become a clip fallback.
            Box::new(|| Some("http://192.0.2.254:9".into())),
        )
        .with_media_route(Box::new(move |player| {
            looked_up.lock().unwrap().push(player.clone());
            routing.lock().unwrap().get(player).cloned()
        }));
        announcements.announcer =
            Announcer::with_timing(Duration::from_millis(5), Duration::from_secs(1));
        let clock = Arc::new(FakeClock::new(
            DateTime::parse_from_rfc3339("2026-10-01T10:00:00Z").unwrap(),
        ));
        let surface = Arc::new(
            Surface::new(
                Box::new(sim.lan()),
                Box::new(move |_| Ok(households.clone())),
                Policy::default(),
                Box::new(SharedClock(Arc::clone(&clock))),
            )
            .with_action_log(Box::new(MemStore::default()), "announcement-routing")
            .with_announcements(announcements),
        );
        Self {
            sim,
            surface,
            routes,
            lookups,
            clock,
            _sinks: sinks,
            _dir: dir,
        }
    }

    fn run<T>(&self, call: impl FnOnce() -> T) -> T {
        let stop = AtomicBool::new(false);
        let clock = self.sim.clock();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for _ in 0..15_000 {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    clock.advance(Duration::from_millis(10));
                    std::thread::sleep(Duration::from_millis(1));
                }
            });
            let result = call();
            stop.store(true, Ordering::Release);
            result
        })
    }

    fn finished(&self, id: &str) -> AnnouncementJobDto {
        let deadline = Instant::now() + WAIT;
        loop {
            let job = self.surface.get_announcement(&Client::Cli, id).unwrap();
            if job.finished_at.is_some() && self.surface.announcements_pending() == 0 {
                return job;
            }
            assert!(
                Instant::now() < deadline,
                "announcement did not complete: {job:?}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn assert_restored(&self) {
        let lan = self.sim.lan();
        for rooms in ROOMS.chunks_exact(2) {
            let member = self.sim.player(rooms[0]).unwrap();
            let lead = self.sim.player(rooms[1]).unwrap();
            assert_eq!(soap::get_media_info(&lan, lead.ip).unwrap().uri, ORIGINAL);
            assert_eq!(
                soap::get_transport_info(&lan, lead.ip).unwrap().state,
                TransportState::Playing
            );
            assert_eq!(soap::get_volume(&lan, lead.ip).unwrap(), 18);
            assert_eq!(soap::get_volume(&lan, member.ip).unwrap(), 12);
            let groups = get_zone_group_state(&lan, lead.ip).unwrap();
            let mut house = HouseholdState::default();
            house.apply_topology(&groups);
            assert_eq!(
                house
                    .rooms
                    .iter()
                    .find(|room| room.primary.0 == member.uuid)
                    .unwrap()
                    .coordinator
                    .0,
                lead.uuid
            );
        }
    }

    fn assert_routes(&self, leads: &[&str]) {
        assert!(self.sim.wait_for_fetches(WAIT));
        let fetched = self.sim.fetch_log();
        assert_eq!(fetched.len(), leads.len(), "{fetched:?}");
        let routes = self.routes.lock().unwrap();
        for name in leads {
            let id = PlayerId(self.sim.player(name).unwrap().uuid.clone());
            let fetch = fetched.iter().find(|fetch| fetch.player == id.0).unwrap();
            let base = routes.get(&id).unwrap();
            assert!(
                fetch.url.starts_with(&format!("{base}/media/")),
                "{fetch:?}"
            );
            assert_eq!(fetch.result, Ok(200));
            assert!(fetch.wav_duration_ms.is_some());
            let installed = self
                .sim
                .soap_log()
                .into_iter()
                .find(|entry| {
                    entry.player == id.0
                        && entry.action == "SetAVTransportURI"
                        && entry
                            .args
                            .iter()
                            .any(|(key, value)| key == "CurrentURI" && value == &fetch.url)
                })
                .unwrap();
            assert!(
                installed
                    .args
                    .iter()
                    .any(|(key, value)| key == "CurrentURIMetaData" && value.contains(&fetch.url))
            );
        }
        self.assert_restored();
    }
}

fn request() -> AnnounceRequest {
    AnnounceRequest {
        chime: Some("beep".into()),
        rooms: ROOMS.iter().map(|name| (*name).to_owned()).collect(),
        ..AnnounceRequest::default()
    }
}

#[test]
fn synchronous_clips_use_each_refreshed_leads_media_route_and_restore_both_households() {
    let house = House::new();
    let report = house.run(|| house.surface.announce(&Client::Cli, &request()).unwrap());
    assert!(report.clean, "{report:?}");
    house.assert_routes(&LEADS);
    let lookups = house.lookups.lock().unwrap();
    assert_eq!(lookups.len(), 2);
    for lead in LEADS {
        assert!(lookups.contains(&PlayerId(house.sim.player(lead).unwrap().uuid.clone())));
    }
    let fetched = house.sim.fetch_log();
    assert_eq!(
        fetched[0].url.split("/media/").nth(1),
        fetched[1].url.split("/media/").nth(1),
        "the same generated clip is served on both listeners"
    );
}

#[test]
fn a_missing_route_fails_only_that_background_household_before_levels_or_speaker_writes() {
    let house = House::new();
    let missing = PlayerId(house.sim.player(LEADS[0]).unwrap().uuid.clone());
    house.routes.lock().unwrap().remove(&missing);
    let before = house.sim.soap_log().len();
    let job = house.run(|| {
        let started = house
            .surface
            .start_announcement(&Client::Cli, &request())
            .unwrap();
        house.finished(&started.id)
    });
    assert_eq!(job.phase, AnnouncementJobPhase::Failed, "{job:?}");
    let report = job.result.unwrap();
    let failed = report
        .households
        .iter()
        .find(|entry| entry.lead == LEADS[0])
        .unwrap();
    assert_eq!(failed.outcome, "failed");
    assert!(
        failed
            .error
            .as_deref()
            .unwrap()
            .contains("no reachable media listener")
    );
    assert!(failed.levels.is_empty() && failed.not_restored.is_empty());
    assert!(
        report
            .households
            .iter()
            .any(|entry| entry.lead == LEADS[1] && entry.outcome == "finished")
    );
    assert!(
        house.sim.soap_log()[before..]
            .iter()
            .filter(|entry| ROOMS[..2].contains(&entry.room.as_str()))
            .all(|entry| entry.action.starts_with("Get")),
        "missing media must not change that household"
    );
    house.assert_routes(&LEADS[1..]);
}

#[test]
fn scheduled_clips_share_household_routes_without_replaying_the_occurrence() {
    let house = House::new();
    let schedule: ScheduleRequest = serde_json::from_value(serde_json::json!({
        "when": "in 1m", "action": "announce", "announcement": request()
    }))
    .unwrap();
    house.surface.add_schedule(&Client::Cli, &schedule).unwrap();
    house.clock.advance(TimeDelta::minutes(1));
    let now = house.clock.now().to_utc();
    let scheduler = Scheduler::new(&house.surface);
    house.run(|| {
        assert_eq!(scheduler.tick_at(now, &Utc).len(), 1);
        assert!(scheduler.wait(WAIT));
    });
    let jobs = house.surface.list_announcements(&Client::Cli).unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].phase, AnnouncementJobPhase::Completed, "{jobs:?}");
    house.assert_routes(&LEADS);
    assert!(
        scheduler
            .tick_at(now + TimeDelta::seconds(1), &Utc)
            .is_empty()
    );
    assert_eq!(house.sim.fetch_log().len(), 2);
}
