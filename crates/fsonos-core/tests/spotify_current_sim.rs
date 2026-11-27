//! Bootstrap Spotify rendering from this group's complete native track.
//! Real S1/S2 simulator sockets; account evidence and faults stay synthetic.

use fsonos_core::{CoreError, HouseholdState, control};
use fsonos_proto::content::{QUEUE, browse_all};
use fsonos_proto::control as soap;
use fsonos_proto::didl::{
    SpotifyContainer, SpotifyRenderParams, spotify_track_didl, spotify_track_uri,
    spotify_uri_from_renderer_uri,
};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};
use std::cell::{Cell, RefCell};
use std::net::IpAddr;
use std::time::Duration;

const CURRENT: &str = "spotify:track:MnOpQr0123456789StUvWx";
const NEXT: &str = "spotify:track:AbCdEf0123456789GhIjKl";
const EPISODE: &str = "spotify:episode:EpIsOd0123456789GhIjKl";
const ALBUM: &str = "spotify:album:0SimReplacementAlbum01";
const EXTERNAL: &str = "http://example.invalid/another-source.flac";

struct House {
    sim: SimHandle,
    houses: Vec<HouseholdState>,
    coordinator: PlayerId,
    host: IpAddr,
    params: SpotifyRenderParams,
}

impl House {
    fn new(generation: u8) -> Self {
        let players = ["Den", "Study"].map(|room| {
            SimPlayerSpec::new(
                room,
                if generation == 1 {
                    SimModel::Play5Gen1
                } else {
                    SimModel::One
                },
            )
        });
        let builder = SimHousehold::builder();
        let sim = if generation == 1 {
            builder.s1(players)
        } else {
            builder.s2(players)
        }
        .spawn()
        .unwrap();
        let host = sim.player("Den").unwrap().ip;
        let coordinator = PlayerId(sim.player("Den").unwrap().uuid.clone());
        let params = sim.render_params(generation).unwrap();
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&sim.lan(), host).unwrap());
        Self {
            sim,
            houses: vec![house],
            coordinator,
            host,
            params,
        }
    }

    fn native(&self, queued: bool) {
        let lan = self.sim.lan();
        let uri = spotify_track_uri(CURRENT, &self.params);
        let metadata = spotify_track_didl(CURRENT, "Native current track", &self.params);
        if queued {
            soap::add_uri_to_queue(&lan, self.host, &uri, &metadata, false).unwrap();
            control::play_queue_from(&lan, &self.houses, &self.coordinator, 1).unwrap();
        } else {
            soap::set_av_transport_uri(&lan, self.host, &uri, &metadata).unwrap();
            soap::play(&lan, self.host).unwrap();
        }
        soap::seek_position(&lan, self.host, 17).unwrap();
    }

    fn without_favorites(&self, generation: u8) {
        self.sim.retain_favorites(generation, |_| false).unwrap();
    }

    fn readonly_since(&self, first: usize) {
        assert!(
            self.sim.soap_log()[first..]
                .iter()
                .all(|call| call.action == "Browse" || call.action.starts_with("Get"))
        );
    }

    fn refresh(&mut self) {
        self.houses[0].apply_topology(&get_zone_group_state(&self.sim.lan(), self.host).unwrap());
    }
}

#[test]
fn native_direct_and_queue_tracks_bootstrap_exact_settings_without_writes() {
    for generation in [1, 2] {
        for queued in [false, true] {
            let house = House::new(generation);
            house.native(queued);
            house.without_favorites(generation);
            let lan = house.sim.lan();
            let before = soap::get_position_info(&lan, house.host).unwrap();
            let first = house.sim.soap_log().len();
            assert_eq!(
                control::spotify_params(&lan, &house.houses, &house.coordinator).unwrap(),
                Some(house.params.clone())
            );
            for uri in [NEXT, EPISODE, ALBUM] {
                control::check_spotify_source(&lan, &house.houses, &house.coordinator, uri)
                    .unwrap();
            }
            house.readonly_since(first);
            let after = soap::get_position_info(&lan, house.host).unwrap();
            assert_eq!(after.uri, before.uri);
            assert_eq!(after.track, before.track);
            assert_eq!(after.position_secs, before.position_secs);
        }
    }
}

#[test]
fn a_member_uses_its_coordinator_but_an_unrelated_group_cannot_supply_settings() {
    for generation in [1, 2] {
        let mut house = House::new(generation);
        house.native(false);
        house.without_favorites(generation);
        let lan = house.sim.lan();
        let member = PlayerId(house.sim.player("Study").unwrap().uuid.clone());
        assert_eq!(
            control::spotify_params(&lan, &house.houses, &member).unwrap(),
            None,
            "an idle independent room cannot borrow another group's account"
        );
        control::join(&lan, &house.houses, &member, &house.coordinator).unwrap();
        house.refresh();
        let first = house.sim.soap_log().len();
        assert_eq!(
            control::spotify_params(&lan, &house.houses, &member).unwrap(),
            Some(house.params.clone())
        );
        house.readonly_since(first);
    }
}

#[test]
fn favorite_templates_keep_their_existing_preference_without_current_source_reads() {
    for generation in [1, 2] {
        let house = House::new(generation);
        let first = house.sim.soap_log().len();
        assert_eq!(
            control::spotify_params(&house.sim.lan(), &house.houses, &house.coordinator).unwrap(),
            Some(house.params.clone())
        );
        assert!(
            house.sim.soap_log()[first..]
                .iter()
                .all(|call| call.action == "Browse")
        );
    }
}

#[test]
fn current_native_settings_drive_direct_play_silent_loading_and_queue_additions() {
    for generation in [1, 2] {
        for silent in [false, true] {
            let house = House::new(generation);
            house.native(false);
            house.without_favorites(generation);
            let lan = house.sim.lan();
            let first = house.sim.soap_log().len();
            if silent {
                control::load_spotify_track(
                    &lan,
                    &house.houses,
                    &house.coordinator,
                    NEXT,
                    "Selected track",
                )
                .unwrap();
            } else {
                control::play_spotify_track(
                    &lan,
                    &house.houses,
                    &house.coordinator,
                    NEXT,
                    "Selected track",
                )
                .unwrap();
            }
            let current = control::playback(&lan, &house.houses, &house.coordinator).unwrap();
            assert_eq!(
                spotify_uri_from_renderer_uri(&current.position.uri).as_deref(),
                Some(NEXT)
            );
            if silent {
                assert!(matches!(
                    current.transport.state,
                    TransportState::Paused | TransportState::Stopped
                ));
                assert!(
                    house.sim.soap_log()[first..]
                        .iter()
                        .all(|call| call.action != "Play")
                );
            } else {
                assert_eq!(current.transport.state, TransportState::Playing);
            }
        }
        for source in [NEXT, EPISODE] {
            for next in [false, true] {
                let house = House::new(generation);
                house.native(true);
                house.without_favorites(generation);
                let lan = house.sim.lan();
                let before = soap::get_position_info(&lan, house.host).unwrap();
                let first = house.sim.soap_log().len();
                let added = control::enqueue_spotify(
                    &lan,
                    &house.houses,
                    &house.coordinator,
                    source,
                    "Selected source",
                    next,
                )
                .unwrap();
                assert_eq!(added.first, 2);
                assert_eq!(added.uris.len(), 1);
                assert!(added.playing_queue);
                let queue = browse_all(&lan, house.host, QUEUE).unwrap();
                assert_eq!(queue.len(), 2);
                assert_eq!(
                    spotify_uri_from_renderer_uri(&queue[1].res.as_ref().unwrap().uri).as_deref(),
                    Some(source)
                );
                let after = soap::get_position_info(&lan, house.host).unwrap();
                assert_eq!(
                    (after.track, after.uri, after.position_secs),
                    (before.track, before.uri, before.position_secs)
                );
                assert!(
                    house.sim.soap_log()[first..]
                        .iter()
                        .filter(|call| call.action != "Browse" && !call.action.starts_with("Get"))
                        .all(|call| call.action == "AddURIToQueue")
                );
            }
        }
    }
}

#[test]
fn a_container_can_replace_the_queue_using_the_current_native_track_account() {
    for generation in [1, 2] {
        let house = House::new(generation);
        house.native(true);
        house.without_favorites(generation);
        let lan = house.sim.lan();
        control::play_spotify_container(
            &lan,
            &house.houses,
            &house.coordinator,
            SpotifyContainer::Album,
            ALBUM,
            "Native account album",
        )
        .unwrap();
        let queue = browse_all(&lan, house.host, QUEUE).unwrap();
        assert!(queue.len() > 1);
        assert!(queue.iter().all(|item| {
            spotify_uri_from_renderer_uri(&item.res.as_ref().unwrap().uri).as_deref()
                != Some(CURRENT)
        }));
        let current = control::playback(&lan, &house.houses, &house.coordinator).unwrap();
        assert_eq!(current.position.track, 1);
        assert_eq!(current.transport.state, TransportState::Playing);
    }
}

#[derive(Debug, Clone, Copy)]
enum Change {
    Source,
    Group,
    Boot,
}

struct Changing<'a> {
    house: &'a House,
    lan: SimLan,
    change: Change,
    reads: Cell<u32>,
    writes: RefCell<Vec<String>>,
}

impl Transport for Changing<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        if name != "Browse" && !name.starts_with("Get") {
            self.writes.borrow_mut().push(name.into());
        }
        if name == "GetPositionInfo" {
            self.reads.set(self.reads.get() + 1);
            if self.reads.get() == 2 {
                match self.change {
                    Change::Source => {
                        soap::set_av_transport_uri(&self.lan, host, EXTERNAL, "")?;
                    }
                    Change::Group => {
                        soap::join_group(
                            &self.lan,
                            self.house.sim.player("Study").unwrap().ip,
                            &self.house.coordinator,
                        )?;
                    }
                    Change::Boot => {
                        self.house.sim.reboot("Den", Duration::ZERO).unwrap();
                    }
                }
            }
        }
        self.lan.soap_post(host, path, action, body)
    }
}

#[test]
fn source_membership_and_boot_drift_reject_learning_without_a_command_write() {
    for generation in [1, 2] {
        for change in [Change::Source, Change::Group, Change::Boot] {
            let house = House::new(generation);
            house.native(false);
            house.without_favorites(generation);
            let transport = Changing {
                house: &house,
                lan: house.sim.lan(),
                change,
                reads: Cell::new(0),
                writes: RefCell::default(),
            };
            let error =
                control::spotify_params(&transport, &house.houses, &house.coordinator).unwrap_err();
            assert!(
                matches!(error, CoreError::TargetChanged(_) | CoreError::Proto(_)),
                "{generation} {change:?}: {error}"
            );
            assert!(transport.writes.borrow().is_empty());
        }
    }
}
