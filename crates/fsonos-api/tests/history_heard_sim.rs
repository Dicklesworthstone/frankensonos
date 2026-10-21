//! What the house plays reaches the history even when fsonos didn't start
//! it: the daemon's live model records a coordinator's new track once,
//! with no DJ running. Music that moves to another room, and what a room
//! plays when the daemon starts, are the plays already recorded. Spotify
//! Connect's tracks are their `spotify:` URIs, and a play the library
//! doesn't know is named as its player named it. Against `fsonos-sim` over
//! real loopback sockets.

use chrono::{DateTime, FixedOffset, TimeDelta};
use fsonos_api::surface::{Surface, Survey, follow};
use fsonos_core::clock::{Clock, FakeClock, SystemClock};
use fsonos_core::control;
use fsonos_core::live::{Live, LiveConfig};
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::{MemStore, Store};
use fsonos_proto::didl::spotify_track_didl;
use fsonos_proto::net::Lan;
use fsonos_proto::soap::{self, AV_TRANSPORT};
use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use fsonos_types::PlayerId;
use std::sync::Arc;
use std::time::{Duration, Instant};

const EVENING: &str = "x-rincon-mp3radio://stream.example.invalid/evening.mp3";
const LATE: &str = "x-rincon-mp3radio://stream.example.invalid/late.mp3";

/// A household of `rooms` and the daemon's live model of it.
fn house(rooms: &[&str]) -> (SimHandle, Arc<Lan>, Arc<Live>) {
    let sim = SimHousehold::builder()
        .s1(rooms
            .iter()
            .map(|&room| SimPlayerSpec::new(room, SimModel::Play5Gen1)))
        .spawn()
        .unwrap();
    let lan = Arc::new(
        Lan::start()
            .unwrap()
            .with_routes(sim.players().iter().map(|p| (p.ip, p.addr)).collect())
            .with_ssdp_target(sim.ssdp_addr()),
    );
    let live = Arc::new(Live::start(Arc::clone(&lan), LiveConfig::new(Vec::new())));
    assert!(live.wait_ready(Duration::from_secs(10)));
    (sim, lan, live)
}

/// A clock the test moves on, shared with the daemon.
struct Shared(Arc<FakeClock>);

impl Clock for Shared {
    fn now(&self) -> DateTime<FixedOffset> {
        self.0.now()
    }
}

/// A daemon's surface on `live` keeping `store`, telling the time by
/// `clock`, following the live model.
fn daemon(
    sim: &SimHandle,
    live: &Arc<Live>,
    store: MemStore,
    clock: Box<dyn Clock>,
) -> Arc<Surface> {
    let survey: Survey = Box::new(|_| Ok(Vec::new()));
    let surface = Arc::new(
        Surface::new(Box::new(sim.lan()), survey, Policy::default(), clock)
            .with_live(live)
            .with_action_log(Box::new(store), "cli"),
    );
    follow(&surface);
    surface
}

fn id(live: &Live, room: &str) -> PlayerId {
    live.households()
        .iter()
        .flat_map(|h| &h.players)
        .find(|p| p.room_name == room)
        .unwrap()
        .id
        .clone()
}

/// Wait up to 10 s for `done`.
fn eventually(mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    done()
}

/// Whether the live model hears `player` play `uri`.
fn plays_now(live: &Live, player: &PlayerId, uri: &str) -> bool {
    live.player(player)
        .is_some_and(|p| p.track_uri.as_deref() == Some(uri))
}

#[test]
fn a_play_started_outside_fsonos_is_in_the_history() {
    let (sim, lan, live) = house(&["Kitchen"]);
    let surface = daemon(&sim, &live, MemStore::default(), Box::new(SystemClock));
    let kitchen = id(&live, "Kitchen");
    let plays = || surface.recent_plays(&Client::Cli, None, 10).unwrap();
    let heard = |n: usize| {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && plays().len() < n {
            std::thread::sleep(Duration::from_millis(100));
        }
        plays()
    };

    // Started on the speaker itself, as the Sonos app would.
    control::play_uri(&*lan, &live.households(), &kitchen, EVENING, "").unwrap();
    let after_one = heard(1);
    assert_eq!(after_one.len(), 1, "{after_one:?}");
    assert_eq!(after_one[0].source_uri, EVENING);
    assert_eq!(after_one[0].room.as_deref(), Some("Kitchen"));

    // A pause and resume is the same play.
    control::pause(&*lan, &live.households(), &kitchen).unwrap();
    control::resume(&*lan, &live.households(), &kitchen).unwrap();
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(plays().len(), 1, "{:?}", plays());

    control::play_uri(&*lan, &live.households(), &kitchen, LATE, "").unwrap();
    let after_two = heard(2);
    assert_eq!(after_two.len(), 2, "{after_two:?}");
    assert_eq!(after_two[0].source_uri, LATE, "newest first");
    sim.shutdown();
}

/// Spotify Connect, as the owner's players report it: each track as
/// `x-sonos-spotify:spotify:track:<id>?…` with its colons unescaped, and
/// between tracks the Connect session (`x-sonos-vli:<player>:2,spotify:…`),
/// which is no track. The history has each track once, as its `spotify:`
/// URI (what the DJ and the library know it by), and no session.
#[test]
fn spotify_connect_plays_are_its_tracks() {
    let (sim, lan, live) = house(&["Kitchen"]);
    let surface = daemon(&sim, &live, MemStore::default(), Box::new(SystemClock));
    let kitchen = id(&live, "Kitchen");
    let plays = || surface.recent_plays(&Client::Cli, None, 10).unwrap();
    let params = sim.render_params(1).unwrap();
    let play = |uri: &str, metadata: &str| {
        control::play_uri(&*lan, &live.households(), &kitchen, uri, metadata).unwrap();
        assert!(eventually(|| plays_now(&live, &kitchen, uri)), "{uri}");
    };
    let session = format!("x-sonos-vli:{}:2,spotify:0123abcd", kitchen.0);
    let track = |id: &str| {
        let uri = format!(
            "x-sonos-spotify:spotify:track:{id}?sid={}&flags=0&sn={}",
            params.sid, params.sn
        );
        play(
            &uri,
            &spotify_track_didl(&format!("spotify:track:{id}"), id, &params),
        );
    };

    play(&session, "");
    std::thread::sleep(Duration::from_secs(1));
    assert!(plays().is_empty(), "{:?}", plays());
    track("connectA");
    assert!(eventually(|| !plays().is_empty()), "{:?}", plays());
    play(&session, "");
    track("connectB");
    assert!(eventually(|| plays().len() >= 2), "{:?}", plays());
    std::thread::sleep(Duration::from_secs(1));
    let uris: Vec<String> = plays().into_iter().map(|p| p.source_uri).collect();
    assert_eq!(
        uris,
        ["spotify:track:connectB", "spotify:track:connectA"],
        "newest first"
    );
    sim.shutdown();
}

/// A play the library doesn't know (a stream, here) is named in the history
/// as its player named it.
#[test]
fn a_heard_play_is_named_as_its_player_named_it() {
    let (sim, lan, live) = house(&["Kitchen"]);
    let surface = daemon(&sim, &live, MemStore::default(), Box::new(SystemClock));
    let kitchen = id(&live, "Kitchen");
    let plays = || surface.recent_plays(&Client::Cli, None, 10).unwrap();
    let metadata = "<DIDL-Lite xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
         xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\" \
         xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\">\
         <item id=\"-1\" parentID=\"-1\" restricted=\"true\">\
         <dc:title>Evening Radio</dc:title><dc:creator>Example FM</dc:creator>\
         <upnp:class>object.item.audioItem.audioBroadcast</upnp:class></item></DIDL-Lite>";
    control::play_uri(&*lan, &live.households(), &kitchen, EVENING, metadata).unwrap();
    assert!(eventually(|| !plays().is_empty()), "{:?}", plays());
    let heard = &plays()[0];
    assert_eq!(heard.source_uri, EVENING);
    assert_eq!(
        (heard.title.as_deref(), heard.artist.as_deref()),
        (Some("Evening Radio"), Some("Example FM")),
        "{heard:?}"
    );
    sim.shutdown();
}

#[test]
fn music_that_moves_to_another_room_is_the_same_play() {
    let (sim, lan, live) = house(&["Kitchen", "Office"]);
    let (kitchen, office) = (id(&live, "Kitchen"), id(&live, "Office"));
    control::join(&*lan, &live.households(), &office, &kitchen).unwrap();
    assert!(eventually(|| live
        .households()
        .iter()
        .any(|h| h.coordinator_of(&office) == Some(&kitchen))));
    let clock = Arc::new(FakeClock::new(SystemClock.now()));
    let surface = daemon(
        &sim,
        &live,
        MemStore::default(),
        Box::new(Shared(Arc::clone(&clock))),
    );
    let plays = || surface.recent_plays(&Client::Cli, None, 10).unwrap();
    control::play_uri(&*lan, &live.households(), &kitchen, EVENING, "").unwrap();
    assert!(eventually(|| plays().len() == 1), "{:?}", plays());

    // Ten minutes on, the Sonos app takes the Kitchen out of its group: the
    // music plays on in Office, the same play.
    clock.advance(TimeDelta::minutes(10));
    let host = sim.player("Kitchen").unwrap().ip;
    soap::call(
        &*lan,
        host,
        &AV_TRANSPORT,
        "DelegateGroupCoordinationTo",
        &soap::args_xml(&[
            ("InstanceID", "0"),
            ("NewCoordinator", &office.0),
            ("RejoinGroup", "0"),
        ]),
    )
    .unwrap();
    assert!(eventually(|| plays_now(&live, &office, EVENING)));
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(plays().len(), 1, "{:?}", plays());
    sim.shutdown();
}

#[test]
fn what_a_room_plays_when_the_daemon_starts_is_the_play_it_left() {
    let (sim, lan, live) = house(&["Kitchen"]);
    let kitchen = id(&live, "Kitchen");
    control::play_uri(&*lan, &live.households(), &kitchen, EVENING, "").unwrap();
    assert!(eventually(|| plays_now(&live, &kitchen, EVENING)));
    // The daemon before this one recorded it an hour ago.
    let mut store = MemStore::default();
    let hour_ago = SystemClock.now().timestamp() - 3600;
    store.record_play(&kitchen.0, EVENING, hour_ago).unwrap();
    let surface = daemon(&sim, &live, store, Box::new(SystemClock));
    let plays = || surface.recent_plays(&Client::Cli, None, 10).unwrap();

    // The first it hears of the Kitchen: still that play.
    control::pause(&*lan, &live.households(), &kitchen).unwrap();
    control::resume(&*lan, &live.households(), &kitchen).unwrap();
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(plays().len(), 1, "{:?}", plays());

    // Stopped, and played again: a new play.
    control::stop(&*lan, &live.households(), &kitchen).unwrap();
    std::thread::sleep(Duration::from_millis(500));
    control::play_uri(&*lan, &live.households(), &kitchen, EVENING, "").unwrap();
    assert!(eventually(|| plays().len() == 2), "{:?}", plays());
    sim.shutdown();
}
