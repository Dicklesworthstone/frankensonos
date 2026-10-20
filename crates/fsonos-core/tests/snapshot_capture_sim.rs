//! Capture changing playback through the production snapshot path on S1/S2.
//! Only the injected change is synthetic; every read reaches a real socket.

use fsonos_core::{HouseholdState, actions};
use fsonos_core::snapshot::{self, SnapshotSource, ZoneSnapshot};
use fsonos_proto::content::{self, BrowseFlag};
use fsonos_proto::control as soap;
use fsonos_proto::play_mode::{get_play_mode, set_play_mode};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayMode, PlayerId, TransportState};
use std::cell::{Cell, RefCell};
use std::net::IpAddr;
use std::time::Duration;

const FIRST: &str = "x-file-cifs://example.invalid/first.flac";
const SECOND: &str = "x-file-cifs://example.invalid/second.flac";
const OTHER: &str = "x-file-cifs://example.invalid/other.flac";

struct Fixture {
    sim: SimHandle,
    lan: SimLan,
    houses: Vec<HouseholdState>,
    host: IpAddr,
    player: PlayerId,
}

impl Fixture {
    fn new(generation: u8) -> Self {
        let builder = SimHousehold::builder();
        let builder = if generation == 1 {
            builder.s1([SimPlayerSpec::new("Room", SimModel::Play5Gen1)])
        } else {
            builder.s2([SimPlayerSpec::new("Room", SimModel::One)])
        };
        let sim = builder.spawn().unwrap();
        let lan = sim.lan();
        let room = sim.player("Room").unwrap();
        let (host, player) = (room.ip, PlayerId(room.uuid.clone()));
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&lan, host).unwrap());
        for uri in [FIRST, SECOND] {
            soap::add_uri_to_queue(&lan, host, uri, "", false).unwrap();
        }
        soap::play_from_queue(&lan, host, &player).unwrap();
        soap::seek_track(&lan, host, 2).unwrap();
        soap::seek_position(&lan, host, 37).unwrap();
        soap::play(&lan, host).unwrap();
        set_play_mode(&lan, host, PlayMode::ShuffleNoRepeat).unwrap();
        Self {
            sim,
            lan,
            houses: vec![house],
            host,
            player,
        }
    }

    fn capture<T: Transport>(&self, transport: &T) -> Result<ZoneSnapshot, fsonos_core::CoreError> {
        snapshot::capture(transport, &self.houses, &self.player, 123)
    }

    fn direct(&self, uri: &str) {
        let metadata = format!(
            "<DIDL-Lite><item id=\"synthetic\"><res duration=\"0:03:00\">{uri}</res></item></DIDL-Lite>"
        );
        soap::set_av_transport_uri(&self.lan, self.host, uri, &metadata).unwrap();
        soap::seek_position(&self.lan, self.host, 7).unwrap();
        soap::play(&self.lan, self.host).unwrap();
    }
}

struct After<'a, F> {
    lan: &'a SimLan,
    hook: F,
    reads: RefCell<Vec<String>>,
}

impl<'a, F> After<'a, F> {
    fn new(lan: &'a SimLan, hook: F) -> Self {
        Self {
            lan,
            hook,
            reads: RefCell::new(Vec::new()),
        }
    }

    fn count(&self, action: &str) -> usize {
        self.reads.borrow().iter().filter(|a| *a == action).count()
    }

    fn assert_read_only(&self) {
        assert!(
            self.reads
                .borrow()
                .iter()
                .all(|a| a.starts_with("Get") || a == "Browse")
        );
    }
}

impl<F: Fn(&str)> Transport for After<'_, F> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        self.reads.borrow_mut().push(name.to_string());
        let reply = self.lan.soap_post(host, path, action, body);
        (self.hook)(name);
        reply
    }
}

#[test]
fn a_stable_queue_captures_the_selected_track_and_round_trips() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let t = After::new(&f.lan, |_: &str| {});
        let snap = f.capture(&t).unwrap();
        t.assert_read_only();
        assert!(matches!(
            snap.source, SnapshotSource::Queue { track: 2, position_secs: 37, .. }
        ));
        assert_eq!(snap.play_mode, Some(PlayMode::ShuffleNoRepeat));
        assert_eq!(snap.transport_state, TransportState::Playing);
        assert_eq!(snap.captured_at, 123);
        let saved = actions::before_state(std::slice::from_ref(&snap)).unwrap();
        assert_eq!(
            actions::BeforeState::parse(&saved).unwrap().zones,
            [snap.clone()]
        );
        f.direct(OTHER);
        let report = snapshot::restore(&f.lan, &f.houses, &snap).unwrap();
        assert!(report.skipped.is_empty(), "{report:?}");
        let position = soap::get_position_info(&f.lan, f.host).unwrap();
        assert_eq!((position.track, position.position_secs), (2, Some(37)));
    }
}

#[test]
fn a_source_switch_discards_the_old_sources_position() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let fired = Cell::new(false);
        let t = After::new(&f.lan, |action: &str| {
            if action == "GetPositionInfo" && !fired.replace(true) {
                f.direct(OTHER);
            }
        });
        let snap = f.capture(&t).unwrap();
        assert!(matches!(&snap.source, SnapshotSource::Uri { uri, position_secs: Some(7), .. } if uri == OTHER
        ));
        assert_eq!(snap.play_mode, None);
        t.assert_read_only();
    }
}

#[test]
fn a_track_change_during_room_reads_is_recaptured_at_its_own_position() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let fired = Cell::new(false);
        let t = After::new(&f.lan, |action: &str| {
            if action == "GetVolume" && !fired.replace(true) {
                soap::seek_track(&f.lan, f.host, 1).unwrap();
                soap::seek_position(&f.lan, f.host, 3).unwrap();
            }
        });
        let snap = f.capture(&t).unwrap();
        assert!(matches!(
            snap.source, SnapshotSource::Queue { track: 1, position_secs: 3, .. }
        ));
        t.assert_read_only();
    }
}

#[test]
fn a_queue_edit_cannot_pair_old_positions_with_a_new_revision() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let fired = Cell::new(false);
        let t = After::new(&f.lan, |action: &str| {
            if action == "GetPositionInfo" && !fired.replace(true) {
                soap::enqueue_uri(&f.lan, f.host, OTHER, "", 1, false).unwrap();
            }
        });
        let snap = f.capture(&t).unwrap();
        let page = content::browse(
            &f.lan,
            f.host,
            content::QUEUE,
            BrowseFlag::DirectChildren,
            0,
            1,
        )
        .unwrap();
        assert!(matches!(
            snap.source, SnapshotSource::Queue { track: 3, update_id, .. } if update_id == page.update_id
        ));
        t.assert_read_only();
    }
}

#[test]
fn a_pause_and_mode_change_during_capture_keep_the_new_choices() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let fired = Cell::new(false);
        let t = After::new(&f.lan, |action: &str| {
            if action == "GetVolume" && !fired.replace(true) {
                soap::pause(&f.lan, f.host).unwrap();
                set_play_mode(&f.lan, f.host, PlayMode::RepeatOne).unwrap();
            }
        });
        let snap = f.capture(&t).unwrap();
        assert_eq!(snap.transport_state, TransportState::Paused);
        assert_eq!(snap.play_mode, Some(PlayMode::RepeatOne));
        t.assert_read_only();
    }
}

#[test]
fn a_backward_seek_is_not_mistaken_for_the_same_playback_epoch() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let fired = Cell::new(false);
        let t = After::new(&f.lan, |action: &str| {
            if action == "GetVolume" && !fired.replace(true) {
                soap::seek_position(&f.lan, f.host, 9).unwrap();
            }
        });
        let snap = f.capture(&t).unwrap();
        assert!(matches!(
            snap.source, SnapshotSource::Queue { position_secs: 9, .. }
        ));
        assert_eq!(t.count("GetVolume"), 2);
        t.assert_read_only();
    }
}

#[test]
fn ordinary_clock_progress_does_not_force_another_capture() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let t = After::new(&f.lan, |action: &str| {
            if action == "GetVolume" {
                f.sim.clock().advance(Duration::from_secs(1));
            }
        });
        let snap = f.capture(&t).unwrap();
        assert!(matches!(
            snap.source, SnapshotSource::Queue { position_secs: 38, .. }
        ));
        assert_eq!(t.count("GetVolume"), 1);
        t.assert_read_only();
    }
}

#[test]
fn continuous_changes_are_bounded_and_never_saved_as_undo_state() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let odd = Cell::new(false);
        let t = After::new(&f.lan, |action: &str| {
            if action == "GetVolume" {
                odd.set(!odd.get());
                let track = if odd.get() { 1 } else { 2 };
                soap::seek_track(&f.lan, f.host, track).unwrap();
            }
        });
        let (snaps, missed) =
            actions::capture_zones(&t, &f.houses, std::slice::from_ref(&f.player), 123);
        assert!(snaps.is_empty());
        assert_eq!(missed.len(), 1);
        assert!(missed[0].1.contains("capturing the snapshot"));
        assert_eq!(t.count("GetVolume"), 3);
        t.assert_read_only();
    }
}

#[test]
fn required_reads_fail_without_fabricating_or_retrying_state() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        f.sim.upnp_fault("Room", "GetPositionInfo", 701).unwrap();
        let t = After::new(&f.lan, |_: &str| {});
        assert!(f.capture(&t).is_err());
        assert_eq!(t.count("GetPositionInfo"), 1);
        t.assert_read_only();
    }
}

#[test]
fn unsupported_optional_settings_do_not_invent_normal_or_block_capture() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        f.sim.upnp_fault("Room", "GetTransportSettings", 401).unwrap();
        let t = After::new(&f.lan, |_: &str| {});
        let snap = f.capture(&t).unwrap();
        assert_eq!(snap.play_mode, None);
        assert!(matches!(
            snap.source, SnapshotSource::Queue { track: 2, .. }
        ));
        t.assert_read_only();
    }
}

#[test]
fn another_players_queue_is_not_captured_as_our_own() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        soap::set_av_transport_uri(
            &f.lan,
            f.host,
            "x-rincon-queue:RINCON_SYNTHETIC_OTHER#0",
            "",
        )
        .unwrap();
        let t = After::new(&f.lan, |_: &str| {});
        assert!(f.capture(&t).is_err());
        t.assert_read_only();
    }
}

#[test]
fn an_empty_queue_remains_an_empty_queue_without_starting_it() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        soap::remove_all_tracks_from_queue(&f.lan, f.host).unwrap();
        let t = After::new(&f.lan, |_: &str| {});
        let snap = f.capture(&t).unwrap();
        assert!(matches!(
            snap.source, SnapshotSource::Queue { track: 0, position_secs: 0, .. }
        ));
        assert_eq!(snap.transport_state, TransportState::Stopped);
        assert_eq!(snap.play_mode, Some(get_play_mode(&f.lan, f.host).unwrap()));
        t.assert_read_only();
    }
}
