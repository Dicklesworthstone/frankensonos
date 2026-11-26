use super::*;
use crate::zones::fixtures::households;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::Policy;
use fsonos_core::rooms::Aliases;
use fsonos_proto::{ProtoError, Transport};
use serde_json::json;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

struct NoSpeakerIo(Arc<AtomicUsize>);

impl Transport for NoSpeakerIo {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(ProtoError::NotWired(
            "no speaker I/O before review validation",
        ))
    }
}

fn uri() -> ReviewedSource {
    ReviewedSource::Uri {
        source_uri: "spotify:track:1000000000000000000001".into(),
        title: Some("Evening recording".into()),
    }
}

fn tracks() -> ReviewedSource {
    ReviewedSource::Tracks {
        tracks: vec![QueueTrackInput {
            source_uri: "spotify:track:1000000000000000000001".into(),
            title: Some("Selected occurrence".into()),
        }],
    }
}

fn request(rooms: &Rooms<'_>, target: &str) -> ReviewedSourceRequest {
    ReviewedSourceRequest {
        zone: target.into(),
        expected_revision: resolve_view(rooms, target).unwrap().revision,
        mode: ReviewedSourceMode::Play,
        source: uri(),
    }
}

fn surface(policy: Policy) -> (Surface, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let reads = Arc::new(AtomicUsize::new(0));
    let surveys = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&surveys);
    (
        Surface::new(
            Box::new(NoSpeakerIo(Arc::clone(&reads))),
            Box::new(move |_| {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(households())
            }),
            policy,
            Box::new(SystemClock),
        ),
        surveys,
        reads,
    )
}

#[test]
fn one_snapshot_checks_the_revision_and_deduplicates_the_reviewed_groups() {
    let h = households();
    let aliases = Aliases::parse("[aliases]\nset = ['Den', 'Kitchen@S1', 'Kitchen@S2']").unwrap();
    let rooms = Rooms {
        households: &h,
        aliases: Some(&aliases),
        client: None,
    };
    let request = request(&rooms, "set");
    let (surface, _, reads) = surface(Policy::default());
    let commands = request.plan(&surface, &rooms).unwrap();
    assert_eq!(commands.len(), 2);
    assert!(commands.iter().all(|command| matches!(
        command, Command::Play { title: Some(title), .. } if title == "Evening recording"
    )));
    assert_eq!(reads.load(Ordering::SeqCst), 0);
}

#[test]
fn alias_expansion_client_defaults_and_target_substitution_invalidate_the_review() {
    let h = households();
    let original = Aliases::parse(
        "[aliases]\nset = ['Den']\n[defaults]\nroom = 'Den'\n[defaults.clients.guest]\nroom = 'Kitchen@S2'",
    ).unwrap();
    let expanded = Aliases::parse("[aliases]\nset = ['Den', 'Kitchen@S2']").unwrap();
    let rooms = Rooms {
        households: &h,
        aliases: Some(&original),
        client: None,
    };
    let (surface, _, reads) = surface(Policy::default());
    let selected = request(&rooms, "set");
    assert_eq!(
        selected
            .plan(
                &surface,
                &Rooms {
                    aliases: Some(&expanded),
                    ..rooms
                }
            )
            .unwrap_err()
            .code,
        ErrorCode::NotCoordinator,
    );
    let mut default = request(&rooms, "here");
    assert_eq!(
        default
            .plan(
                &surface,
                &Rooms {
                    client: Some("guest"),
                    ..rooms
                }
            )
            .unwrap_err()
            .code,
        ErrorCode::NotCoordinator,
    );
    default.zone = "all".into();
    assert_eq!(
        default.plan(&surface, &rooms).unwrap_err().code,
        ErrorCode::NotCoordinator
    );
    assert_eq!(reads.load(Ordering::SeqCst), 0);
}

#[test]
fn every_source_and_mode_rejects_stale_reviews_before_catalog_caps_or_queue_reads() {
    for mode in [
        ReviewedSourceMode::Play,
        ReviewedSourceMode::Next,
        ReviewedSourceMode::Append,
    ] {
        for source in [
            uri(),
            tracks(),
            ReviewedSource::Favorite {
                favorite: "Morning".into(),
            },
            ReviewedSource::Playlist {
                playlist: "Evening".into(),
            },
        ] {
            let (surface, surveys, reads) = surface(Policy::default());
            let request = ReviewedSourceRequest {
                zone: "Den".into(),
                expected_revision: "0".repeat(64),
                mode,
                source,
            };
            let error = surface
                .control_reviewed_source(&Client::LoopbackHttp, &request)
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::NotCoordinator, "{mode:?}");
            assert!(error.detail.contains("nothing was changed"));
            assert_eq!(surveys.load(Ordering::SeqCst), 1);
            assert_eq!(reads.load(Ordering::SeqCst), 0);
        }
    }
}

#[test]
fn reviewed_sources_keep_existing_tool_permissions_before_discovery() {
    let h = households();
    for mode in [
        ReviewedSourceMode::Play,
        ReviewedSourceMode::Next,
        ReviewedSourceMode::Append,
    ] {
        for source in [
            uri(),
            tracks(),
            ReviewedSource::Favorite {
                favorite: "Morning".into(),
            },
            ReviewedSource::Playlist {
                playlist: "Evening".into(),
            },
        ] {
            let request = ReviewedSourceRequest {
                mode,
                source,
                ..request(&Rooms::from(&h), "Den")
            };
            let policy = Policy::from_toml(&format!(
                "[clients.'loopback-http']\ndeny = ['{}']",
                request.tool(),
            ))
            .unwrap();
            let (surface, surveys, reads) = surface(policy);
            assert_eq!(
                surface
                    .control_reviewed_source(&Client::LoopbackHttp, &request)
                    .unwrap_err()
                    .code,
                ErrorCode::PolicyDenied,
            );
            assert_eq!(surveys.load(Ordering::SeqCst), 0);
            assert_eq!(reads.load(Ordering::SeqCst), 0);
        }
    }
}

#[test]
fn malformed_reviews_and_invalid_source_modes_fail_before_discovery() {
    let (surface, surveys, reads) = surface(Policy::default());
    let h = households();
    let base = request(&Rooms::from(&h), "Den");
    for revision in ["".into(), "a".repeat(63), "A".repeat(64), "g".repeat(64)] {
        let request = ReviewedSourceRequest {
            expected_revision: revision,
            ..base.clone()
        };
        assert_eq!(
            surface
                .control_reviewed_source(&Client::LoopbackHttp, &request)
                .unwrap_err()
                .code,
            ErrorCode::InvalidArgument,
        );
    }
    let stream = ReviewedSourceRequest {
        source: ReviewedSource::Uri {
            source_uri: "https://audio.example.invalid/live".into(),
            title: None,
        },
        mode: ReviewedSourceMode::Append,
        ..base.clone()
    };
    assert_eq!(
        surface
            .control_reviewed_source(&Client::LoopbackHttp, &stream)
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument,
    );
    assert_eq!(surveys.load(Ordering::SeqCst), 0);
    assert_eq!(reads.load(Ordering::SeqCst), 0);
    for value in [
        json!({"zone":"Den","expected_revision":base.expected_revision,"mode":"end",
            "source":{"kind":"favorite","favorite":"Morning"}}),
        json!({"zone":"Den","expected_revision":base.expected_revision,"mode":"play",
            "source":{"kind":"favorite","favorite":"Morning","source_uri":"untrusted"}}),
        json!({"zone":"Den","expected_revision":base.expected_revision,"mode":"play",
            "source":{"kind":"playlist","playlist":"Evening","metadata":"untrusted"}}),
    ] {
        assert!(serde_json::from_value::<ReviewedSourceRequest>(value).is_err());
    }
}

struct RenamedAudience(Arc<AtomicUsize>);

impl Transport for RenamedAudience {
    fn soap_post(&self, _: IpAddr, _: &str, action: &str, _: &str) -> Result<String, ProtoError> {
        let verb = action.trim_matches('"').rsplit('#').next().unwrap_or("");
        if verb != "GetZoneGroupState" {
            self.0.fetch_add(1, Ordering::SeqCst);
            return Err(ProtoError::NotWired(
                "a renamed room must fail before cap or playback I/O",
            ));
        }
        // Exactly the original group, endpoints and player IDs. Only the
        // logical room name (and therefore its possible policy cap) changed.
        let topology = r#"<ZoneGroups><ZoneGroup Coordinator="RINCON_DEN" ID="group">
            <ZoneGroupMember UUID="RINCON_DEN" ZoneName="Bedroom" SWGen="1"
                Location="http://192.0.2.10:1400/xml/device_description.xml"/>
            <ZoneGroupMember UUID="RINCON_KIT1" ZoneName="Kitchen" SWGen="1"
                Location="http://192.0.2.11:1400/xml/device_description.xml"/>
            </ZoneGroup></ZoneGroups>"#;
        let args = fsonos_proto::soap::args_xml(&[("ZoneGroupState", topology)]);
        Ok(format!(
            "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>\
             <u:GetZoneGroupStateResponse xmlns:u=\"urn:x\">{args}</u:GetZoneGroupStateResponse>\
             </s:Body></s:Envelope>"
        ))
    }
}

#[test]
fn unchanged_physical_members_cannot_hide_a_renamed_policy_audience() {
    let writes = Arc::new(AtomicUsize::new(0));
    let mut h = households();
    h[0].players
        .iter_mut()
        .find(|player| player.id.0 == "RINCON_KIT1")
        .unwrap()
        .ip = "192.0.2.11".parse().unwrap();
    let known = h.clone();
    let surface = Surface::new(
        Box::new(RenamedAudience(Arc::clone(&writes))),
        Box::new(move |_| Ok(known.clone())),
        Policy::default(),
        Box::new(SystemClock),
    );
    let request = ReviewedSourceRequest {
        source: ReviewedSource::Uri {
            source_uri: "https://audio.example.invalid/live".into(),
            title: None,
        },
        ..request(&Rooms::from(&h), "Den")
    };
    let failure = surface
        .control_reviewed_source(&Client::LoopbackHttp, &request)
        .unwrap_err();
    assert_eq!(failure.code, ErrorCode::NotCoordinator);
    assert_eq!(
        writes.load(Ordering::SeqCst),
        0,
        "no stale room-name cap or playback step may run"
    );
}

#[test]
fn reviewed_tracks_validate_every_occurrence_and_use_play_or_queue_permission() {
    let h = households();
    let mut request = request(&Rooms::from(&h), "Den");
    request.source = tracks();
    for mode in [
        ReviewedSourceMode::Play,
        ReviewedSourceMode::Next,
        ReviewedSourceMode::Append,
    ] {
        request.mode = mode;
        request.validate().unwrap();
        assert_eq!(
            request.tool(),
            if mode == ReviewedSourceMode::Play {
                "play"
            } else {
                super::super::queue::ADD
            }
        );
        let selected = request.tracks_request().unwrap();
        assert_eq!(selected.next, mode == ReviewedSourceMode::Next);
        assert!(selected.expected_revision.is_none());
    }
    let value = serde_json::json!({
        "zone":"Den",
        "expected_revision":request.expected_revision,
        "mode":"append",
        "source":{"kind":"tracks","tracks":[
            {"source_uri":"spotify:track:1000000000000000000001","title":"First"},
            {"source_uri":"https://open.spotify.com/episode/1000000000000000000002"},
            {"source_uri":"spotify:track:1000000000000000000001","title":"Repeated"}
        ]}
    });
    let parsed: ReviewedSourceRequest = serde_json::from_value(value.clone()).unwrap();
    parsed.validate().unwrap();
    let selected = parsed.tracks_request().unwrap();
    assert_eq!(selected.tracks.len(), 3);
    assert_eq!(selected.tracks[0].source_uri, selected.tracks[2].source_uri);
    assert_ne!(selected.tracks[0].title, selected.tracks[2].title);
    let (surface, surveys, reads) = surface(Policy::default());
    for tracks in [
        Vec::new(),
        vec![selected.tracks[0].clone(); super::super::queue::MAX_QUEUE_TRACKS + 1],
        vec![QueueTrackInput {
            source_uri: "spotify:album:1000000000000000000001".into(),
            title: None,
        }],
        vec![QueueTrackInput {
            source_uri: "spotify:track:invalid".into(),
            title: None,
        }],
    ] {
        let invalid = ReviewedSourceRequest {
            source: ReviewedSource::Tracks { tracks },
            ..parsed.clone()
        };
        assert_eq!(
            surface
                .control_reviewed_source(&Client::LoopbackHttp, &invalid)
                .unwrap_err()
                .code,
            ErrorCode::InvalidArgument
        );
    }
    assert_eq!(surveys.load(Ordering::SeqCst), 0);
    assert_eq!(reads.load(Ordering::SeqCst), 0);
    for source in [
        json!({"kind":"tracks"}),
        json!({"kind":"tracks","tracks":[{"source_uri":"spotify:track:1000000000000000000001","metadata":"raw"}]}),
        json!({"kind":"tracks","tracks":[],"source_uri":"discarded"}),
        json!({"kind":"tracks","tracks":[],"expected_revision":"queue-token"}),
    ] {
        let mut invalid = value.clone();
        invalid["source"] = source;
        assert!(serde_json::from_value::<ReviewedSourceRequest>(invalid).is_err());
    }
}

#[test]
fn reviewed_line_in_is_play_only_and_keeps_play_authorization() {
    let h = households();
    let request = ReviewedSourceRequest {
        source: ReviewedSource::LineIn {
            source: "RINCON_DEN".into(),
        },
        ..request(&Rooms::from(&h), "Den")
    };
    request.validate().unwrap();
    assert_eq!(request.tool(), "play");
    let (surface, surveys, reads) = surface(Policy::default());
    for mode in [ReviewedSourceMode::Next, ReviewedSourceMode::Append] {
        assert_eq!(
            surface
                .control_reviewed_source(
                    &Client::LoopbackHttp,
                    &ReviewedSourceRequest {
                        mode,
                        ..request.clone()
                    },
                )
                .unwrap_err()
                .code,
            ErrorCode::InvalidArgument
        );
    }
    for source in ["", " ", "RINCON_DEN\n"] {
        let invalid = ReviewedSourceRequest {
            source: ReviewedSource::LineIn {
                source: source.into(),
            },
            ..request.clone()
        };
        assert_eq!(
            surface
                .control_reviewed_source(&Client::LoopbackHttp, &invalid)
                .unwrap_err()
                .code,
            ErrorCode::InvalidArgument
        );
    }
    assert_eq!(surveys.load(Ordering::SeqCst), 0);
    assert_eq!(reads.load(Ordering::SeqCst), 0);
    let policy = Policy::from_toml("[clients.'loopback-http']\ndeny = ['play']").unwrap();
    let (denied, surveys, reads) = self::surface(policy);
    assert_eq!(
        denied
            .control_reviewed_source(&Client::LoopbackHttp, &request)
            .unwrap_err()
            .code,
        ErrorCode::PolicyDenied
    );
    assert_eq!(surveys.load(Ordering::SeqCst), 0);
    assert_eq!(reads.load(Ordering::SeqCst), 0);
}

#[test]
fn reviewed_line_in_never_substitutes_a_source_room_or_alias_for_its_uuid() {
    let h = households();
    let aliases = Aliases::parse("[aliases]\ninput = ['Den']").unwrap();
    let rooms = Rooms {
        households: &h,
        aliases: Some(&aliases),
        client: None,
    };
    let (surface, _, reads) = surface(Policy::default());
    for source in ["Den", "input", "RINCON_MISSING"] {
        let request = ReviewedSourceRequest {
            source: ReviewedSource::LineIn {
                source: source.into(),
            },
            ..request(&rooms, "Den")
        };
        let failure = request.plan(&surface, &rooms).unwrap_err();
        assert_eq!(failure.code, ErrorCode::InvalidArgument);
        assert!(failure.detail.contains("exact input player UUID"));
    }
    let stale = ReviewedSourceRequest {
        source: ReviewedSource::LineIn {
            source: "RINCON_DEN".into(),
        },
        expected_revision: "0".repeat(64),
        ..request(&rooms, "Den")
    };
    assert_eq!(
        stale.plan(&surface, &rooms).unwrap_err().code,
        ErrorCode::NotCoordinator
    );
    assert_eq!(reads.load(Ordering::SeqCst), 0);
}

#[test]
fn reviewed_line_in_json_accepts_only_its_exact_input_field() {
    let value = json!({
        "zone": "Den",
        "expected_revision": "a".repeat(64),
        "mode": "play",
        "source": {"kind": "line_in", "source": "RINCON_INPUT"}
    });
    let request: ReviewedSourceRequest = serde_json::from_value(value.clone()).unwrap();
    request.validate().unwrap();
    assert_eq!(serde_json::to_value(request).unwrap(), value);
    for source in [
        json!({"kind": "line_in"}),
        json!({"kind": "line_in", "source": 42}),
        json!({"kind": "line_in", "source": "RINCON_INPUT", "source_uri": "untrusted"}),
        json!({"kind": "line_in", "source": "RINCON_INPUT", "metadata": "untrusted"}),
        json!({"kind": "line_in", "source": "RINCON_INPUT", "title": "invented"}),
    ] {
        let mut invalid = value.clone();
        invalid["source"] = source;
        assert!(serde_json::from_value::<ReviewedSourceRequest>(invalid).is_err());
    }
}
