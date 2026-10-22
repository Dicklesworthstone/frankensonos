//! Spotify replacement uses the same staged queue path as Sonos favorites.
//! Real S1/S2 simulator sockets, with failures injected only at the transport
//! boundary. No physical speakers or Spotify account are needed.

use fsonos_core::{CoreError, HouseholdState, control};
use fsonos_proto::content::{QUEUE, browse_all};
use fsonos_proto::control as soap;
use fsonos_proto::didl::SpotifyContainer;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};
use std::cell::RefCell;
use std::net::IpAddr;

const OLD: &str = "x-file-cifs://example.invalid/keep-playing.flac";
const ALBUM: &str = "spotify:album:0SimReplacementAlbum01";
const PLAYLIST: &str = "spotify:playlist:0SimReplacementList001";

#[derive(Debug, Clone, Copy)]
enum Fault {
    None,
    RefusedAdd,
    LostAdd,
    RefusedRemoval,
    LostRemoval,
    LostStart,
}

struct Replies {
    inner: SimLan,
    fault: Fault,
    writes: RefCell<Vec<String>>,
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
        if [
            "AddURIToQueue",
            "RemoveTrackRangeFromQueue",
            "RemoveAllTracksFromQueue",
            "SetAVTransportURI",
            "Seek",
            "Play",
        ]
        .contains(&name)
        {
            self.writes.borrow_mut().push(name.to_string());
        }
        if (name == "AddURIToQueue" && matches!(self.fault, Fault::RefusedAdd))
            || (name == "RemoveTrackRangeFromQueue" && matches!(self.fault, Fault::RefusedRemoval))
        {
            return Err(ProtoError::SoapFault {
                code: 800,
                reason: "injected refusal".into(),
            });
        }
        let lost = || ProtoError::Network {
            target: host.to_string(),
            detail: "injected lost reply".into(),
        };
        if matches!(self.fault, Fault::LostStart) {
            // Destination/source preflight now reads GetMediaInfo before any
            // replacement mutation. Inject uncertainty only once the start
            // write has actually been attempted, so this test still exercises
            // the post-commit no-replay path it names.
            let start_attempted = self
                .writes
                .borrow()
                .iter()
                .any(|write| write == "SetAVTransportURI");
            if name == "SetAVTransportURI" || (name == "GetMediaInfo" && start_attempted) {
                return Err(lost());
            }
        }
        let response = self.inner.soap_post(host, path, action, body)?;
        if (name == "AddURIToQueue" && matches!(self.fault, Fault::LostAdd))
            || (name == "RemoveTrackRangeFromQueue" && matches!(self.fault, Fault::LostRemoval))
        {
            return Err(lost());
        }
        Ok(response)
    }
}

fn fixture(generation: u8) -> (SimHandle, Vec<HouseholdState>, PlayerId, IpAddr) {
    let builder = SimHousehold::builder();
    let builder = if generation == 1 {
        builder.s1([SimPlayerSpec::new("Den", SimModel::Play5Gen1)])
    } else {
        builder.s2([SimPlayerSpec::new("Den", SimModel::One)])
    };
    let sim = builder.spawn().unwrap();
    let player = sim.player("Den").unwrap();
    let host = player.ip;
    let id = PlayerId(player.uuid.clone());
    let mut house = HouseholdState::default();
    house.apply_topology(&get_zone_group_state(&sim.lan(), host).unwrap());
    let houses = vec![house];
    soap::add_uri_to_queue(&sim.lan(), host, OLD, "", false).unwrap();
    control::play_queue_from(&sim.lan(), &houses, &id, 1).unwrap();
    // Both container kinds must work without a Spotify track favorite.
    sim.retain_favorites(generation, |uri| uri.starts_with("x-rincon-cpcontainer:"))
        .unwrap();
    (sim, houses, id, host)
}

fn queue(sim: &SimHandle, host: IpAddr) -> Vec<String> {
    browse_all(&sim.lan(), host, QUEUE)
        .unwrap()
        .into_iter()
        .map(|item| item.res.unwrap().uri)
        .collect()
}

fn replace(
    replies: &Replies,
    houses: &[HouseholdState],
    id: &PlayerId,
    uri: &str,
) -> Result<(), CoreError> {
    control::play_spotify_container(
        replies,
        houses,
        id,
        SpotifyContainer::of(uri).unwrap(),
        uri,
        "Replacement",
    )
}

#[test]
fn album_and_playlist_replacements_play_after_verified_commit_on_s1_and_s2() {
    for generation in [1, 2] {
        for uri in [ALBUM, PLAYLIST] {
            for fault in [Fault::None, Fault::LostRemoval] {
                let (sim, houses, id, host) = fixture(generation);
                let replies = Replies {
                    inner: sim.lan(),
                    fault,
                    writes: RefCell::default(),
                };
                replace(&replies, &houses, &id, uri).unwrap();
                assert_eq!(
                    *replies.writes.borrow(),
                    [
                        "AddURIToQueue",
                        "RemoveTrackRangeFromQueue",
                        "SetAVTransportURI",
                        "Seek",
                        "Play",
                    ],
                    "{generation} {uri} {fault:?}"
                );
                let current = queue(&sim, host);
                assert!(current.len() > 1, "the simulator expands the container");
                assert!(current.iter().all(|item| item != OLD));
                let now = control::playback(&sim.lan(), &houses, &id).unwrap();
                assert_eq!(now.transport.state, TransportState::Playing);
                assert_eq!(now.position.track, 1);
                assert_eq!(now.position.uri, current[0]);
            }
        }
    }
}

#[test]
fn rejected_and_uncertain_spotify_replacements_do_not_destroy_existing_music() {
    for generation in [1, 2] {
        for fault in [Fault::RefusedAdd, Fault::LostAdd, Fault::RefusedRemoval] {
            let (sim, houses, id, host) = fixture(generation);
            let replies = Replies {
                inner: sim.lan(),
                fault,
                writes: RefCell::default(),
            };
            let error = replace(&replies, &houses, &id, ALBUM).unwrap_err();
            let expected = match fault {
                Fault::RefusedAdd => {
                    assert!(matches!(error, CoreError::RenderParamsStale));
                    vec!["AddURIToQueue", "AddURIToQueue"]
                }
                Fault::RefusedRemoval => {
                    assert!(matches!(error, CoreError::Uncertain(_)), "{error:?}");
                    vec!["AddURIToQueue", "RemoveTrackRangeFromQueue"]
                }
                _ => {
                    assert!(matches!(error, CoreError::Uncertain(_)), "{error:?}");
                    vec!["AddURIToQueue"]
                }
            };
            assert_eq!(*replies.writes.borrow(), expected, "{fault:?}");
            let current = queue(&sim, host);
            assert_eq!(current[0], OLD);
            if matches!(fault, Fault::RefusedAdd) {
                assert_eq!(current, [OLD]);
            }
            let now = control::playback(&sim.lan(), &houses, &id).unwrap();
            assert_eq!(now.transport.state, TransportState::Playing);
            assert_eq!(now.position.uri, OLD);
        }
    }
}

#[test]
fn post_commit_start_uncertainty_does_not_request_address_healing_or_reenqueue() {
    for generation in [1, 2] {
        let (sim, houses, id, host) = fixture(generation);
        let replies = Replies {
            inner: sim.lan(),
            fault: Fault::LostStart,
            writes: RefCell::default(),
        };
        let error = replace(&replies, &houses, &id, ALBUM).unwrap_err();
        assert!(matches!(error, CoreError::Uncertain(_)), "{error:?}");
        assert_eq!(
            *replies.writes.borrow(),
            [
                "AddURIToQueue",
                "RemoveTrackRangeFromQueue",
                "SetAVTransportURI"
            ]
        );
        assert!(queue(&sim, host).iter().all(|item| item != OLD));
    }
}
