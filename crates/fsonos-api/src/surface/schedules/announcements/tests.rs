//! Recipe validation plus real protocol/media/store regressions for scheduled announcements.

use super::*;
use crate::surface::announce::Announcements;
use crate::surface::schedules::{ScheduleDto, ScheduleRequest, Scheduler};
use crate::surface::{Surface, Survey};
use chrono::{DateTime, FixedOffset, TimeDelta, Utc};
use fsonos_core::announce::clip::MediaStore;
use fsonos_core::clock::{Clock, FakeClock};
use fsonos_core::policy::{Client, Policy};
use fsonos_core::schedule::{Schedule, ScheduleSpec};
use fsonos_core::store::{ActionFilter, MemStore, SqliteStore, Store};
use fsonos_core::{HouseholdState, control, resolve_room};
use fsonos_proto::control as soap;
use fsonos_proto::net::{EventSink, MediaFiles};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use fsonos_types::TransportState;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn request(payload: Value) -> ScheduleRequest {
    serde_json::from_value(json!({
        "when": "daily 10:01", "action": "announce", "announcement": payload
    }))
    .unwrap()
}

fn chime(rooms: &[&str]) -> ScheduleRequest {
    request(json!({"chime": " BeLL ", "rooms": rooms, "volume": 40}))
}

#[test]
fn scheduled_recipes_are_normalized_and_round_trip_without_expiring_media() {
    for payload in [
        json!({"text":"  Time to leave  ", "voice":"  Local voice  ", "rooms":[" Room0 "], "volume":0}),
        json!({"chime":"  BeLL  ", "rooms":[]}),
    ] {
        let req = request(payload);
        req.validate().unwrap();
        let action = req.action().unwrap();
        let encoded = serde_json::to_string(&action).unwrap();
        assert!(!encoded.contains("wav_base64") && !encoded.contains("/media/"));
        assert_eq!(
            serde_json::from_str::<ScheduleAction>(&encoded).unwrap(),
            action
        );
        let now = DateTime::parse_from_rfc3339("2026-10-01T10:00:00Z").unwrap();
        let schedule = Schedule {
            id: 1,
            spec: ScheduleSpec::parse(&req.when, now).unwrap(),
            action,
            creator: "tag:reminder".into(),
            enabled: true,
            created: now.to_utc(),
            last_fired: None,
        };
        let dto = ScheduleDto::new(&schedule, now.to_utc());
        assert_eq!(dto.action, "announce");
        assert_eq!(dto.zone, None);
        assert_eq!(dto.volume, None);
        let recipe = dto.announcement.as_ref().unwrap();
        assert_eq!(super::action(recipe).unwrap(), schedule.action);
        if recipe.text.is_some() {
            assert_eq!(recipe.text.as_deref(), Some("Time to leave"));
            assert_eq!(recipe.voice.as_deref(), Some("Local voice"));
            assert_eq!(recipe.rooms, ["Room0"]);
            assert_eq!(recipe.volume, Some(0));
        } else {
            assert_eq!(recipe.chime.as_deref(), Some("bell"));
            assert_eq!(recipe.volume, None);
            assert!(dto.summary.contains("every room"));
        }
        assert_eq!(
            serde_json::from_value::<ScheduleDto>(serde_json::to_value(&dto).unwrap()).unwrap(),
            dto,
        );
    }
    for old in [
        json!({"kind":"pause", "room":"Room0"}),
        json!({"kind":"volume", "room":"Room0", "level":20}),
        json!({"kind":"apply_scene", "name":"Evening"}),
        json!({"kind":"dj_start", "room":"Room0"}),
    ] {
        assert!(serde_json::from_value::<ScheduleAction>(old).is_ok());
    }
}

#[test]
fn invalid_recipes_and_conflicting_action_fields_fail_before_io() {
    let long_text = "x".repeat(1001);
    let long_voice = "x".repeat(4097);
    let long_room = "x".repeat(129);
    for payload in [
        json!({}),
        json!({"text":" "}),
        json!({"text":long_text}),
        json!({"text":"hello", "chime":"bell"}),
        json!({"chime":"unknown"}),
        json!({"chime":"bell", "voice":"voice"}),
        json!({"text":"hello", "voice":"-option"}),
        json!({"text":"hello", "voice":"bad\nvoice"}),
        json!({"text":"hello", "voice":long_voice}),
        json!({"chime":"bell", "volume":101}),
        json!({"chime":"bell", "rooms":[""]}),
        json!({"chime":"bell", "rooms":[long_room]}),
        json!({"wav_base64":"not even base64"}),
        json!({"text":"hello", "wav_base64":""}),
    ] {
        let failure = request(payload.clone()).validate().unwrap_err();
        assert_eq!(failure.code, crate::ErrorCode::InvalidArgument, "{payload}");
    }
    assert!(request(json!({"text":"é".repeat(1000)})).validate().is_ok());
    for (name, value) in [
        ("zone", json!("Room0")),
        ("scene", json!("Evening")),
        ("mood", json!("calm")),
        ("direction", json!({})),
        ("volume", json!(20)),
        ("fade_secs", json!(0)),
    ] {
        let mut value_with_conflict = serde_json::to_value(chime(&["Room0"])).unwrap();
        value_with_conflict[name] = value;
        let req: ScheduleRequest = serde_json::from_value(value_with_conflict).unwrap();
        assert!(req.validate().unwrap_err().detail.contains(name));
    }
    for kind in ["pause", "volume", "dj_start", "apply_scene"] {
        let mut req = chime(&["Room0"]);
        req.action = kind.into();
        assert!(req.validate().unwrap_err().detail.contains("announcement"));
    }
}

struct SharedClock(Arc<FakeClock>);

impl Clock for SharedClock {
    fn now(&self) -> DateTime<FixedOffset> {
        self.0.now()
    }
}

struct House {
    sim: SimHandle,
    dir: tempfile::TempDir,
    media: MediaStore,
    sink: EventSink,
    houses: Vec<HouseholdState>,
    inventory: Arc<Mutex<Vec<HouseholdState>>>,
    ready: Arc<AtomicBool>,
    clock: Arc<FakeClock>,
}

impl House {
    fn new(s2: bool) -> Self {
        let specs = ["Room0", "Room1"].map(|name| {
            SimPlayerSpec::new(
                name,
                if s2 {
                    SimModel::One
                } else {
                    SimModel::Play5Gen1
                },
            )
        });
        let builder = SimHousehold::builder();
        let sim = if s2 {
            builder.s2(specs)
        } else {
            builder.s1(specs)
        }
        .spawn()
        .unwrap();
        let mut house = HouseholdState::default();
        house.apply_topology(
            &get_zone_group_state(&sim.lan(), sim.player("Room0").unwrap().ip).unwrap(),
        );
        let houses = vec![house];
        let inventory = Arc::new(Mutex::new(houses.clone()));
        let dir = tempfile::tempdir().unwrap();
        let media = MediaStore::new(dir.path());
        let files: MediaFiles = {
            let media = media.clone();
            Arc::new(move |name| media.path(name))
        };
        let sink = EventSink::start_serving("127.0.0.1:0".parse().unwrap(), Some(files)).unwrap();
        Self {
            sim,
            dir,
            media,
            sink,
            houses,
            inventory,
            ready: Arc::new(AtomicBool::new(true)),
            clock: Arc::new(FakeClock::new(
                DateTime::parse_from_rfc3339("2026-10-01T10:00:00Z").unwrap(),
            )),
        }
    }

    fn surface(&self, store: Box<dyn Store + Send>) -> Arc<Surface> {
        let inventory = Arc::clone(&self.inventory);
        let survey: Survey = Box::new(move |_| Ok(inventory.lock().unwrap().clone()));
        let ready = Arc::clone(&self.ready);
        let base = format!("http://{}", self.sink.local_addr());
        let policy =
            std::fs::read_to_string(self.dir.path().join("policy.toml")).unwrap_or_default();
        Arc::new(
            Surface::new(
                Box::new(self.sim.lan()),
                survey,
                Policy::from_toml(&policy).unwrap(),
                Box::new(SharedClock(Arc::clone(&self.clock))),
            )
            .with_action_log(store, "scheduled-announcement-test")
            .with_policy_file(self.dir.path().to_path_buf())
            .with_aliases_file(self.dir.path().join("aliases.toml"))
            .with_announcements(Announcements::new(
                self.media.clone(),
                Box::new(move || ready.load(Ordering::Acquire).then(|| base.clone())),
            )),
        )
    }

    fn persistent(&self) -> Arc<Surface> {
        self.surface(Box::new(
            SqliteStore::open(&self.dir.path().join("schedules.db")).unwrap(),
        ))
    }

    fn run(&self, scheduler: &Scheduler, now: DateTime<Utc>) {
        self.clock.set(now.fixed_offset());
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
            scheduler.tick_at(now, &Utc);
            let finished = scheduler.wait(Duration::from_secs(10));
            stop.store(true, Ordering::Release);
            assert!(finished, "scheduled announcement did not finish");
        });
    }

    fn without(&self, name: &str) -> Vec<HouseholdState> {
        let mut houses = self.houses.clone();
        let player = resolve_room(&houses, name).unwrap().player.id.clone();
        for house in &mut houses {
            house.players.retain(|p| p.id != player);
            house.rooms.retain(|r| r.primary != player);
            house.groups.retain(|g| g.coordinator != player);
        }
        houses
    }
}

fn creator() -> Client {
    Client::Tailnet("tag:reminder".into())
}

#[test]
fn a_persisted_chime_fetches_audio_once_and_restores_music_in_both_generations() {
    for s2 in [false, true] {
        let house = House::new(s2);
        let lan = house.sim.lan();
        let target = resolve_room(&house.houses, "Room0").unwrap();
        let stream = "x-rincon-mp3radio://stream.example.invalid/before-reminder.mp3";
        control::play_uri(&lan, &house.houses, &target.player.id, stream, "").unwrap();
        soap::set_volume(&lan, target.player.ip, 18).unwrap();
        let before = house.sim.soap_log().len();
        let surface = house.persistent();
        let added = surface
            .add_schedule(&creator(), &chime(&["Room0"]))
            .unwrap();
        assert_eq!(
            house.sim.soap_log().len(),
            before,
            "saving a recipe never changes a speaker"
        );
        assert!(
            !house.media.dir().exists(),
            "saving a recipe never synthesizes audio"
        );
        drop(surface);
        let surface = house.persistent();
        assert_eq!(
            surface.schedules(&creator()).unwrap()[0].announcement,
            added.announcement
        );
        std::fs::write(
            house.dir.path().join("policy.toml"),
            "[defaults]\nmax_volume = 23\n",
        )
        .unwrap();
        let at = house.clock.now().to_utc() + TimeDelta::minutes(1);
        let scheduler = Scheduler::new(&surface);
        house.run(&scheduler, at);
        assert_eq!(surface.load_schedules().unwrap()[0].last_fired, Some(at));
        let fetches = house.sim.fetch_log();
        assert_eq!(fetches.len(), 1, "{fetches:?}");
        assert_eq!(fetches[0].result, Ok(200));
        assert!(fetches[0].wav_duration_ms.is_some());
        assert_eq!(fetches[0].room, "Room0");
        assert_eq!(
            soap::get_media_info(&lan, target.player.ip).unwrap().uri,
            stream
        );
        assert_eq!(
            soap::get_transport_info(&lan, target.player.ip)
                .unwrap()
                .state,
            TransportState::Playing
        );
        assert_eq!(soap::get_volume(&lan, target.player.ip).unwrap(), 18);
        let actions = surface
            .recent_actions(&Client::Cli, &ActionFilter::default())
            .unwrap();
        let delivered = actions
            .iter()
            .find(|a| a.action.intent.starts_with("announce:"))
            .unwrap();
        assert_eq!(delivered.action.client, creator().key());
        assert!(delivered.action.decision.contains("capped at 23"));
        assert!(
            scheduler
                .tick_at(at + TimeDelta::seconds(1), &Utc)
                .is_empty()
        );
        assert_eq!(house.sim.fetch_log().len(), 1);
        drop(scheduler);
        drop(surface);
        let surface = house.persistent();
        let scheduler = Scheduler::new(&surface);
        assert!(
            scheduler
                .tick_at(at + TimeDelta::minutes(1), &Utc)
                .is_empty()
        );
        house.run(&scheduler, at + TimeDelta::days(1));
        assert_eq!(house.sim.fetch_log().len(), 2);
    }
}

#[test]
fn admission_waits_for_the_media_listener_and_every_explicit_alias_member() {
    let house = House::new(true);
    std::fs::write(
        house.dir.path().join("aliases.toml"),
        "[aliases]\nreminder = ['Room0', 'Room1']\n",
    )
    .unwrap();
    let surface = house.surface(Box::new(MemStore::default()));
    let speech = request(json!({"text":"A stored reminder", "rooms":["Room0"]}));
    let saved = surface.add_schedule(&creator(), &speech).unwrap();
    surface.remove_schedule(&creator(), saved.id).unwrap();
    assert!(
        !house.media.dir().exists(),
        "speech was validated without running a backend"
    );
    surface
        .add_schedule(&creator(), &chime(&["reminder"]))
        .unwrap();
    let at = house.clock.now().to_utc() + TimeDelta::minutes(1);
    let scheduler = Scheduler::new(&surface);
    house.ready.store(false, Ordering::Release);
    house.run(&scheduler, at);
    assert_eq!(surface.load_schedules().unwrap()[0].last_fired, None);
    house.ready.store(true, Ordering::Release);
    *house.inventory.lock().unwrap() = house.without("Room1");
    surface.invalidate();
    house.run(&scheduler, at + TimeDelta::minutes(1));
    assert_eq!(surface.load_schedules().unwrap()[0].last_fired, None);
    assert!(house.sim.fetch_log().is_empty());
    assert!(!house.media.dir().exists());
    *house.inventory.lock().unwrap() = house.houses.clone();
    surface.invalidate();
    house.run(&scheduler, at + TimeDelta::minutes(2));
    assert_eq!(surface.load_schedules().unwrap()[0].last_fired, Some(at));
    assert!(!house.sim.fetch_log().is_empty());
}

#[test]
fn a_revoked_creator_cannot_deliver_and_empty_rooms_means_every_room() {
    let house = House::new(true);
    let surface = house.surface(Box::new(MemStore::default()));
    let all = chime(&[]);
    let added = surface.add_schedule(&creator(), &all).unwrap();
    assert!(added.announcement.as_ref().unwrap().rooms.is_empty());
    let at = house.clock.now().to_utc() + TimeDelta::minutes(1);
    std::fs::write(
        house.dir.path().join("policy.toml"),
        "[clients.\"tag:reminder\"]\ndeny = ['announce']\n",
    )
    .unwrap();
    let scheduler = Scheduler::new(&surface);
    let before = house.sim.soap_log().len();
    house.run(&scheduler, at);
    assert_eq!(house.sim.soap_log().len(), before);
    assert!(!house.media.dir().exists());
    assert_eq!(surface.load_schedules().unwrap()[0].last_fired, Some(at));
    assert!(surface.add_schedule(&creator(), &all).is_err());
    std::fs::write(
        house.dir.path().join("policy.toml"),
        "[defaults]\nmax_volume = 25\n",
    )
    .unwrap();
    house.run(&scheduler, at + TimeDelta::days(1));
    let actions = surface
        .recent_actions(&Client::Cli, &ActionFilter::default())
        .unwrap();
    let delivery = actions
        .iter()
        .find(|a| a.action.intent.starts_with("announce:"))
        .unwrap();
    assert!(delivery.action.intent.contains("Room0"));
    assert!(delivery.action.intent.contains("Room1"));
    for room in ["Room0", "Room1"] {
        assert!(
            house
                .sim
                .soap_log()
                .iter()
                .any(|call| call.room == room && call.action == "SetVolume")
        );
    }
}

#[test]
fn busy_announcement_capacity_defers_claim_and_delivery_reuses_its_reservation() {
    let house = House::new(true);
    let surface = house.surface(Box::new(MemStore::default()));
    surface
        .add_schedule(&creator(), &chime(&["Room0"]))
        .unwrap();
    let at = house.clock.now().to_utc() + TimeDelta::minutes(1);
    let first = surface.reserve_announcement(&creator()).unwrap();
    let second = surface.reserve_announcement(&creator()).unwrap();
    let scheduler = Scheduler::new(&surface);
    house.run(&scheduler, at);
    assert_eq!(surface.load_schedules().unwrap()[0].last_fired, None);
    assert!(!house.media.dir().exists());
    drop(first);
    // One occupied slot remains. The worker's reservation uses the other:
    // delivery would fail if it tried to acquire a third slot after claiming.
    house.run(&scheduler, at + TimeDelta::minutes(1));
    assert_eq!(surface.load_schedules().unwrap()[0].last_fired, Some(at));
    assert_eq!(house.sim.fetch_log().len(), 1);
    assert_eq!(house.sim.fetch_log()[0].result, Ok(200));
    assert!(
        scheduler
            .tick_at(at + TimeDelta::minutes(2), &Utc)
            .is_empty()
    );
    drop(second);
}
