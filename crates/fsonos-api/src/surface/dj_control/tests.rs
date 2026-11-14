use super::*;
use crate::dj::DjSteer;
use crate::zones::fixtures::households;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::Policy;
use fsonos_core::rooms::Aliases;
use fsonos_proto::{ProtoError, Transport};
use serde_json::json;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

fn request(rooms: &Rooms<'_>, target: &str, action: ReviewedDjAction) -> ReviewedDjRequest {
    ReviewedDjRequest {
        zone: target.into(),
        expected_revision: resolve_view(rooms, target).unwrap().revision,
        action,
        mood: None,
        constraints: SteerConstraints::default(),
        for_secs: None,
        clear: false,
    }
}

#[test]
fn reviewed_aliases_deduplicate_groups_and_preserve_the_exact_direction() {
    let h = households();
    let aliases = Aliases::parse("[aliases]\nset = ['Den', 'Kitchen@S1', 'Kitchen@S2']").unwrap();
    let rooms = Rooms {
        households: &h,
        aliases: Some(&aliases),
        client: None,
    };
    let mut request = request(&rooms, "set", ReviewedDjAction::Start);
    assert_eq!(request.plan(&rooms).unwrap().len(), 2);
    request.mood = Some("calm".into());
    request.constraints.include_genres = vec!["jazz".into()];
    request.for_secs = Some(7200);
    request.validate().unwrap();
    for command in request.plan(&rooms).unwrap() {
        let Command::DjSteer {
            steer, start: true, ..
        } = command
        else {
            panic!("expected a directed start")
        };
        assert_eq!(
            *steer,
            DjSteer::Set {
                mood: request.mood.clone(),
                constraints: request.constraints.clone(),
                for_secs: Some(7200),
            }
        );
    }
    request.action = ReviewedDjAction::Steer;
    request.clear = true;
    request.mood = None;
    request.constraints = SteerConstraints::default();
    request.for_secs = None;
    request.validate().unwrap();
    assert!(request.plan(&rooms).unwrap().iter().all(|command| matches!(
        command, Command::DjSteer { steer, start: false, .. } if **steer == DjSteer::Clear
    )));
}

#[test]
fn an_alias_cannot_expand_after_review_even_if_its_original_groups_remain() {
    let h = households();
    let first = Aliases::parse("[aliases]\nset = ['Den']").unwrap();
    let expanded = Aliases::parse("[aliases]\nset = ['Den', 'Kitchen@S2']").unwrap();
    let rooms = Rooms {
        households: &h,
        aliases: Some(&first),
        client: None,
    };
    let request = request(&rooms, "set", ReviewedDjAction::Stop);
    let changed = Rooms {
        aliases: Some(&expanded),
        ..rooms
    };
    let error = request.plan(&changed).unwrap_err();
    assert_eq!(error.code, ErrorCode::NotCoordinator);
    assert!(error.detail.contains("nothing was changed"));
}

#[test]
fn a_revision_cannot_be_reused_for_another_target_or_client_default() {
    let h = households();
    let aliases =
        Aliases::parse("[defaults]\nroom = 'Den'\n[defaults.clients.guest]\nroom = 'Kitchen@S2'")
            .unwrap();
    let rooms = Rooms {
        households: &h,
        aliases: Some(&aliases),
        client: None,
    };
    let mut request = request(&rooms, "here", ReviewedDjAction::Skip);
    assert_eq!(
        request
            .plan(&Rooms {
                client: Some("guest"),
                ..rooms
            })
            .unwrap_err()
            .code,
        ErrorCode::NotCoordinator
    );
    request.zone = "all".into();
    assert_eq!(
        request.plan(&rooms).unwrap_err().code,
        ErrorCode::NotCoordinator
    );
}

struct NoSpeakerIo(Arc<AtomicUsize>);
impl Transport for NoSpeakerIo {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(ProtoError::NotWired(
            "no speaker I/O before review validation",
        ))
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
fn stale_review_fails_before_readiness_or_session_access() {
    let (surface, surveys, reads) = surface(Policy::default());
    let h = households();
    let mut request = request(&Rooms::from(&h), "Den", ReviewedDjAction::Start);
    request.expected_revision = "0".repeat(64);
    assert_eq!(
        surface
            .control_reviewed_dj(&Client::Unknown, &request)
            .unwrap_err()
            .code,
        ErrorCode::NotCoordinator
    );
    assert_eq!(surveys.load(Ordering::SeqCst), 1);
    assert_eq!(reads.load(Ordering::SeqCst), 0);
}

#[test]
fn reviewed_actions_enforce_the_existing_tool_policy_before_discovery() {
    for action in [
        ReviewedDjAction::Start,
        ReviewedDjAction::Steer,
        ReviewedDjAction::Skip,
        ReviewedDjAction::Stop,
    ] {
        let policy = Policy::from_toml(&format!(
            "[clients.'loopback-http']\ndeny = ['{}']",
            action.tool()
        ))
        .unwrap();
        let (surface, surveys, reads) = surface(policy);
        let h = households();
        let mut request = request(&Rooms::from(&h), "Den", action);
        request.clear = action == ReviewedDjAction::Steer;
        assert_eq!(
            surface
                .control_reviewed_dj(&Client::LoopbackHttp, &request)
                .unwrap_err()
                .code,
            ErrorCode::PolicyDenied
        );
        assert_eq!(surveys.load(Ordering::SeqCst), 0);
        assert_eq!(reads.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn malformed_revisions_and_ignored_action_fields_are_rejected_before_discovery() {
    let (surface, surveys, reads) = surface(Policy::default());
    let h = households();
    let base = request(&Rooms::from(&h), "Den", ReviewedDjAction::Start);
    let mut invalid = Vec::new();
    for revision in [
        String::new(),
        "a".repeat(63),
        "A".repeat(64),
        "g".repeat(64),
    ] {
        invalid.push(ReviewedDjRequest {
            expected_revision: revision,
            ..base.clone()
        });
    }
    invalid.push(ReviewedDjRequest {
        clear: true,
        ..base.clone()
    });
    invalid.push(ReviewedDjRequest {
        for_secs: Some(60),
        ..base.clone()
    });
    invalid.push(ReviewedDjRequest {
        action: ReviewedDjAction::Steer,
        ..base.clone()
    });
    for action in [ReviewedDjAction::Skip, ReviewedDjAction::Stop] {
        invalid.push(ReviewedDjRequest {
            action,
            mood: Some("calm".into()),
            ..base.clone()
        });
        invalid.push(ReviewedDjRequest {
            action,
            clear: true,
            ..base.clone()
        });
    }
    for request in invalid {
        assert_eq!(
            surface
                .control_reviewed_dj(&Client::Unknown, &request)
                .unwrap_err()
                .code,
            ErrorCode::InvalidArgument
        );
    }
    assert_eq!(surveys.load(Ordering::SeqCst), 0);
    assert_eq!(reads.load(Ordering::SeqCst), 0);
    assert!(serde_json::from_value::<ReviewedDjRequest>(json!({
        "zone": "Den", "expected_revision": base.expected_revision, "action": "stop", "start": true,
    })).is_err());
}
