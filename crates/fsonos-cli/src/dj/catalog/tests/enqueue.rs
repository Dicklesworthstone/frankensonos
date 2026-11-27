//! Catalog publication during speaker I/O cannot admit an obsolete selection.
//! The real MemStore contract and S1/S2 simulator sockets exercise the final
//! admission boundary, including confirmed prefixes and renderer retries.

use super::{ObservedStore, song};
use crate::dj::SpotifyDj;
use fsonos_api::ErrorCode;
use fsonos_api::dj::{DjEngine, DjSpeakers};
use fsonos_api::plan::DjAction;
use fsonos_core::HouseholdState;
use fsonos_core::clock::{Clock, FakeClock};
use fsonos_core::store::{MemStore, SpotifyArtistDiscoveryFence, Store};
use fsonos_proto::content::{QUEUE, browse_snapshot};
use fsonos_proto::control;
use fsonos_proto::didl::spotify_uri_from_renderer_uri;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_spotify::cache::{apply_library_read, works_from_store};
use fsonos_spotify::dj::{DjConfig, WorkPool};
use fsonos_spotify::feed::{CatalogReceipt, FeedError, Planning, QueueFeed, Speakers};
use fsonos_spotify::library::LibraryItem;
use fsonos_types::PlayerId;
use std::cell::{Cell, RefCell};
use std::net::IpAddr;
use std::rc::Rc;

struct House {
    sim: SimHandle,
    households: Vec<HouseholdState>,
    coordinator: PlayerId,
}

impl House {
    fn new(s1: bool) -> Self {
        let builder = SimHousehold::builder();
        let sim = if s1 {
            builder.s1([SimPlayerSpec::new("Den", SimModel::Play5Gen1)])
        } else {
            builder.s2([SimPlayerSpec::new("Den", SimModel::One)])
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

    fn at<'a>(&'a self, transport: &'a dyn Transport) -> DjSpeakers<'a> {
        DjSpeakers {
            transport,
            households: &self.households,
            coordinator: &self.coordinator,
        }
    }

    fn speakers<'a>(&'a self, transport: &'a dyn Transport) -> Speakers<'a, dyn Transport + 'a> {
        Speakers {
            transport,
            households: &self.households,
            coordinator: &self.coordinator,
        }
    }

    fn host(&self) -> IpAddr {
        self.sim.player("Den").unwrap().ip
    }

    fn queue(&self) -> Vec<String> {
        browse_snapshot(&self.sim.lan(), self.host(), QUEUE)
            .unwrap()
            .objects
            .into_iter()
            .map(|object| {
                let uri = object.res.unwrap().uri;
                spotify_uri_from_renderer_uri(&uri).unwrap_or(uri)
            })
            .collect()
    }
}

fn clock() -> FakeClock {
    FakeClock::new(chrono::DateTime::parse_from_rfc3339("2026-10-22T12:00:00Z").unwrap())
}

fn store(items: &[LibraryItem]) -> ObservedStore {
    let mut store = ObservedStore {
        revision_supported: Cell::new(true),
        ..ObservedStore::default()
    };
    store.adopt_spotify_library_owner("original-owner").unwrap();
    apply_library_read(&mut store, items).unwrap();
    store
}

fn planning<'a>(pool: &'a WorkPool, receipt: &'a CatalogReceipt) -> Planning<'a> {
    Planning {
        pool,
        catalog: Some(receipt),
        steer: None,
        feedback: None,
        now: clock().now().timestamp(),
        local_hour: Some(12),
        queued_elsewhere: &[],
    }
}

#[derive(Clone, Copy, Debug)]
enum Change {
    Publication,
    Adoption,
    Denial,
    Unreadable,
    Listening,
}

struct DuringPreflight {
    lan: SimLan,
    store: Rc<RefCell<MemStore>>,
    fail_receipt: Rc<Cell<bool>>,
    change: Cell<Option<Change>>,
    after_adds: usize,
    adds: Cell<usize>,
    favorites: Cell<usize>,
    retry: bool,
}

impl DuringPreflight {
    fn new(house: &House, store: &ObservedStore, change: Change, after_adds: usize) -> Self {
        Self {
            lan: house.sim.lan(),
            store: Rc::clone(&store.inner),
            fail_receipt: Rc::clone(&store.fail_receipt),
            change: Cell::new(Some(change)),
            after_adds,
            adds: Cell::new(0),
            favorites: Cell::new(0),
            retry: false,
        }
    }

    fn publish(&self) {
        let Some(change) = self.change.take() else {
            return;
        };
        let mut store = self.store.borrow_mut();
        match change {
            Change::Publication => {
                apply_library_read(&mut *store, &(90..94).map(song).collect::<Vec<_>>()).unwrap();
            }
            Change::Adoption => {
                store
                    .adopt_spotify_library_owner("replacement-owner")
                    .unwrap();
            }
            Change::Denial => {
                let owner = store.spotify_library_owner().unwrap().unwrap();
                let fence = SpotifyArtistDiscoveryFence {
                    expected: store.spotify_artist_discovery(&owner).unwrap(),
                    progress: "new unavailable-track observation".into(),
                };
                let unavailable = store
                    .library()
                    .unwrap()
                    .into_iter()
                    .map(|entry| entry.track.source_uri)
                    .collect::<Vec<_>>();
                assert!(
                    store
                        .publish_spotify_artist_continuation(&owner, &fence, &[], &unavailable)
                        .unwrap()
                );
            }
            Change::Unreadable => self.fail_receipt.set(true),
            Change::Listening => {
                store
                    .record_play("Other room", &song(500).source_uri, 1)
                    .unwrap();
            }
        }
    }
}

impl Transport for DuringPreflight {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if action.contains("#Browse") && body.contains("FV:2") {
            self.favorites.set(self.favorites.get() + 1);
            if self.retry && self.favorites.get() == 2 {
                self.publish();
            }
        }
        if !self.retry && action.contains("#GetMediaInfo") && self.adds.get() >= self.after_adds {
            self.publish();
        }
        if action.contains("#AddURIToQueue") {
            self.adds.set(self.adds.get() + 1);
            if self.retry && self.adds.get() == 1 {
                return Err(ProtoError::SoapFault {
                    code: 800,
                    reason: "injected definite renderer refusal".into(),
                });
            }
        }
        self.lan.soap_post(host, path, action, body)
    }

    fn http_get(&self, url: &str) -> Result<String, ProtoError> {
        self.lan.http_get(url)
    }
}

#[test]
fn changed_or_unreadable_catalog_during_preflight_sends_no_add_and_can_retry() {
    for s1 in [true, false] {
        for change in [
            Change::Publication,
            Change::Adoption,
            Change::Denial,
            Change::Unreadable,
        ] {
            let house = House::new(s1);
            let lan = house.sim.lan();
            let owner = "http://example.invalid/owner.mp3";
            control::add_uri_to_queue(&lan, house.host(), owner, "", false).unwrap();
            let media = control::get_media_info(&lan, house.host()).unwrap();
            let mut store = store(&(1..5).map(song).collect::<Vec<_>>());
            let dj = SpotifyDj::new(None);
            let transport = DuringPreflight::new(&house, &store, change, 0);
            let marker = house.sim.soap_log().len();
            let error = dj
                .act(house.at(&transport), &mut store, DjAction::Start, &clock())
                .unwrap_err();
            assert_eq!(
                error.code,
                if matches!(change, Change::Unreadable) {
                    ErrorCode::Internal
                } else {
                    ErrorCode::NotReady
                },
                "{change:?}: {error:?}"
            );
            assert!(transport.change.get().is_none(), "the preflight race ran");
            assert_eq!(transport.adds.get(), 0, "{change:?}");
            assert_eq!(house.queue(), [owner]);
            assert_eq!(
                control::get_media_info(&lan, house.host()).unwrap().uri,
                media.uri
            );
            assert!(!dj.feeds(&house.coordinator));
            assert!(house.sim.soap_log()[marker..].iter().all(|call| {
                !matches!(
                    call.action.as_str(),
                    "AddURIToQueue"
                        | "RemoveTrackFromQueue"
                        | "RemoveAllTracksFromQueue"
                        | "SetAVTransportURI"
                        | "Seek"
                        | "SetPlayMode"
                        | "Play"
                        | "Pause"
                        | "Stop"
                )
            }));

            store.fail_receipt.set(false);
            let fresh = (90..94).map(song).collect::<Vec<_>>();
            apply_library_read(&mut store, &fresh).unwrap();
            dj.act(house.at(&lan), &mut store, DjAction::Start, &clock())
                .unwrap();
            let queued = house.queue();
            assert_eq!(queued[0], owner);
            assert!(queued.len() > 1);
            assert!(
                queued[1..]
                    .iter()
                    .all(|uri| fresh.iter().any(|item| &item.source_uri == uri))
            );
        }
    }
}

#[test]
fn a_read_only_readiness_check_rejects_a_catalog_changed_during_speaker_preflight() {
    let house = House::new(false);
    let store = store(&(1..5).map(song).collect::<Vec<_>>());
    let transport = DuringPreflight::new(&house, &store, Change::Publication, 0);
    let dj = SpotifyDj::new(None);
    let error = dj
        .start_ready(house.at(&transport), &store, None, &clock())
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::NotReady);
    assert!(transport.change.get().is_none());
    assert_eq!(transport.adds.get(), 0);
    assert!(house.queue().is_empty());
    assert!(dj.state().feeds.is_empty());
}

#[test]
fn a_current_catalog_allows_append_without_reading_the_full_library_per_movement() {
    let house = House::new(false);
    let mut store = store(&(1..7).map(song).collect::<Vec<_>>());
    let dj = SpotifyDj::new(None);
    dj.state()
        .reload(&store, None, clock().now().timestamp())
        .unwrap();
    let reads = store.library_reads.get();
    let receipt = CatalogReceipt::read(&store).unwrap();
    let transport = DuringPreflight::new(&house, &store, Change::Listening, 0);
    dj.act(house.at(&transport), &mut store, DjAction::Start, &clock())
        .unwrap();
    assert!(transport.change.get().is_none());
    assert!(transport.adds.get() >= 2);
    assert_eq!(house.queue().len(), transport.adds.get());
    assert_eq!(CatalogReceipt::read(&store).unwrap(), receipt);
    assert_eq!(store.library_reads.get(), reads);
}

#[test]
fn catalog_change_before_renderer_retry_is_not_an_uncertain_add_or_credentials_failure() {
    let house = House::new(false);
    let mut store = store(&(1..5).map(song).collect::<Vec<_>>());
    let mut transport = DuringPreflight::new(&house, &store, Change::Publication, 0);
    transport.retry = true;
    let dj = SpotifyDj::new(None);
    let error = dj
        .act(house.at(&transport), &mut store, DjAction::Start, &clock())
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::NotReady);
    assert!(
        transport.change.get().is_none(),
        "the recovery favorites read ran"
    );
    assert_eq!(
        transport.adds.get(),
        1,
        "the refused movement was not retried"
    );
    assert!(house.queue().is_empty());
    assert!(!dj.feeds(&house.coordinator));
}

#[test]
fn a_catalog_change_after_one_movement_keeps_the_prefix_without_replaying_a_retired_suffix() {
    for (s1, playing) in [(true, false), (true, true), (false, false), (false, true)] {
        let house = House::new(s1);
        let items = ["I. Allegro", "II. Adagio", "III. Presto"]
            .into_iter()
            .enumerate()
            .map(|(index, movement)| {
                let mut item = song(index + 1);
                item.title = format!("Symphony No. 1 in C Major, Op. 1: {movement}");
                item.artists = vec!["Ludwig van Beethoven".into(), "Test Ensemble".into()];
                item.genres = vec!["classical".into()];
                item.album = Some("Beethoven: Symphony No. 1".into());
                item.album_uri = Some("spotify:album:0000000000000000000001".into());
                item.album_artists = vec!["Test Ensemble".into()];
                item
            })
            .collect::<Vec<_>>();
        let mut store = store(&items);
        let original_pool = works_from_store(&store).unwrap();
        assert_eq!(original_pool.len(), 1);
        assert_eq!(original_pool.works()[0].movements.len(), 3);
        let receipt = CatalogReceipt::read(&store).unwrap();
        let plan = planning(&original_pool, &receipt);
        let transport = DuringPreflight::new(&house, &store, Change::Publication, 1);
        let mut feed = QueueFeed::new(&house.coordinator, DjConfig::default(), 1);
        assert!(matches!(
            feed.start(&house.speakers(&transport), plan, &mut store),
            Err(FeedError::CatalogChanged)
        ));
        assert_eq!(transport.adds.get(), 1);
        let prefix = house.queue();
        assert_eq!(prefix, [items[0].source_uri.clone()]);
        let snapshot = feed.snapshot().unwrap();
        assert_eq!(
            snapshot.queued[0]
                .movements
                .iter()
                .filter(|m| m.added)
                .count(),
            1
        );

        // Neither an explicit retry with the old plan nor a restart with a
        // newly rebuilt plan can lend the old suffix fresh catalog authority.
        let lan = house.sim.lan();
        if playing {
            fsonos_core::control::play_queue_from_in_order(
                &lan,
                &house.households,
                &house.coordinator,
                1,
            )
            .unwrap();
            control::seek_position(&lan, house.host(), 37).unwrap();
        }
        let before = control::get_position_info(&lan, house.host()).unwrap();
        assert!(matches!(
            feed.start(&house.speakers(&lan), plan, &mut store),
            Err(FeedError::CatalogChanged)
        ));
        assert_eq!(house.queue(), prefix);
        let mut restored = QueueFeed::restore(snapshot, DjConfig::default(), 1);
        let pool = works_from_store(&store).unwrap();
        let receipt = CatalogReceipt::read(&store).unwrap();
        restored
            .start(&house.speakers(&lan), planning(&pool, &receipt), &mut store)
            .unwrap();
        let queued = house.queue();
        assert_eq!(queued[0], prefix[0]);
        assert!(queued.len() > 1);
        assert!(queued[1..].iter().all(|uri| pool.work_of(uri).is_some()));
        assert!(
            items[1..]
                .iter()
                .all(|item| !queued.contains(&item.source_uri))
        );
        let retained = restored.snapshot().unwrap();
        assert_eq!(retained.queued[0].movements.len(), 1);
        assert_eq!(retained.queued[0].movements[0].uri, prefix[0]);
        assert_eq!(retained.queued[0].movements[0].position, Some(1));
        let shown = crate::dj_view::status(
            "Den".into(),
            Some(&restored),
            &original_pool,
            Some(1),
            crate::dj_view::program_now(&fsonos_spotify::steer::Moods::builtin(), &clock()),
            None,
        )
        .now
        .unwrap();
        assert_eq!(shown.movements, 1);
        assert_eq!(shown.minutes, Some(3), "only the retained portion is shown");
        assert!(store.recent_plays(None, 10).unwrap().is_empty());
        if playing {
            let after = control::get_position_info(&lan, house.host()).unwrap();
            assert_eq!(after.track, before.track);
            assert_eq!(after.position_secs, before.position_secs);
        }
    }
}
