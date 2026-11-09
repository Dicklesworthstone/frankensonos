//! Cancellation during read-only preparation must preserve a valid interrupted
//! sleep, while an external source change must not acquire that continuation.

use super::*;
use crate::surface::announce::Announcements;
use fsonos_core::announce::clip::MediaStore;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::Policy;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHousehold, SimLan, SimModel, SimPlayerSpec};
use std::sync::Weak;

const ORIGINAL: &str = "x-rincon-mp3radio://stream.example.org/original.mp3";
const MANUAL: &str = "x-rincon-mp3radio://stream.example.org/manual.mp3";

struct CancelDuringSnapshot {
    lan: SimLan,
    surface: Arc<Mutex<Weak<Surface>>>,
    first: AtomicBool,
    change_source: bool,
    writes: Arc<Mutex<Vec<String>>>,
}

impl Transport for CancelDuringSnapshot {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action
            .trim_matches('"')
            .rsplit('#')
            .next()
            .unwrap_or_default();
        if !name.starts_with("Get") && name != "Browse" {
            self.writes.lock().unwrap().push(name.to_owned());
        }
        let response = self.lan.soap_post(host, path, action, body);
        // Source evidence has been captured, the API cap callback has
        // interrupted sleep, and snapshot capture is still entirely reads.
        if name == "GetGroupVolume" && self.first.swap(false, Ordering::AcqRel) {
            let surface = self.surface.lock().unwrap().upgrade().unwrap();
            let jobs = surface.list_announcements(&Client::Cli).unwrap();
            assert_eq!(jobs.len(), 1);
            assert!(
                surface
                    .cancel_announcement(&Client::Cli, &jobs[0].id)
                    .unwrap()
                    .cancel_requested
            );
            if self.change_source {
                soap::set_av_transport_uri(&self.lan, host, MANUAL, "")?;
                soap::play(&self.lan, host)?;
            }
        }
        response
    }
}

fn cancelled_preparation_keeps_only_owned_sleep(change_source: bool) {
    let sim = SimHousehold::builder()
        .s2([SimPlayerSpec::new("Kitchen", SimModel::One)])
        .spawn()
        .unwrap();
    let lan = sim.lan();
    let host = sim.player("Kitchen").unwrap().ip;
    soap::set_av_transport_uri(&lan, host, ORIGINAL, "").unwrap();
    soap::play(&lan, host).unwrap();
    let mut house = HouseholdState::default();
    house.apply_topology(&get_zone_group_state(&lan, host).unwrap());
    let coordinator = house.rooms[0].coordinator.clone();
    let houses = vec![house];
    let survey = houses.clone();
    let data = tempfile::tempdir().unwrap();
    let slot = Arc::new(Mutex::new(Weak::new()));
    let writes = Arc::new(Mutex::new(Vec::new()));
    let announcements = Announcements::new(
        MediaStore::new(data.path()),
        Box::new(|| Some("http://192.0.2.250:3400".into())),
    );
    let surface = Arc::new(
        Surface::new(
            Box::new(CancelDuringSnapshot {
                lan: lan.clone(),
                surface: Arc::clone(&slot),
                first: AtomicBool::new(true),
                change_source,
                writes: Arc::clone(&writes),
            }),
            Box::new(move |_| Ok(survey.clone())),
            Policy::default(),
            Box::new(SystemClock),
        )
        .with_sleep(Sleep::default())
        .with_announcements(announcements),
    );
    *slot.lock().unwrap() = Arc::downgrade(&surface);
    let sleep = surface.sleep.as_ref().unwrap();
    let now = Utc::now();
    let original = sleep
        .timers
        .start(
            &*surface.transport,
            &houses,
            &sleep.fader,
            &coordinator,
            Duration::from_secs(600),
            now,
        )
        .unwrap();
    assert_eq!(sleep.timers.due(now + TimeDelta::minutes(10)).len(), 1);
    let report = surface
        .announce(
            &Client::Cli,
            &AnnounceRequest {
                chime: Some("beep".into()),
                rooms: vec!["Kitchen".into()],
                ..AnnounceRequest::default()
            },
        )
        .unwrap();
    let after = sleep.timers.get(&coordinator);
    if change_source {
        assert!(
            after.is_none_or(|timer| timer.fading && timer.ends == original.ends),
            "a new source must not inherit the cancelled announcement's sleep continuation"
        );
        assert!(!report.households[0].not_restored.is_empty(), "{report:?}");
        assert_eq!(soap::get_media_info(&lan, host).unwrap().uri, MANUAL);
    } else {
        let timer = after.expect("verified unchanged playback resumes the interrupted sleep");
        assert!(!timer.fading);
        assert_ne!(timer.ends, original.ends);
        assert!(timer.ends <= Utc::now() + delta(FADE));
        assert_eq!(report.households[0].outcome, "cancelled", "{report:?}");
        assert!(report.households[0].not_restored.is_empty(), "{report:?}");
        assert_eq!(soap::get_media_info(&lan, host).unwrap().uri, ORIGINAL);
    }
    let writes = writes.lock().unwrap();
    assert!(
        writes.iter().all(|action| action == "ConfigureSleepTimer"),
        "preparation cancellation must not play, stop, regroup or change levels: {writes:?}"
    );
    assert_eq!(surface.announcements_pending(), 0);
    sim.shutdown();
}

#[test]
fn cancelled_read_only_preparation_resumes_its_verified_sleep() {
    cancelled_preparation_keeps_only_owned_sleep(false);
}

#[test]
fn an_external_source_change_during_cancelled_preparation_never_restarts_old_sleep() {
    cancelled_preparation_keeps_only_owned_sleep(true);
}
