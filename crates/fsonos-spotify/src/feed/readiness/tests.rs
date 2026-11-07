use super::*;
use crate::dj::{DjConfig, WorkPool};
use crate::feed::Movement;
use crate::test_shelf::{MIDNIGHT, song_items, works_of};
use fsonos_core::HouseholdState;
use fsonos_core::store::MemStore;
use fsonos_proto::ProtoError;
use fsonos_proto::control;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::PlayerId;
use std::net::IpAddr;

struct House {
    sim: SimHandle,
    households: Vec<HouseholdState>,
    coordinator: PlayerId,
}

impl House {
    fn new(s2: bool) -> Self {
        let builder = SimHousehold::builder();
        let sim = if s2 {
            builder.s2([SimPlayerSpec::new("Den", SimModel::One)])
        } else {
            builder.s1([SimPlayerSpec::new("Den", SimModel::Play5Gen1)])
        }
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

    fn speakers<'a, T: Transport + ?Sized>(&'a self, transport: &'a T) -> Speakers<'a, T> {
        Speakers {
            transport,
            households: &self.households,
            coordinator: &self.coordinator,
        }
    }

    fn cache_key(&self) -> &str {
        self.households[0]
            .id
            .as_ref()
            .map(|id| id.0.as_str())
            .filter(|id| !id.is_empty())
            .unwrap_or(&self.coordinator.0)
    }
}

fn planning(pool: &WorkPool) -> Planning<'_> {
    Planning {
        pool,
        steer: None,
        feedback: None,
        now: MIDNIGHT,
        local_hour: Some(12),
        queued_elsewhere: &[],
    }
}

struct Favorites {
    lan: SimLan,
    fail: bool,
}

impl Transport for Favorites {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if action.contains("#Browse") && body.contains("FV:2") {
            if self.fail {
                return Err(ProtoError::Network {
                    target: "synthetic-readiness-player".into(),
                    detail: "favorite read unavailable".into(),
                });
            }
            return Ok(concat!(
                r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body>"#,
                r#"<u:BrowseResponse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1">"#,
                r#"<Result>&lt;DIDL-Lite xmlns=&quot;urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/&quot;&gt;&lt;/DIDL-Lite&gt;</Result>"#,
                "<NumberReturned>0</NumberReturned><TotalMatches>0</TotalMatches><UpdateID>1</UpdateID>",
                "</u:BrowseResponse></s:Body></s:Envelope>"
            ).into());
        }
        self.lan.soap_post(host, path, action, body)
    }
}

#[test]
fn readiness_waits_for_a_library_and_never_advances_or_publishes_a_fresh_feed() {
    for s2 in [false, true] {
        let house = House::new(s2);
        let lan = house.sim.lan();
        let store = MemStore::default();
        let feed = QueueFeed::new(&house.coordinator, DjConfig::default(), 17);
        let before = format!("{feed:?}");
        let calls = house.sim.soap_log().len();
        assert!(matches!(
            feed.start_ready(
                &house.speakers(&lan),
                planning(&WorkPool::default()),
                &store
            ),
            Err(FeedError::EmptyPool)
        ));
        assert_eq!(house.sim.soap_log().len(), calls);

        let pool = works_of(&song_items());
        for _ in 0..2 {
            feed.start_ready(&house.speakers(&lan), planning(&pool), &store)
                .unwrap();
        }
        assert_eq!(format!("{feed:?}"), before, "RNG and feed remain untouched");
        assert_eq!(store.render_params(house.cache_key()).unwrap(), None);
        assert!(store.recent_plays(None, 100).unwrap().is_empty());
        assert!(
            house.sim.soap_log()[calls..]
                .iter()
                .all(|call| call.action == "Browse" || call.action.starts_with("Get"))
        );
    }
}

#[test]
fn renderer_readiness_waits_for_linkage_and_preserves_cached_observation_times() {
    let house = House::new(true);
    let lan = house.sim.lan();
    let no_favorites = Favorites {
        lan: lan.clone(),
        fail: false,
    };
    let fault = Favorites {
        lan: lan.clone(),
        fail: true,
    };
    let pool = works_of(&song_items());
    let mut store = MemStore::default();
    let feed = QueueFeed::new(&house.coordinator, DjConfig::default(), 19);
    assert!(matches!(
        feed.start_ready(&house.speakers(&no_favorites), planning(&pool), &store),
        Err(FeedError::NoRenderParams)
    ));
    feed.start_ready(&house.speakers(&lan), planning(&pool), &store)
        .unwrap();
    assert_eq!(store.render_params(house.cache_key()).unwrap(), None);

    let params = fsonos_core::control::spotify_params(&lan, &house.households, &house.coordinator)
        .unwrap()
        .unwrap();
    store
        .save_render_params(house.cache_key(), &params, 42)
        .unwrap();
    let cached = store.render_params(house.cache_key()).unwrap();
    feed.start_ready(&house.speakers(&no_favorites), planning(&pool), &store)
        .unwrap();
    assert_eq!(store.render_params(house.cache_key()).unwrap(), cached);
    assert!(
        feed.start_ready(&house.speakers(&fault), planning(&pool), &store)
            .is_err()
    );
    assert_eq!(store.render_params(house.cache_key()).unwrap(), cached);
    assert!(!feed.is_active());
}

#[test]
fn a_populated_library_still_requires_music_the_preferences_allow() {
    let house = House::new(true);
    let lan = house.sim.lan();
    let pool = works_of(
        &song_items()
            .into_iter()
            .filter(|item| item.explicit)
            .collect::<Vec<_>>(),
    );
    assert!(!pool.is_empty());
    let feed = QueueFeed::new(&house.coordinator, DjConfig::default(), 23);
    let calls = house.sim.soap_log().len();
    assert!(matches!(
        feed.start_ready(&house.speakers(&lan), planning(&pool), &MemStore::default()),
        Err(FeedError::NothingAllowed(_))
    ));
    assert!(
        house.sim.soap_log()[calls..]
            .iter()
            .all(|call| call.action == "Browse" || call.action.starts_with("Get"))
    );
}

#[test]
fn a_retained_complete_set_can_resume_but_an_owner_cleared_queue_needs_new_music() {
    let house = House::new(true);
    let lan = house.sim.lan();
    let pool = works_of(&song_items());
    let empty = WorkPool::default();
    let mut store = MemStore::default();
    let mut feed = QueueFeed::new(&house.coordinator, DjConfig::default(), 29);
    feed.start(&house.speakers(&lan), planning(&pool), &mut store)
        .unwrap();
    let before = format!("{feed:?}");
    let no_favorites = Favorites {
        lan: lan.clone(),
        fail: true,
    };
    feed.start_ready(&house.speakers(&no_favorites), planning(&empty), &store)
        .unwrap();
    assert_eq!(format!("{feed:?}"), before);

    control::remove_all_tracks_from_queue(&lan, house.sim.player("Den").unwrap().ip).unwrap();
    let calls = house.sim.soap_log().len();
    assert!(matches!(
        feed.start_ready(&house.speakers(&lan), planning(&empty), &store),
        Err(FeedError::EmptyPool)
    ));
    assert_eq!(
        format!("{feed:?}"),
        before,
        "the preflight cannot relinquish the live feed"
    );
    assert!(
        house.sim.soap_log()[calls..]
            .iter()
            .all(|call| call.action == "Browse" || call.action.starts_with("Get"))
    );
}

#[test]
fn a_partial_work_already_played_past_cannot_count_toward_start_readiness() {
    let house = House::new(true);
    let lan = house.sim.lan();
    let pool = works_of(&song_items());
    let mut store = MemStore::default();
    let mut feed = QueueFeed::new(&house.coordinator, DjConfig::default(), 31);
    feed.start(&house.speakers(&lan), planning(&pool), &mut store)
        .unwrap();
    assert_eq!(feed.queued.len(), 2);
    feed.queued[0].movements.push(Movement {
        uri: "spotify:track:0000000000000000000999".into(),
        title: "Unadded movement".into(),
        position: None,
        added: false,
    });
    feed.playing = feed.queued[1].first();
    let before = format!("{feed:?}");
    let calls = house.sim.soap_log().len();
    let empty = WorkPool::default();
    assert!(matches!(
        feed.start_ready(&house.speakers(&lan), planning(&empty), &store),
        Err(FeedError::EmptyPool)
    ));
    assert_eq!(format!("{feed:?}"), before);
    assert!(
        house.sim.soap_log()[calls..]
            .iter()
            .all(|call| call.action == "Browse" || call.action.starts_with("Get"))
    );
    assert!(matches!(
        feed.start(&house.speakers(&lan), planning(&empty), &mut store),
        Err(FeedError::EmptyPool)
    ));
}

struct Unavailable {
    lan: SimLan,
    prerequisite: &'static str,
}

impl Transport for Unavailable {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let blocked = if self.prerequisite == "queue" {
            action.contains("#Browse") && body.contains("Q:0")
        } else {
            action.contains(&format!("#{}", self.prerequisite))
        };
        if blocked {
            return Err(ProtoError::Network {
                target: "synthetic-dj-admission".into(),
                detail: "enqueue prerequisite is still unavailable".into(),
            });
        }
        self.lan.soap_post(host, path, action, body)
    }
}

#[test]
fn a_current_coordinator_waits_for_owner_source_and_queue_reads_before_admission() {
    for s2 in [false, true] {
        let house = House::new(s2);
        let pool = works_of(&song_items());
        let store = MemStore::default();
        let feed = QueueFeed::new(&house.coordinator, DjConfig::default(), 37);
        let before = format!("{feed:?}");
        for prerequisite in ["GetZoneGroupState", "GetMediaInfo", "queue"] {
            let unavailable = Unavailable {
                lan: house.sim.lan(),
                prerequisite,
            };
            let calls = house.sim.soap_log().len();
            let error = feed
                .start_ready(&house.speakers(&unavailable), planning(&pool), &store)
                .unwrap_err();
            assert!(
                error.to_string().contains("enqueue prerequisite"),
                "{prerequisite}: {error}"
            );
            assert_eq!(format!("{feed:?}"), before);
            assert!(
                house.sim.soap_log()[calls..]
                    .iter()
                    .all(|call| call.action == "Browse" || call.action.starts_with("Get"))
            );
        }
    }
}

#[test]
fn a_scene_can_admit_its_future_coordinator_before_regrouping() {
    let sim = SimHousehold::builder()
        .s2([
            SimPlayerSpec::new("Den", SimModel::One),
            SimPlayerSpec::new("Office", SimModel::One),
        ])
        .spawn()
        .unwrap();
    let lan = sim.lan();
    let leader = PlayerId(sim.player("Den").unwrap().uuid.clone());
    let member = PlayerId(sim.player("Office").unwrap().uuid.clone());
    let leader_ip = sim.player("Den").unwrap().ip;
    let mut household = HouseholdState::default();
    household.apply_topology(&get_zone_group_state(&lan, leader_ip).unwrap());
    fsonos_core::control::join(&lan, &[household.clone()], &member, &leader).unwrap();
    household.apply_topology(&get_zone_group_state(&lan, leader_ip).unwrap());
    assert!(
        !household
            .groups
            .iter()
            .any(|group| group.coordinator == member)
    );

    let house = House {
        sim,
        households: vec![household],
        coordinator: member,
    };
    let feed = QueueFeed::new(&house.coordinator, DjConfig::default(), 41);
    let pool = works_of(&song_items());
    let calls = house.sim.soap_log().len();
    feed.start_ready(&house.speakers(&lan), planning(&pool), &MemStore::default())
        .unwrap();
    assert!(!feed.is_active());
    assert!(
        house.sim.soap_log()[calls..]
            .iter()
            .all(|call| call.action == "Browse")
    );
}
