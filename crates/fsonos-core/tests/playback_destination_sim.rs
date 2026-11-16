//! Every player reports its whole household. An expected coordinator appearing
//! in that document does not make a different room's endpoint authoritative.

use fsonos_core::snapshot::{self, Aspect};
use fsonos_core::{CoreError, HouseholdState, control};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};
use std::cell::{Cell, RefCell};
use std::net::IpAddr;

const OLD: &str = "x-file-cifs://example.invalid/original.flac";
const NEW: &str = "x-rincon-mp3radio://example.invalid/selected.mp3";

struct Fixture {
    sim: SimHandle,
    houses: Vec<HouseholdState>,
    player: PlayerId,
    host: IpAddr,
    peer: IpAddr,
}

impl Fixture {
    fn new(generation: u8) -> Self {
        let model = if generation == 1 {
            SimModel::Play5Gen1
        } else {
            SimModel::One
        };
        let rooms = [
            SimPlayerSpec::new("Requested", model),
            SimPlayerSpec::new("Other", model),
        ];
        let builder = SimHousehold::builder();
        let sim = if generation == 1 {
            builder.s1(rooms)
        } else {
            builder.s2(rooms)
        }
        .spawn()
        .unwrap();
        let lan = sim.lan();
        let host = sim.player("Requested").unwrap().ip;
        let peer = sim.player("Other").unwrap().ip;
        let player = PlayerId(sim.player("Requested").unwrap().uuid.clone());
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&lan, host).unwrap());
        for room in ["Requested", "Other"] {
            let endpoint = sim.player(room).unwrap();
            soap::add_uri_to_queue(&lan, endpoint.ip, OLD, "", false).unwrap();
            soap::play_from_queue(&lan, endpoint.ip, &PlayerId(endpoint.uuid.clone())).unwrap();
            soap::play(&lan, endpoint.ip).unwrap();
        }
        Self {
            sim,
            houses: vec![house],
            player,
            host,
            peer,
        }
    }

    /// A cached binding still points at an address now owned by another room.
    fn stale(&self) -> Vec<HouseholdState> {
        let mut houses = self.houses.clone();
        for player in &mut houses[0].players {
            if player.ip == self.host {
                player.ip = self.peer;
            } else if player.ip == self.peer {
                player.ip = self.host;
            }
        }
        houses
    }

    fn requested_untouched(&self) {
        let actual = control::playback(&self.sim.lan(), &self.houses, &self.player).unwrap();
        assert_eq!(actual.transport.state, TransportState::Playing);
        assert_eq!(actual.position.uri, OLD);
    }
}

/// All observations and effects use real simulator sockets. Only one write's
/// acknowledgement is withheld; no read is fabricated to look successful.
struct Replies {
    inner: SimLan,
    lose: Option<&'static str>,
    lost: Cell<bool>,
    calls: RefCell<Vec<String>>,
}

impl Replies {
    fn new(sim: &SimHandle, lose: Option<&'static str>) -> Self {
        Self {
            inner: sim.lan(),
            lose,
            lost: Cell::new(false),
            calls: RefCell::default(),
        }
    }

    fn writes(&self) -> Vec<String> {
        self.calls
            .borrow()
            .iter()
            .filter(|name| !name.starts_with("Get") && name.as_str() != "Browse")
            .cloned()
            .collect()
    }
}

impl Transport for Replies {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        self.calls.borrow_mut().push(name.into());
        let answer = self.inner.soap_post(host, path, action, body)?;
        if self.lose == Some(name) && !self.lost.replace(true) {
            return Err(ProtoError::Network {
                target: host.to_string(),
                detail: "injected missing destination acknowledgement".into(),
            });
        }
        Ok(answer)
    }
}

#[test]
fn silent_loaders_reject_another_rooms_endpoint_before_any_write() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let stale = f.stale();
        for operation in 0..4 {
            let replies = Replies::new(&f.sim, None);
            let result = match operation {
                0 => control::pause_confirmed(&replies, &stale, &f.player),
                1 => control::load_uri(&replies, &stale, &f.player, NEW, ""),
                2 => control::load_queue_from(&replies, &stale, &f.player, 1),
                _ => control::load_queue_from_in_order(&replies, &stale, &f.player, 1),
            };
            assert!(
                result.is_err(),
                "operation {operation} accepted the wrong address"
            );
            assert!(
                replies.writes().is_empty(),
                "operation {operation} wrote to another room"
            );
            assert_eq!(
                soap::get_transport_info(&f.sim.lan(), f.peer)
                    .unwrap()
                    .state,
                TransportState::Playing,
            );
            f.requested_untouched();
        }
    }
}

#[test]
fn a_matching_source_on_another_endpoint_cannot_authorize_selection_or_play() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        soap::set_av_transport_uri(&f.sim.lan(), f.peer, NEW, "").unwrap();
        soap::play(&f.sim.lan(), f.peer).unwrap();
        let replies = Replies::new(&f.sim, Some("SetAVTransportURI"));
        let error = control::play_uri(&replies, &f.stale(), &f.player, NEW, "").unwrap_err();
        assert!(matches!(error, CoreError::TargetChanged(_)));
        assert!(replies.writes().is_empty());
        assert!(!replies.lost.get(), "the stale endpoint receives no write");
        assert_eq!(soap::get_media_info(&f.sim.lan(), f.peer).unwrap().uri, NEW);
        assert_eq!(
            soap::get_transport_info(&f.sim.lan(), f.peer)
                .unwrap()
                .state,
            TransportState::Playing,
            "the unrelated room keeps playing its original matching source",
        );
        f.requested_untouched();
    }
}

#[test]
fn a_matching_queue_uri_on_another_endpoint_cannot_authorize_mode_or_seek() {
    for generation in [1, 2] {
        for ordered in [false, true] {
            let f = Fixture::new(generation);
            let own_queue = format!("x-rincon-queue:{}#0", f.player.0);
            soap::set_av_transport_uri(&f.sim.lan(), f.peer, &own_queue, "").unwrap();
            let replies = Replies::new(&f.sim, Some("SetAVTransportURI"));
            let error = if ordered {
                control::play_queue_from_in_order(&replies, &f.stale(), &f.player, 1)
            } else {
                control::play_queue_from(&replies, &f.stale(), &f.player, 1)
            }
            .unwrap_err();
            assert!(matches!(error, CoreError::TargetChanged(_)));
            assert!(replies.writes().is_empty());
            assert!(!replies.lost.get(), "the stale endpoint receives no write");
            assert_eq!(
                soap::get_media_info(&f.sim.lan(), f.peer).unwrap().uri,
                own_queue
            );
            f.requested_untouched();
        }
    }
}

#[test]
fn another_rooms_playing_state_cannot_authorize_the_requested_resume() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let replies = Replies::new(&f.sim, Some("Play"));
        let error = control::resume(&replies, &f.stale(), &f.player).unwrap_err();
        assert!(matches!(error, CoreError::TargetChanged(_)));
        assert!(
            replies.writes().is_empty(),
            "Play is refused before dispatch"
        );
        assert!(!replies.lost.get(), "the stale endpoint receives no write");
        f.requested_untouched();
    }
}

#[test]
fn snapshot_source_restoration_cannot_rewrite_another_rooms_queue() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let saved = snapshot::capture(&f.sim.lan(), &f.houses, &f.player, 1).unwrap();
        let replies = Replies::new(&f.sim, None);
        let report = snapshot::restore_only(
            &replies,
            &f.stale(),
            &saved,
            &[Aspect::Source, Aspect::Position, Aspect::Transport],
        )
        .unwrap();
        assert!(!report.restored.contains(&Aspect::Source));
        assert!(
            report
                .skipped
                .iter()
                .any(|(aspect, _)| *aspect == Aspect::Source)
        );
        assert!(replies.writes().is_empty(), "{report:?}");
        assert_eq!(
            soap::get_transport_info(&f.sim.lan(), f.peer)
                .unwrap()
                .state,
            TransportState::Playing,
        );
        f.requested_untouched();
    }
}
