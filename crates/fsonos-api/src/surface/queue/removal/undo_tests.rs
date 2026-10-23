//! Queue recovery has its own complete permission preflight and preserves
//! policy reductions before the saved playback can become audible again.

use super::{QueueRemoveRequest, Surface};
use fsonos_core::HouseholdState;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::MemStore;
use fsonos_proto::content::{QUEUE, browse_snapshot};
use fsonos_proto::control as soap;
use fsonos_proto::play_mode::set_play_mode;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use fsonos_types::{PlayMode, PlayerId, TransportState};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

const FIRST: &str = "x-file-cifs://example.invalid/first.flac";
const SECOND: &str = "x-file-cifs://example.invalid/second.flac";
const TOOLS: [&str; 9] = [
    "undo_last",
    "pause",
    "add_to_queue",
    "remove_from_queue",
    "play_queue",
    "seek",
    "resume",
    "set_play_mode",
    "set_volume",
];

struct Fixture {
    sim: SimHandle,
    surface: Surface,
}

impl Fixture {
    fn new(s2: bool, playing: bool) -> Self {
        let model = if s2 {
            SimModel::One
        } else {
            SimModel::Play5Gen1
        };
        let players = [
            SimPlayerSpec::new("Alpha", model),
            SimPlayerSpec::new("Beta", model),
        ];
        let builder = SimHousehold::builder();
        let sim = if s2 {
            builder.s2(players)
        } else {
            builder.s1(players)
        }
        .spawn()
        .unwrap();
        let lan = sim.lan();
        let alpha = sim.player("Alpha").unwrap();
        let beta = sim.player("Beta").unwrap();
        let coordinator = PlayerId(alpha.uuid.clone());
        soap::join_group(&lan, beta.ip, &coordinator).unwrap();
        soap::set_volume(&lan, alpha.ip, 35).unwrap();
        soap::set_volume(&lan, beta.ip, 25).unwrap();
        for uri in [FIRST, SECOND] {
            soap::add_uri_to_queue(&lan, alpha.ip, uri, "", false).unwrap();
        }
        soap::play_from_queue(&lan, alpha.ip, &coordinator).unwrap();
        soap::seek_track(&lan, alpha.ip, 2).unwrap();
        soap::seek_position(&lan, alpha.ip, 17).unwrap();
        set_play_mode(&lan, alpha.ip, PlayMode::RepeatAll).unwrap();
        soap::play(&lan, alpha.ip).unwrap();
        if !playing {
            soap::pause(&lan, alpha.ip).unwrap();
        }
        let mut household = HouseholdState::default();
        household.apply_topology(&get_zone_group_state(&lan, alpha.ip).unwrap());
        let surface = Surface::new(
            Box::new(sim.lan()),
            Box::new(move |_| Ok(vec![household.clone()])),
            Policy::default(),
            Box::new(SystemClock),
        )
        .with_action_log(Box::new(MemStore::default()), "test");
        Self { sim, surface }
    }

    fn remove(&self) -> super::QueueRemovedDto {
        self.surface
            .queue_remove(
                &Client::McpStdio,
                &QueueRemoveRequest {
                    zone: "Beta".into(),
                    position: 1,
                    expected_uris: vec![FIRST.into()],
                    expected_revision: None,
                },
            )
            .unwrap()
    }

    fn policy(&self, cap: u8, omitted: &[&str]) {
        let allowed: Vec<_> = TOOLS
            .iter()
            .filter(|tool| !omitted.contains(*tool))
            .copied()
            .collect();
        let text = format!(
            "[defaults]\nmax_volume = {cap}\n[clients.\"mcp-stdio\"]\nallow = {}\n",
            serde_json::to_string(&allowed).unwrap()
        );
        *self.surface.policy.write().unwrap() = Arc::new(Policy::from_toml(&text).unwrap());
    }

    fn writes(&self, from: usize) -> Vec<String> {
        self.sim.soap_log()[from..]
            .iter()
            .filter(|entry| !entry.action.starts_with("Get") && entry.action != "Browse")
            .map(|entry| entry.action.clone())
            .collect()
    }

    fn queue(&self) -> Vec<String> {
        browse_snapshot(&self.sim.lan(), self.sim.player("Alpha").unwrap().ip, QUEUE)
            .unwrap()
            .objects
            .into_iter()
            .map(|item| item.res.unwrap().uri)
            .collect()
    }
}

#[test]
fn undo_cannot_use_any_reconstruction_tool_missing_from_the_callers_allowlist() {
    for missing in &TOOLS[1..] {
        let f = Fixture::new(true, true);
        assert!(f.remove().undoable);
        f.policy(70, &[*missing]);
        let start = f.sim.soap_log().len();
        let report = f.surface.undo(&Client::McpStdio, true).unwrap().unwrap();
        assert!(!report.failures.is_empty(), "{missing}: {report:?}");
        assert!(
            report
                .failures
                .iter()
                .any(|(_, detail)| detail.contains(*missing)),
            "{missing}: {report:?}"
        );
        assert!(
            f.writes(start).is_empty(),
            "{missing}: no mutation precedes complete authority"
        );
        assert_eq!(f.queue(), [SECOND]);
    }
}

#[test]
fn a_paused_queue_is_restored_without_resume_or_volume_privileges() {
    for s2 in [false, true] {
        let f = Fixture::new(s2, false);
        assert!(f.remove().undoable);
        f.policy(10, &["resume", "set_volume"]);
        let start = f.sim.soap_log().len();
        let report = f.surface.undo(&Client::McpStdio, true).unwrap().unwrap();
        assert!(report.failures.is_empty(), "{report:?}");
        assert_eq!(f.queue(), [FIRST, SECOND]);
        let writes = f.writes(start);
        assert!(
            !writes
                .iter()
                .any(|name| name == "Play" || name == "SetVolume" || name == "SetMute")
        );
        assert_ne!(
            soap::get_transport_info(&f.sim.lan(), f.sim.player("Alpha").unwrap().ip)
                .unwrap()
                .state,
            TransportState::Playing
        );
    }
}

#[test]
fn stricter_current_caps_lower_every_room_before_any_reconstruction_or_play() {
    for s2 in [false, true] {
        let f = Fixture::new(s2, true);
        assert!(f.remove().undoable);
        f.policy(10, &[]);
        let start = f.sim.soap_log().len();
        let report = f.surface.undo(&Client::McpStdio, true).unwrap().unwrap();
        assert!(report.failures.is_empty(), "{report:?}");
        assert_eq!(f.queue(), [FIRST, SECOND]);
        let writes = f.writes(start);
        assert_eq!(&writes[..2], ["SetVolume", "SetVolume"]);
        assert!(writes.iter().any(|name| name == "Play"));
        assert!(!writes.iter().any(|name| name == "SetMute"));
        for name in ["Alpha", "Beta"] {
            assert_eq!(
                soap::get_volume(&f.sim.lan(), f.sim.player(name).unwrap().ip).unwrap(),
                10
            );
        }
    }
}

#[test]
fn a_failed_action_log_never_advertises_or_replays_an_already_confirmed_removal() {
    let f = Fixture::new(true, true);
    let failed = catch_unwind(AssertUnwindSafe(|| {
        let _held = f.surface.log.as_ref().unwrap().store.lock().unwrap();
        panic!("synthetic unavailable action store");
    }));
    assert!(failed.is_err());
    let start = f.sim.soap_log().len();
    let result = f.remove();
    assert!(!result.undoable);
    assert!(result.done.contains("action log could not be saved"));
    assert_eq!(f.queue(), [SECOND]);
    assert_eq!(f.writes(start), ["RemoveTrackRangeFromQueue"]);
}
