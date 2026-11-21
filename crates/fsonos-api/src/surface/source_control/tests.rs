use super::*;
use crate::zones::fixtures::households;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::Policy;
use fsonos_core::rooms::Aliases;
use fsonos_proto::ProtoError;
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
    let reads = Arc::new(AtomicUsize::new(0));
    let commands = request
        .plan(&NoSpeakerIo(Arc::clone(&reads)), &rooms)
        .unwrap();
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
    let reads = Arc::new(AtomicUsize::new(0));
    let wire = NoSpeakerIo(Arc::clone(&reads));
    let selected = request(&rooms, "set");
    assert_eq!(
        selected
            .plan(
                &wire,
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
                &wire,
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
        default.plan(&wire, &rooms).unwrap_err().code,
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
