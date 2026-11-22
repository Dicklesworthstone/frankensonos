//! Native selections forward complete service-owned XML through S1/S2 SOAP.
//! The transport adds synthetic fields the display model intentionally omits.

use fsonos_core::favorites::Favorite;
use fsonos_core::{CoreError, HouseholdState, control, favorite_queue, favorites, playlists};
use fsonos_proto::content::{self, BrowseFlag, QUEUE};
use fsonos_proto::control as soap;
use fsonos_proto::didl::{parse_didl, xml_escape};
use fsonos_proto::soap::{CONTENT_DIRECTORY, args_xml, envelope, parse_response};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{HouseholdId, PlayerId};
use std::cell::{Cell, RefCell};
use std::net::IpAddr;

struct Fixture {
    sim: SimHandle,
    lan: SimLan,
    houses: Vec<HouseholdState>,
    id: PlayerId,
    host: IpAddr,
}

impl Fixture {
    fn new(s2: bool) -> Self {
        let model = if s2 {
            SimModel::One
        } else {
            SimModel::Play5Gen1
        };
        let builder = SimHousehold::builder();
        let players = [SimPlayerSpec::new("Room", model)];
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
        Self {
            sim,
            lan,
            houses: vec![house],
            id,
            host,
        }
    }

    fn revision(&self, source: &str) -> String {
        if source == QUEUE {
            let page = content::browse(
                &self.lan,
                self.host,
                QUEUE,
                BrowseFlag::DirectChildren,
                0,
                1,
            )
            .unwrap();
            playlists::queue_selection_revision(&self.id, &page)
        } else {
            playlists::contents(&self.lan, &self.houses, &self.id, source, 0, 1, None)
                .unwrap()
                .revision
        }
    }

    fn select(
        &self,
        transport: &NativeReplies<'_>,
        positions: &[u32],
    ) -> Result<Vec<Favorite>, CoreError> {
        let revision = self.revision(transport.source);
        if transport.source == QUEUE {
            playlists::select_queue_tracks(transport, &self.houses, &self.id, positions, &revision)
        } else {
            playlists::select_tracks(
                transport,
                &self.houses,
                &self.id,
                transport.source,
                positions,
                &revision,
            )
        }
    }

    fn retained_local_queue(&self) {
        for uri in [
            "x-file-cifs://example.invalid/one.flac",
            "x-file-cifs://example.invalid/two.flac",
        ] {
            soap::add_uri_to_queue(&self.lan, self.host, uri, "", false).unwrap();
        }
        soap::set_av_transport_uri(
            &self.lan,
            self.host,
            "x-rincon-mp3radio://example.invalid/radio.mp3",
            "",
        )
        .unwrap();
        soap::play(&self.lan, self.host).unwrap();
    }
}

struct NativeReplies<'a> {
    lan: &'a SimLan,
    source: &'a str,
    reads: Cell<usize>,
    drift_at: Option<usize>,
    padding: usize,
    writes: RefCell<Vec<(String, String)>>,
}

impl<'a> NativeReplies<'a> {
    fn new(lan: &'a SimLan, source: &'a str) -> Self {
        Self {
            lan,
            source,
            reads: Cell::new(0),
            drift_at: None,
            padding: 0,
            writes: RefCell::new(Vec::new()),
        }
    }

    fn assert_metadata(&self, action: &str, argument: &str, expected: &[&str]) {
        let writes = self.writes.borrow();
        let actual: Vec<_> = writes.iter().filter(|(name, _)| name == action).collect();
        assert_eq!(
            actual.len(),
            expected.len(),
            "native {action} dispatch count"
        );
        for ((_, body), expected) in actual.into_iter().zip(expected) {
            let fragment = format!("<{argument}>{}</{argument}>", xml_escape(expected));
            assert!(
                body.contains(&fragment),
                "{action} must forward the exact selected metadata"
            );
        }
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
        let response = self.lan.soap_post(host, path, action, body)?;
        if name != "Browse"
            || !body.contains(&format!("<ObjectID>{}</ObjectID>", self.source))
            || !body.contains("<BrowseFlag>BrowseDirectChildren</BrowseFlag>")
        {
            return Ok(response);
        }
        self.reads.set(self.reads.get() + 1);
        let variant = if self.drift_at.is_some_and(|at| self.reads.get() >= at) {
            "changed-extension"
        } else {
            "original-extension"
        };
        let mut reply = parse_response(&response, "Browse")?;
        let result = reply
            .args
            .iter_mut()
            .find(|(name, _)| name == "Result")
            .unwrap();
        result.1 = result.1.replacen(
            "<DIDL-Lite ",
            &format!("<DIDL-Lite xmlns:vendor=\"urn:example:native-service\" xml:base=\"https://example.invalid/media/\" vendor:context=\"source &amp; account\" vendor:padding=\"{}\" ", "x".repeat(self.padding)),
            1,
        ).replace(
            "<item ",
            "<item refID=\"original-service-item\" vendor:license=\"fixture-token\" ",
        ).replace(
            "<res ",
            "<res bitrate=\"320000\" sampleFrequency=\"48000\" vendor:codec=\"lossless\" ",
        ).replace(
            "</item>",
            &format!("<res protocolInfo=\"http-get:*:audio/flac:*\" vendor:variant=\"fallback\">https://example.invalid/alternate.flac</res><desc id=\"extra\" nameSpace=\"urn:example:native-service\">fixture-route</desc><desc id=\"another\" nameSpace=\"urn:example:native-service\">fixture-context</desc><vendor:playback mode=\"{variant}\"><vendor:value><![CDATA[retained <native> & payload]]></vendor:value></vendor:playback></item>"),
        );
        let args: Vec<_> = reply
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

fn assert_native(selected: &Favorite) {
    for native in [
        "xmlns:vendor=\"urn:example:native-service\"",
        "xml:base=\"https://example.invalid/media/\"",
        "vendor:context=\"source &amp; account\"",
        "refID=\"original-service-item\"",
        "vendor:license=\"fixture-token\"",
        "bitrate=\"320000\"",
        "sampleFrequency=\"48000\"",
        "<desc id=\"extra\"",
        "<desc id=\"another\"",
        "alternate.flac",
        "<![CDATA[retained <native> & payload]]>",
    ] {
        assert!(
            selected.metadata.contains(native),
            "missing native metadata: {native}"
        );
    }
    let objects = parse_didl(&selected.metadata).unwrap();
    assert_eq!(objects.len(), 1);
    assert_eq!(objects[0].id, selected.id);
}

#[test]
fn saved_selection_forwards_complete_metadata_to_single_and_batch_playback() {
    for s2 in [false, true] {
        let f = Fixture::new(s2);
        let t = NativeReplies::new(&f.lan, "SQ:7");
        let selected = f.select(&t, &[3, 1, 3]).unwrap();
        for track in &selected {
            assert_native(track);
        }
        assert_eq!(
            selected[0], selected[2],
            "repeated occurrences keep the same native XML"
        );
        assert_ne!(
            selected[0].metadata, selected[1].metadata,
            "equal resources keep their distinct occurrence ids"
        );
        favorites::play(&t, &f.houses, &f.id, &selected[0]).unwrap();
        t.assert_metadata(
            "SetAVTransportURI",
            "CurrentURIMetaData",
            &[&selected[0].metadata],
        );

        t.writes.borrow_mut().clear();
        favorite_queue::enqueue(&t, &f.houses, &f.id, &selected[1], false).unwrap();
        t.assert_metadata(
            "AddURIToQueue",
            "EnqueuedURIMetaData",
            &[&selected[1].metadata],
        );

        t.writes.borrow_mut().clear();
        let destination = control::TrackBatchDestination::read(&t, f.host, &f.id).unwrap();
        control::play_native_tracks_bound_with_receipt(
            &t,
            &f.houses,
            &f.id,
            &selected,
            &destination,
        )
        .unwrap();
        t.assert_metadata(
            "AddURIToQueue",
            "EnqueuedURIMetaData",
            &selected
                .iter()
                .map(|track| track.metadata.as_str())
                .collect::<Vec<_>>(),
        );
    }
}

#[test]
fn retained_queue_copies_forward_native_xml_without_interrupting_the_current_source() {
    for s2 in [false, true] {
        let f = Fixture::new(s2);
        f.retained_local_queue();
        let before = control::playback(&f.lan, &f.houses, &f.id).unwrap();
        let queue_before = content::browse_snapshot(&f.lan, f.host, QUEUE).unwrap();
        let t = NativeReplies::new(&f.lan, QUEUE);
        let selected = f.select(&t, &[2, 1, 2]).unwrap();
        for track in &selected {
            assert_native(track);
        }
        let revision = f.revision("SQ:1");
        let result =
            playlists::append_tracks(&t, &f.houses, &f.id, "SQ:1", &revision, &selected).unwrap();
        assert_eq!(result.total, result.previous_total + 3);
        t.assert_metadata(
            "AddURIToSavedQueue",
            "EnqueuedURIMetaData",
            &selected
                .iter()
                .map(|track| track.metadata.as_str())
                .collect::<Vec<_>>(),
        );
        assert_eq!(control::playback(&f.lan, &f.houses, &f.id).unwrap(), before);
        assert_eq!(
            content::browse_snapshot(&f.lan, f.host, QUEUE).unwrap(),
            queue_before
        );
        assert!(
            t.writes
                .borrow()
                .iter()
                .all(|(name, _)| name == "AddURIToSavedQueue")
        );
    }
}

#[test]
fn extension_only_changes_at_the_same_native_counter_invalidate_both_source_selections() {
    for s2 in [false, true] {
        for source in ["SQ:7", QUEUE] {
            let f = Fixture::new(s2);
            if source == QUEUE {
                f.retained_local_queue();
            }
            let mut t = NativeReplies::new(&f.lan, source);
            t.drift_at = Some(2);
            let error = f.select(&t, &[1]).unwrap_err();
            if source == QUEUE {
                assert!(matches!(error, CoreError::QueueChanged(_)), "{error:?}");
            } else {
                assert!(matches!(error, CoreError::PlaylistChanged(_)), "{error:?}");
            }
            assert_eq!(t.reads.get(), 2);
            assert!(t.writes.borrow().is_empty());
        }
    }
}

#[test]
fn shared_root_amplification_and_deliberate_repeats_stay_inside_the_selection_budget() {
    let f = Fixture::new(false);
    for (padding, positions) in [
        (playlists::MAX_PLAYLIST_EDIT_BYTES / 2, vec![1, 2, 3]),
        (playlists::MAX_PLAYLIST_EDIT_BYTES / 8, vec![1; 10]),
    ] {
        let mut t = NativeReplies::new(&f.lan, "SQ:7");
        t.padding = padding;
        let error = f.select(&t, &positions).unwrap_err();
        assert!(
            error.to_string().contains("byte limit") || error.to_string().contains("exceed 4 MiB"),
            "{error:?}"
        );
        assert!(t.writes.borrow().is_empty());
    }
    assert!(
        f.sim
            .soap_log()
            .iter()
            .all(|entry| entry.action == "Browse" || entry.action.starts_with("Get"))
    );
}

#[test]
fn an_oversized_extension_in_a_later_track_rejects_the_whole_append_before_io() {
    struct NoRequests;
    impl Transport for NoRequests {
        fn soap_post(
            &self,
            _host: IpAddr,
            _path: &str,
            action: &str,
            _body: &str,
        ) -> Result<String, ProtoError> {
            panic!("batch metadata must be validated before {action}")
        }
    }

    let f = Fixture::new(false);
    let t = NativeReplies::new(&f.lan, "SQ:7");
    let selected = f.select(&t, &[1]).unwrap().remove(0);
    let mut oversized = selected.clone();
    oversized.metadata = oversized.metadata.replacen(
        "</item>",
        &format!(
            "<vendor:large>{}</vendor:large></item>",
            "x".repeat(fsonos_proto::queue::MAX_SAVED_TRACK_METADATA_BYTES),
        ),
        1,
    );
    let error = playlists::append_tracks(
        &NoRequests,
        &f.houses,
        &f.id,
        "SQ:1",
        "observed-revision",
        &[selected, oversized],
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("metadata exceeds 4 MiB"),
        "{error:?}"
    );
}
