//! Queue relocation sends native service metadata intact and retains that
//! exact source observation through the destination write and origin release.

use fsonos_core::moving::{self, MoveMethod};
use fsonos_core::{HouseholdState, resolve_room};
use fsonos_proto::content::{self, BrowseFlag, QUEUE};
use fsonos_proto::control as soap;
use fsonos_proto::play_mode::{get_play_mode, set_play_mode};
use fsonos_proto::soap::{CONTENT_DIRECTORY, args_xml, envelope, parse_response};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayMode, PlayerId, TransportState};
use std::cell::Cell;
use std::net::IpAddr;
use std::time::Duration;

struct Fixture {
    sim: SimHandle,
    lan: SimLan,
    houses: Vec<HouseholdState>,
    from: &'static str,
    to: &'static str,
    source: IpAddr,
    target: IpAddr,
    uris: Vec<String>,
}

impl Fixture {
    fn new(s2: bool, fallback: bool, playing: bool) -> Self {
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
        let from = if s2 { "S2 Source" } else { "S1 Source" };
        let to = if s2 == fallback {
            "S2 Target"
        } else {
            "S1 Target"
        };
        let source = sim.player(from).unwrap().ip;
        let target = sim.player(to).unwrap().ip;
        let lan = sim.lan();
        let houses = ["S1 Source", "S2 Source"]
            .iter()
            .map(|room| {
                let mut house = HouseholdState::default();
                house.apply_topology(
                    &get_zone_group_state(&lan, sim.player(room).unwrap().ip).unwrap(),
                );
                house
            })
            .collect();
        let uris: Vec<_> = ["repeated", "repeated", "last"]
            .iter()
            .map(|name| format!("x-file-cifs://example.invalid/{name}.flac"))
            .collect();
        for uri in &uris {
            soap::add_uri_to_queue(&lan, source, uri, "", false).unwrap();
        }
        soap::play_from_queue(
            &lan,
            source,
            &PlayerId(sim.player(from).unwrap().uuid.clone()),
        )
        .unwrap();
        soap::seek_track(&lan, source, 2).unwrap();
        soap::seek_position(&lan, source, 17).unwrap();
        set_play_mode(&lan, source, PlayMode::RepeatAll).unwrap();
        soap::play(&lan, source).unwrap();
        if !playing {
            soap::pause(&lan, source).unwrap();
        }
        soap::add_uri_to_queue(
            &lan,
            target,
            "x-file-cifs://example.invalid/original-destination.flac",
            "",
            false,
        )
        .unwrap();
        soap::set_av_transport_uri(
            &lan,
            target,
            "x-rincon-mp3radio://example.invalid/destination-station",
            "",
        )
        .unwrap();
        soap::play(&lan, target).unwrap();
        if fallback {
            sim.upnp_fault(from, "DelegateGroupCoordinationTo", 401)
                .unwrap();
        }
        Self {
            sim,
            lan,
            houses,
            from,
            to,
            source,
            target,
            uris,
        }
    }

    fn relocate<T: Transport>(
        &self,
        t: &T,
        fallback: bool,
    ) -> Result<moving::MoveReport, moving::MoveError> {
        let from = resolve_room(&self.houses, self.from).unwrap();
        let to = resolve_room(&self.houses, self.to).unwrap();
        if fallback {
            moving::move_playback_within(t, &self.houses, &from, &to, Duration::ZERO)
        } else {
            moving::copy_playback(t, &self.houses, &from, &to)
        }
    }

    fn assert_destination(&self, playing: bool) {
        let queue = content::browse_snapshot(&self.lan, self.target, QUEUE).unwrap();
        let uris: Vec<_> = queue
            .objects
            .iter()
            .map(|item| item.res.as_ref().unwrap().uri.clone())
            .collect();
        assert_eq!(uris, self.uris);
        let position = soap::get_position_info(&self.lan, self.target).unwrap();
        assert_eq!((position.track, position.position_secs), (2, Some(17)));
        assert_eq!(position.uri, self.uris[1]);
        assert_eq!(
            get_play_mode(&self.lan, self.target).unwrap(),
            PlayMode::RepeatAll
        );
        let state = soap::get_transport_info(&self.lan, self.target)
            .unwrap()
            .state;
        assert_eq!(
            state,
            if playing {
                TransportState::Playing
            } else {
                TransportState::Stopped
            }
        );
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Drift {
    None,
    DuringCapture,
    BeforeWrite,
    GrowBeforeWrite,
    AfterWrite,
}

struct NativeReplies<'a> {
    fixture: &'a Fixture,
    native_reads: Cell<usize>,
    changed: Cell<bool>,
    short_pages: bool,
    drift: Drift,
}

impl<'a> NativeReplies<'a> {
    fn new(fixture: &'a Fixture, drift: Drift) -> Self {
        Self {
            fixture,
            native_reads: Cell::new(0),
            changed: Cell::new(false),
            short_pages: false,
            drift,
        }
    }

    fn originals(&self) -> Vec<String> {
        (0..3)
            .map(|offset| {
                content::browse_with_metadata(
                    self,
                    self.fixture.source,
                    QUEUE,
                    BrowseFlag::DirectChildren,
                    offset,
                    1,
                    16_384,
                )
                .unwrap()
                .objects
                .remove(0)
                .metadata
            })
            .collect()
    }
}

impl Transport for NativeReplies<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        if host == self.fixture.target
            && ((matches!(self.drift, Drift::BeforeWrite | Drift::GrowBeforeWrite)
                && name == "GetPositionInfo")
                || (self.drift == Drift::AfterWrite && name == "AddURIToQueue"))
        {
            self.changed.set(true);
        }
        let native = host == self.fixture.source
            && name == "Browse"
            && body.contains("<ObjectID>Q:0</ObjectID>")
            && body.contains("<BrowseFlag>BrowseDirectChildren</BrowseFlag>");
        let count = body
            .split_once("<RequestedCount>")
            .and_then(|(_, tail)| tail.split_once("</RequestedCount>"))
            .map(|(count, _)| count);
        let forwarded = if native && self.short_pages {
            count.map_or_else(
                || body.to_owned(),
                |count| {
                    body.replace(
                        &format!("<RequestedCount>{count}</RequestedCount>"),
                        "<RequestedCount>1</RequestedCount>",
                    )
                },
            )
        } else {
            body.to_owned()
        };
        let response = self.fixture.lan.soap_post(host, path, action, &forwarded)?;
        if !native {
            return Ok(response);
        }
        if count != Some("1") {
            self.native_reads.set(self.native_reads.get() + 1);
        }
        let changed = self.changed.get()
            || (self.drift == Drift::DuringCapture && self.native_reads.get() > 1);
        let variant = if changed { "changed" } else { "original" };
        let padding = if changed && self.drift == Drift::GrowBeforeWrite {
            "x".repeat(16_384)
        } else {
            String::new()
        };
        let mut response = parse_response(&response, "Browse")?;
        let (_, metadata) = response
            .args
            .iter_mut()
            .find(|(key, _)| key == "Result")
            .unwrap();
        *metadata = metadata
            .replacen(
                "<DIDL-Lite ",
                &format!("<DIDL-Lite xmlns:vendor=\"urn:example:native-service\" vendor:padding=\"{padding}\" xml:base=\"https://example.invalid/media/\" "),
                1,
            )
            .replace("<item ", "<item vendor:license=\"fixture-token\" refID=\"service-item\" ")
            .replace("<res ", "<res bitrate=\"320000\" vendor:codec=\"lossless\" ")
            .replace(
                "</item>",
                &format!("<res protocolInfo=\"http-get:*:audio/flac:*\">https://example.invalid/alternate.flac</res><desc id=\"extra\" nameSpace=\"urn:example:native-service\">fixture-route</desc><desc id=\"alternate\" nameSpace=\"urn:example:native-service\">other-route</desc><vendor:playback mode=\"{variant}\"><![CDATA[retained <native> & payload]]></vendor:playback></item>"),
            );
        let args: Vec<_> = response
            .args
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect();
        Ok(envelope(
            &CONTENT_DIRECTORY,
            "BrowseResponse",
            &args_xml(&args),
        ))
    }
}

#[test]
fn native_copies_and_handoff_fallbacks_keep_complete_metadata_and_duplicate_selection() {
    for s2 in [false, true] {
        for fallback in [false, true] {
            for playing in [false, true] {
                let f = Fixture::new(s2, fallback, playing);
                let mut t = NativeReplies::new(&f, Drift::None);
                t.short_pages = !playing;
                let originals = t.originals();
                assert_ne!(
                    originals[0], originals[1],
                    "duplicate resources retain occurrence metadata"
                );
                assert!(originals[0].contains("vendor:license=\"fixture-token\""));
                assert!(originals[0].contains("alternate.flac"));
                assert!(originals[0].contains("<![CDATA[retained <native> & payload]]>"));
                let since = f.sim.soap_log().len();
                let result = f.relocate(&t, fallback).unwrap();
                assert_eq!(result.method, MoveMethod::Copied);
                let log = f.sim.soap_log();
                let additions: Vec<_> = log[since..]
                    .iter()
                    .filter(|entry| entry.action == "AddURIToQueue")
                    .map(|entry| {
                        &entry
                            .args
                            .iter()
                            .find(|(key, _)| key == "EnqueuedURIMetaData")
                            .unwrap()
                            .1
                    })
                    .collect();
                assert_eq!(additions, originals.iter().collect::<Vec<_>>());
                assert!(
                    !log[since..]
                        .iter()
                        .any(|entry| entry.action == "RemoveAllTracksFromQueue")
                );
                if !playing {
                    assert!(!log[since..].iter().any(|entry| entry.action == "Play"));
                }
                f.assert_destination(playing);
                assert_eq!(
                    soap::get_transport_info(&f.lan, f.source).unwrap().state,
                    TransportState::Stopped
                );
            }
        }
    }
}

#[test]
fn source_metadata_changes_during_capture_or_destination_preparation_send_no_writes() {
    for s2 in [false, true] {
        for drift in [
            Drift::DuringCapture,
            Drift::BeforeWrite,
            Drift::GrowBeforeWrite,
        ] {
            let f = Fixture::new(s2, false, true);
            let t = NativeReplies::new(&f, drift);
            let since = f.sim.soap_log().len();
            let error = f.relocate(&t, false).unwrap_err();
            let message = error.to_string();
            assert!(
                message.contains("metadata changed") || message.contains("byte limit"),
                "{error:?}"
            );
            assert!(
                f.sim.soap_log()[since..]
                    .iter()
                    .all(|entry| entry.action.starts_with("Get") || entry.action == "Browse")
            );
            assert_eq!(
                soap::get_transport_info(&f.lan, f.source).unwrap().state,
                TransportState::Playing
            );
            assert_eq!(
                soap::get_transport_info(&f.lan, f.target).unwrap().state,
                TransportState::Playing
            );
            assert_eq!(
                content::browse_snapshot(&f.lan, f.target, QUEUE)
                    .unwrap()
                    .objects
                    .len(),
                1
            );
        }
    }
}

#[test]
fn later_native_metadata_changes_do_not_release_the_working_source() {
    for s2 in [false, true] {
        let f = Fixture::new(s2, false, true);
        let t = NativeReplies::new(&f, Drift::AfterWrite);
        let since = f.sim.soap_log().len();
        let error = f.relocate(&t, false).unwrap_err();
        assert!(error.to_string().contains("metadata changed"), "{error:?}");
        let source_id = f.sim.player(f.from).unwrap().uuid.clone();
        assert!(
            !f.sim.soap_log()[since..]
                .iter()
                .any(|entry| entry.player == source_id && entry.action == "Stop")
        );
        assert_eq!(
            soap::get_transport_info(&f.lan, f.source).unwrap().state,
            TransportState::Playing
        );
        f.assert_destination(true);
    }
}
