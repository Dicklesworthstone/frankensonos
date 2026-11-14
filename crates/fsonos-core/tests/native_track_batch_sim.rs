//! Selected local and native service tracks use the shared ordered queue path.
//! Every mutation below reaches the real S1/S2 simulator over localhost HTTP.

use fsonos_core::control::TrackBatchDestination;
use fsonos_core::favorites::Favorite;
use fsonos_core::{CoreError, HouseholdState, control, playlists};
use fsonos_proto::content::{QUEUE, browse_snapshot};
use fsonos_proto::control as soap;
use fsonos_proto::didl::parse_didl;
use fsonos_proto::play_mode::{get_play_mode, set_play_mode};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayMode, PlayerId, TransportState};
use std::cell::Cell;
use std::net::IpAddr;
use std::time::Duration;

const LOCAL: &str = "x-file-cifs://example.invalid/music/native.flac";
const SERVICE: &str = "x-sonos-http:native-catalog.mp3?sid=7&flags=8224&sn=4";
const ORIGINAL: [&str; 3] = [
    "x-file-cifs://example.invalid/owner-one.flac",
    "x-file-cifs://example.invalid/owner-one.flac",
    "x-file-cifs://example.invalid/owner-last.flac",
];
const EXTERNAL: &str = "x-file-cifs://example.invalid/another-controller.flac";

struct Rig {
    sim: SimHandle,
    houses: Vec<HouseholdState>,
    coordinator: PlayerId,
    host: IpAddr,
    playlist_id: String,
}

impl Rig {
    fn new(generation: u8) -> Self {
        let model = if generation == 1 {
            SimModel::Play5Gen1
        } else {
            SimModel::One
        };
        let players = [
            SimPlayerSpec::new("Lead", model),
            SimPlayerSpec::new("Peer", model),
        ];
        let builder = SimHousehold::builder();
        let sim = if generation == 1 {
            builder.s1(players)
        } else {
            builder.s2(players)
        }
        .spawn()
        .unwrap();
        let lan = sim.lan();
        let host = sim.player("Lead").unwrap().ip;
        let coordinator = PlayerId(sim.player("Lead").unwrap().uuid.clone());
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&lan, host).unwrap());
        let houses = vec![house];
        for (index, uri) in [LOCAL, SERVICE, LOCAL].into_iter().enumerate() {
            let descriptor = if uri == SERVICE {
                "<desc id=\"cdudn\" nameSpace=\"urn:schemas-rinconnetworks-com:metadata-1-0/\">SA_RINCON7_fixture</desc>"
            } else {
                ""
            };
            let metadata = format!(
                concat!(
                    "<DIDL-Lite xmlns:dc=\"http://purl.org/dc/elements/1.1/\" ",
                    "xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\">",
                    "<item id=\"native-{}\" parentID=\"library\" restricted=\"true\">",
                    "<dc:title>Native &amp; occurrence {}</dc:title>",
                    "<dc:creator>Library artist</dc:creator><upnp:album>Native album</upnp:album>",
                    "<upnp:class>object.item.audioItem.musicTrack</upnp:class>",
                    "<upnp:albumArtURI>https://example.invalid/native.jpg</upnp:albumArtURI>",
                    "<res protocolInfo=\"http-get:*:audio/flac:*\" duration=\"0:03:00\">{}</res>",
                    "{}</item></DIDL-Lite>"
                ),
                index,
                index + 1,
                uri.replace('&', "&amp;"),
                descriptor
            );
            soap::add_uri_to_queue(&lan, host, uri, &metadata, false).unwrap();
        }
        let saved =
            playlists::save_queue(&lan, &houses, &coordinator, "Mixed native library").unwrap();
        soap::remove_all_tracks_from_queue(&lan, host).unwrap();
        for uri in ORIGINAL {
            soap::add_uri_to_queue(&lan, host, uri, "", false).unwrap();
        }
        control::play_queue_from(&lan, &houses, &coordinator, 2).unwrap();
        soap::seek_position(&lan, host, 17).unwrap();
        soap::pause(&lan, host).unwrap();
        soap::set_volume(&lan, host, 63).unwrap();
        set_play_mode(&lan, host, PlayMode::RepeatAll).unwrap();
        sim.retain_favorites(generation, |_| false).unwrap();
        Self {
            sim,
            houses,
            coordinator,
            host,
            playlist_id: saved.id,
        }
    }

    fn selection(&self) -> (TrackBatchDestination, Vec<Favorite>) {
        let lan = self.sim.lan();
        let destination = TrackBatchDestination::read(&lan, self.host, &self.coordinator).unwrap();
        let page = playlists::contents(
            &lan,
            &self.houses,
            &self.coordinator,
            &self.playlist_id,
            0,
            3,
            None,
        )
        .unwrap();
        let tracks = playlists::select_tracks(
            &lan,
            &self.houses,
            &self.coordinator,
            &self.playlist_id,
            &[3, 2, 1, 3],
            &page.revision,
        )
        .unwrap();
        assert_eq!(tracks[0], tracks[3]);
        assert_eq!(tracks[0].uri, tracks[2].uri);
        assert_ne!(tracks[0].metadata, tracks[2].metadata);
        assert_eq!(parse_didl(&tracks[0].metadata).unwrap()[0].desc, None);
        assert!(parse_didl(&tracks[1].metadata).unwrap()[0].desc.is_some());
        (destination, tracks)
    }

    fn queue(&self) -> Vec<String> {
        browse_snapshot(&self.sim.lan(), self.host, QUEUE)
            .unwrap()
            .objects
            .into_iter()
            .map(|item| item.res.unwrap().uri)
            .collect()
    }

    fn writes(&self, start: usize) -> Vec<String> {
        self.sim.soap_log()[start..]
            .iter()
            .filter(|entry| !entry.action.starts_with("Get") && entry.action != "Browse")
            .map(|entry| entry.action.clone())
            .collect()
    }

    fn enqueue<T: Transport + ?Sized>(
        &self,
        t: &T,
        tracks: &[Favorite],
        next: bool,
        destination: &TrackBatchDestination,
    ) -> Result<control::Queued, CoreError> {
        control::enqueue_native_tracks_bound(
            t,
            &self.houses,
            &self.coordinator,
            tracks,
            next,
            destination,
        )
    }

    fn replace<T: Transport + ?Sized>(
        &self,
        t: &T,
        tracks: &[Favorite],
        destination: &TrackBatchDestination,
    ) -> Result<control::QueueReplacement, CoreError> {
        control::play_native_tracks_bound_with_receipt(
            t,
            &self.houses,
            &self.coordinator,
            tracks,
            destination,
        )
    }
}

fn uris(tracks: &[Favorite]) -> Vec<String> {
    tracks
        .iter()
        .map(|track| track.uri.clone().unwrap())
        .collect()
}

#[test]
fn next_and_append_keep_native_didl_order_duplicates_and_quiet_playback() {
    for generation in [1, 2] {
        for next in [false, true] {
            let rig = Rig::new(generation);
            let lan = rig.sim.lan();
            let (destination, tracks) = rig.selection();
            let playback = control::playback(&lan, &rig.houses, &rig.coordinator).unwrap();
            let start = rig.sim.soap_log().len();
            let queued = rig.enqueue(&lan, &tracks, next, &destination).unwrap();
            assert_eq!(queued.first, if next { 3 } else { 4 });
            assert_eq!(queued.uris, uris(&tracks));
            assert!(queued.proof.is_some());
            assert!(queued.playing_queue);
            let mut expected = ORIGINAL.map(str::to_string).to_vec();
            expected.splice(if next { 2..2 } else { 3..3 }, uris(&tracks));
            assert_eq!(rig.queue(), expected);
            assert_eq!(
                control::playback(&lan, &rig.houses, &rig.coordinator).unwrap(),
                playback
            );
            assert_eq!(get_play_mode(&lan, rig.host).unwrap(), PlayMode::RepeatAll);
            assert_eq!(soap::get_volume(&lan, rig.host).unwrap(), 63);
            assert_eq!(rig.writes(start), vec!["AddURIToQueue"; tracks.len()]);
            let log = rig.sim.soap_log();
            let adds: Vec<_> = log[start..]
                .iter()
                .filter(|entry| entry.action == "AddURIToQueue")
                .collect();
            for (index, call) in adds.iter().enumerate() {
                let track = &tracks[if next {
                    tracks.len() - index - 1
                } else {
                    index
                }];
                assert_eq!(
                    call.args
                        .iter()
                        .find(|(name, _)| name == "EnqueuedURI")
                        .unwrap()
                        .1,
                    *track.uri.as_ref().unwrap()
                );
                assert_eq!(
                    call.args
                        .iter()
                        .find(|(name, _)| name == "EnqueuedURIMetaData")
                        .unwrap()
                        .1,
                    track.metadata
                );
            }
            assert!(
                log[start..].iter().all(|entry| !entry
                    .args
                    .iter()
                    .any(|(name, value)| name == "ObjectID" && value == "FV:2")),
                "native tracks never need account or renderer learning"
            );
        }
    }
}

#[test]
fn native_play_stages_every_occurrence_before_replacing_the_old_queue() {
    for generation in [1, 2] {
        let rig = Rig::new(generation);
        let lan = rig.sim.lan();
        let (destination, tracks) = rig.selection();
        let start = rig.sim.soap_log().len();
        rig.replace(&lan, &tracks, &destination).unwrap();
        assert_eq!(rig.queue(), uris(&tracks));
        assert_eq!(soap::get_position_info(&lan, rig.host).unwrap().track, 1);
        assert_eq!(
            soap::get_transport_info(&lan, rig.host).unwrap().state,
            TransportState::Playing
        );
        assert_eq!(get_play_mode(&lan, rig.host).unwrap(), PlayMode::Normal);
        assert_eq!(soap::get_volume(&lan, rig.host).unwrap(), 63);
        let writes = rig.writes(start);
        assert_eq!(&writes[..tracks.len()], vec!["AddURIToQueue"; tracks.len()]);
        assert_eq!(
            writes
                .iter()
                .filter(|action| *action == "RemoveTrackRangeFromQueue")
                .count(),
            1
        );
        assert_eq!(writes.iter().filter(|action| *action == "Play").count(), 1);
        assert!(
            !writes
                .iter()
                .any(|action| action == "RemoveAllTracksFromQueue")
        );
    }
}

#[test]
fn changed_queue_source_group_or_boot_refuses_both_batch_actions_before_writes() {
    for generation in [1, 2] {
        for change in 0..4 {
            let rig = Rig::new(generation);
            let lan = rig.sim.lan();
            let (destination, tracks) = rig.selection();
            match change {
                0 => {
                    soap::add_uri_to_queue(&lan, rig.host, EXTERNAL, "", false).unwrap();
                }
                1 => soap::set_av_transport_uri(&lan, rig.host, EXTERNAL, "").unwrap(),
                2 => soap::join_group(&lan, rig.sim.player("Peer").unwrap().ip, &rig.coordinator)
                    .unwrap(),
                _ => rig.sim.reboot("Lead", Duration::ZERO).unwrap(),
            }
            let before = rig.queue();
            let start = rig.sim.soap_log().len();
            assert!(rig.enqueue(&lan, &tracks, true, &destination).is_err());
            assert!(rig.replace(&lan, &tracks, &destination).is_err());
            assert_eq!(rig.queue(), before);
            assert!(rig.writes(start).is_empty());
        }
    }
}

#[derive(Clone, Copy)]
enum Failure {
    LostReply,
    RefusedFirst,
    RefusedSecond,
    NotApplied,
    UnreadableAfterAdd,
}

struct Faulty {
    lan: SimLan,
    failure: Failure,
    adds: Cell<usize>,
}

impl Transport for Faulty {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        let lost = || ProtoError::Network {
            target: host.to_string(),
            detail: "injected native batch interruption".into(),
        };
        if name == "Browse"
            && self.adds.get() > 0
            && matches!(self.failure, Failure::UnreadableAfterAdd)
        {
            return Err(lost());
        }
        if name == "AddURIToQueue" {
            self.adds.set(self.adds.get() + 1);
            if (matches!(self.failure, Failure::RefusedFirst) && self.adds.get() == 1)
                || (matches!(self.failure, Failure::RefusedSecond) && self.adds.get() == 2)
            {
                return Err(ProtoError::SoapFault {
                    code: 800,
                    reason: "injected native track refusal".into(),
                });
            }
            if matches!(self.failure, Failure::NotApplied) {
                return Err(lost());
            }
        }
        let response = self.lan.soap_post(host, path, action, body)?;
        if name == "AddURIToQueue" && matches!(self.failure, Failure::LostReply) {
            Err(lost())
        } else {
            Ok(response)
        }
    }
}

#[test]
fn lost_replies_are_proven_without_replay_and_partial_or_ambiguous_batches_stop() {
    for generation in [1, 2] {
        for failure in [
            Failure::LostReply,
            Failure::RefusedFirst,
            Failure::RefusedSecond,
            Failure::NotApplied,
            Failure::UnreadableAfterAdd,
        ] {
            let rig = Rig::new(generation);
            let (destination, tracks) = rig.selection();
            let transport = Faulty {
                lan: rig.sim.lan(),
                failure,
                adds: Cell::new(0),
            };
            let start = rig.sim.soap_log().len();
            let result = rig.enqueue(&transport, &tracks, false, &destination);
            let added = match failure {
                Failure::LostReply => {
                    let queued = result.unwrap();
                    assert_eq!(queued.uris, uris(&tracks));
                    assert!(queued.proof.is_some());
                    assert_eq!(transport.adds.get(), tracks.len());
                    tracks.len()
                }
                Failure::RefusedFirst => {
                    assert!(matches!(
                        result,
                        Err(CoreError::Proto(ProtoError::SoapFault { .. }))
                    ));
                    assert_eq!(transport.adds.get(), 1);
                    0
                }
                Failure::RefusedSecond => {
                    assert!(matches!(result, Err(CoreError::Uncertain(_))));
                    assert_eq!(transport.adds.get(), 2);
                    1
                }
                Failure::NotApplied | Failure::UnreadableAfterAdd => {
                    assert!(matches!(result, Err(CoreError::Uncertain(_))));
                    assert_eq!(transport.adds.get(), 1);
                    usize::from(matches!(failure, Failure::UnreadableAfterAdd))
                }
            };
            let mut expected = ORIGINAL.map(str::to_string).to_vec();
            expected.extend(uris(&tracks[..added]));
            assert_eq!(rig.queue(), expected);
            assert_eq!(rig.writes(start), vec!["AddURIToQueue"; added]);
        }
    }
}

#[test]
fn partial_staging_preserves_the_old_queue_and_a_failed_start_cannot_replay() {
    for generation in [1, 2] {
        let rig = Rig::new(generation);
        let (destination, tracks) = rig.selection();
        let transport = Faulty {
            lan: rig.sim.lan(),
            failure: Failure::RefusedSecond,
            adds: Cell::new(0),
        };
        let start = rig.sim.soap_log().len();
        assert!(matches!(
            rig.replace(&transport, &tracks, &destination),
            Err(CoreError::Uncertain(_))
        ));
        assert_eq!(rig.queue()[..ORIGINAL.len()], ORIGINAL);
        assert_eq!(rig.queue().len(), ORIGINAL.len() + 1);
        assert_eq!(rig.writes(start), ["AddURIToQueue"]);

        let rig = Rig::new(generation);
        let (destination, tracks) = rig.selection();
        rig.sim.upnp_fault("Lead", "Play", 701).unwrap();
        let start = rig.sim.soap_log().len();
        assert!(matches!(
            rig.replace(&rig.sim.lan(), &tracks, &destination),
            Err(CoreError::Uncertain(_))
        ));
        assert_eq!(rig.queue(), uris(&tracks));
        let writes = rig.writes(start);
        assert_eq!(
            writes
                .iter()
                .filter(|action| *action == "AddURIToQueue")
                .count(),
            tracks.len()
        );
        assert_eq!(writes.iter().filter(|action| *action == "Play").count(), 1);
        assert!(
            !writes
                .iter()
                .any(|action| action == "RemoveAllTracksFromQueue")
        );
    }
}

struct NoIo;
impl Transport for NoIo {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("invalid native selections must be rejected before any I/O")
    }
}

#[test]
fn the_complete_batch_is_validated_before_network_reads_or_mutation() {
    let rig = Rig::new(1);
    let (destination, tracks) = rig.selection();
    let mut wrong_resource = tracks.clone();
    wrong_resource.last_mut().unwrap().uri = Some(EXTERNAL.into());
    let mut stream = tracks.clone();
    let last = stream.last_mut().unwrap();
    last.uri = Some("x-sonosapi-stream:radio?sid=254".into());
    last.metadata = last
        .metadata
        .replace(LOCAL, "x-sonosapi-stream:radio?sid=254");
    let mut missing_metadata = tracks.clone();
    missing_metadata.last_mut().unwrap().metadata.clear();
    let mut container = tracks.clone();
    container.last_mut().unwrap().metadata = container.last().unwrap().metadata.replace(
        "object.item.audioItem.musicTrack",
        "object.container.playlistContainer",
    );
    for invalid in [
        Vec::new(),
        vec![tracks[0].clone(); 11],
        wrong_resource,
        stream,
        missing_metadata,
        container,
    ] {
        assert!(rig.enqueue(&NoIo, &invalid, false, &destination).is_err());
        assert!(rig.replace(&NoIo, &invalid, &destination).is_err());
    }
}
