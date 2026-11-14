use super::*;
use fsonos_api::ErrorCode;
use fsonos_api::dj::{DjEngine, SteerConstraints};
use fsonos_api::plan::DjAction;
use fsonos_core::HouseholdState;
use fsonos_core::clock::FakeClock;
use fsonos_core::store::{DjSession as StoredSession, MemStore};
use fsonos_proto::Transport;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use fsonos_spotify::cache::apply_library_read;
use fsonos_spotify::library::{LibraryItem, Origin};

struct House {
    sim: SimHandle,
    households: Vec<HouseholdState>,
    coordinator: PlayerId,
}

impl House {
    fn new() -> Self {
        let sim = SimHousehold::builder()
            .s2([SimPlayerSpec::new("Den", SimModel::One)])
            .spawn()
            .unwrap();
        let player = sim.player("Den").unwrap();
        let coordinator = PlayerId(player.uuid.clone());
        let mut household = HouseholdState::default();
        household.apply_topology(&get_zone_group_state(&sim.lan(), player.ip).unwrap());
        Self {
            sim,
            households: vec![household],
            coordinator,
        }
    }

    fn at<'a>(&'a self, transport: &'a dyn Transport) -> DjSpeakers<'a> {
        DjSpeakers {
            transport,
            households: &self.households,
            coordinator: &self.coordinator,
        }
    }
}

fn clock() -> FakeClock {
    FakeClock::new(chrono::DateTime::parse_from_rfc3339("2026-10-12T07:30:00Z").unwrap())
}

fn song(n: usize) -> LibraryItem {
    LibraryItem {
        source_uri: format!("spotify:track:{n:022}"),
        title: format!("Synthetic song {n}"),
        artists: vec![format!("Synthetic artist {n}")],
        artist_id: None,
        album: Some(format!("Synthetic album {n}")),
        album_uri: Some(format!("spotify:album:{n:022}")),
        album_artists: vec![format!("Synthetic artist {n}")],
        disc_number: Some(1),
        track_number: Some(1),
        added_at: None,
        genres: vec!["pop".into()],
        release_year: Some(2015),
        taste_pm: None,
        label: None,
        duration_secs: Some(200),
        explicit: false,
        origin: Origin::SavedAlbum,
    }
}

#[test]
fn a_pending_library_can_become_ready_without_publishing_a_plan_or_feed_early() {
    let house = House::new();
    let lan = house.sim.lan();
    let at = house.at(&lan);
    let dj = SpotifyDj::new(None);
    let mut store = MemStore::default();
    let clock = clock();
    let before = house.sim.soap_log().len();
    assert_eq!(
        dj.start_ready(at, &store, None, &clock).unwrap_err().code,
        ErrorCode::SpotifyAuthRequired
    );
    assert_eq!(house.sim.soap_log().len(), before);

    // An independent importer can make the next admission succeed; the
    // readiness check does not rely on a consumed in-memory sync notice.
    apply_library_read(&mut store, &[song(1), song(2)]).unwrap();
    dj.start_ready(at, &store, None, &clock).unwrap();
    {
        let state = dj.state();
        assert!(!state.planned);
        assert!(state.pool.is_empty());
        assert!(state.feeds.is_empty());
        assert!(state.refill_retries.is_empty());
    }
    assert!(store.dj_sessions().unwrap().is_empty());
    assert!(store.recent_plays(None, 100).unwrap().is_empty());
    assert!(
        house.sim.soap_log()[before..]
            .iter()
            .all(|call| call.action == "Browse" || call.action.starts_with("Get"))
    );

    dj.act(at, &mut store, DjAction::Start, &clock).unwrap();
    assert!(dj.feeds(&house.coordinator));
    let saved = dj.state().feeds[&house.coordinator].snapshot();
    dj.start_ready(at, &store, None, &clock).unwrap();
    assert_eq!(dj.state().feeds[&house.coordinator].snapshot(), saved);
}

#[test]
fn prospective_direction_matches_the_actual_steer_without_saving_it() {
    let house = House::new();
    let lan = house.sim.lan();
    let at = house.at(&lan);
    let clock = clock();
    let dj = SpotifyDj::new(None);
    let mut store = MemStore::default();
    apply_library_read(&mut store, &[song(1), song(2)]).unwrap();
    let plan = State::read_plan(&store, None, clock.now().timestamp()).unwrap();
    let directions = [
        None,
        Some(DjSteer::Clear),
        Some(DjSteer::Set {
            mood: Some("focus".into()),
            constraints: SteerConstraints::default(),
            for_secs: Some(600),
        }),
        Some(DjSteer::Set {
            mood: None,
            constraints: SteerConstraints {
                include_artists: vec!["Synthetic artist 1".into()],
                ..SteerConstraints::default()
            },
            for_secs: Some(1200),
        }),
        Some(DjSteer::Set {
            mood: Some("focus".into()),
            constraints: SteerConstraints::default(),
            for_secs: Some(0),
        }),
    ];
    for direction in directions {
        let saved = StoredSession {
            coordinator: house.coordinator.0.clone(),
            mood: Some("calm".into()),
            constraints: None,
            expires: i64::MAX,
        };
        store.save_dj_session(&saved).unwrap();
        let preview = context(
            &plan,
            &store,
            &house.coordinator,
            direction.as_ref(),
            &clock,
        )
        .unwrap();
        assert_eq!(store.dj_session(&house.coordinator.0).unwrap(), Some(saved));
        if let Some(direction) = &direction {
            dj.steer(at, &mut store, direction, &clock).unwrap();
        }
        let actual = plan.context(&store, &house.coordinator, &clock).unwrap();
        assert_eq!(preview.steer, actual.steer, "{direction:?}");
        assert_eq!(preview.now, actual.now);
        assert_eq!(preview.local_hour, actual.local_hour);
        if matches!(direction, Some(DjSteer::Clear)) {
            assert_eq!(preview.steer.unwrap().mood.as_deref(), Some("bright"));
        }
    }
}

#[test]
fn invalid_prospective_steering_leaves_the_previous_session_and_speakers_untouched() {
    let house = House::new();
    let lan = house.sim.lan();
    let dj = SpotifyDj::new(None);
    let mut store = MemStore::default();
    let clock = clock();
    apply_library_read(&mut store, &[song(1)]).unwrap();
    let saved = StoredSession {
        coordinator: house.coordinator.0.clone(),
        mood: Some("calm".into()),
        constraints: None,
        expires: i64::MAX,
    };
    store.save_dj_session(&saved).unwrap();
    let direction = DjSteer::Set {
        mood: Some("unknown-readiness-mood".into()),
        constraints: SteerConstraints::default(),
        for_secs: Some(600),
    };
    let before = house.sim.soap_log().len();
    let error = dj
        .start_ready(house.at(&lan), &store, Some(&direction), &clock)
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::UnknownMood);
    assert_eq!(store.dj_session(&house.coordinator.0).unwrap(), Some(saved));
    assert_eq!(house.sim.soap_log().len(), before);
    assert!(dj.state().feeds.is_empty());
}

#[test]
fn steering_readiness_validates_without_saving_and_clearing_needs_no_library() {
    let house = House::new();
    let lan = house.sim.lan();
    let dj = SpotifyDj::new(None);
    let mut store = MemStore::default();
    let clock = clock();
    let saved = StoredSession {
        coordinator: house.coordinator.0.clone(),
        mood: Some("calm".into()),
        constraints: None,
        expires: i64::MAX,
    };
    store.save_dj_session(&saved).unwrap();
    let before = house.sim.soap_log().len();
    dj.steer_ready(house.at(&lan), &store, &DjSteer::Clear, &clock).unwrap();
    let invalid = DjSteer::Set {
        mood: Some("unknown-batch-mood".into()),
        constraints: SteerConstraints::default(),
        for_secs: Some(600),
    };
    assert_eq!(
        dj.steer_ready(house.at(&lan), &store, &invalid, &clock).unwrap_err().code,
        ErrorCode::UnknownMood
    );
    assert_eq!(store.dj_session(&house.coordinator.0).unwrap(), Some(saved));
    assert_eq!(house.sim.soap_log().len(), before);
    let state = dj.state();
    assert!(!state.planned && state.feeds.is_empty());
}

#[test]
fn checked_mutations_run_the_gate_under_the_engine_lock_before_state_or_store_changes() {
    let house = House::new();
    let lan = house.sim.lan();
    let dj = SpotifyDj::new(None);
    let mut store = MemStore::default();
    let clock = clock();
    let saved = StoredSession {
        coordinator: house.coordinator.0.clone(),
        mood: Some("bright".into()),
        constraints: None,
        expires: i64::MAX,
    };
    store.save_dj_session(&saved).unwrap();
    let before = || {
        assert!(dj.state.try_lock().is_err(), "validation must follow the engine lock");
        Err(Failure::new(ErrorCode::NotCoordinator, "the selected audience changed"))
    };
    let direction = DjSteer::Set {
        mood: Some("calm".into()), constraints: SteerConstraints::default(), for_secs: Some(600),
    };
    let at = house.at(&lan);
    let error = dj.steer_checked(at, &mut store, &direction, &clock, &before).unwrap_err();
    assert_eq!(error.code, ErrorCode::NotCoordinator);
    let error = dj.act_as_checked(at, &mut store, DjAction::Start, &clock,
        &fsonos_core::policy::Client::Cli, &before).unwrap_err();
    assert_eq!(error.code, ErrorCode::NotCoordinator);
    assert_eq!(store.dj_session(&house.coordinator.0).unwrap(), Some(saved));
    let state = dj.state();
    assert!(!state.planned && state.feeds.is_empty() && state.starters.is_empty());
}

/// Whole works can be appended before activation. The next non-enqueue
/// mutation models an audience fence refusing a newly expanded group.
struct RefuseActivation<'a> {
    lan: &'a fsonos_sim::SimLan,
    guest: std::net::IpAddr,
    coordinator: &'a PlayerId,
    refused: std::cell::Cell<bool>,
}

impl Transport for RefuseActivation<'_> {
    fn soap_post(
        &self,
        host: std::net::IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, fsonos_proto::ProtoError> {
        let verb = action.trim_matches('"').rsplit('#').next().unwrap();
        if !verb.starts_with("Get") && !matches!(verb, "Browse" | "AddURIToQueue") {
            if !self.refused.replace(true) {
                fsonos_proto::control::join_group(self.lan, self.guest, self.coordinator).unwrap();
            }
            return Err(fsonos_proto::ProtoError::SoapFault {
                code: 403,
                reason: "the original DJ audience changed before activation".into(),
            });
        }
        self.lan.soap_post(host, path, action, body)
    }
}

#[test]
fn a_new_feed_refused_after_enqueue_cannot_continue_on_events_or_after_restart() {
    use fsonos_core::playback::{Playback, PlaybackObservation};
    use fsonos_proto::content::{QUEUE, browse_all};
    use fsonos_proto::control as soap;
    use std::time::Instant;

    let sim = SimHousehold::builder()
        .s2(["Den", "Guest"].map(|name| SimPlayerSpec::new(name, SimModel::One)))
        .spawn().unwrap();
    let lan = sim.lan();
    let host = sim.player("Den").unwrap().ip;
    let coordinator = PlayerId(sim.player("Den").unwrap().uuid.clone());
    let mut household = HouseholdState::default();
    household.apply_topology(&get_zone_group_state(&lan, host).unwrap());
    let households = vec![household];
    // An owner queue is already active; a failed DJ start must not acquire
    // a background claim merely because it appended some complete works.
    soap::add_uri_to_queue(&lan, host, "x-file-cifs://example.invalid/owner.flac", "", false).unwrap();
    soap::play_from_queue(&lan, host, &coordinator).unwrap();
    soap::play(&lan, host).unwrap();

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("dj-feeds.json");
    let dj = SpotifyDj::new(None).with_saved_feeds(Some(path.clone()));
    let mut store = MemStore::default();
    apply_library_read(&mut store, &[song(1), song(2), song(3), song(4)]).unwrap();
    let clock = clock();
    let fence = RefuseActivation {
        lan: &lan,
        guest: sim.player("Guest").unwrap().ip,
        coordinator: &coordinator,
        refused: std::cell::Cell::new(false),
    };
    let before = || {
        if fence.refused.get() {
            Err(Failure::new(ErrorCode::NotCoordinator, "the selected audience expanded"))
        } else {
            Ok(())
        }
    };
    let error = dj.act_as_checked(
        DjSpeakers { transport: &fence, households: &households, coordinator: &coordinator },
        &mut store, DjAction::Start, &clock, &fsonos_core::policy::Client::Cli, &before,
    ).unwrap_err();
    assert_eq!(error.code, ErrorCode::NotCoordinator);
    assert!(fence.refused.get(), "the start reached activation after enqueuing");
    let queue = browse_all(&lan, host, QUEUE).unwrap();
    assert!(queue.len() > 1, "accepted DJ work remains beside the owner's queue");
    assert!(!dj.feeds(&coordinator), "the partial start has no continuation authority");
    assert!(dj.state().starters.get(&coordinator).is_none());

    // A later real playback observation must not finish the refused start.
    let observation = PlaybackObservation {
        transport: soap::get_transport_info(&lan, host).unwrap(),
        position: soap::get_position_info(&lan, host).unwrap(),
        media: soap::get_media_info(&lan, host).unwrap(),
    };
    let mut playback = Playback::default();
    playback.apply_observation(&coordinator, &observation, Instant::now());
    let event = playback.of(&coordinator).unwrap().clone();
    let at = DjSpeakers { transport: &lan, households: &households, coordinator: &coordinator };
    let start = sim.soap_log().len();
    dj.on_playback(at, &mut store, &event, &clock);
    let restarted = SpotifyDj::new(None).with_saved_feeds(Some(path));
    assert!(!restarted.feeds(&coordinator), "restart cannot revive the refused new feed");
    restarted.on_playback(at, &mut store, &event, &clock);
    assert!(sim.soap_log().iter().skip(start).all(|call|
        call.action == "Browse" || call.action.starts_with("Get")));
    assert_eq!(browse_all(&lan, host, QUEUE).unwrap(), queue, "no compensating queue cleanup");
}
