//! Durable scenes and queue undo replay complete native metadata over real
//! localhost SOAP. Synthetic extensions stand in for service-owned fields
//! that the display model does not represent.

use fsonos_core::actions;
use fsonos_core::control::queue_removal;
use fsonos_core::policy::Client;
use fsonos_core::scenes::{self, ApplyOptions, Scene, SceneSource};
use fsonos_core::store::{Action, MemStore};
use fsonos_core::{HouseholdState, control, snapshot};
use fsonos_proto::content::{self, QUEUE};
use fsonos_proto::control as soap;
use fsonos_proto::didl::{
    native_account_descriptor, parse_didl, spotify_track_didl, spotify_track_uri, xml_escape,
};
use fsonos_proto::play_mode::{get_play_mode, set_play_mode};
use fsonos_proto::soap::{CONTENT_DIRECTORY, args_xml, envelope, parse_response};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{HouseholdId, PlayMode, PlayerId, TransportState};
use std::cell::{Cell, RefCell};
use std::net::IpAddr;

struct Fixture {
    _sim: SimHandle,
    lan: SimLan,
    houses: Vec<HouseholdState>,
    id: PlayerId,
    host: IpAddr,
    uris: Vec<String>,
}

impl Fixture {
    fn new(s2: bool, spotify: bool) -> Self {
        let model = if s2 {
            SimModel::One
        } else {
            SimModel::Play5Gen1
        };
        let players = [SimPlayerSpec::new("Room", model)];
        let builder = SimHousehold::builder();
        let sim = if s2 {
            builder.s2(players)
        } else {
            builder.s1(players)
        }
        .spawn()
        .unwrap();
        let player = sim.player("Room").unwrap();
        let host = player.ip;
        let id = PlayerId(player.uuid.clone());
        let lan = sim.lan();
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&lan, host).unwrap());
        house.id = Some(HouseholdId(player.household.clone()));
        let houses = vec![house];
        let params = spotify.then(|| {
            control::spotify_params(&lan, &houses, &id)
                .unwrap()
                .unwrap()
        });
        let mut uris = Vec::new();
        for (source, title) in [
            ("AAAAAAAAAAAAAAAAAAAAAA", "Repeated track"),
            ("AAAAAAAAAAAAAAAAAAAAAA", "Repeated track"),
            ("BBBBBBBBBBBBBBBBBBBBBB", "Last track"),
        ] {
            let (uri, metadata) = if let Some(params) = &params {
                let source = format!("spotify:track:{source}");
                (
                    spotify_track_uri(&source, params),
                    spotify_track_didl(&source, title, params),
                )
            } else {
                (
                    format!("x-file-cifs://example.invalid/{source}.flac"),
                    String::new(),
                )
            };
            soap::add_uri_to_queue(&lan, host, &uri, &metadata, false).unwrap();
            uris.push(uri);
        }
        soap::play_from_queue(&lan, host, &id).unwrap();
        soap::seek_track(&lan, host, 2).unwrap();
        soap::seek_position(&lan, host, 17).unwrap();
        set_play_mode(&lan, host, PlayMode::RepeatAll).unwrap();
        soap::play(&lan, host).unwrap();
        soap::pause(&lan, host).unwrap();
        Self {
            _sim: sim,
            lan,
            houses,
            id,
            host,
            uris,
        }
    }

    fn capture<T: Transport>(&self, transport: &T) -> Scene {
        scenes::capture_with_queue_backups(transport, &self.houses, "Native queue", &[], 1).unwrap()
    }

    fn replace(&self) {
        soap::remove_all_tracks_from_queue(&self.lan, self.host).unwrap();
        soap::add_uri_to_queue(
            &self.lan,
            self.host,
            "https://example.invalid/other.flac",
            "",
            false,
        )
        .unwrap();
        soap::set_av_transport_uri(
            &self.lan,
            self.host,
            "x-rincon-mp3radio://example.invalid/live",
            "",
        )
        .unwrap();
        soap::play(&self.lan, self.host).unwrap();
    }

    fn apply<T: Transport>(&self, transport: &T, scene: &Scene) {
        let snapshots: Vec<_> = scenes::zones(scene, &self.houses)
            .iter()
            .map(|id| snapshot::capture(&self.lan, &self.houses, id, 2).unwrap())
            .collect();
        let plan = scenes::plan_apply(scene, &self.houses, &snapshots).unwrap();
        let report = scenes::apply(transport, &self.houses, &plan, &ApplyOptions::default());
        assert!(report.is_complete(), "{report:?}");
    }

    fn assert_restored(&self) {
        let queue = content::browse_snapshot(&self.lan, self.host, QUEUE).unwrap();
        let uris: Vec<_> = queue
            .objects
            .iter()
            .map(|item| item.res.as_ref().unwrap().uri.clone())
            .collect();
        assert_eq!(uris, self.uris);
        let position = soap::get_position_info(&self.lan, self.host).unwrap();
        assert_eq!((position.track, position.position_secs), (2, Some(17)));
        assert_eq!(position.uri, self.uris[1]);
        assert_eq!(
            get_play_mode(&self.lan, self.host).unwrap(),
            PlayMode::RepeatAll
        );
        assert!(matches!(
            soap::get_transport_info(&self.lan, self.host)
                .unwrap()
                .state,
            TransportState::Paused | TransportState::Stopped
        ));
    }
}

struct NativeReplies<'a> {
    lan: &'a SimLan,
    writes: RefCell<Vec<(String, String)>>,
    native_reads: Cell<usize>,
    drift: bool,
    short_pages: bool,
    padding: usize,
}

impl<'a> NativeReplies<'a> {
    fn new(lan: &'a SimLan) -> Self {
        Self {
            lan,
            writes: RefCell::new(Vec::new()),
            native_reads: Cell::new(0),
            drift: false,
            short_pages: false,
            padding: 0,
        }
    }

    fn assert_replayed(&self, expected: &[String]) {
        let writes = self.writes.borrow();
        let additions: Vec<_> = writes
            .iter()
            .filter(|(name, _)| name == "AddURIToQueue")
            .collect();
        assert_eq!(additions.len(), expected.len());
        for ((_, body), metadata) in additions.into_iter().zip(expected) {
            assert!(
                body.contains(&format!(
                    "<EnqueuedURIMetaData>{}</EnqueuedURIMetaData>",
                    xml_escape(metadata)
                )),
                "the restored occurrence must replay its exact complete saved metadata"
            );
        }
        assert!(
            !writes
                .iter()
                .any(|(name, _)| name == "RemoveAllTracksFromQueue")
        );
        assert!(
            !writes.iter().any(|(name, _)| name == "Play"),
            "restoring a quiet queue must not briefly start playback"
        );
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
        if !name.starts_with("Get") && name != "Browse" {
            self.writes.borrow_mut().push((name.into(), body.into()));
        }
        let queue_read = name == "Browse"
            && body.contains("<ObjectID>Q:0</ObjectID>")
            && body.contains("<BrowseFlag>BrowseDirectChildren</BrowseFlag>");
        let count = body
            .split_once("<RequestedCount>")
            .and_then(|(_, tail)| tail.split_once("</RequestedCount>"))
            .map(|(count, _)| count);
        let forwarded = if self.short_pages && queue_read {
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
        let response = self.lan.soap_post(host, path, action, &forwarded)?;
        if !queue_read {
            return Ok(response);
        }
        if count != Some("1") {
            self.native_reads.set(self.native_reads.get() + 1);
        }
        let variant = if self.drift && self.native_reads.get() > 1 {
            "changed"
        } else {
            "original"
        };
        let mut response = parse_response(&response, "Browse")?;
        let (_, metadata) = response
            .args
            .iter_mut()
            .find(|(name, _)| name == "Result")
            .unwrap();
        *metadata = metadata.replacen(
            "<DIDL-Lite ",
            &format!(
                "<DIDL-Lite xmlns:vendor=\"urn:example:native-service\" vendor:padding=\"{}\" xml:base=\"https://example.invalid/media/\" ",
                "x".repeat(self.padding)
            ),
            1,
        ).replace(
            "<item ",
            "<item vendor:license=\"fixture-token\" refID=\"original-service-item\" ",
        ).replace(
            "<res ",
            "<res bitrate=\"320000\" sampleFrequency=\"48000\" vendor:codec=\"lossless\" ",
        ).replace(
            "</item>",
            &format!(
                "<res protocolInfo=\"http-get:*:audio/flac:*\">https://example.invalid/alternate.flac</res><desc id=\"extra\" nameSpace=\"urn:example:native-service\">fixture-route</desc><vendor:playback mode=\"{variant}\"><![CDATA[retained <native> & payload]]></vendor:playback></item>"
            ),
        );
        let args: Vec<_> = response
            .args
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        Ok(envelope(
            &CONTENT_DIRECTORY,
            "BrowseResponse",
            &args_xml(&args),
        ))
    }
}

fn assert_native(metadata: &str) {
    for extension in [
        "xmlns:vendor=\"urn:example:native-service\"",
        "xml:base=\"https://example.invalid/media/\"",
        "vendor:license=\"fixture-token\"",
        "refID=\"original-service-item\"",
        "bitrate=\"320000\"",
        "sampleFrequency=\"48000\"",
        "vendor:codec=\"lossless\"",
        "alternate.flac",
        "<desc id=\"extra\"",
        "<![CDATA[retained <native> & payload]]>",
    ] {
        assert!(
            metadata.contains(extension),
            "native metadata lost {extension}"
        );
    }
}

#[test]
fn scenes_preserve_complete_native_metadata_through_persistence_and_reconstruction() {
    for s2 in [false, true] {
        for spotify in [false, true] {
            let f = Fixture::new(s2, spotify);
            let mut transport = NativeReplies::new(&f.lan);
            transport.short_pages = !spotify;
            let scene = f.capture(&transport);
            let SceneSource::QueueBackup { backup } = &scene.groups[0].source else {
                panic!("{scene:?}");
            };
            let metadata: Vec<_> = backup
                .entries
                .iter()
                .map(|entry| entry.metadata.clone())
                .collect();
            for item in &metadata {
                assert_native(item);
                if spotify {
                    let account = native_account_descriptor(item).unwrap().unwrap();
                    let params = control::spotify_params(&f.lan, &f.houses, &f.id)
                        .unwrap()
                        .unwrap();
                    assert_eq!(account.value, params.cdudn);
                    assert!(!parse_didl(item).unwrap()[0].id.starts_with("Q:0/"));
                }
            }
            assert_eq!(backup.entries[0].uri, backup.entries[1].uri);
            if !spotify {
                assert_ne!(
                    metadata[0], metadata[1],
                    "duplicate resources retain distinct native occurrences"
                );
            }
            let mut store = MemStore::default();
            scenes::save(&mut store, &scene, 1).unwrap();
            let loaded = scenes::load(&store, "Native queue").unwrap();
            assert_eq!(loaded, scene);
            f.replace();
            transport.writes.borrow_mut().clear();
            f.apply(&transport, &loaded);
            transport.assert_replayed(&metadata);
            f.assert_restored();
        }
    }
}

#[test]
fn durable_queue_undo_replays_every_native_occurrence_and_its_original_metadata() {
    for s2 in [false, true] {
        let f = Fixture::new(s2, false);
        let transport = NativeReplies::new(&f.lan);
        let pending = actions::capture_queue_undo(&transport, &f.houses, &f.id, 100)
            .unwrap()
            .removal(1, 1)
            .unwrap();
        let before = queue_removal::read(&f.lan, f.host).unwrap();
        let removed = queue_removal::remove(&f.lan, f.host, &f.id, &before, 1, 1).unwrap();
        let saved = actions::finish_queue_undo(
            &transport,
            &f.houses,
            pending,
            removed.revision,
            &before.uris[1..],
            101,
        )
        .unwrap();
        let value = serde_json::to_value(&saved).unwrap();
        let metadata: Vec<_> = value["backup"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["metadata"].as_str().unwrap().to_owned())
            .collect();
        for item in &metadata {
            assert_native(item);
        }
        let mut store = MemStore::default();
        actions::record(
            &mut store,
            &Action {
                at: 101,
                client: Client::Cli.key().into(),
                surface: "test".into(),
                intent: "queue edit".into(),
                decision: "allow".into(),
                result: "confirmed native edit".into(),
                before_state: Some(actions::with_queue_undo(None, &[saved]).unwrap()),
                undo_of: None,
            },
        )
        .unwrap();
        transport.writes.borrow_mut().clear();
        let report = actions::undo_last_vetted(
            &transport,
            &f.houses,
            &mut store,
            Some(&Client::Cli),
            &Client::Cli,
            "test",
            102,
            false,
            &|step| Ok(step.clone()),
        )
        .unwrap()
        .unwrap();
        assert!(report.failures.is_empty(), "{report:?}");
        assert!(
            report.zones.iter().all(|(_, zone)| zone.skipped.is_empty()),
            "{report:?}"
        );
        transport.assert_replayed(&metadata);
        f.assert_restored();
    }
}

#[test]
fn extension_only_changes_and_shared_root_amplification_refuse_a_backup_before_writes() {
    for drift in [false, true] {
        let f = Fixture::new(false, false);
        let mut transport = NativeReplies::new(&f.lan);
        transport.drift = drift;
        if !drift {
            transport.padding = scenes::MAX_QUEUE_BACKUP_BYTES / 2;
        }
        let error =
            scenes::capture_with_queue_backups(&transport, &f.houses, "Unsafe capture", &[], 1)
                .unwrap_err();
        let message = error.to_string();
        assert!(
            if drift {
                message.contains("metadata changed")
            } else {
                message.contains("byte limit")
            },
            "{error:?}"
        );
        assert!(transport.writes.borrow().is_empty());
    }
}
