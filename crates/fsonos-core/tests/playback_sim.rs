//! Live playback state follows a simulated player through real GENA: a real
//! `net::Lan` subscribes to the virtual player's AVTransport and
//! RenderingControl, a real `EventSink` receives the NOTIFYs, and
//! `playback::Playback` folds them while `control` and `favorites` drive the
//! player.

use fsonos_core::favorites::{self, FavoriteKind};
use fsonos_core::playback::{Changes, EventSource, Playback};
use fsonos_core::{HouseholdState, control, resolve_room};
use fsonos_proto::gena::Notify;
use fsonos_proto::net::{EventSink, Lan};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};
use std::time::{Duration, Instant};

const AVT: &str = "/MediaRenderer/AVTransport/Event";
const RCS: &str = "/MediaRenderer/RenderingControl/Event";

struct Watch {
    lan: Lan,
    sink: EventSink,
    player: PlayerId,
    avt: String,
    rcs: String,
    playback: Playback,
}

impl Watch {
    fn new(sim: &SimHandle, room: &str, player: PlayerId) -> Self {
        let lan = Lan::start().unwrap();
        let sink = EventSink::start("127.0.0.1:0".parse().unwrap()).unwrap();
        let base = &sim.player(room).unwrap().base_url;
        let sub = |path: &str, tag: &str| {
            lan.subscribe_at(&format!("{base}{path}"), &sink.callback_url(tag), 300)
                .unwrap()
                .sid
        };
        let (avt, rcs) = (sub(AVT, "AVTransport"), sub(RCS, "RenderingControl"));
        Self {
            lan,
            sink,
            player,
            avt,
            rcs,
            playback: Playback::default(),
        }
    }

    fn apply(&mut self, n: &Notify) -> Changes {
        let source = if n.sid == self.avt {
            EventSource::AvTransport
        } else {
            assert_eq!(n.sid, self.rcs, "an event from an unknown subscription");
            EventSource::RenderingControl
        };
        self.playback
            .apply(&self.player, source, n, Instant::now())
            .unwrap()
    }

    /// Fold events until `done` holds for the player's state, or fail.
    fn until(
        &mut self,
        what: &str,
        done: impl Fn(&fsonos_core::playback::PlayerPlayback) -> bool,
    ) -> Vec<Changes> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen = Vec::new();
        loop {
            if self.playback.of(&self.player).is_some_and(&done) {
                return seen;
            }
            assert!(Instant::now() < deadline, "state never reached: {what}");
            if let Some(n) = self.sink.recv_timeout(Duration::from_millis(200)) {
                seen.push(self.apply(&n));
            }
        }
    }
}

#[test]
fn playback_state_follows_the_player_through_real_events() {
    let sim = SimHousehold::builder()
        .s1([SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1)])
        .spawn()
        .unwrap();
    let simlan = sim.lan();
    let mut st = HouseholdState::default();
    st.apply_topology(&get_zone_group_state(&simlan, sim.player("Kitchen").unwrap().ip).unwrap());
    let houses = vec![st];
    let target = resolve_room(&houses, "Kitchen").unwrap();
    let coordinator = target.coordinator.id.clone();
    let mut watch = Watch::new(&sim, "Kitchen", coordinator.clone());

    // The initial full-state events arrive on their own.
    watch.until("initial transport and volume", |p| {
        p.transport.is_some() && p.volume.is_some()
    });

    // Play a track favorite: transport goes PLAYING and the track is reported.
    let listed = favorites::list(&simlan, &houses, &coordinator).unwrap();
    let track = listed
        .iter()
        .find(|f| f.kind == FavoriteKind::Track)
        .unwrap();
    favorites::play(&simlan, &houses, &coordinator, track).unwrap();
    let changes = watch.until("playing the favorite", |p| {
        p.transport == Some(TransportState::Playing)
            && p.track_uri.as_deref() == track.uri.as_deref()
    });
    assert!(
        changes.iter().any(|c| c.track.is_some()),
        "the new track was reported"
    );
    let st = watch.playback.of(&coordinator).unwrap();
    assert!(
        st.now_playing.as_ref().is_some_and(|n| !n.title.is_empty()),
        "{st:?}"
    );

    // Pause: the state follows.
    control::pause(&simlan, &houses, &coordinator).unwrap();
    watch.until("paused", |p| p.transport == Some(TransportState::Paused));

    // Volume: a RenderingControl event reports exactly the new level.
    control::set_volume(&simlan, &houses, &coordinator, 23).unwrap();
    let changes = watch.until("volume 23", |p| p.volume == Some(23));
    assert!(changes.iter().any(|c| c.volume == Some(23)));

    // An album becomes the queue; Next moves to its second track.
    let album = favorites::find(&listed, "sim symphonies").unwrap();
    favorites::play(&simlan, &houses, &coordinator, album).unwrap();
    watch.until("album playing from track 1", |p| {
        p.transport == Some(TransportState::Playing) && p.queue_position == Some(1)
    });
    control::next(&simlan, &houses, &coordinator).unwrap();
    let changes = watch.until("second track", |p| p.queue_position == Some(2));
    assert!(
        changes.iter().any(|c| c.track.is_some()),
        "Next reported a track change"
    );

    for sid in [&watch.avt, &watch.rcs] {
        let base = &sim.player("Kitchen").unwrap().base_url;
        let path = if *sid == watch.avt { AVT } else { RCS };
        watch
            .lan
            .unsubscribe_at(&format!("{base}{path}"), sid)
            .unwrap();
    }
}
