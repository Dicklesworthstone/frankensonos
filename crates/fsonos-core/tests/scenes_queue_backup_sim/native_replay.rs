use super::*;
use fsonos_core::{actions, control, favorites, playlists};
use fsonos_proto::content::{self, BrowseFlag};
use fsonos_proto::didl::{
    SpotifyRenderParams, parse_didl, spotify_track_didl, spotify_uri_from_renderer_uri, xml_escape,
};
use fsonos_proto::soap::{AV_TRANSPORT, CONTENT_DIRECTORY, args_xml, envelope, parse_response};

fn seed(f: &Fixture) -> SpotifyRenderParams {
    let parameters = control::spotify_params(&f.lan, &f.houses, &f.coordinator)
        .unwrap()
        .unwrap();
    let selected = playlists::list(&f.lan, &f.houses, &f.coordinator)
        .unwrap()
        .into_iter()
        .find(|playlist| playlist.id == "SQ:7")
        .unwrap();
    favorites::play(&f.lan, &f.houses, &f.coordinator, &selected).unwrap();
    soap::seek_track(&f.lan, f.host, 2).unwrap();
    soap::seek_position(&f.lan, f.host, 17).unwrap();
    soap::pause(&f.lan, f.host).unwrap();
    set_play_mode(&f.lan, f.host, PlayMode::ShuffleRepeatOne).unwrap();
    parameters
}

#[derive(Clone, Copy, Debug)]
enum Damage {
    EmptyCurrent,
    WrongCurrentTrack,
    AmbiguousCurrentAccount,
    ForeignQueueAccount,
    ForeignQueueDescriptor,
}

struct Replies<'a> {
    fixture: &'a Fixture,
    parameters: SpotifyRenderParams,
    native_queue: bool,
    damage: Option<Damage>,
}

impl Transport for Replies<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let response = self.fixture.lan.soap_post(host, path, action, body)?;
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        if name == "GetPositionInfo" {
            let mut reply = parse_response(&response, name)?;
            let metadata = &mut reply
                .args
                .iter_mut()
                .find(|(key, _)| key == "TrackMetaData")
                .unwrap()
                .1;
            match self.damage {
                Some(Damage::EmptyCurrent) => metadata.clear(),
                Some(Damage::WrongCurrentTrack) => {
                    *metadata = spotify_track_didl(
                        "spotify:track:AnotherTrack",
                        "Another",
                        &self.parameters,
                    );
                }
                Some(Damage::AmbiguousCurrentAccount) => {
                    *metadata = metadata
                        .replace("</item>", "<desc id='cdudn'>another-account</desc></item>");
                }
                _ => {}
            }
            let args: Vec<_> = reply
                .args
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str()))
                .collect();
            return Ok(envelope(
                &AV_TRANSPORT,
                "GetPositionInfoResponse",
                &args_xml(&args),
            ));
        }
        if name != "Browse" || !body.contains("<ObjectID>Q:0</ObjectID>") {
            return Ok(response);
        }
        let mut reply = parse_response(&response, name)?;
        let metadata = &mut reply
            .args
            .iter_mut()
            .find(|(key, _)| key == "Result")
            .unwrap()
            .1;
        if self.native_queue {
            for item in parse_didl(metadata)? {
                let source = spotify_uri_from_renderer_uri(&item.res.unwrap().uri).unwrap();
                let id = format!(
                    "{}{}",
                    self.parameters.item_id_prefix,
                    source.replace(':', "%3a")
                );
                *metadata =
                    metadata.replace(&format!("id=\"{}\"", item.id), &format!("id=\"{id}\""));
            }
            *metadata = metadata.replace("</item>", &format!("<desc id=\"cdudn\" nameSpace=\"urn:schemas-rinconnetworks-com:metadata-1-0/\">{}</desc></item>", xml_escape(&self.parameters.cdudn)));
        }
        if matches!(self.damage, Some(Damage::ForeignQueueAccount)) {
            *metadata = metadata.replace(&format!("sn={}", self.parameters.sn), "sn=99");
        }
        if matches!(self.damage, Some(Damage::ForeignQueueDescriptor)) {
            *metadata = metadata.replace("</item>", "<desc id='cdudn' nameSpace='urn:schemas-rinconnetworks-com:metadata-1-0/'>different-account</desc></item>");
        }
        *metadata = metadata
            .replace("<DIDL-Lite ", "<DIDL-Lite xmlns:v='urn:example:native' v:context='unchanged' ")
            .replace("<item ", "<item v:license='keep' ")
            .replace("</item>", "<desc id='extra' nameSpace='urn:example:native'>retained</desc><v:data><![CDATA[native <payload> & bytes]]></v:data></item>");
        let args: Vec<_> = reply
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
fn current_track_metadata_recovers_native_queue_undo_without_favorites() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let parameters = seed(&f);
        f.sim.retain_favorites(generation, |_| false).unwrap();
        let original = f.queue();
        let transport = Replies {
            fixture: &f,
            parameters,
            native_queue: false,
            damage: None,
        };
        let start = f.sim.soap_log().len();
        actions::capture_queue_undo(&transport, &f.houses, &f.coordinator, 1).unwrap();
        assert!(
            !f.sim.soap_log()[start..].iter().any(|call| {
                call.action == "Browse"
                    && call
                        .args
                        .iter()
                        .any(|(key, value)| key == "ObjectID" && value == "FV:2")
            }),
            "complete current-track evidence needs no Favorites lookup"
        );
        let scene =
            scenes::capture_with_queue_backups(&transport, &f.houses, "Native queue", &[], 1)
                .unwrap();
        let SceneSource::QueueBackup { backup } = &scene.groups[0].source else {
            panic!("missing backup")
        };
        for entry in &backup.entries {
            assert!(entry.metadata.contains("v:license='keep'"));
            assert!(
                entry
                    .metadata
                    .contains("<![CDATA[native <payload> & bytes]]>")
            );
            assert!(entry.metadata.contains("<desc id='extra'"));
        }
        f.replace();
        let report = f.apply(&f.lan, &scene);
        assert!(report.is_complete(), "{report:?}");
        assert_eq!(f.queue(), original);
        let position = soap::get_position_info(&f.lan, f.host).unwrap();
        assert_eq!((position.track, position.position_secs), (2, Some(17)));
        assert_eq!(
            get_play_mode(&f.lan, f.host).unwrap(),
            PlayMode::ShuffleRepeatOne
        );
        assert!(matches!(
            soap::get_transport_info(&f.lan, f.host).unwrap().state,
            TransportState::Paused | TransportState::Stopped
        ));
    }
}

#[test]
fn complete_native_queue_metadata_needs_neither_favorites_nor_current_track_metadata() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let parameters = seed(&f);
        f.sim.retain_favorites(generation, |_| false).unwrap();
        let transport = Replies {
            fixture: &f,
            parameters,
            native_queue: true,
            damage: Some(Damage::EmptyCurrent),
        };
        let observed = content::browse_with_metadata(
            &transport,
            f.host,
            QUEUE,
            BrowseFlag::DirectChildren,
            0,
            100,
            scenes::MAX_QUEUE_BACKUP_BYTES,
        )
        .unwrap();
        let scene =
            scenes::capture_with_queue_backups(&transport, &f.houses, "Native queue", &[], 1)
                .unwrap();
        let SceneSource::QueueBackup { backup } = &scene.groups[0].source else {
            panic!("missing backup")
        };
        assert_eq!(
            backup
                .entries
                .iter()
                .map(|entry| &entry.metadata)
                .collect::<Vec<_>>(),
            observed
                .objects
                .iter()
                .map(|native| &native.metadata)
                .collect::<Vec<_>>()
        );
        let original = f.queue();
        f.replace();
        let report = f.apply(&f.lan, &scene);
        assert!(report.is_complete(), "{report:?}");
        assert_eq!(f.queue(), original);
    }
}

#[test]
fn complete_native_metadata_cannot_change_the_selected_tracks_linked_account() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let parameters = seed(&f);
        f.sim.retain_favorites(generation, |_| false).unwrap();
        let transport = Replies {
            fixture: &f,
            parameters,
            native_queue: true,
            damage: Some(Damage::ForeignQueueAccount),
        };
        let start = f.sim.soap_log().len();
        let error =
            actions::capture_queue_undo(&transport, &f.houses, &f.coordinator, 1).unwrap_err();
        assert!(
            error.to_string().contains("selected queue track changed"),
            "S{generation}: {error}"
        );
        assert!(f.writes_since(start).is_empty());
    }
}

#[test]
fn missing_current_replay_evidence_still_uses_the_households_favorites() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let parameters = seed(&f);
        let original = f.queue();
        let transport = Replies {
            fixture: &f,
            parameters,
            native_queue: false,
            damage: Some(Damage::EmptyCurrent),
        };
        let start = f.sim.soap_log().len();
        actions::capture_queue_undo(&transport, &f.houses, &f.coordinator, 1).unwrap();
        assert!(f.sim.soap_log()[start..].iter().any(|call| {
            call.action == "Browse"
                && call
                    .args
                    .iter()
                    .any(|(key, value)| key == "ObjectID" && value == "FV:2")
        }));
        let scene =
            scenes::capture_with_queue_backups(&transport, &f.houses, "Fallback queue", &[], 1)
                .unwrap();
        f.replace();
        let report = f.apply(&f.lan, &scene);
        assert!(report.is_complete(), "{report:?}");
        assert_eq!(f.queue(), original);
    }
}

#[test]
fn conflicting_track_or_account_evidence_refuses_backup_without_writes() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let parameters = seed(&f);
        for damage in [
            Damage::WrongCurrentTrack,
            Damage::AmbiguousCurrentAccount,
            Damage::ForeignQueueAccount,
            Damage::ForeignQueueDescriptor,
        ] {
            let transport = Replies {
                fixture: &f,
                parameters: parameters.clone(),
                native_queue: false,
                damage: Some(damage),
            };
            let start = f.sim.soap_log().len();
            let result = actions::capture_queue_undo(&transport, &f.houses, &f.coordinator, 1);
            assert!(result.is_err(), "S{generation}, {damage:?}");
            assert!(f.writes_since(start).is_empty());
        }
    }
}

struct RegroupDuringReplayMetadata<'a> {
    fixture: &'a Fixture,
    native_page_count: String,
    read_queue: Cell<bool>,
    changed: Cell<bool>,
}

impl Transport for RegroupDuringReplayMetadata<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let response = self.fixture.lan.soap_post(host, path, action, body)?;
        if host != self.fixture.host {
            return Ok(response);
        }
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        if name == "Browse"
            && body.contains("<ObjectID>Q:0</ObjectID>")
            && body.contains(&self.native_page_count)
        {
            self.read_queue.set(true);
        }
        // Snapshot/reference reads fetch one occurrence. The first position
        // read after a complete native page belongs to replay identity capture.
        if name == "GetPositionInfo" && self.read_queue.get() && !self.changed.replace(true) {
            let member = self.fixture.sim.player("Beta").unwrap();
            soap::leave_group(&self.fixture.lan, member.ip, &PlayerId(member.uuid.clone()))?;
        }
        Ok(response)
    }
}

#[test]
fn a_group_change_during_native_replay_metadata_cannot_become_the_scene_baseline() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        seed(&f);
        f.sim.retain_favorites(generation, |_| false).unwrap();
        let original = f.queue();
        assert!(original.len() > 1);
        let transport = RegroupDuringReplayMetadata {
            fixture: &f,
            native_page_count: format!("<RequestedCount>{}</RequestedCount>", original.len()),
            read_queue: Cell::new(false),
            changed: Cell::new(false),
        };
        let start = f.sim.soap_log().len();
        let error =
            scenes::capture_with_queue_backups(&transport, &f.houses, "Native queue", &[], 1)
                .unwrap_err();
        assert!(
            transport.changed.get(),
            "the group changed during renderer metadata capture"
        );
        assert!(
            error.to_string().contains("group changed while saving"),
            "S{generation}: {error}"
        );
        assert_eq!(f.queue(), original);
        assert_eq!(
            f.writes_since(start),
            ["SetAVTransportURI"],
            "only the external member's leave may write; scene capture stays read-only"
        );
        let topology = get_zone_group_state(&f.lan, f.host).unwrap();
        let source = topology
            .groups
            .iter()
            .find(|group| group.coordinator == f.coordinator)
            .unwrap();
        assert_eq!(source.members.len(), 1);
    }
}
