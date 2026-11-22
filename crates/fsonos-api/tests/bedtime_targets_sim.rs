//! Whole-house bedtime controls use real simulator SOAP, retain their original
//! groups, and account for every success or refusal without replaying writes.

use fsonos_api::surface::schedules::{
    FadePauseRequest, Sleep, SleepOutcome, SleepRequest, SleepSetDto,
};
use fsonos_api::{ControlResultDto, ErrorCode, Surface};
use fsonos_core::actions::BeforeState;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::rooms::ControlTarget;
use fsonos_core::store::{ActionFilter, LoggedAction, MemStore};
use fsonos_core::{HouseholdState, inventory, resolve_room};
use fsonos_proto::control as soap;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec, SoapLogEntry};
use fsonos_types::{PlayerId, TransportState};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const GROUPS: [&str; 4] = ["Lead@S1", "Guest@S1", "Lead@S2", "Guest@S2"];

struct House {
    sim: Arc<SimHandle>,
    households: Vec<HouseholdState>,
    directory: tempfile::TempDir,
}

impl House {
    fn new() -> Self {
        let sim = Arc::new(
            SimHousehold::builder()
                .s1(["Lead", "Member", "Guest"]
                    .map(|room| SimPlayerSpec::new(room, SimModel::Play5Gen1)))
                .s2(["Lead", "Member", "Guest"].map(|room| SimPlayerSpec::new(room, SimModel::One)))
                .spawn()
                .unwrap(),
        );
        for generation in [1, 2] {
            let player = |room| {
                sim.players()
                    .iter()
                    .find(|p| p.generation == generation && p.room == room)
                    .unwrap()
            };
            soap::join_group(
                &sim.lan(),
                player("Member").ip,
                &PlayerId(player("Lead").uuid.clone()),
            )
            .unwrap();
        }
        let households = inventory::survey(&sim.lan(), &[], Duration::from_millis(500))
            .unwrap()
            .households;
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("aliases.toml"),
            "[aliases]\nsingle = ['Member@S1', 'Lead@S1']\nselected = ['Member@S1', 'Lead@S1', 'Member@S2', 'Lead@S2']\n").unwrap();
        Self {
            sim,
            households,
            directory,
        }
    }

    fn target(&self, zone: &str) -> ControlTarget<'_> {
        resolve_room(&self.households, zone).unwrap()
    }

    fn surface(&self, daemon: bool) -> Surface {
        self.over(Box::new(self.sim.lan()), daemon)
    }

    fn over(&self, transport: Box<dyn Transport + Send + Sync>, daemon: bool) -> Surface {
        let known = self.households.clone();
        let surface = Surface::new(
            transport,
            Box::new(move |_| Ok(known.clone())),
            Policy::default(),
            Box::new(SystemClock),
        )
        .with_aliases_file(self.directory.path().join("aliases.toml"))
        .with_action_log(Box::new(MemStore::default()), "bedtime-targets");
        if daemon {
            surface.with_sleep(Sleep::default())
        } else {
            surface
        }
    }

    fn writes(&self, start: usize) -> Vec<SoapLogEntry> {
        self.sim
            .soap_log()
            .into_iter()
            .skip(start)
            .filter(|call| {
                !call.action.starts_with("Get")
                    && !matches!(call.action.as_str(), "Browse" | "Search")
            })
            .collect()
    }

    fn native_timer(&self, zone: &str) -> Option<u32> {
        soap::get_remaining_sleep_timer(&self.sim.lan(), self.target(zone).coordinator.ip).unwrap()
    }

    fn play(&self, zone: &str) {
        let ip = self.target(zone).coordinator.ip;
        soap::set_av_transport_uri(
            &self.sim.lan(),
            ip,
            "x-rincon-mp3radio://stream.example.invalid/bedtime.mp3",
            "",
        )
        .unwrap();
        soap::play(&self.sim.lan(), ip).unwrap();
    }
}

fn request(zone: &str) -> SleepRequest {
    SleepRequest {
        zone: zone.into(),
        duration: Some("45m".into()),
        ..SleepRequest::default()
    }
}

fn batch(outcome: SleepOutcome) -> SleepSetDto {
    let SleepOutcome::Groups(receipt) = outcome else {
        panic!("expected a multi-group receipt")
    };
    receipt
}

fn actions(surface: &Surface) -> Vec<LoggedAction> {
    surface
        .recent_actions(&Client::Cli, &ActionFilter::default())
        .unwrap()
}

fn barrier(action: &LoggedAction) -> Option<String> {
    action
        .action
        .before_state
        .as_deref()
        .map(|state| BeforeState::parse(state).unwrap())
        .and_then(|state| state.not_undoable)
}

#[test]
fn timers_set_extend_read_and_cancel_each_distinct_group_on_both_households() {
    for daemon in [false, true] {
        let house = House::new();
        let surface = house.surface(daemon);
        for (target, count) in [("selected", 2), ("all", 4)] {
            let start = house.sim.soap_log().len();
            let result = batch(
                surface
                    .set_sleep_timers(&Client::Cli, &request(target))
                    .unwrap(),
            );
            assert!(result.complete && result.changed);
            assert_eq!(result.applied.len(), count);
            assert!(result.failed.is_empty());
            assert_eq!(
                house
                    .writes(start)
                    .iter()
                    .filter(|c| c.action == "ConfigureSleepTimer")
                    .count(),
                count
            );
            for row in &result.applied {
                let zone = format!("{}@{}", row.timer.zone, row.household);
                assert_eq!(row.coordinator, house.target(&zone).coordinator.id.0);
                assert_eq!(row.timer.fades, daemon);
                assert_eq!(row.timer.remaining_secs, Some(2700));
            }
        }
        assert_eq!(actions(&surface).len(), 2, "one boundary per user action");
        assert!(
            actions(&surface)
                .iter()
                .all(|a| barrier(a).as_deref() == Some("sleep timer"))
        );
        assert_eq!(
            surface
                .sleep_timers(&Client::Cli, Some("selected"))
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            surface
                .sleep_timers(&Client::Cli, Some("all@S1"))
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            surface
                .sleep_timers(&Client::Cli, Some("all"))
                .unwrap()
                .len(),
            4
        );
        let extend = SleepRequest {
            duration: Some("15m".into()),
            extend: true,
            ..request("selected")
        };
        let extended = batch(surface.set_sleep_timers(&Client::Cli, &extend).unwrap());
        assert!(extended.complete);
        assert!(
            extended
                .applied
                .iter()
                .all(|r| (3590..=3600).contains(&r.timer.remaining_secs.unwrap()))
        );
        let cancel = SleepRequest {
            zone: "all".into(),
            cancel: true,
            ..SleepRequest::default()
        };
        let cleared = batch(surface.set_sleep_timers(&Client::Cli, &cancel).unwrap());
        assert!(cleared.complete && cleared.changed);
        assert_eq!(cleared.applied.len(), 4);
        assert!(
            cleared
                .applied
                .iter()
                .all(|r| r.timer.ends.is_none() && r.timer.remaining_secs.is_none())
        );
        assert!(
            surface
                .sleep_timers(&Client::Cli, Some("all"))
                .unwrap()
                .is_empty()
        );
        let before = house.sim.soap_log().len();
        let absent = batch(surface.set_sleep_timers(&Client::Cli, &cancel).unwrap());
        assert!(absent.complete && !absent.changed);
        assert!(house.writes(before).is_empty());
        assert!(barrier(&actions(&surface)[0]).is_none());
    }
}

#[test]
fn same_group_alias_keeps_the_scalar_response_and_a_reviewed_request_is_supported() {
    let house = House::new();
    let surface = house.surface(false);
    for reviewed in [false, true] {
        let mut req = request("single");
        if reviewed {
            req.expected_revision = Some(
                surface
                    .resolve_rooms(&Client::Cli, "single")
                    .unwrap()
                    .revision,
            );
        }
        let start = house.sim.soap_log().len();
        let result = surface.set_sleep_timers(&Client::Cli, &req).unwrap();
        assert!(matches!(result, SleepOutcome::Single(_)));
        assert_eq!(house.writes(start).len(), 1);
    }
    assert_eq!(actions(&surface).len(), 2);
}

#[test]
fn extension_keeps_missing_timers_as_explicit_failures_including_an_all_failed_receipt() {
    for daemon in [false, true] {
        let house = House::new();
        let surface = house.surface(daemon);
        surface
            .set_sleep_timers(&Client::Cli, &request("Lead@S1"))
            .unwrap();
        let req = SleepRequest {
            extend: true,
            duration: Some("15m".into()),
            ..request("selected")
        };
        let start = house.sim.soap_log().len();
        let result = batch(surface.set_sleep_timers(&Client::Cli, &req).unwrap());
        assert!(!result.complete && result.changed);
        assert_eq!(result.applied.len(), 1);
        assert_eq!(result.applied[0].household, "S1");
        assert_eq!(result.failed.len(), 1);
        assert_eq!(result.failed[0].household, "S2");
        assert_eq!(house.writes(start).len(), 1);
        assert!(house.native_timer("Lead@S2").is_none());
        let start = house.sim.soap_log().len();
        let missing = batch(
            surface
                .set_sleep_timers(
                    &Client::Cli,
                    &SleepRequest {
                        zone: "all@S2".into(),
                        ..req
                    },
                )
                .unwrap(),
        );
        assert!(!missing.complete && !missing.changed);
        assert!(missing.applied.is_empty());
        assert_eq!(missing.failed.len(), 2);
        assert!(house.writes(start).is_empty());
        assert!(barrier(&actions(&surface)[0]).is_none());
    }
}

#[test]
fn a_stale_review_writes_nothing_and_a_changed_physical_group_cannot_expand_the_audience() {
    let house = House::new();
    let surface = house.surface(false);
    let revision = surface
        .resolve_rooms(&Client::Cli, "selected")
        .unwrap()
        .revision;
    let start = house.sim.soap_log().len();
    let mut req = request("selected");
    req.expected_revision = Some("0".repeat(64));
    assert_eq!(
        surface
            .set_sleep_timers(&Client::Cli, &req)
            .unwrap_err()
            .code,
        ErrorCode::NotCoordinator
    );
    assert!(house.writes(start).is_empty());
    soap::join_group(
        &house.sim.lan(),
        house.target("Guest@S1").coordinator.ip,
        &house.target("Lead@S1").coordinator.id,
    )
    .unwrap();
    req.expected_revision = Some(revision);
    let start = house.sim.soap_log().len();
    let result = batch(surface.set_sleep_timers(&Client::Cli, &req).unwrap());
    assert_eq!(result.applied.len(), 1);
    assert_eq!(result.applied[0].household, "S2");
    assert_eq!(result.failed.len(), 1);
    assert_eq!(result.failed[0].household, "S1");
    assert_eq!(result.failed[0].error.code, ErrorCode::NotCoordinator);
    assert_eq!(house.writes(start).len(), 1);
    assert!(house.native_timer("Lead@S1").is_none());
}

struct ChangeAlias {
    lan: SimLan,
    path: PathBuf,
    changed: AtomicBool,
}

impl Transport for ChangeAlias {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if action.contains("#ConfigureSleepTimer") && !self.changed.swap(true, Ordering::SeqCst) {
            std::fs::write(
                &self.path,
                "[aliases]\nselected = ['Guest@S1', 'Guest@S2']\n",
            )
            .unwrap();
        }
        self.lan.soap_post(host, path, action, body)
    }
}

#[test]
fn an_alias_edit_during_timer_dispatch_does_not_redirect_remaining_groups() {
    let house = House::new();
    let surface = house.over(
        Box::new(ChangeAlias {
            lan: house.sim.lan(),
            path: house.directory.path().join("aliases.toml"),
            changed: AtomicBool::new(false),
        }),
        false,
    );
    let result = batch(
        surface
            .set_sleep_timers(&Client::Cli, &request("selected"))
            .unwrap(),
    );
    assert!(result.complete);
    assert_eq!(result.applied.len(), 2);
    for row in result.applied {
        assert_eq!(row.timer.zone, "Lead");
    }
    for zone in ["Lead@S1", "Lead@S2"] {
        assert!(house.native_timer(zone).is_some());
    }
    for zone in ["Guest@S1", "Guest@S2"] {
        assert!(house.native_timer(zone).is_none());
    }
    assert_eq!(actions(&surface).len(), 1);
}

struct LostWrite {
    lan: SimLan,
    host: IpAddr,
    verb: &'static str,
    lost: AtomicBool,
}

impl Transport for LostWrite {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let reply = self.lan.soap_post(host, path, action, body)?;
        if host == self.host
            && action
                .trim_matches('"')
                .ends_with(&format!("#{}", self.verb))
            && !self.lost.swap(true, Ordering::SeqCst)
        {
            return Err(ProtoError::Network {
                target: "synthetic-bedtime-player".into(),
                detail: "accepted the write but lost the reply".into(),
            });
        }
        Ok(reply)
    }
}

#[test]
fn a_lost_timer_reply_is_accounted_without_replaying_it_or_losing_the_undo_barrier() {
    let house = House::new();
    let surface = house.over(
        Box::new(LostWrite {
            lan: house.sim.lan(),
            host: house.target("Lead@S1").coordinator.ip,
            verb: "ConfigureSleepTimer",
            lost: AtomicBool::new(false),
        }),
        false,
    );
    let before = house.sim.soap_log().len();
    let result = batch(
        surface
            .set_sleep_timers(&Client::Cli, &request("selected"))
            .unwrap(),
    );
    assert!(!result.complete);
    assert_eq!(result.applied.len(), 1);
    assert_eq!(result.failed.len(), 1);
    assert_eq!(result.failed[0].error.code, ErrorCode::OutcomeUncertain);
    assert!(!result.failed[0].error.retryable);
    assert_eq!(house.writes(before).len(), 2);
    for zone in ["Lead@S1", "Lead@S2"] {
        assert!(house.native_timer(zone).is_some());
    }
    let logged = actions(&surface);
    assert_eq!(logged.len(), 1);
    assert_eq!(barrier(&logged[0]).as_deref(), Some("sleep timer"));
    assert!(logged[0].action.result.contains("OUTCOME_UNCERTAIN"));
}

#[test]
fn whole_house_fades_pause_each_playing_group_restore_volumes_and_skip_inactive_groups() {
    let house = House::new();
    for player in house.sim.players() {
        soap::set_volume(&house.sim.lan(), player.ip, 37).unwrap();
    }
    for zone in ["Lead@S1", "Lead@S2", "Guest@S2"] {
        house.play(zone);
    }
    soap::pause(&house.sim.lan(), house.target("Guest@S2").coordinator.ip).unwrap();
    let surface = house.surface(true);
    let revision = surface.resolve_rooms(&Client::Cli, "all").unwrap().revision;
    let before = house.sim.soap_log().len();
    let result = surface
        .fade_out(
            &Client::Cli,
            &FadePauseRequest {
                zone: "all".into(),
                fade_secs: 1,
                expected_revision: Some(revision),
            },
        )
        .unwrap();
    assert!(result.complete() && result.changed);
    let batch = result.batch.unwrap();
    assert_eq!(batch.results.len(), 4);
    let writes = house.writes(before);
    assert_eq!(writes.iter().filter(|w| w.action == "Pause").count(), 2);
    assert!(writes.iter().all(|w| w.room != "Guest"));
    for zone in GROUPS {
        let target = house.target(zone);
        let row = batch
            .results
            .iter()
            .find(|r| r.target == target.coordinator.id.0)
            .unwrap();
        assert!(
            matches!(row.result, ControlResultDto::Succeeded { changed, .. } if changed == zone.starts_with("Lead"))
        );
        let state = soap::get_transport_info(&house.sim.lan(), target.coordinator.ip)
            .unwrap()
            .state;
        assert!(matches!(
            state,
            TransportState::Paused | TransportState::Stopped
        ));
    }
    for player in house.sim.players() {
        assert_eq!(soap::get_volume(&house.sim.lan(), player.ip).unwrap(), 37);
    }
    let logged = actions(&surface);
    assert_eq!(logged.len(), 1);
    assert_eq!(barrier(&logged[0]).as_deref(), Some("fade-out"));
}

#[test]
fn a_lost_group_pause_keeps_other_fades_and_one_uncertain_receipt_without_replay() {
    let house = House::new();
    for zone in ["Lead@S1", "Lead@S2"] {
        house.play(zone);
    }
    let surface = house.over(
        Box::new(LostWrite {
            lan: house.sim.lan(),
            host: house.target("Lead@S1").coordinator.ip,
            verb: "Pause",
            lost: AtomicBool::new(false),
        }),
        true,
    );
    let before = house.sim.soap_log().len();
    let result = surface
        .fade_out(
            &Client::Cli,
            &FadePauseRequest {
                zone: "selected".into(),
                fade_secs: 1,
                expected_revision: None,
            },
        )
        .unwrap();
    assert!(!result.complete());
    let batch = result.batch.unwrap();
    assert_eq!(batch.results.len(), 2);
    assert!(batch.results.iter().any(|r| r.household == "S1" && matches!(&r.result, ControlResultDto::Failed { error } if error.code == ErrorCode::OutcomeUncertain)));
    assert!(
        batch
            .results
            .iter()
            .any(|r| r.household == "S2" && r.result.complete())
    );
    assert_eq!(
        house
            .writes(before)
            .iter()
            .filter(|w| w.action == "Pause")
            .count(),
        2
    );
    assert_eq!(actions(&surface).len(), 1);
    assert_eq!(barrier(&actions(&surface)[0]).as_deref(), Some("fade-out"));
}
