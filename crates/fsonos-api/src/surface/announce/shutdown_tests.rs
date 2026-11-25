//! Shutdown races retain admitted controls and never start untracked work.

use super::*;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::Policy;
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::{Generation, HouseholdId, Player, ZoneGroup};
use std::net::IpAddr;
use std::sync::mpsc;
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(10);

struct NoIo;

impl Transport for NoIo {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("shutdown must refuse preparation before speaker I/O")
    }
}

fn household() -> HouseholdState {
    let player = PlayerId("RINCON_TEST_KITCHEN".into());
    HouseholdState {
        id: Some(HouseholdId("TEST_HOUSEHOLD".into())),
        players: vec![Player {
            id: player.clone(),
            room_name: "Kitchen".into(),
            ip: "192.0.2.10".parse().unwrap(),
            model: String::new(),
            generation: Generation::S2,
        }],
        groups: vec![ZoneGroup {
            coordinator: player.clone(),
            members: vec![player.clone()],
        }],
        rooms: vec![Room {
            name: "Kitchen".into(),
            primary: player.clone(),
            players: vec![player.clone()],
            missing: Vec::new(),
            coordinator: player,
        }],
    }
}

fn announcements() -> Announcements {
    Announcements::new(
        MediaStore::new(std::path::Path::new("/unused-shutdown-test")),
        Box::new(|| Some("http://192.0.2.1:3400".into())),
    )
}

#[test]
fn shutdown_during_target_resolution_refuses_sync_background_and_reserved_runs() {
    enum Call {
        Synchronous,
        Background,
        Scheduled,
    }
    for call in [Call::Synchronous, Call::Background, Call::Scheduled] {
        let (entered, resolving) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let blocked = Mutex::new(blocked);
        let surface = Arc::new(
            Surface::new(
                Box::new(NoIo),
                Box::new(move |_| {
                    entered.send(()).unwrap();
                    blocked.lock().unwrap().recv_timeout(WAIT).unwrap();
                    Ok(vec![household()])
                }),
                Policy::default(),
                Box::new(SystemClock),
            )
            .with_announcements(announcements()),
        );
        let request = AnnounceRequest {
            chime: Some("bell".into()),
            rooms: vec!["Kitchen".into()],
            ..AnnounceRequest::default()
        };
        let permit = matches!(call, Call::Scheduled)
            .then(|| surface.reserve_announcement(&Client::Cli).unwrap());
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| match call {
                Call::Synchronous => surface.announce(&Client::Cli, &request).map(|_| ()),
                Call::Background => surface
                    .start_announcement(&Client::Cli, &request)
                    .map(|_| ()),
                Call::Scheduled => surface
                    .announce_admitted(&Client::Cli, &request, permit)
                    .map(|_| ()),
            });
            resolving.recv_timeout(WAIT).unwrap();
            assert_eq!(surface.announcements_pending(), 1);
            surface.stop_announcements();
            assert!(surface.list_announcements(&Client::Cli).unwrap().is_empty());
            assert_eq!(
                surface
                    .reserve_announcement(&Client::Cli)
                    .err()
                    .unwrap()
                    .code,
                ErrorCode::NotReady
            );
            release.send(()).unwrap();
            let refused = worker.join().unwrap().unwrap_err();
            assert_eq!(refused.code, ErrorCode::NotReady);
            assert!(refused.detail.contains("stopping"), "{refused:?}");
        });
        assert_eq!(surface.announcements_pending(), 0);
        assert!(surface.list_announcements(&Client::Cli).unwrap().is_empty());
    }
}

#[test]
fn a_published_job_is_cancelled_before_a_late_worker_can_generate_speech() {
    let surface = Surface::new(
        Box::new(NoIo),
        Box::new(|_| Ok(vec![household()])),
        Policy::default(),
        Box::new(SystemClock),
    )
    .with_announcements(announcements());
    let request = AnnounceRequest {
        text: Some("The worker has not started yet".into()),
        rooms: vec!["Kitchen".into()],
        ..AnnounceRequest::default()
    };
    let (prepared, turn, job) = surface
        .prepare_announcement(&Client::Cli, &request, None, TOOL)
        .unwrap();
    assert_eq!(surface.announcements_pending(), 1);
    surface.stop_announcements();
    assert!(job.control.is_cancelled());
    job.preparing();
    let result = surface.deliver_announcement(&Client::Cli, prepared, &job.control);
    job.finish(&result, 20);
    drop(turn);
    let report = result.unwrap();
    assert_eq!(report.households[0].outcome, "cancelled");
    assert!(report.households[0].levels.is_empty());
    assert!(report.households[0].not_restored.is_empty());
    let jobs = surface.list_announcements(&Client::Cli).unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].phase, AnnouncementJobPhase::Cancelled);
    assert_eq!(jobs[0].finished_at, Some(20));
    assert_eq!(surface.announcements_pending(), 0);
}
