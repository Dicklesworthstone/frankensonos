use super::*;
use crate::events::{Service, Subscriber, Want};
use crate::playback::Playback;
use crate::reconcile::Health;
use fsonos_proto::ProtoError;
use fsonos_proto::gena::{Notify, Subscription};
use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use std::time::Duration;

fn fixture() -> (SimHandle, Reconciler) {
    let sim = SimHousehold::builder()
        .s1([SimPlayerSpec::new("Legacy", SimModel::Play5Gen1)])
        .s2([SimPlayerSpec::new("Modern", SimModel::One)])
        .spawn()
        .unwrap();
    let mut rec = Reconciler::new(
        Duration::from_secs(300),
        Duration::from_secs(300),
        Instant::now(),
    );
    let seeds: Vec<_> = sim.players().iter().map(|p| p.ip).collect();
    rec.refresh_inventory(&sim.lan(), &seeds, Instant::now())
        .unwrap();
    assert_eq!(rec.households.len(), 2);
    (sim, rec)
}

// Pure subscription-state fixture; inventory reads themselves use the real
// simulator HTTP endpoints. Finishing an inventory has no Subscriber input.
struct Lease;

impl Subscriber for Lease {
    fn subscribe(&self, _: &Want, _: &str) -> Result<Subscription, ProtoError> {
        Ok(Subscription {
            sid: "uuid:inventory-test".into(),
            timeout_secs: 1800,
        })
    }

    fn renew(&self, want: &Want, _: &str) -> Result<Subscription, ProtoError> {
        self.subscribe(want, "")
    }

    fn unsubscribe(&self, _: &Want, _: &str) -> Result<(), ProtoError> {
        panic!("inventory application must not issue network operations")
    }
}

fn callback(_: Service) -> String {
    "http://127.0.0.1:3400/RenderingControl".into()
}

#[test]
fn finishing_inventory_preserves_newer_events_health_and_their_subscription_cursor() {
    let (sim, mut rec) = fixture();
    let player = rec.households[0].players[0].clone();
    let other = rec.households[1].players[0].clone();
    let now = Instant::now();
    rec.subscriptions.sync(
        &Lease,
        &[Want {
            player: player.id.clone(),
            host: player.ip,
            service: Service::RenderingControl,
        }],
        callback,
        now,
    );
    let plan = rec.inventory_plan(&[]);
    let mut survey = plan.read(&sim.lan()).unwrap();
    // The read says unreachable, but a later event proves the player alive.
    survey
        .unreachable
        .push((player.ip.to_string(), "old failed probe".into()));
    let event = Notify {
        sid: "uuid:inventory-test".into(),
        seq: 0,
        path: "/RenderingControl".into(),
        properties: vec![(
            "LastChange".into(),
            "<Event><InstanceID val=\"0\"><Volume channel=\"Master\" val=\"59\"/>\
             <Mute channel=\"Master\" val=\"0\"/></InstanceID></Event>"
                .into(),
        )],
    };
    let mut playback = Playback::default();
    rec.on_notify(
        &Lease,
        &mut playback,
        &event,
        callback,
        now + Duration::from_secs(1),
    )
    .unwrap();
    // Conversely, successful old reads must not erase newer failed renewals.
    rec.health.failed(&other.id, "new renewal failure");
    let health = rec.health.of(&player.id).cloned();
    let other_health = rec.health.of(&other.id).cloned();
    assert!(
        rec.finish_inventory(&plan, Ok(survey), Instant::now())
            .unwrap()
            .is_some()
    );
    assert_eq!(rec.health.of(&player.id), health.as_ref());
    assert_eq!(rec.health.of(&other.id), other_health.as_ref());
    assert_eq!(playback.of(&player.id).unwrap().volume, Some(59));
    assert_eq!(rec.subscriptions.len(), 1);
    assert!(
        rec.subscriptions.route(&event).is_none(),
        "SEQ 0 remains consumed"
    );
    let next = Notify { seq: 1, ..event };
    assert!(rec.subscriptions.route(&next).is_some());
    sim.shutdown();
}

#[test]
fn an_equal_returning_topology_cannot_revive_an_old_inventory_plan() {
    let (sim, mut rec) = fixture();
    let plan = rec.inventory_plan(&[]);
    let survey = plan.read(&sim.lan()).unwrap();
    let before = rec.households.clone();
    rec.invalidate_inventory();
    rec.invalidate_inventory();
    let now = Instant::now();
    assert!(
        rec.finish_inventory(&plan, Ok(survey), now)
            .unwrap()
            .is_none()
    );
    assert_eq!(rec.households, before);
    assert!(rec.schedule.next_at() > now);
    assert!(rec.schedule.next_at() <= now + crate::events::RETRY);
    sim.shutdown();
}

#[test]
fn newer_player_presence_rejects_a_survey_that_removes_or_relocates_it() {
    for relocate in [false, true] {
        let (sim, mut rec) = fixture();
        let player = rec.households[0].players[0].clone();
        let plan = rec.inventory_plan(&[]);
        let mut survey = plan.read(&sim.lan()).unwrap();
        let house = survey
            .households
            .iter_mut()
            .find(|h| h.player(&player.id).is_some())
            .unwrap();
        if relocate {
            house
                .players
                .iter_mut()
                .find(|p| p.id == player.id)
                .unwrap()
                .ip = "192.0.2.200".parse().unwrap();
        } else {
            house.players.retain(|p| p.id != player.id);
        }
        rec.health
            .ok(&player.id, Instant::now() + Duration::from_secs(1));
        let before = rec.households.clone();
        assert!(
            rec.finish_inventory(&plan, Ok(survey), Instant::now())
                .unwrap()
                .is_none()
        );
        assert_eq!(rec.households, before);
        assert_eq!(rec.health.of(&player.id).unwrap().health, Health::Healthy);
        sim.shutdown();
    }
}

#[test]
fn real_disappearance_is_committed_when_no_later_event_contradicts_it() {
    let (sim, mut rec) = fixture();
    let player = rec.households[0].players[0].clone();
    let plan = rec.inventory_plan(&[]);
    let mut survey = plan.read(&sim.lan()).unwrap();
    survey.households.retain(|h| h.player(&player.id).is_none());
    let report = rec
        .finish_inventory(&plan, Ok(survey), Instant::now())
        .unwrap()
        .unwrap();
    assert!(report.missing.contains(&player.id));
    assert!(
        rec.households
            .iter()
            .all(|h| h.player(&player.id).is_none())
    );
    assert_eq!(rec.health.of(&player.id).unwrap().health, Health::Offline);
    assert!(!plan.current(&rec));
    // The independent discovery hints still remember the absent household.
    assert!(
        rec.inventory_plan(&[])
            .known
            .iter()
            .any(|h| h.player(&player.id).is_some())
    );
    sim.shutdown();
}

#[test]
fn failed_background_surveys_preserve_state_and_retain_exponential_backoff() {
    let (sim, mut rec) = fixture();
    let before = rec.households.clone();
    let now = Instant::now();
    let mut last = Duration::ZERO;
    for n in 0..3 {
        let at = now + Duration::from_secs(n * 1000);
        let plan = rec.inventory_plan(&[]);
        let failed = Err(CoreError::Proto(ProtoError::NotWired(
            "failed inventory test",
        )));
        assert!(rec.finish_inventory(&plan, failed, at).is_err());
        let delay = rec.schedule.next_at().duration_since(at);
        assert!(delay > last);
        last = delay;
        assert!(plan.current(&rec));
        assert_eq!(rec.households, before);
    }
    sim.shutdown();
}
