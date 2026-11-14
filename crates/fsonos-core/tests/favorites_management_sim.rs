//! My Sonos authoring through real localhost S1/S2 SOAP. Catalog writes never
//! change the renderer's source, ordered queue, playhead, mode or levels.

use fsonos_core::favorites::{self, FavoriteKind};
use fsonos_core::{CoreError, HouseholdState, control};
use fsonos_proto::content::{self, BrowseSnapshot, QUEUE};
use fsonos_proto::control as soap;
use fsonos_proto::didl::parse_didl;
use fsonos_proto::play_mode::{get_play_mode, set_play_mode};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{HouseholdId, PlayMode, PlayerId};
use std::cell::Cell;
use std::net::IpAddr;
use std::time::Duration;

struct Fixture {
    sim: SimHandle,
    houses: Vec<HouseholdState>,
    coordinator: PlayerId,
    host: IpAddr,
}

impl Fixture {
    fn new(generation: u8) -> Self {
        let model = if generation == 1 {
            SimModel::Play5Gen1
        } else {
            SimModel::One
        };
        let players = [
            SimPlayerSpec::new("Alpha", model),
            SimPlayerSpec::new("Beta", model),
        ];
        let builder = SimHousehold::builder();
        let sim = if generation == 1 {
            builder.s1(players)
        } else {
            builder.s2(players)
        }
        .spawn()
        .unwrap();
        let host = sim.player("Alpha").unwrap().ip;
        let coordinator = PlayerId(sim.player("Alpha").unwrap().uuid.clone());
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&sim.lan(), host).unwrap());
        house.id = Some(HouseholdId(sim.player("Alpha").unwrap().household.clone()));
        Self {
            sim,
            houses: vec![house],
            coordinator,
            host,
        }
    }

    fn queue(&self) {
        let lan = self.sim.lan();
        let catalog = favorites::list(&lan, &self.houses, &self.coordinator).unwrap();
        let track = catalog
            .iter()
            .find(|favorite| favorite.kind == FavoriteKind::Track)
            .unwrap();
        // Repeated identical resources are separate selected occurrences.
        for _ in 0..3 {
            soap::add_uri_to_queue(
                &lan,
                self.host,
                track.uri.as_deref().unwrap(),
                &track.metadata,
                false,
            )
            .unwrap();
        }
        control::play_queue_from(&lan, &self.houses, &self.coordinator, 2).unwrap();
        soap::seek_position(&lan, self.host, 17).unwrap();
        set_play_mode(&lan, self.host, PlayMode::RepeatAll).unwrap();
        soap::set_volume(&lan, self.host, 28).unwrap();
        // Saving a source already in FV:2 is a native duplicate refusal. Keep
        // the loaded/queued source but remove its old favorite for creation.
        content::destroy_object(&lan, self.host, &track.id).unwrap();
    }

    fn preview(&self) -> favorites::ObservedFavorite {
        favorites::current(&self.sim.lan(), &self.houses, &self.coordinator).unwrap()
    }

    fn writes_since(&self, start: usize) -> Vec<String> {
        self.sim.soap_log()[start..]
            .iter()
            .filter(|entry| entry.action != "Browse" && !entry.action.starts_with("Get"))
            .map(|entry| entry.action.clone())
            .collect()
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
}

#[test]
fn current_queue_track_is_saved_renamed_and_removed_without_renderer_changes() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        f.queue();
        let lan = f.sim.lan();
        let before = f.playback();
        let preview = f.preview();
        assert!(preview.favorite.id.is_empty());
        assert_eq!(preview.favorite.kind, FavoriteKind::Track);
        let start = f.sim.soap_log().len();
        let saved = favorites::save_current(
            &lan,
            &f.houses,
            &f.coordinator,
            &preview.revision,
            "  Dvořák & <friends>  ",
        )
        .unwrap();
        assert!(saved.changed);
        assert_eq!(saved.favorite.title, "Dvořák & <friends>");
        assert!(favorites::native_id(&saved.favorite.id));
        assert_eq!(saved.favorite.uri, preview.favorite.uri);
        assert_eq!(
            parse_didl(&saved.favorite.metadata).unwrap(),
            parse_didl(&fsonos_proto::didl::favorite_metadata(&preview.favorite.metadata).unwrap())
                .unwrap()
        );
        let native = parse_didl(&saved.favorite.metadata).unwrap().remove(0);
        assert!(!native.id.starts_with("Q:0/"));
        assert!(
            native.desc.is_some(),
            "saving from a queue retains the renderer's service descriptor"
        );
        assert_eq!(f.writes_since(start), ["CreateObject"]);
        assert_eq!(f.playback(), before);

        let selected =
            favorites::inspect(&lan, &f.houses, &f.coordinator, &saved.favorite.id).unwrap();
        let other = favorites::list(&lan, &f.houses, &f.coordinator)
            .unwrap()
            .into_iter()
            .find(|item| item.id != saved.favorite.id)
            .unwrap();
        let renamed = favorites::rename(
            &lan,
            &f.houses,
            &f.coordinator,
            &saved.favorite.id,
            &selected.revision,
            &other.title,
        )
        .unwrap();
        assert_eq!(renamed.favorite.id, saved.favorite.id);
        assert_eq!(renamed.favorite.uri, saved.favorite.uri);
        let selected =
            favorites::inspect(&lan, &f.houses, &f.coordinator, &saved.favorite.id).unwrap();
        let no_op = favorites::rename(
            &lan,
            &f.houses,
            &f.coordinator,
            &saved.favorite.id,
            &selected.revision,
            &other.title,
        )
        .unwrap();
        assert!(!no_op.changed);
        let removed = favorites::remove(
            &lan,
            &f.houses,
            &f.coordinator,
            &saved.favorite.id,
            &selected.revision,
        )
        .unwrap();
        assert_eq!(removed.favorite, renamed.favorite);
        assert_eq!(
            f.writes_since(start),
            ["CreateObject", "UpdateObject", "DestroyObject"]
        );
        assert_eq!(f.playback(), before);
        assert!(
            favorites::list(&lan, &f.houses, &f.coordinator)
                .unwrap()
                .contains(&other)
        );
    }
}

#[test]
fn radio_saves_the_station_source_while_retaining_its_queued_music() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        f.queue();
        let lan = f.sim.lan();
        let radio = favorites::list(&lan, &f.houses, &f.coordinator)
            .unwrap()
            .into_iter()
            .find(|item| item.kind == FavoriteKind::Stream)
            .unwrap();
        favorites::play(&lan, &f.houses, &f.coordinator, &radio).unwrap();
        content::destroy_object(&lan, f.host, &radio.id).unwrap();
        let before = f.playback();
        let preview = f.preview();
        assert_eq!(preview.favorite.uri, radio.uri);
        assert_eq!(preview.favorite.metadata, radio.metadata);
        assert_eq!(preview.favorite.kind, FavoriteKind::Stream);
        let start = f.sim.soap_log().len();
        let saved = favorites::save_current(
            &lan,
            &f.houses,
            &f.coordinator,
            &preview.revision,
            "My station",
        )
        .unwrap();
        assert_eq!(saved.favorite.uri, radio.uri);
        assert_eq!(saved.favorite.kind, FavoriteKind::Stream);
        assert_eq!(f.playback(), before);
        assert_eq!(f.writes_since(start), ["CreateObject"]);
    }
}

#[test]
fn a_favorite_token_ignores_elapsed_seconds_but_fences_identical_queue_occurrences() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        f.queue();
        let before = f.preview();
        f.sim.advance(Duration::from_secs(3));
        assert_eq!(f.preview().revision, before.revision);
        soap::seek_track(&f.sim.lan(), f.host, 3).unwrap();
        let after = f.preview();
        assert_eq!(after.favorite.uri, before.favorite.uri);
        assert_ne!(after.revision, before.revision);
        let start = f.sim.soap_log().len();
        assert!(matches!(
            favorites::save_current(
                &f.sim.lan(),
                &f.houses,
                &f.coordinator,
                &before.revision,
                "Old occurrence"
            ),
            Err(CoreError::FavoriteChanged(_))
        ));
        assert!(f.writes_since(start).is_empty());
    }
}

#[test]
fn catalog_edits_invalidate_rename_remove_and_apparent_noops_before_mutation() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let lan = f.sim.lan();
        let observed = favorites::inspect(&lan, &f.houses, &f.coordinator, "FV:2/0").unwrap();
        content::rename_object(
            &lan,
            f.host,
            "FV:2/0",
            &observed.favorite.title,
            "Changed externally",
        )
        .unwrap();
        let start = f.sim.soap_log().len();
        assert!(matches!(
            favorites::rename(
                &lan,
                &f.houses,
                &f.coordinator,
                "FV:2/0",
                &observed.revision,
                "Changed externally"
            ),
            Err(CoreError::FavoriteChanged(_))
        ));
        assert!(matches!(
            favorites::remove(
                &lan,
                &f.houses,
                &f.coordinator,
                "FV:2/0",
                &observed.revision
            ),
            Err(CoreError::FavoriteChanged(_))
        ));
        assert!(f.writes_since(start).is_empty());
    }
}

#[derive(Clone, Copy)]
enum ReplyFailure {
    Lost,
    Malformed,
    ExistingId,
    Readback,
}

struct FailedWrite {
    lan: SimLan,
    action: &'static str,
    failure: ReplyFailure,
    writes: Cell<usize>,
}

impl Transport for FailedWrite {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if self.writes.get() > 0
            && matches!(self.failure, ReplyFailure::Readback)
            && action.ends_with("#Browse\"")
        {
            return Err(ProtoError::Network {
                target: host.to_string(),
                detail: "catalog observation failed".into(),
            });
        }
        let response = self.lan.soap_post(host, path, action, body)?;
        if action.ends_with(&format!("#{}\"", self.action)) {
            self.writes.set(self.writes.get() + 1);
            return match self.failure {
                ReplyFailure::Lost => Err(ProtoError::Network {
                    target: host.to_string(),
                    detail: "reply lost after native mutation".into(),
                }),
                ReplyFailure::Malformed => Ok("not a SOAP response".into()),
                ReplyFailure::ExistingId => Ok(fsonos_proto::soap::envelope(
                    &fsonos_proto::soap::CONTENT_DIRECTORY,
                    "CreateObjectResponse",
                    "<ObjectID>FV:2/1</ObjectID><Result/>",
                )),
                ReplyFailure::Readback => Ok(response),
            };
        }
        Ok(response)
    }
}

#[test]
fn uncertain_catalog_mutations_are_sent_once_and_leave_playback_intact() {
    for generation in [1, 2] {
        for action in ["CreateObject", "UpdateObject", "DestroyObject"] {
            for failure in [
                ReplyFailure::Lost,
                ReplyFailure::Malformed,
                ReplyFailure::Readback,
            ] {
                let f = Fixture::new(generation);
                f.queue();
                let before = f.playback();
                let transport = FailedWrite {
                    lan: f.sim.lan(),
                    action,
                    failure,
                    writes: Cell::new(0),
                };
                let error = if action == "CreateObject" {
                    favorites::save_current(
                        &transport,
                        &f.houses,
                        &f.coordinator,
                        &f.preview().revision,
                        "Saved once",
                    )
                } else {
                    let observed =
                        favorites::inspect(&f.sim.lan(), &f.houses, &f.coordinator, "FV:2/1")
                            .unwrap();
                    if action == "UpdateObject" {
                        favorites::rename(
                            &transport,
                            &f.houses,
                            &f.coordinator,
                            "FV:2/1",
                            &observed.revision,
                            "Renamed once",
                        )
                    } else {
                        favorites::remove(
                            &transport,
                            &f.houses,
                            &f.coordinator,
                            "FV:2/1",
                            &observed.revision,
                        )
                    }
                }
                .unwrap_err();
                assert!(
                    matches!(error, CoreError::Uncertain(_)),
                    "{action}: {error}"
                );
                assert_eq!(transport.writes.get(), 1);
                assert_eq!(f.playback(), before);
            }
        }
    }
}

#[test]
fn a_creation_response_naming_an_existing_favorite_is_not_treated_as_success() {
    let f = Fixture::new(2);
    f.queue();
    let transport = FailedWrite {
        lan: f.sim.lan(),
        action: "CreateObject",
        failure: ReplyFailure::ExistingId,
        writes: Cell::new(0),
    };
    assert!(matches!(
        favorites::save_current(
            &transport,
            &f.houses,
            &f.coordinator,
            &f.preview().revision,
            "Unconfirmed"
        ),
        Err(CoreError::Uncertain(_))
    ));
    assert_eq!(transport.writes.get(), 1);
}

#[test]
fn duplicate_source_refusal_remains_definite_and_never_creates_another_entry() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        f.queue();
        let preview = f.preview();
        let lan = f.sim.lan();
        favorites::save_current(&lan, &f.houses, &f.coordinator, &preview.revision, "First")
            .unwrap();
        let catalog = favorites::list(&lan, &f.houses, &f.coordinator).unwrap();
        let start = f.sim.soap_log().len();
        let error = favorites::save_current(
            &lan,
            &f.houses,
            &f.coordinator,
            &f.preview().revision,
            "Second",
        )
        .unwrap_err();
        assert!(
            matches!(
                error,
                CoreError::Proto(ProtoError::SoapFault { code: 803, .. })
            ),
            "{error}"
        );
        assert_eq!(f.writes_since(start), ["CreateObject"]);
        assert_eq!(
            favorites::list(&lan, &f.houses, &f.coordinator).unwrap(),
            catalog
        );
    }
}

struct NoRequests;
impl Transport for NoRequests {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("invalid favorite request performed I/O");
    }
}

#[test]
fn malformed_identifiers_revisions_and_titles_fail_before_io() {
    let coordinator = PlayerId("unknown".into());
    for id in ["", "FV:2", "FV:2/", "FV:2/1/2", "SQ:1", "A station"] {
        assert!(favorites::inspect(&NoRequests, &[], &coordinator, id).is_err());
        assert!(favorites::remove(&NoRequests, &[], &coordinator, id, "observed").is_err());
    }
    for revision in ["".into(), "bad\nrevision".into(), "x".repeat(4097)] {
        assert!(
            favorites::save_current(&NoRequests, &[], &coordinator, &revision, "Title").is_err()
        );
        assert!(favorites::remove(&NoRequests, &[], &coordinator, "FV:2/1", &revision).is_err());
    }
    for title in ["".into(), "  ".into(), "bad\ntitle".into(), "é".repeat(512)] {
        assert!(
            favorites::save_current(&NoRequests, &[], &coordinator, "observed", &title).is_err()
        );
        assert!(
            favorites::rename(&NoRequests, &[], &coordinator, "FV:2/1", "observed", &title)
                .is_err()
        );
    }
}

#[test]
fn native_art_is_preserved_outside_resmd_without_losing_service_extensions() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let lan = f.sim.lan();
        let metadata = "<DIDL-Lite xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\" xmlns:r=\"urn:example\"><item id=\"local-track\" parentID=\"-1\" restricted=\"true\"><dc:title>Native recording</dc:title><upnp:class>object.item.audioItem.musicTrack</upnp:class><upnp:albumArtURI>https://example.invalid/art?a=1&amp;b=2</upnp:albumArtURI><r:custom>preserve exactly</r:custom></item></DIDL-Lite>";
        soap::set_av_transport_uri(
            &lan,
            f.host,
            "https://example.invalid/recording.flac",
            metadata,
        )
        .unwrap();
        soap::play(&lan, f.host).unwrap();
        let preview = f.preview();
        let saved = favorites::save_current(
            &lan,
            &f.houses,
            &f.coordinator,
            &preview.revision,
            "Native favorite",
        )
        .unwrap();
        assert_eq!(
            saved.favorite.art_uri.as_deref(),
            Some("https://example.invalid/art?a=1&b=2")
        );
        assert!(!saved.favorite.metadata.contains("albumArtURI"));
        assert!(
            saved
                .favorite
                .metadata
                .contains("<r:custom>preserve exactly</r:custom>")
        );
        assert_eq!(
            soap::get_media_info(&lan, f.host).unwrap().uri_metadata,
            metadata
        );
    }
}

struct ChangedMetadata {
    lan: SimLan,
    created: Cell<bool>,
}

impl Transport for ChangedMetadata {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let response = self.lan.soap_post(host, path, action, body)?;
        if action.ends_with("#CreateObject\"") {
            self.created.set(true);
        }
        if self.created.get() && action.ends_with("#Browse\"") {
            Ok(response.replace("preserve original extension", "lost original extension"))
        } else {
            Ok(response)
        }
    }
}

#[test]
fn queued_favorites_keep_original_native_item_ids_and_unmodeled_service_extensions() {
    for generation in [1, 2] {
        for changed_readback in [false, true] {
            let f = Fixture::new(generation);
            let lan = f.sim.lan();
            let metadata = "<DIDL-Lite xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\" xmlns:r=\"urn:example\"><item id=\"native-service-item\" parentID=\"native-library\" restricted=\"true\"><dc:title>Native queue track</dc:title><upnp:class>object.item.audioItem.musicTrack</upnp:class><r:custom>preserve original extension</r:custom><desc id=\"cdudn\" nameSpace=\"urn:example\">SYNTHETIC_SERVICE</desc></item></DIDL-Lite>";
            soap::add_uri_to_queue(
                &lan,
                f.host,
                "https://example.invalid/native-track.flac",
                metadata,
                false,
            )
            .unwrap();
            control::play_queue_from(&lan, &f.houses, &f.coordinator, 1).unwrap();
            let preview = f.preview();
            assert_eq!(
                preview.favorite.metadata, metadata,
                "capture must retain the same SOAP response's raw TrackMetaData"
            );
            let before = f.playback();
            let transport = ChangedMetadata {
                lan: f.sim.lan(),
                created: Cell::new(false),
            };
            let target: &dyn Transport = if changed_readback { &transport } else { &lan };
            let start = f.sim.soap_log().len();
            let result = favorites::save_current(
                target,
                &f.houses,
                &f.coordinator,
                &preview.revision,
                "Native selected track",
            );
            if changed_readback {
                assert!(
                    matches!(result, Err(CoreError::Uncertain(_))),
                    "a parsed subset cannot confirm truncated native metadata"
                );
            } else {
                let saved = result.unwrap();
                assert_eq!(saved.favorite.metadata, metadata);
                assert_eq!(
                    parse_didl(&saved.favorite.metadata).unwrap()[0].id,
                    "native-service-item"
                );
            }
            assert_eq!(f.writes_since(start), ["CreateObject"]);
            assert_eq!(f.playback(), before);
        }
    }
}

struct ChangeOccurrence {
    lan: SimLan,
    changed: Cell<bool>,
}

impl Transport for ChangeOccurrence {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if !self.changed.get()
            && action.ends_with("#Browse\"")
            && body.contains("<ObjectID>FV:2</ObjectID>")
        {
            self.changed.set(true);
            soap::seek_track(&self.lan, host, 3)?;
        }
        self.lan.soap_post(host, path, action, body)
    }
}

#[test]
fn a_track_change_during_catalog_preparation_is_not_saved_as_the_reviewed_song() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        f.queue();
        let preview = f.preview();
        let transport = ChangeOccurrence {
            lan: f.sim.lan(),
            changed: Cell::new(false),
        };
        let start = f.sim.soap_log().len();
        assert!(matches!(
            favorites::save_current(
                &transport,
                &f.houses,
                &f.coordinator,
                &preview.revision,
                "Review"
            ),
            Err(CoreError::FavoriteChanged(_))
        ));
        assert_eq!(
            f.writes_since(start),
            ["Seek"],
            "only the simulated other controller may write"
        );
    }
}

#[test]
fn group_and_reboot_changes_invalidate_a_current_source_review() {
    for generation in [1, 2] {
        for reboot in [false, true] {
            let f = Fixture::new(generation);
            f.queue();
            let preview = f.preview();
            if reboot {
                f.sim.reboot("Alpha", Duration::ZERO).unwrap();
            } else {
                soap::join_group(
                    &f.sim.lan(),
                    f.sim.player("Beta").unwrap().ip,
                    &f.coordinator,
                )
                .unwrap();
            }
            let start = f.sim.soap_log().len();
            assert!(
                favorites::save_current(
                    &f.sim.lan(),
                    &f.houses,
                    &f.coordinator,
                    &preview.revision,
                    "Stale group"
                )
                .is_err()
            );
            assert!(f.writes_since(start).is_empty());
        }
    }
}

#[test]
fn identical_native_ids_in_other_households_do_not_accept_each_others_revision() {
    let sim = SimHousehold::builder()
        .s1([SimPlayerSpec::new("Alpha", SimModel::Play5Gen1)])
        .s2([SimPlayerSpec::new("Beta", SimModel::One)])
        .spawn()
        .unwrap();
    let mut houses = Vec::new();
    let mut coordinators = Vec::new();
    for room in ["Alpha", "Beta"] {
        let player = sim.player(room).unwrap();
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&sim.lan(), player.ip).unwrap());
        house.id = Some(HouseholdId(player.household.clone()));
        houses.push(house);
        coordinators.push(PlayerId(player.uuid.clone()));
    }
    let observed = favorites::inspect(&sim.lan(), &houses, &coordinators[0], "FV:2/0").unwrap();
    let start = sim.soap_log().len();
    assert!(matches!(
        favorites::remove(
            &sim.lan(),
            &houses,
            &coordinators[1],
            "FV:2/0",
            &observed.revision
        ),
        Err(CoreError::FavoriteChanged(_))
    ));
    assert!(
        sim.soap_log()[start..]
            .iter()
            .all(|entry| entry.action == "Browse" || entry.action.starts_with("Get"))
    );
}
