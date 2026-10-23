//! Spotify podcasts and direct artist/show sources must be rendered for the
//! destination household while retaining their case-sensitive catalog identity.

use fsonos_core::moving::{MoveError, MoveMethod, copy_playback, move_playback_within};
use fsonos_core::{HouseholdState, control, resolve_room};
use fsonos_proto::content::{QUEUE, browse_snapshot};
use fsonos_proto::control as soap;
use fsonos_proto::didl::{parse_didl, spotify_uri_from_renderer_uri};
use fsonos_proto::play_mode::{get_play_mode, set_play_mode};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayMode, PlayerId, TransportState};
use std::net::IpAddr;
use std::time::Duration;

const EPISODE: &str = "spotify:episode:AbCdEf0123456789GhIjKl";
const TRACK: &str = "spotify:track:MnOpQr0123456789StUvWx";
const PAIRS: [(&str, &str); 2] = [("S1 Source", "S2 Target"), ("S2 Source", "S1 Target")];

struct Fixture {
    sim: SimHandle,
    houses: Vec<HouseholdState>,
}

impl Fixture {
    fn new() -> Self {
        let sim = SimHousehold::builder()
            .s1([
                SimPlayerSpec::new("S1 Source", SimModel::Play5Gen1),
                SimPlayerSpec::new("S1 Target", SimModel::Play5Gen1),
            ])
            .s2([
                SimPlayerSpec::new("S2 Source", SimModel::One),
                SimPlayerSpec::new("S2 Target", SimModel::One),
            ])
            .spawn()
            .unwrap();
        let lan = sim.lan();
        let houses = ["S1 Source", "S2 Source"]
            .map(|room| {
                let mut house = HouseholdState::default();
                house.apply_topology(
                    &get_zone_group_state(&lan, sim.player(room).unwrap().ip).unwrap(),
                );
                house
            })
            .into();
        Self { sim, houses }
    }

    fn host(&self, room: &str) -> IpAddr {
        self.sim.player(room).unwrap().ip
    }

    fn id(&self, room: &str) -> PlayerId {
        PlayerId(self.sim.player(room).unwrap().uuid.clone())
    }

    fn queue(&self, room: &str, paused: bool) {
        let lan = self.sim.lan();
        for (uri, title) in [
            (EPISODE, "Episode & one"),
            (EPISODE, "Episode & one"),
            (TRACK, "Song"),
        ] {
            control::enqueue_spotify(&lan, &self.houses, &self.id(room), uri, title, false)
                .unwrap();
        }
        let host = self.host(room);
        soap::play_from_queue(&lan, host, &self.id(room)).unwrap();
        soap::seek_track(&lan, host, 2).unwrap();
        soap::seek_position(&lan, host, 37).unwrap();
        set_play_mode(&lan, host, PlayMode::RepeatOne).unwrap();
        soap::play(&lan, host).unwrap();
        if paused {
            soap::pause(&lan, host).unwrap();
        }
    }

    fn assert_queue(&self, room: &str, paused: bool) {
        let lan = self.sim.lan();
        let queue = browse_snapshot(&lan, self.host(room), QUEUE).unwrap();
        let identities: Vec<_> = queue
            .objects
            .iter()
            .map(|object| spotify_uri_from_renderer_uri(&object.res.as_ref().unwrap().uri).unwrap())
            .collect();
        assert_eq!(identities, [EPISODE, EPISODE, TRACK]);
        assert_eq!(queue.objects[0].title, "Episode & one");
        for episode in &queue.objects[..2] {
            let uri = &episode.res.as_ref().unwrap().uri;
            assert!(uri.ends_with("?sid=12"), "{uri}");
            assert!(!uri.contains("flags=") && !uri.contains("sn="));
        }
        let position = soap::get_position_info(&lan, self.host(room)).unwrap();
        assert_eq!((position.track, position.position_secs), (2, Some(37)));
        assert_eq!(
            get_play_mode(&lan, self.host(room)).unwrap(),
            PlayMode::RepeatOne
        );
        let state = soap::get_transport_info(&lan, self.host(room))
            .unwrap()
            .state;
        assert_eq!(
            state,
            if paused {
                TransportState::Stopped
            } else {
                TransportState::Playing
            }
        );
    }
}

/// Read observations from the source carry a different synthetic household
/// service id. The destination's real simulator still requires its own id.
struct SourceAccount {
    inner: SimLan,
    source: IpAddr,
}

impl Transport for SourceAccount {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let result = self.inner.soap_post(host, path, action, body)?;
        if host == self.source
            && (action.contains("#GetMediaInfo")
                || action.contains("#GetPositionInfo")
                || action.contains("#Browse"))
        {
            Ok(result.replace("?sid=12", "?sid=77"))
        } else {
            Ok(result)
        }
    }
}

#[test]
fn mixed_episode_queues_copy_both_directions_with_duplicate_selection_and_mode() {
    for (from, to) in PAIRS {
        for paused in [false, true] {
            let f = Fixture::new();
            f.queue(from, paused);
            soap::enqueue_uri(
                &f.sim.lan(),
                f.host(to),
                "https://example.invalid/old.mp3",
                "",
                0,
                false,
            )
            .unwrap();
            let t = SourceAccount {
                inner: f.sim.lan(),
                source: f.host(from),
            };
            let since = f.sim.soap_log().len();
            let report = copy_playback(
                &t,
                &f.houses,
                &resolve_room(&f.houses, from).unwrap(),
                &resolve_room(&f.houses, to).unwrap(),
            )
            .unwrap();
            assert_eq!(report.method, MoveMethod::Copied);
            f.assert_queue(to, paused);
            assert!(
                f.sim.soap_log()[since..]
                    .iter()
                    .all(|entry| entry.action != "RemoveAllTracksFromQueue")
            );
            if paused {
                assert!(
                    f.sim.soap_log()[since..]
                        .iter()
                        .filter(|entry| entry.room == to)
                        .all(|entry| entry.action != "Play" && entry.action != "Pause")
                );
            }
        }
    }
}

#[test]
fn unsupported_native_handoff_replays_podcast_queues_on_both_generations() {
    for (from, to) in [("S1 Source", "S1 Target"), ("S2 Source", "S2 Target")] {
        let f = Fixture::new();
        f.queue(from, false);
        f.sim
            .upnp_fault(from, "DelegateGroupCoordinationTo", 401)
            .unwrap();
        let report = move_playback_within(
            &f.sim.lan(),
            &f.houses,
            &resolve_room(&f.houses, from).unwrap(),
            &resolve_room(&f.houses, to).unwrap(),
            Duration::ZERO,
        )
        .unwrap();
        assert_eq!(report.method, MoveMethod::Copied);
        f.assert_queue(to, false);
    }
}

#[test]
fn direct_bare_sources_use_destination_linkage_and_keep_catalog_case() {
    for (from, to) in PAIRS {
        for kind in ["episode", "artist", "show"] {
            let f = Fixture::new();
            let uri = format!("spotify:{kind}:AbCdEf0123456789GhIjKl");
            control::play_spotify_bare(&f.sim.lan(), &f.houses, &f.id(from), &uri, "A & B")
                .unwrap();
            let t = SourceAccount {
                inner: f.sim.lan(),
                source: f.host(from),
            };
            copy_playback(
                &t,
                &f.houses,
                &resolve_room(&f.houses, from).unwrap(),
                &resolve_room(&f.houses, to).unwrap(),
            )
            .unwrap();
            let media = soap::get_media_info(&f.sim.lan(), f.host(to)).unwrap();
            assert_eq!(spotify_uri_from_renderer_uri(&media.uri), Some(uri));
            assert!(media.uri.ends_with("?sid=12"), "{}", media.uri);
            let metadata = parse_didl(&media.uri_metadata).unwrap();
            assert_eq!(metadata[0].title, "A & B");
            assert!(metadata[0].desc.is_none());
            assert_eq!(
                soap::get_transport_info(&f.sim.lan(), f.host(from))
                    .unwrap()
                    .state,
                TransportState::Stopped
            );
        }
    }
}

#[test]
fn missing_destination_linkage_rejects_episode_transfer_before_any_write() {
    for (from, to) in PAIRS {
        for queued in [false, true] {
            let f = Fixture::new();
            if queued {
                f.queue(from, false);
            } else {
                control::play_spotify_bare(
                    &f.sim.lan(),
                    &f.houses,
                    &f.id(from),
                    EPISODE,
                    "Episode",
                )
                .unwrap();
            }
            f.sim
                .retain_favorites(f.sim.player(to).unwrap().generation, |_| false)
                .unwrap();
            let before = f.sim.soap_log().len();
            let result = copy_playback(
                &f.sim.lan(),
                &f.houses,
                &resolve_room(&f.houses, from).unwrap(),
                &resolve_room(&f.houses, to).unwrap(),
            );
            assert!(
                matches!(result, Err(MoveError::NoRenderParams(_))),
                "{result:?}"
            );
            assert!(
                f.sim.soap_log()[before..]
                    .iter()
                    .all(|entry| entry.action.starts_with("Get") || entry.action == "Browse")
            );
        }
    }
}
