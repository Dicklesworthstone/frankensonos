//! Quiet hours on the shared surface, against fsonos-sim, with the house
//! clock injected. At 23:30 an agent's volume 60 is held to the quiet-hours
//! cap; the result is noted, logged as a clamp, and confirmed by the sim's
//! volume. The CLI, which the policy leaves uncapped, is not held. At 08:00
//! the agent's same request goes through. The clock is a `FakeClock` handed
//! to the `Surface` here, in test code: no release build can be given one.

mod e2e;

use chrono::{DateTime, FixedOffset};
use e2e::Scenario;
use fsonos_api::plan::plan_volume;
use fsonos_api::surface::{Surface, Survey};
use fsonos_api::{NoteCode, VolumeRequest};
use fsonos_core::clock::{Clock, FakeClock};
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::{ActionFilter, MemStore};
use fsonos_core::{control, resolve_room};
use fsonos_sim::SimHousehold;
use std::sync::Arc;
use std::time::Duration;

/// Caps of 70, with no step limit in the way, and quiet hours 22:00 to
/// 07:00 at 25.
const POLICY: &str = "[defaults]\nmax_volume = 70\nmax_step = 100\n\n\
                      [quiet_hours]\nstart = \"22:00\"\nend = \"07:00\"\nmax_volume = 25\n";

/// The test's clock, shared with the surface.
struct Shared(Arc<FakeClock>);

impl Clock for Shared {
    fn now(&self) -> DateTime<FixedOffset> {
        self.0.now()
    }
}

fn at(time: &str) -> DateTime<FixedOffset> {
    DateTime::parse_from_rfc3339(time).expect("an RFC 3339 time")
}

fn office_to(level: i64) -> VolumeRequest {
    VolumeRequest {
        zone: "Office".into(),
        volume: Some(level),
        delta: None,
        group: false,
    }
}

#[test]
fn quiet_hours_hold_agents_down_at_night_and_not_by_day() {
    let mut s = Scenario::start("quiet-hours");
    s.sim(SimHousehold::standard());
    let lan = s.lan();
    let survey: Survey = Box::new(|t| {
        Ok(fsonos_core::inventory::survey(t, &[], Duration::from_millis(500))?.households)
    });
    let houses = fsonos_core::inventory::survey(&lan, &[], Duration::from_millis(500))
        .expect("the sim answers")
        .households;
    let office = resolve_room(&houses, "Office")
        .expect("Office is in the sim")
        .player
        .id
        .clone();
    let policy = Policy::from_toml(POLICY).expect("the test policy parses");
    let clock = Arc::new(FakeClock::new(at("2026-10-07T23:30:00-04:00")));
    let surface = Surface::new(
        Box::new(s.lan()),
        survey,
        policy,
        Box::new(Shared(Arc::clone(&clock))),
    )
    .with_action_log(Box::new(MemStore::default()), "mcp");
    let agent = Client::Tailnet("tag:agent".into());
    let level = || control::volume(&lan, &houses, &office).ok();
    let _ = control::set_volume(&lan, &houses, &office, 20);

    let night = surface.control(&agent, "set_volume", |h| plan_volume(h, &office_to(60)));
    let noted = night.as_ref().is_ok_and(|o| {
        o.volume == Some(25)
            && o.notes.iter().any(|n| {
                n.code == NoteCode::VolumeClamped
                    && n.detail.contains("during quiet hours (22:00–07:00)")
            })
    });
    s.check(
        "night-agent",
        "surface",
        "at 23:30 an agent's 60 is held to the quiet-hours 25, with the note saying why",
        noted && level() == Some(25),
        format!("{night:?}; sim volume {:?}", level()),
    );

    let logged = surface
        .recent_actions(&agent, &ActionFilter::default())
        .map(|log| log.first().map(|a| a.action.decision.clone()));
    s.check(
        "night-log",
        "surface",
        "the action log records the clamp and why",
        logged.as_ref().is_ok_and(|d| {
            d.as_deref()
                .is_some_and(|d| d.starts_with("clamp: ") && d.contains("quiet hours"))
        }),
        format!("{logged:?}"),
    );

    let cli = surface.control(&Client::Cli, "set_volume", |h| {
        plan_volume(h, &office_to(60))
    });
    s.check(
        "night-cli",
        "surface",
        "the CLI, uncapped by the policy, is not held by quiet hours",
        cli.as_ref()
            .is_ok_and(|o| o.volume == Some(60) && o.notes.is_empty())
            && level() == Some(60),
        format!("{cli:?}; sim volume {:?}", level()),
    );

    let _ = control::set_volume(&lan, &houses, &office, 20);
    clock.set(at("2026-10-08T08:00:00-04:00"));
    let day = surface.control(&agent, "set_volume", |h| plan_volume(h, &office_to(60)));
    s.check(
        "day-agent",
        "surface",
        "at 08:00 the agent's same 60 goes through, with nothing to note",
        day.as_ref()
            .is_ok_and(|o| o.volume == Some(60) && o.notes.is_empty())
            && level() == Some(60),
        format!("{day:?}; sim volume {:?}", level()),
    );
    s.finish();
}
