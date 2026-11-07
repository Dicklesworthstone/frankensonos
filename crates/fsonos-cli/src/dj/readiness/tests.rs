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
