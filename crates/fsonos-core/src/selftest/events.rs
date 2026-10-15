//! [`EVENTS`]: the daemon follows each player through four event services;
//! on one group's coordinator per household, each must SUBSCRIBE, send its
//! full-state event, accept a renewal and UNSUBSCRIBE, as the live model's
//! subscriptions do.

use super::{HOUSE, Labeled, Shared, scrub_ip};
use crate::doctor::{Check, CheckContext, CheckId, CheckResult};
use crate::events::Service;
use fsonos_proto::net::{EventSink, Lan};
use serde_json::json;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const EVENTS: CheckId = CheckId("selftest.events");

/// The services the live model subscribes to on every player.
const SERVICES: [Service; 4] = [
    Service::AvTransport,
    Service::RenderingControl,
    Service::GroupRenderingControl,
    Service::ZoneGroupTopology,
];

/// How long a new subscription's full-state event may take.
const FIRST_EVENT: Duration = Duration::from_secs(10);

/// A first event slower than this is a warning (as [`super::SLOW_READ`]).
const SLOW_EVENT: Duration = Duration::from_millis(1500);

/// One service's round trip: how long its first event took, or why it failed.
type Trip = (&'static str, Result<Duration, String>);

pub(super) struct EventsCheck(pub(super) Arc<Shared>);

impl Check for EventsCheck {
    fn id(&self) -> CheckId {
        EVENTS
    }
    fn title(&self) -> &'static str {
        "Events from every service the daemon follows"
    }
    fn requires(&self) -> &[CheckId] {
        &[HOUSE]
    }
    fn timeout(&self) -> Option<Duration> {
        Some(Duration::from_secs(120))
    }
    fn run(&self, ctx: &CheckContext) -> CheckResult {
        let labeled = match self.0.house() {
            Ok(l) => l,
            Err(failed) => return *failed,
        };
        let mut rows = Vec::new();
        let mut failed = Vec::new();
        let mut slow = Vec::new();
        for (h, house) in labeled.houses.iter().enumerate() {
            let Some(group) = house.groups.first() else {
                continue;
            };
            let Some(ip) = labeled.ip(h, &group.coordinator) else {
                continue;
            };
            let at = format!("{} {}", Labeled::house(h), labeled.room(&group.coordinator));
            let trips = match round_trips(&self.0.lan, ip, ctx) {
                Ok(trips) => trips,
                Err(why) => {
                    failed.push(format!("{at}: {why}"));
                    continue;
                }
            };
            for (service, trip) in trips {
                match &trip {
                    Ok(after) if *after > SLOW_EVENT => {
                        slow.push(format!("{at} {service} ({} ms)", after.as_millis()));
                    }
                    Ok(_) => {}
                    Err(why) => failed.push(format!("{at} {service}: {why}")),
                }
                rows.push(json!({
                    "at": at,
                    "service": service,
                    "first_event_ms": trip.as_ref().ok().map(Duration::as_millis),
                    "error": trip.as_ref().err(),
                }));
            }
        }
        verdict(&rows, &failed, &slow)
    }
}

fn verdict(rows: &[serde_json::Value], failed: &[String], slow: &[String]) -> CheckResult {
    let count = rows.len();
    let evidence = json!({ "subscriptions": rows });
    if count == 0 && failed.is_empty() {
        return CheckResult::skip("no group to subscribe to").with_evidence(evidence);
    }
    if !failed.is_empty() {
        return CheckResult::fail(
            format!(
                "{} of {count} event round trips failed: {}",
                failed.len(),
                failed.join("; ")
            ),
            "The daemon learns of changes through these events: check that this host's \
             firewall accepts the players' connections (`fsonos doctor`), then run the \
             self-test again.",
        )
        .with_evidence(evidence);
    }
    if !slow.is_empty() {
        return CheckResult::warn(
            format!(
                "all {count} event round trips worked; slow first events: {}",
                slow.join(", ")
            ),
            "The network or a player is struggling; the daemon still works, later.",
        )
        .with_evidence(evidence);
    }
    CheckResult::pass(format!(
        "all {count} event round trips worked (subscribe, first event, renew, unsubscribe)"
    ))
    .with_evidence(evidence)
}

/// On the player at `ip`, each of [`SERVICES`] in turn: SUBSCRIBE, wait for
/// its full-state event, renew, UNSUBSCRIBE (always, whatever happened).
fn round_trips(lan: &Lan, ip: IpAddr, ctx: &CheckContext) -> Result<Vec<Trip>, String> {
    let local = lan
        .local_address_toward(ip)
        .map_err(|e| scrub_ip(&e.to_string()))?;
    let sink = EventSink::start(SocketAddr::new(local, 0)).map_err(|e| scrub_ip(&e.to_string()))?;
    let mut trips = Vec::new();
    for service in SERVICES {
        let (name, path) = (service.tag(), service.event_path());
        let started = Instant::now();
        let sub = match lan.subscribe(ip, path, &sink.callback_url(name), 60) {
            Ok(sub) => sub,
            Err(e) => {
                trips.push((
                    name,
                    Err(format!("SUBSCRIBE: {}", scrub_ip(&e.to_string()))),
                ));
                continue;
            }
        };
        let mut first = None;
        while first.is_none() && started.elapsed() < FIRST_EVENT && !ctx.is_cancelled() {
            if let Some(event) = sink.recv_timeout(Duration::from_millis(100))
                && event.sid == sub.sid
            {
                first = Some(started.elapsed());
            }
        }
        let renewed = lan.renew(ip, path, &sub.sid, 60);
        let _ = lan.unsubscribe(ip, path, &sub.sid);
        trips.push((
            name,
            match (first, renewed) {
                (Some(after), Ok(_)) => Ok(after),
                (None, _) => Err(format!("no event within {} s", FIRST_EVENT.as_secs())),
                (Some(_), Err(e)) => Err(format!("renew: {}", scrub_ip(&e.to_string()))),
            },
        ));
    }
    Ok(trips)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::Status;

    fn row(at: &str, ms: Option<u128>, error: Option<&str>) -> serde_json::Value {
        json!({ "at": at, "first_event_ms": ms, "error": error })
    }

    #[test]
    fn the_verdict_names_failures_then_slow_events() {
        let ok = vec![row("H1 R1", Some(40), None)];
        assert_eq!(verdict(&ok, &[], &[]).status, Status::Pass);
        let slow = verdict(&ok, &[], &["H1 R1 AVTransport (2100 ms)".into()]);
        assert_eq!(slow.status, Status::Warn);
        assert!(slow.summary.contains("2100 ms"), "{}", slow.summary);
        let failed = verdict(
            &[row("H1 R1", None, Some("no event within 10 s"))],
            &["H1 R1 ZoneGroupTopology: no event within 10 s".into()],
            &[],
        );
        assert_eq!(failed.status, Status::Fail);
        assert!(failed.summary.contains("1 of 1"), "{}", failed.summary);
        assert_eq!(verdict(&[], &[], &[]).status, Status::Skip);
    }
}
