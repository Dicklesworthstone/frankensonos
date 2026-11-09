use super::*;
use crate::surface::announce::{AnnouncedDto, Announcements};
use fsonos_core::announce::clip::MediaStore;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::Policy;
use fsonos_proto::{ProtoError, Transport};
use std::net::IpAddr;

struct NoIo;

impl Transport for NoIo {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("job inspection/cancellation and refused admission must not contact speakers")
    }
}

fn surface(policy: Policy) -> Surface {
    Surface::new(
        Box::new(NoIo),
        Box::new(|_| panic!("no job read surveys speakers")),
        policy,
        Box::new(SystemClock),
    )
    .with_announcements(Announcements::new(
        MediaStore::new(std::path::Path::new("/unused-job-test")),
        Box::new(|| Some("http://192.0.2.1:3400".into())),
    ))
}

fn report(outcome: &str, restored: bool) -> AnnounceDto {
    AnnounceDto {
        done: format!("announcement {outcome}"),
        clean: outcome == "finished" && restored,
        households: vec![AnnouncedDto {
            lead: "Kitchen".into(),
            rooms: vec!["Kitchen".into()],
            levels: Vec::new(),
            outcome: outcome.into(),
            error: None,
            not_restored: if restored {
                Vec::new()
            } else {
                vec!["manual source retained".into()]
            },
        }],
    }
}

fn insert(surface: &Surface, client: &Client, at: i64) -> Arc<Job> {
    surface
        .announcements
        .as_ref()
        .unwrap()
        .jobs
        .insert(client, "speech", vec!["Kitchen@S1".into()], at)
        .unwrap()
}

#[test]
fn cancellation_is_a_request_until_the_worker_finishes_restoration() {
    let surface = surface(Policy::default());
    let job = insert(&surface, &Client::McpStdio, 10);
    let first = surface
        .cancel_announcement(&Client::McpStdio, &job.id)
        .unwrap();
    assert!(first.cancel_requested);
    assert!(!first.can_cancel);
    assert_eq!(first.phase, AnnouncementJobPhase::Queued);
    assert_eq!(first.finished_at, None);
    assert!(first.result.is_none());
    assert_eq!(
        surface
            .cancel_announcement(&Client::McpStdio, &job.id)
            .unwrap(),
        first
    );
    job.finish(&Ok(report("cancelled", true)), 20);
    let final_state = surface
        .get_announcement(&Client::McpStdio, &job.id)
        .unwrap();
    assert_eq!(final_state.phase, AnnouncementJobPhase::Cancelled);
    assert_eq!(final_state.finished_at, Some(20));
    assert_eq!(
        final_state.result.unwrap().households[0].outcome,
        "cancelled"
    );
}

#[test]
fn a_late_cancellation_neither_changes_the_terminal_receipt_nor_sets_the_token() {
    let surface = surface(Policy::default());
    let job = insert(&surface, &Client::McpStdio, 10);
    job.finish(&Ok(report("finished", true)), 20);
    let before = surface
        .get_announcement(&Client::McpStdio, &job.id)
        .unwrap();
    assert_eq!(
        surface
            .cancel_announcement(&Client::McpStdio, &job.id)
            .unwrap(),
        before
    );
    assert!(!job.control.is_cancelled());
    job.finish(&Err(Failure::new(ErrorCode::Internal, "late failure")), 30);
    assert_eq!(
        surface
            .get_announcement(&Client::McpStdio, &job.id)
            .unwrap(),
        before
    );
}

#[test]
fn cancellation_that_cannot_restore_remains_a_failed_job_with_its_details() {
    let surface = surface(Policy::default());
    let job = insert(&surface, &Client::McpStdio, 10);
    job.control.cancel();
    job.finish(&Ok(report("cancelled", false)), 20);
    let status = job.view();
    assert_eq!(status.phase, AnnouncementJobPhase::Failed);
    assert!(status.cancel_requested && !status.can_cancel);
    assert_eq!(
        status.result.unwrap().households[0].not_restored,
        ["manual source retained"]
    );
}

#[test]
fn ordinary_callers_cannot_read_or_cancel_another_callers_jobs() {
    let surface = surface(Policy::default().with_owner(Some("owner@example.com".into())));
    let alice = Client::Tailnet("alice@example.com".into());
    let bob = Client::Tailnet("bob@example.com".into());
    let job = insert(&surface, &alice, 10);
    assert!(surface.list_announcements(&bob).unwrap().is_empty());
    assert_eq!(
        surface.get_announcement(&bob, &job.id).unwrap_err().code,
        ErrorCode::NoMatch
    );
    assert_eq!(
        surface.cancel_announcement(&bob, &job.id).unwrap_err().code,
        ErrorCode::NoMatch
    );
    assert!(!job.control.is_cancelled());
    for owner in [Client::Cli, Client::Tailnet("OWNER@example.com".into())] {
        assert_eq!(surface.list_announcements(&owner).unwrap().len(), 1);
        assert_eq!(
            surface.get_announcement(&owner, &job.id).unwrap().id,
            job.id
        );
    }
    assert!(
        surface
            .cancel_announcement(&Client::Cli, &job.id)
            .unwrap()
            .cancel_requested
    );
}

#[test]
fn policy_applies_before_job_reads_cancellation_or_background_admission() {
    let surface = Arc::new(surface(
        Policy::from_toml("[clients.mcp-stdio]\nallow = [\"list_zones\"]\n").unwrap(),
    ));
    let job = insert(&surface, &Client::McpStdio, 10);
    assert_eq!(
        surface
            .list_announcements(&Client::McpStdio)
            .unwrap_err()
            .code,
        ErrorCode::PolicyDenied
    );
    assert_eq!(
        surface
            .get_announcement(&Client::McpStdio, &job.id)
            .unwrap_err()
            .code,
        ErrorCode::PolicyDenied
    );
    assert_eq!(
        surface
            .cancel_announcement(&Client::McpStdio, &job.id)
            .unwrap_err()
            .code,
        ErrorCode::PolicyDenied
    );
    let request = AnnounceRequest {
        chime: Some("bell".into()),
        ..AnnounceRequest::default()
    };
    assert_eq!(
        surface
            .start_announcement(&Client::McpStdio, &request)
            .unwrap_err()
            .code,
        ErrorCode::PolicyDenied
    );
    assert!(!job.control.is_cancelled());
}

#[test]
fn retained_receipts_are_bounded_and_never_evict_active_work() {
    let surface = surface(Policy::default());
    let active = insert(&surface, &Client::Cli, 1);
    let old = insert(&surface, &Client::Cli, 2);
    old.finish(&Ok(report("finished", true)), 3);
    for at in 3..100 {
        let next = insert(&surface, &Client::Cli, at);
        next.finish(&Ok(report("finished", true)), at);
    }
    let jobs = surface.list_announcements(&Client::Cli).unwrap();
    assert_eq!(jobs.len(), MAX_RETAINED);
    assert!(jobs.iter().any(|job| job.id == active.id));
    assert!(!jobs.iter().any(|job| job.id == old.id));
    assert_eq!(jobs[0].created_at, 99);
    let encoded = serde_json::to_value(&jobs[0]).unwrap();
    assert!(
        encoded.get("text").is_none()
            && encoded.get("wav_base64").is_none()
            && encoded.get("owner").is_none()
    );
}

#[test]
fn an_exact_background_grant_does_not_require_the_legacy_synchronous_tool() {
    let surface = Arc::new(Surface::new(
        Box::new(NoIo),
        Box::new(|_| panic!("disabled media must fail before discovery")),
        Policy::from_toml("[clients.mcp-stdio]\nallow = [\"announce_start\"]\n").unwrap(),
        Box::new(SystemClock),
    ));
    let request = AnnounceRequest {
        chime: Some("bell".into()),
        ..AnnounceRequest::default()
    };
    assert_eq!(
        surface
            .start_announcement(&Client::McpStdio, &request)
            .unwrap_err()
            .code,
        ErrorCode::NotImplemented,
        "the exact new tool is admitted and reaches media readiness"
    );
    assert_eq!(
        surface
            .announce(&Client::McpStdio, &request)
            .unwrap_err()
            .code,
        ErrorCode::PolicyDenied,
        "the distinct synchronous tool remains denied"
    );
}

#[test]
fn invalid_ids_and_an_empty_or_restarted_registry_do_not_dispatch() {
    let surface = surface(Policy::default());
    for id in [
        "",
        "../another",
        "ABCDEF0123456789abcdef0123456789",
        "a?b=1",
    ] {
        assert_eq!(
            surface.get_announcement(&Client::Cli, id).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
        assert_eq!(
            surface
                .cancel_announcement(&Client::Cli, id)
                .unwrap_err()
                .code,
            ErrorCode::InvalidArgument
        );
    }
    assert!(surface.list_announcements(&Client::Cli).unwrap().is_empty());
    assert_eq!(
        surface
            .get_announcement(&Client::Cli, &"a".repeat(32))
            .unwrap_err()
            .code,
        ErrorCode::NoMatch
    );
}
