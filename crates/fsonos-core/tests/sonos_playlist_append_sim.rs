//! Compose saved playlists through real S1/S2 sockets, independently of the
//! playing queue. A failed multi-track append never repeats its earlier work.

use fsonos_core::favorites::{Favorite, FavoriteKind};
use fsonos_core::playlists::{PlaylistContents, PlaylistTrackChange};
use fsonos_core::{CoreError, HouseholdState, control, favorites, playlists};
use fsonos_proto::content::{self, BrowseFlag, BrowseSnapshot, QUEUE};
use fsonos_proto::control as soap;
use fsonos_proto::didl::{DidlObject, parse_didl};
use fsonos_proto::play_mode::{get_play_mode, set_play_mode};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport, queue};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{HouseholdId, PlayMode, PlayerId};
use std::cell::Cell;
use std::net::IpAddr;

const SOURCE: &str = "SQ:7";
const DESTINATION: &str = "SQ:1";
const ACTION: &str = "AddURIToSavedQueue";
const RADIO: &str = "x-rincon-mp3radio://example.invalid/owner-radio.mp3";

struct Fixture {
    sim: SimHandle,
    houses: Vec<HouseholdState>,
    coordinator: PlayerId,
    host: IpAddr,
}

impl Fixture {
    fn new(s2: bool) -> Self {
        let model = if s2 {
            SimModel::One
        } else {
            SimModel::Play5Gen1
        };
        let players = [
            SimPlayerSpec::new("Alpha", model),
            SimPlayerSpec::new("Beta", model),
        ];
        let builder = SimHousehold::builder();
        let sim = if s2 {
            builder.s2(players)
        } else {
            builder.s1(players)
        }
        .spawn()
        .unwrap();
        let host = sim.player("Alpha").unwrap().ip;
        let coordinator = PlayerId(sim.player("Alpha").unwrap().uuid.clone());
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&sim.lan(), host).unwrap());
        house.id = Some(HouseholdId(sim.player("Alpha").unwrap().household.clone()));
        let houses = vec![house];
        let source = playlists::list(&sim.lan(), &houses, &coordinator)
            .unwrap()
            .into_iter()
            .find(|playlist| playlist.id == SOURCE)
            .unwrap();
        favorites::play(&sim.lan(), &houses, &coordinator, &source).unwrap();
        soap::seek_track(&sim.lan(), host, 3).unwrap();
        soap::seek_position(&sim.lan(), host, 17).unwrap();
        set_play_mode(&sim.lan(), host, PlayMode::RepeatAll).unwrap();
        soap::set_volume(&sim.lan(), host, 28).unwrap();
        Self {
            sim,
            houses,
            coordinator,
            host,
        }
    }

    fn contents(&self, id: &str) -> PlaylistContents {
        playlists::contents(
            &self.sim.lan(),
            &self.houses,
            &self.coordinator,
            id,
            0,
            100,
            None,
        )
        .unwrap()
    }

    fn selected(&self) -> Vec<Favorite> {
        let source = self.contents(SOURCE);
        playlists::select_tracks(
            &self.sim.lan(),
            &self.houses,
            &self.coordinator,
            SOURCE,
            &[3, 1, 2],
            &source.revision,
        )
        .unwrap()
    }

    fn queue_revision(&self) -> String {
        let page = content::browse(
            &self.sim.lan(),
            self.host,
            QUEUE,
            BrowseFlag::DirectChildren,
            0,
            1,
        )
        .unwrap();
        playlists::queue_selection_revision(&self.coordinator, &page)
    }

    fn append<T: Transport + ?Sized>(
        &self,
        t: &T,
        revision: &str,
        tracks: &[Favorite],
    ) -> Result<PlaylistTrackChange, CoreError> {
        playlists::append_tracks(
            t,
            &self.houses,
            &self.coordinator,
            DESTINATION,
            revision,
            tracks,
        )
    }

    fn playback(&self) -> (BrowseSnapshot, control::Playback, PlayMode, u8, bool) {
        let lan = self.sim.lan();
        (
            content::browse_snapshot(&lan, self.host, QUEUE).unwrap(),
            control::playback(&lan, &self.houses, &self.coordinator).unwrap(),
            get_play_mode(&lan, self.host).unwrap(),
            soap::get_volume(&lan, self.host).unwrap(),
            soap::get_mute(&lan, self.host).unwrap(),
        )
    }

    fn writes_since(&self, start: usize) -> Vec<String> {
        self.sim.soap_log()[start..]
            .iter()
            .filter(|entry| entry.action != "Browse" && !entry.action.starts_with("Get"))
            .map(|entry| entry.action.clone())
            .collect()
    }
}

fn normalized(items: &[DidlObject]) -> Vec<DidlObject> {
    items
        .iter()
        .cloned()
        .map(|mut item| {
            item.id.clear();
            item.parent_id.clear();
            item
        })
        .collect()
}

fn retained(tracks: &[Favorite]) -> Vec<DidlObject> {
    tracks
        .iter()
        .map(|track| parse_didl(&track.metadata).unwrap().remove(0))
        .collect()
}

#[test]
fn selected_saved_and_retained_queue_tracks_append_in_order_without_interrupting_music() {
    for s2 in [false, true] {
        for from_queue in [false, true] {
            let f = Fixture::new(s2);
            let lan = f.sim.lan();
            let source = f.contents(SOURCE);
            let before = f.contents(DESTINATION);
            if from_queue {
                soap::set_av_transport_uri(&lan, f.host, RADIO, "").unwrap();
                soap::play(&lan, f.host).unwrap();
            }
            let playing = f.playback();
            let start = f.sim.soap_log().len();
            let tracks = if from_queue {
                playlists::select_queue_tracks(
                    &lan,
                    &f.houses,
                    &f.coordinator,
                    &[3, 1, 2, 3],
                    &f.queue_revision(),
                )
                .unwrap()
            } else {
                playlists::select_tracks(
                    &lan,
                    &f.houses,
                    &f.coordinator,
                    SOURCE,
                    &[3, 1, 2, 3],
                    &source.revision,
                )
                .unwrap()
            };
            assert!(f.writes_since(start).is_empty(), "selection is read-only");
            assert_eq!(tracks[0], tracks[3], "a repeated occurrence stays repeated");
            let start = f.sim.soap_log().len();
            let result = f.append(&lan, &before.revision, &tracks).unwrap();
            let after = f.contents(DESTINATION);
            let mut expected = before.items.clone();
            expected.extend(retained(&tracks));
            assert_eq!(normalized(&after.items), normalized(&expected));
            assert_eq!(result.playlist, before.playlist);
            assert_eq!(result.revision, after.revision);
            assert_ne!(result.revision, before.revision);
            assert_eq!(
                (result.previous_total, result.total, result.changed),
                (before.total, before.total + 4, true)
            );
            assert_eq!(f.writes_since(start), [ACTION; 4]);
            let unchanged_source = f.contents(SOURCE);
            assert_eq!(unchanged_source.playlist, source.playlist);
            assert_eq!(
                unchanged_source.items, source.items,
                "the source playlist is untouched"
            );
            assert_eq!(f.playback(), playing);
        }
    }
}

#[test]
fn empty_targets_and_copying_a_playlist_into_itself_keep_the_same_native_identity() {
    for s2 in [false, true] {
        let f = Fixture::new(s2);
        let lan = f.sim.lan();
        let playing = f.playback();
        let tracks = f.selected();
        let before = f.contents(DESTINATION);
        let empty = playlists::remove_tracks(
            &lan,
            &f.houses,
            &f.coordinator,
            DESTINATION,
            &before.revision,
            1,
            before.total,
        )
        .unwrap();
        let appended = f.append(&lan, &empty.revision, &tracks).unwrap();
        assert_eq!(appended.playlist, before.playlist);
        assert_eq!((appended.previous_total, appended.total), (0, 3));
        assert_eq!(
            normalized(&f.contents(DESTINATION).items),
            normalized(&retained(&tracks))
        );

        let original = f.contents(SOURCE);
        let source = playlists::select_tracks(
            &lan,
            &f.houses,
            &f.coordinator,
            SOURCE,
            &[3, 1, 3],
            &original.revision,
        )
        .unwrap();
        let result = playlists::append_tracks(
            &lan,
            &f.houses,
            &f.coordinator,
            SOURCE,
            &original.revision,
            &source,
        )
        .unwrap();
        let mut expected = original.items.clone();
        expected.extend(retained(&source));
        assert_eq!(normalized(&f.contents(SOURCE).items), normalized(&expected));
        assert_eq!(result.playlist, original.playlist);
        assert_eq!(
            (result.previous_total, result.total),
            (original.total, original.total + 3)
        );
        assert_eq!(f.playback(), playing);
    }
}

struct ShortPages {
    lan: SimLan,
    browses: Cell<usize>,
}

impl Transport for ShortPages {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if action.contains("#Browse") {
            self.browses.set(self.browses.get() + 1);
            let start = body.find("<RequestedCount>").unwrap() + "<RequestedCount>".len();
            let end = body[start..].find("</RequestedCount>").unwrap() + start;
            let mut body = body.to_owned();
            body.replace_range(start..end, "1");
            return self.lan.soap_post(host, path, action, &body);
        }
        self.lan.soap_post(host, path, action, body)
    }
}

#[test]
fn queue_selection_continues_short_pages_and_preserves_duplicate_occurrence_order() {
    for s2 in [false, true] {
        let f = Fixture::new(s2);
        let revision = f.queue_revision();
        let t = ShortPages {
            lan: f.sim.lan(),
            browses: Cell::new(0),
        };
        let start = f.sim.soap_log().len();
        let selected =
            playlists::select_queue_tracks(&t, &f.houses, &f.coordinator, &[3, 1, 2, 3], &revision)
                .unwrap();
        assert_eq!(
            selected
                .iter()
                .map(|track| track.id.as_str())
                .collect::<Vec<_>>(),
            ["Q:0/3", "Q:0/1", "Q:0/2", "Q:0/3"]
        );
        assert_eq!(t.browses.get(), 6, "three short pages, each confirmed once");
        assert!(f.writes_since(start).is_empty());
    }
}

#[test]
fn changed_source_or_destination_revisions_never_append_selected_tracks() {
    for s2 in [false, true] {
        let f = Fixture::new(s2);
        let lan = f.sim.lan();
        let before = f.contents(DESTINATION);
        let selected = f.selected();
        let revision = f.queue_revision();
        soap::add_uri_to_queue(
            &lan,
            f.host,
            "x-file-cifs://example.invalid/added.flac",
            "",
            false,
        )
        .unwrap();
        let start = f.sim.soap_log().len();
        assert!(matches!(
            playlists::select_queue_tracks(&lan, &f.houses, &f.coordinator, &[1], &revision),
            Err(CoreError::QueueChanged(_))
        ));
        assert!(f.writes_since(start).is_empty());

        content::rename_object(
            &lan,
            f.host,
            DESTINATION,
            &before.playlist.title,
            "Owner's new title",
        )
        .unwrap();
        let start = f.sim.soap_log().len();
        assert!(matches!(
            f.append(&lan, &before.revision, &selected),
            Err(CoreError::PlaylistChanged(_))
        ));
        assert!(f.writes_since(start).is_empty());
    }
}

#[derive(Clone, Copy, Debug)]
enum Fault {
    RefuseFirst,
    StaleFirst,
    RefuseSecond,
    LostSecond,
    MalformedSecond,
    WrongCount,
    ChangedGroup,
}

struct Replies<'a> {
    fixture: &'a Fixture,
    fault: Fault,
    attempts: Cell<u32>,
    after_groups: Cell<u32>,
}

impl Transport for Replies<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if action.contains(&format!("#{ACTION}")) {
            let attempt = self.attempts.get() + 1;
            self.attempts.set(attempt);
            let refusal = match self.fault {
                Fault::RefuseFirst if attempt == 1 => Some(701),
                Fault::StaleFirst if attempt == 1 => Some(queue::STALE_QUEUE_FAULT),
                Fault::RefuseSecond if attempt == 2 => Some(701),
                _ => None,
            };
            if let Some(code) = refusal {
                return Err(ProtoError::SoapFault {
                    code,
                    reason: "injected saved append refusal".into(),
                });
            }
            let reply = self.fixture.sim.lan().soap_post(host, path, action, body)?;
            match self.fault {
                Fault::LostSecond if attempt == 2 => {
                    return Err(ProtoError::Network {
                        target: host.to_string(),
                        detail: "saved append reply lost".into(),
                    });
                }
                Fault::MalformedSecond if attempt == 2 => return Ok("<broken>".into()),
                Fault::WrongCount => {
                    let altered = reply.replace(
                        "<NumTracksAdded>1</NumTracksAdded>",
                        "<NumTracksAdded>2</NumTracksAdded>",
                    );
                    assert_ne!(reply, altered);
                    return Ok(altered);
                }
                _ => {}
            }
            return Ok(reply);
        }
        let reply = self.fixture.sim.lan().soap_post(host, path, action, body)?;
        if matches!(self.fault, Fault::ChangedGroup)
            && self.attempts.get() == 1
            && action.contains("#GetZoneGroupState")
        {
            let read = self.after_groups.get() + 1;
            self.after_groups.set(read);
            if read == 2 {
                // The first append's complete read has captured its final
                // group response. The next write must acquire a new proof.
                let peer = self.fixture.sim.player("Beta").unwrap().ip;
                soap::join_group(&self.fixture.sim.lan(), peer, &self.fixture.coordinator)?;
            }
        }
        Ok(reply)
    }
}

#[test]
fn failed_batches_report_confirmed_progress_and_never_replay_or_continue() {
    for s2 in [false, true] {
        for fault in [
            Fault::RefuseFirst,
            Fault::StaleFirst,
            Fault::RefuseSecond,
            Fault::LostSecond,
            Fault::MalformedSecond,
            Fault::WrongCount,
            Fault::ChangedGroup,
        ] {
            let f = Fixture::new(s2);
            let before = f.contents(DESTINATION);
            let tracks = f.selected();
            let playing = f.playback();
            let t = Replies {
                fixture: &f,
                fault,
                attempts: Cell::new(0),
                after_groups: Cell::new(0),
            };
            let error = f.append(&t, &before.revision, &tracks).unwrap_err();
            match fault {
                Fault::RefuseFirst => assert!(matches!(
                    error,
                    CoreError::Proto(ProtoError::SoapFault { code: 701, .. })
                )),
                Fault::StaleFirst => assert!(matches!(error, CoreError::PlaylistChanged(_))),
                _ => {
                    let confirmed = u32::from(!matches!(fault, Fault::WrongCount));
                    let CoreError::Uncertain(detail) = error else {
                        panic!("{fault:?}: {error}");
                    };
                    assert!(
                        detail.contains(&format!("{confirmed} of 3 additions confirmed")),
                        "{fault:?}: {detail}"
                    );
                }
            }
            let attempts = if matches!(
                fault,
                Fault::RefuseSecond | Fault::LostSecond | Fault::MalformedSecond
            ) {
                2
            } else {
                1
            };
            assert_eq!(
                t.attempts.get(),
                attempts,
                "{fault:?}: never replay or continue"
            );
            if !matches!(fault, Fault::ChangedGroup) {
                let applied = match fault {
                    Fault::RefuseFirst | Fault::StaleFirst => 0,
                    Fault::RefuseSecond | Fault::WrongCount => 1,
                    Fault::LostSecond | Fault::MalformedSecond => 2,
                    Fault::ChangedGroup => unreachable!(),
                };
                assert_eq!(
                    f.contents(DESTINATION).total,
                    before.total + applied,
                    "{fault:?}"
                );
                assert_eq!(f.playback(), playing, "{fault:?}");
            }
        }
    }
}

struct NoRequests;
impl Transport for NoRequests {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("invalid selection must fail before speaker I/O");
    }
}

#[test]
fn invalid_native_selections_fail_before_discovery_or_saved_writes() {
    let f = Fixture::new(false);
    let coordinator = &f.coordinator;
    for (positions, revision) in [
        (vec![], "q1"),
        (vec![0], "q1"),
        (vec![1; 11], "q1"),
        (vec![1], ""),
    ] {
        assert!(
            playlists::select_queue_tracks(
                &NoRequests,
                &f.houses,
                coordinator,
                &positions,
                revision
            )
            .is_err()
        );
    }
    let mut selected = Favorite {
        id: "Q:0/1".into(), title: "One".into(), kind: FavoriteKind::Track,
        uri: Some("x-file-cifs://example.invalid/one.flac".into()),
        metadata: "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\"><item id=\"Q:0/1\" parentID=\"Q:0\" restricted=\"true\"><dc:title>One</dc:title><upnp:class>object.item.audioItem.musicTrack</upnp:class><res>x-file-cifs://example.invalid/one.flac</res></item></DIDL-Lite>".into(),
        description: None, art_uri: None,
    };
    selected.metadata =
        fsonos_proto::didl::item_metadata(&parse_didl(&selected.metadata).unwrap()[0]);
    for (id, revision, tracks) in [
        ("Q:0", "revision", vec![selected.clone()]),
        (DESTINATION, "", vec![selected.clone()]),
        (DESTINATION, "revision", vec![]),
        (DESTINATION, "revision", vec![selected.clone(); 11]),
    ] {
        assert!(
            playlists::append_tracks(&NoRequests, &f.houses, coordinator, id, revision, &tracks)
                .is_err()
        );
    }
    selected.uri = Some("x-file-cifs://example.invalid/substituted.flac".into());
    assert!(
        playlists::append_tracks(
            &NoRequests,
            &f.houses,
            coordinator,
            DESTINATION,
            "revision",
            &[selected.clone()]
        )
        .is_err()
    );
    selected.metadata = " ".repeat(playlists::MAX_PLAYLIST_EDIT_BYTES + 1);
    assert!(
        playlists::append_tracks(
            &NoRequests,
            &f.houses,
            coordinator,
            DESTINATION,
            "revision",
            &[selected]
        )
        .is_err()
    );
}

const SPOTIFY_SELECTION: [(&str, &str); 4] = [
    ("spotify:track:0000000000000000000701", "Song & friends"),
    ("spotify:episode:0000000000000000000702", "Episode <two>"),
    ("spotify:track:0000000000000000000701", "Song & friends"),
    ("spotify:track:0000000000000000000703", "Découverte"),
];

#[test]
fn explicit_spotify_tracks_and_episodes_compose_saved_playlists_without_touching_playback() {
    use fsonos_proto::didl::{spotify_bare_uri, spotify_track_uri, spotify_uri_from_renderer_uri};

    for s2 in [false, true] {
        for radio in [false, true] {
            let f = Fixture::new(s2);
            let lan = f.sim.lan();
            if radio {
                soap::set_av_transport_uri(&lan, f.host, RADIO, "").unwrap();
                soap::play(&lan, f.host).unwrap();
            }
            let before = f.contents(DESTINATION);
            let playing = f.playback();
            let params = control::spotify_params(&lan, &f.houses, &f.coordinator)
                .unwrap()
                .unwrap();
            let start = f.sim.soap_log().len();
            let tracks = playlists::prepare_spotify_tracks(
                &lan,
                &f.houses,
                &f.coordinator,
                &SPOTIFY_SELECTION,
            )
            .unwrap();
            assert_eq!(tracks.len(), SPOTIFY_SELECTION.len());
            assert_eq!(tracks[0], tracks[2], "repeated tracks stay repeated");
            let preparation = f.sim.soap_log();
            assert!(
                preparation[start..].iter().all(|entry| {
                    entry.action == "GetZoneGroupState"
                        || (entry.action == "Browse"
                            && entry
                                .args
                                .iter()
                                .any(|(key, value)| key == "ObjectID" && value == "FV:2"))
                }),
                "preparation reads only topology and household favorites"
            );
            assert!(f.writes_since(start).is_empty());
            for (track, (source, title)) in tracks.iter().zip(SPOTIFY_SELECTION) {
                let item = parse_didl(&track.metadata).unwrap().remove(0);
                let uri = track.uri.as_deref().unwrap();
                assert_eq!(track.kind, FavoriteKind::Track);
                assert_eq!(track.title, title);
                assert_eq!(item.res.as_ref().unwrap().uri, uri);
                assert_eq!(spotify_uri_from_renderer_uri(uri).as_deref(), Some(source));
                if source.starts_with("spotify:episode:") {
                    assert_eq!(uri, spotify_bare_uri(source, params.sid));
                    assert!(
                        item.desc.is_none(),
                        "episodes have no track account descriptor"
                    );
                    assert!(!uri.contains("flags=") && !uri.contains("sn="));
                } else {
                    assert_eq!(uri, spotify_track_uri(source, &params));
                    assert_eq!(item.desc.unwrap().value, params.cdudn);
                }
            }
            let start = f.sim.soap_log().len();
            let result = f.append(&lan, &before.revision, &tracks).unwrap();
            let after = f.contents(DESTINATION);
            assert_eq!(
                &after.items[..before.items.len()],
                before.items.as_slice(),
                "the previously saved occurrences remain exact"
            );
            let additions = &after.items[before.items.len()..];
            assert_eq!(additions.len(), tracks.len());
            for (actual, supplied) in additions.iter().zip(retained(&tracks)) {
                assert_eq!(actual.parent_id, DESTINATION);
                assert_eq!(actual.title, supplied.title);
                assert_eq!(actual.class, supplied.class);
                assert_eq!(actual.desc, supplied.desc, "the service account is retained");
                let resource = actual.res.as_ref().unwrap();
                assert_eq!(resource.uri, supplied.res.as_ref().unwrap().uri);
                assert!(resource.protocol_info.is_some(), "Sonos enriches the resource");
                assert!(resource.duration.is_some(), "Sonos supplies its duration");
                assert!(actual.album_art_uri.is_some(), "Sonos supplies catalog art");
            }
            assert_eq!(result.playlist, before.playlist);
            assert_eq!(result.revision, after.revision);
            assert_eq!(result.total, before.total + 4);
            assert_eq!(f.writes_since(start), [ACTION; 4]);
            assert_eq!(f.playback(), playing);
        }
    }
}

#[test]
fn malformed_spotify_selections_fail_before_household_or_playlist_reads() {
    let id = PlayerId("synthetic-player".into());
    let oversized = "x".repeat(playlists::MAX_PLAYLIST_EDIT_BYTES + 1);
    for tracks in [
        vec![],
        vec![SPOTIFY_SELECTION[0]; 11],
        vec![("spotify:album:0000000000000000000701", "Container")],
        vec![("spotify:artist:0000000000000000000701", "Unresolved artist")],
        vec![("spotify:show:0000000000000000000701", "Unresolved show")],
        vec![("spotify:track:short", "Short")],
        vec![("spotify:episode:00000000000000000007?1", "Invalid")],
        vec![(SPOTIFY_SELECTION[0].0, "Invalid\0title")],
        vec![(SPOTIFY_SELECTION[0].0, oversized.as_str())],
    ] {
        assert!(playlists::prepare_spotify_tracks(&NoRequests, &[], &id, &tracks).is_err());
    }
}

struct NoLinkedSpotify(SimLan);

impl Transport for NoLinkedSpotify {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if action.contains("#Browse") && body.contains("<ObjectID>FV:2</ObjectID>") {
            return Ok(fsonos_proto::soap::envelope(
                &fsonos_proto::soap::CONTENT_DIRECTORY,
                "BrowseResponse",
                "<Result>&lt;DIDL-Lite/&gt;</Result><NumberReturned>0</NumberReturned>\
                 <TotalMatches>0</TotalMatches><UpdateID>1</UpdateID>",
            ));
        }
        self.0.soap_post(host, path, action, body)
    }
}

#[test]
fn missing_household_renderer_stops_saved_playlist_preparation_without_mutation() {
    for s2 in [false, true] {
        let f = Fixture::new(s2);
        let playing = f.playback();
        let before = f.contents(DESTINATION);
        let start = f.sim.soap_log().len();
        assert!(matches!(
            playlists::prepare_spotify_tracks(
                &NoLinkedSpotify(f.sim.lan()),
                &f.houses,
                &f.coordinator,
                &SPOTIFY_SELECTION,
            ),
            Err(CoreError::NoSpotifyFavorite)
        ));
        assert!(f.writes_since(start).is_empty());
        assert_eq!(f.contents(DESTINATION).items, before.items);
        assert_eq!(f.playback(), playing);
    }
}

#[test]
fn prepared_spotify_selection_cannot_append_to_a_renamed_saved_playlist() {
    for s2 in [false, true] {
        let f = Fixture::new(s2);
        let lan = f.sim.lan();
        let before = f.contents(DESTINATION);
        let playing = f.playback();
        let tracks =
            playlists::prepare_spotify_tracks(&lan, &f.houses, &f.coordinator, &SPOTIFY_SELECTION)
                .unwrap();
        content::rename_object(
            &lan,
            f.host,
            DESTINATION,
            &before.playlist.title,
            "Another saved title",
        )
        .unwrap();
        let start = f.sim.soap_log().len();
        assert!(matches!(
            f.append(&lan, &before.revision, &tracks),
            Err(CoreError::PlaylistChanged(_))
        ));
        assert!(f.writes_since(start).is_empty());
        assert_eq!(f.contents(DESTINATION).items, before.items);
        assert_eq!(f.playback(), playing);
    }
}
