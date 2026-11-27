//! Same-household moves reuse the source's observed account and native queue
//! metadata. Removing My Sonos favorites cannot strand already-playing music.

use fsonos_core::moving::{self, MoveMethod};
use fsonos_core::{HouseholdState, control, resolve_room};
use fsonos_proto::content::{QUEUE, browse_snapshot};
use fsonos_proto::control as soap;
use fsonos_proto::didl::{
    SpotifyRenderParams, parse_didl, spotify_bare_didl, spotify_bare_uri, spotify_track_didl,
    spotify_track_uri, spotify_uri_from_renderer_uri, xml_escape,
};
use fsonos_proto::play_mode::{get_play_mode, set_play_mode};
use fsonos_proto::soap::{AV_TRANSPORT, CONTENT_DIRECTORY, args_xml, envelope, parse_response};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayMode, PlayerId, TransportState};
use std::cell::Cell;
use std::net::IpAddr;
use std::time::Duration;

const TRACK: &str = "spotify:track:MnOpQr0123456789StUvWx";
const LAST: &str = "spotify:track:AbCdEf0123456789GhIjKl";

struct Fixture {
    sim: SimHandle,
    lan: SimLan,
    houses: Vec<HouseholdState>,
    source: IpAddr,
    target: IpAddr,
    source_id: PlayerId,
    parameters: SpotifyRenderParams,
}

impl Fixture {
    fn new(generation: u8) -> Self {
        let model = if generation == 1 {
            SimModel::Play5Gen1
        } else {
            SimModel::One
        };
        let players = [
            SimPlayerSpec::new("Source", model),
            SimPlayerSpec::new("Target", model),
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
        let source = sim.player("Source").unwrap().ip;
        let target = sim.player("Target").unwrap().ip;
        let source_id = PlayerId(sim.player("Source").unwrap().uuid.clone());
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&lan, source).unwrap());
        let houses = vec![house];
        let parameters = control::spotify_params(&lan, &houses, &source_id)
            .unwrap()
            .unwrap();
        soap::add_uri_to_queue(
            &lan,
            target,
            "https://example.invalid/original.mp3",
            "",
            false,
        )
        .unwrap();
        soap::set_av_transport_uri(
            &lan,
            target,
            "x-rincon-mp3radio://example.invalid/station",
            "",
        )
        .unwrap();
        soap::play(&lan, target).unwrap();
        Self {
            sim,
            lan,
            houses,
            source,
            target,
            source_id,
            parameters,
        }
    }

    fn queue(&self, paused: bool) {
        for (uri, title) in [(TRACK, "Repeated"), (TRACK, "Repeated"), (LAST, "Last")] {
            control::enqueue_spotify(
                &self.lan,
                &self.houses,
                &self.source_id,
                uri,
                title,
                false,
            )
            .unwrap();
        }
        soap::play_from_queue(&self.lan, self.source, &self.source_id).unwrap();
        soap::seek_track(&self.lan, self.source, 2).unwrap();
        soap::seek_position(&self.lan, self.source, 37).unwrap();
        set_play_mode(&self.lan, self.source, PlayMode::RepeatOne).unwrap();
        soap::play(&self.lan, self.source).unwrap();
        if paused {
            soap::pause(&self.lan, self.source).unwrap();
        }
    }

    fn relocate<T: Transport>(
        &self,
        t: &T,
        fallback: bool,
    ) -> Result<moving::MoveReport, moving::MoveError> {
        let from = resolve_room(&self.houses, "Source").unwrap();
        let to = resolve_room(&self.houses, "Target").unwrap();
        if fallback {
            moving::move_playback_within(t, &self.houses, &from, &to, Duration::ZERO)
        } else {
            moving::copy_playback(t, &self.houses, &from, &to)
        }
    }

    fn assert_queue(&self, paused: bool) {
        let queue = browse_snapshot(&self.lan, self.target, QUEUE).unwrap();
        let uris: Vec<_> = queue
            .objects
            .iter()
            .map(|object| {
                spotify_uri_from_renderer_uri(&object.res.as_ref().unwrap().uri).unwrap()
            })
            .collect();
        assert_eq!(uris, [TRACK, TRACK, LAST]);
        let position = soap::get_position_info(&self.lan, self.target).unwrap();
        assert_eq!((position.track, position.position_secs), (2, Some(37)));
        assert_eq!(
            get_play_mode(&self.lan, self.target).unwrap(),
            PlayMode::RepeatOne
        );
        assert_eq!(
            soap::get_transport_info(&self.lan, self.target)
                .unwrap()
                .state,
            if paused {
                TransportState::Stopped
            } else {
                TransportState::Playing
            }
        );
        assert_eq!(
            soap::get_transport_info(&self.lan, self.source)
                .unwrap()
                .state,
            TransportState::Stopped
        );
    }

    fn assert_no_favorites(&self, since: usize) {
        assert!(
            !self.sim.soap_log()[since..].iter().any(|call| {
                call.action == "Browse"
                    && call
                        .args
                        .iter()
                        .any(|(key, value)| key == "ObjectID" && value == "FV:2")
            }),
            "native source evidence does not need Favorites"
        );
    }

    fn assert_no_writes(&self, since: usize) {
        assert!(
            self.sim.soap_log()[since..]
                .iter()
                .all(|call| call.action.starts_with("Get") || call.action == "Browse"),
            "failed preparation must preserve both players"
        );
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Damage {
    EmptyCurrent,
    WrongCurrentTrack,
    ForeignQueueAccount,
    ForeignDescriptor,
    AfterTargetRead,
}

struct Replies<'a> {
    f: &'a Fixture,
    complete: bool,
    damage: Option<Damage>,
    target_read: Cell<bool>,
}

impl Transport for Replies<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let response = self.f.lan.soap_post(host, path, action, body)?;
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        if host == self.f.target && name == "GetTransportInfo" {
            self.target_read.set(true);
        }
        if host != self.f.source {
            return Ok(response);
        }
        let mut reply = parse_response(&response, name)?;
        let service = if name == "GetPositionInfo" {
            let metadata = &mut reply
                .args
                .iter_mut()
                .find(|(key, _)| key == "TrackMetaData")
                .unwrap()
                .1;
            match self.damage {
                Some(Damage::EmptyCurrent) => metadata.clear(),
                Some(Damage::WrongCurrentTrack) => {
                    *metadata = spotify_track_didl(LAST, "Wrong", &self.f.parameters);
                }
                Some(Damage::AfterTargetRead) if self.target_read.get() => {
                    *metadata = metadata.replace(&self.f.parameters.cdudn, "different-account");
                }
                _ => {}
            }
            &AV_TRANSPORT
        } else if name == "Browse" && body.contains("<ObjectID>Q:0</ObjectID>") {
            let metadata = &mut reply
                .args
                .iter_mut()
                .find(|(key, _)| key == "Result")
                .unwrap()
                .1;
            if self.complete {
                for object in parse_didl(metadata)? {
                    let source = spotify_uri_from_renderer_uri(&object.res.unwrap().uri).unwrap();
                    let id = format!(
                        "{}{}",
                        self.f.parameters.item_id_prefix,
                        source.replace(':', "%3a")
                    );
                    *metadata = metadata
                        .replace(&format!("id=\"{}\"", object.id), &format!("id=\"{id}\""));
                }
                *metadata = metadata.replace("</item>", &format!("<desc id=\"cdudn\" nameSpace=\"urn:schemas-rinconnetworks-com:metadata-1-0/\">{}</desc></item>", xml_escape(&self.f.parameters.cdudn)));
            }
            if self.damage == Some(Damage::ForeignQueueAccount) {
                *metadata = metadata.replace(&format!("sn={}", self.f.parameters.sn), "sn=99");
            }
            if self.damage == Some(Damage::ForeignDescriptor) {
                *metadata = metadata.replace("</item>", "<desc id='cdudn' nameSpace='urn:schemas-rinconnetworks-com:metadata-1-0/'>different-account</desc></item>");
            }
            *metadata = metadata
                .replace("<DIDL-Lite ", "<DIDL-Lite xmlns:v='urn:example:native' v:context='kept' ")
                .replace("<item ", "<item v:license='retained' ")
                .replace("</item>", "<v:data><![CDATA[native <payload> & bytes]]></v:data></item>");
            &CONTENT_DIRECTORY
        } else {
            return Ok(response);
        };
        let args: Vec<_> = reply
            .args
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect();
        Ok(envelope(
            service,
            &format!("{name}Response"),
            &args_xml(&args),
        ))
    }
}

#[test]
fn native_queue_replay_supports_same_household_copy_and_delegation_fallback_without_favorites() {
    for generation in [1, 2] {
        for paused in [false, true] {
            for fallback in [false, true] {
                let f = Fixture::new(generation);
                f.queue(paused);
                f.sim.retain_favorites(generation, |_| false).unwrap();
                if fallback {
                    f.sim
                        .upnp_fault("Source", "DelegateGroupCoordinationTo", 401)
                        .unwrap();
                }
                let replies = Replies {
                    f: &f,
                    complete: false,
                    damage: None,
                    target_read: Cell::new(false),
                };
                let since = f.sim.soap_log().len();
                assert_eq!(
                    f.relocate(&replies, fallback).unwrap().method,
                    MoveMethod::Copied
                );
                f.assert_queue(paused);
                f.assert_no_favorites(since);
                let log = f.sim.soap_log();
                let appends: Vec<_> = log[since..]
                    .iter()
                    .filter(|call| call.action == "AddURIToQueue")
                    .collect();
                assert_eq!(appends.len(), 3, "each occurrence is sent once");
                for call in appends {
                    let metadata = &call
                        .args
                        .iter()
                        .find(|(key, _)| key == "EnqueuedURIMetaData")
                        .unwrap()
                        .1;
                    assert!(metadata.contains("v:license='retained'"));
                    assert!(metadata.contains("<![CDATA[native <payload> & bytes]]>"));
                    assert!(metadata.contains(&f.parameters.cdudn));
                }
                assert!(
                    !log[since..]
                        .iter()
                        .any(|call| call.action == "RemoveAllTracksFromQueue")
                );
                if paused {
                    assert!(
                        !log[since..].iter().any(|call| call.action == "Play"),
                        "paused moves stay silent"
                    );
                }
            }
        }
    }
}

#[test]
fn complete_native_queue_metadata_needs_no_current_track_metadata_or_favorites() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        f.queue(true);
        f.sim.retain_favorites(generation, |_| false).unwrap();
        let replies = Replies {
            f: &f,
            complete: true,
            damage: Some(Damage::EmptyCurrent),
            target_read: Cell::new(false),
        };
        let since = f.sim.soap_log().len();
        f.relocate(&replies, false).unwrap();
        f.assert_queue(true);
        f.assert_no_favorites(since);
    }
}

#[test]
fn incomplete_native_metadata_retains_the_household_favorites_fallback() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        f.queue(true);
        let replies = Replies {
            f: &f,
            complete: false,
            damage: Some(Damage::EmptyCurrent),
            target_read: Cell::new(false),
        };
        let since = f.sim.soap_log().len();
        f.relocate(&replies, false).unwrap();
        f.assert_queue(true);
        assert!(f.sim.soap_log()[since..].iter().any(|call| {
            call.action == "Browse"
                && call
                    .args
                    .iter()
                    .any(|(key, value)| key == "ObjectID" && value == "FV:2")
        }));
    }
}

#[test]
fn missing_or_conflicting_replay_evidence_preserves_both_players() {
    for generation in [1, 2] {
        for damage in [
            Damage::EmptyCurrent,
            Damage::WrongCurrentTrack,
            Damage::ForeignQueueAccount,
            Damage::ForeignDescriptor,
            Damage::AfterTargetRead,
        ] {
            let f = Fixture::new(generation);
            f.queue(false);
            f.sim.retain_favorites(generation, |_| false).unwrap();
            let replies = Replies {
                f: &f,
                complete: false,
                damage: Some(damage),
                target_read: Cell::new(false),
            };
            let before = browse_snapshot(&f.lan, f.target, QUEUE).unwrap();
            let since = f.sim.soap_log().len();
            assert!(f.relocate(&replies, false).is_err());
            f.assert_no_writes(since);
            assert_eq!(browse_snapshot(&f.lan, f.target, QUEUE).unwrap(), before);
            assert_eq!(
                soap::get_transport_info(&f.lan, f.source).unwrap().state,
                TransportState::Playing
            );
            assert_eq!(
                soap::get_transport_info(&f.lan, f.target).unwrap().state,
                TransportState::Playing
            );
        }
    }
}

#[test]
fn direct_spotify_sources_retain_their_observed_account_within_a_household() {
    for generation in [1, 2] {
        for source in [
            TRACK,
            "spotify:episode:AbCdEf0123456789GhIjKl",
            "spotify:artist:AbCdEf0123456789GhIjKl",
            "spotify:show:AbCdEf0123456789GhIjKl",
        ] {
            for paused in [false, true] {
                let f = Fixture::new(generation);
                let (uri, metadata) = if source == TRACK {
                    (
                        spotify_track_uri(source, &f.parameters),
                        spotify_track_didl(source, "Selected", &f.parameters),
                    )
                } else {
                    (
                        spotify_bare_uri(source, f.parameters.sid),
                        spotify_bare_didl(source, "Selected", f.parameters.sid),
                    )
                };
                soap::set_av_transport_uri(&f.lan, f.source, &uri, &metadata).unwrap();
                soap::play(&f.lan, f.source).unwrap();
                if paused {
                    soap::pause(&f.lan, f.source).unwrap();
                }
                f.sim.retain_favorites(generation, |_| false).unwrap();
                let since = f.sim.soap_log().len();
                assert_eq!(
                    f.relocate(&f.lan, false).unwrap().method,
                    MoveMethod::Copied
                );
                let loaded = soap::get_media_info(&f.lan, f.target).unwrap();
                assert_eq!((loaded.uri, loaded.uri_metadata), (uri, metadata));
                f.assert_no_favorites(since);
                assert_eq!(
                    soap::get_transport_info(&f.lan, f.target).unwrap().state,
                    if paused {
                        TransportState::Stopped
                    } else {
                        TransportState::Playing
                    }
                );
            }
        }
    }
}
