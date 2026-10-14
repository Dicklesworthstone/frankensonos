//! Keep the inventory current and know which players are healthy.
//!
//! GENA events ([`crate::events`]) keep state fresh between surveys; a
//! periodic survey catches what events cannot: a player that joined, left,
//! or moved to a new address. [`Refresh`] schedules surveys and backs off
//! while they keep failing. [`HealthBoard`] tracks each player from the
//! outcome of every survey, subscription and call. [`Reconciler::refresh`] is one step:
//! survey, replace the household model, record health, and bring the GENA
//! subscriptions in line with the new model. [`Reconciler::on_notify`] folds
//! each NOTIFY into the live state and resubscribes a player that rebooted.

use crate::events::{self, Service, Subscriber, Subscriptions};
use crate::inventory::{self, DISCOVERY_WAIT};
use crate::playback::{Changes, Playback};
use crate::{CoreError, HouseholdState};
use fsonos_proto::gena::Notify;
use fsonos_proto::topology::{VanishedDevice, parse_zone_group_state};
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::PlayerId;
use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

mod discovery;
mod initial;

/// How a player is doing, from the outcome of the calls made to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Healthy,
    /// The last call failed; the player may be busy or briefly unreachable.
    Degraded,
    /// Several calls in a row failed, or the last survey did not find it.
    Offline,
}

/// Failures in a row before a player counts as offline.
pub const OFFLINE_AFTER: u32 = 3;

/// One player's health record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerHealth {
    pub health: Health,
    pub last_ok: Option<Instant>,
    pub consecutive_failures: u32,
    pub last_error: Option<String>,
}

/// Health of every player the daemon has dealt with.
#[derive(Debug, Clone, Default)]
pub struct HealthBoard {
    players: HashMap<PlayerId, PlayerHealth>,
}

impl HealthBoard {
    #[must_use]
    pub fn of(&self, player: &PlayerId) -> Option<&PlayerHealth> {
        self.players.get(player)
    }

    /// Every player's record, in no particular order.
    pub fn iter(&self) -> impl Iterator<Item = (&PlayerId, &PlayerHealth)> {
        self.players.iter()
    }

    /// A call to `player` (or an event from it) succeeded.
    pub fn ok(&mut self, player: &PlayerId, now: Instant) {
        self.players.insert(
            player.clone(),
            PlayerHealth {
                health: Health::Healthy,
                last_ok: Some(now),
                consecutive_failures: 0,
                last_error: None,
            },
        );
    }

    /// A call to `player` failed.
    pub fn failed(&mut self, player: &PlayerId, error: impl Into<String>) {
        let h = self.players.entry(player.clone()).or_insert(PlayerHealth {
            health: Health::Healthy,
            last_ok: None,
            consecutive_failures: 0,
            last_error: None,
        });
        h.consecutive_failures += 1;
        h.last_error = Some(error.into());
        h.health = if h.consecutive_failures >= OFFLINE_AFTER {
            Health::Offline
        } else {
            Health::Degraded
        };
    }

    /// The last survey did not find `player` (it stopped answering, or left
    /// the household): offline at once, with `reason` kept.
    pub fn missing(&mut self, player: &PlayerId, reason: impl Into<String>) {
        self.failed(player, reason);
        if let Some(h) = self.players.get_mut(player) {
            h.health = Health::Offline;
        }
    }

    /// Record what an [`events`] pass saw at `now`: the players that
    /// answered are healthy (a quiet player recovers on its next renewal),
    /// then each failure counts against its player.
    pub fn record(&mut self, report: &events::Report, now: Instant) {
        for player in &report.answered {
            self.ok(player, now);
        }
        for (player, service) in &report.unanswered {
            self.failed(
                player,
                format!("{service:?}: subscription delivered no initial state"),
            );
        }
        for (player, service, error) in &report.failed {
            self.failed(player, format!("{service:?}: {error}"));
        }
    }
}

/// When the next survey runs.
#[derive(Debug, Clone)]
pub struct Refresh {
    interval: Duration,
    max_backoff: Duration,
    next_at: Instant,
    failures: u32,
    /// xorshift state for backoff jitter.
    jitter: u64,
}

impl Refresh {
    /// Survey every `interval`, after failures retry sooner at first and
    /// back off (doubling from a quarter of the interval) up to
    /// `max_backoff`. The first survey is due right away.
    #[must_use]
    pub fn new(interval: Duration, max_backoff: Duration, now: Instant) -> Self {
        Self {
            interval,
            max_backoff,
            next_at: now,
            failures: 0,
            // Any odd seed works; vary it per process so several daemons
            // that failed together do not retry in lockstep.
            jitter: u64::from(std::process::id()) << 1 | 1,
        }
    }

    #[must_use]
    pub fn due(&self, now: Instant) -> bool {
        now >= self.next_at
    }

    #[must_use]
    pub fn next_at(&self) -> Instant {
        self.next_at
    }

    pub fn succeeded(&mut self, now: Instant) {
        self.failures = 0;
        self.next_at = now + self.interval;
    }

    /// Make the next survey due at `now` (a command found a player gone,
    /// say). The backoff state is kept.
    pub fn due_now(&mut self, now: Instant) {
        self.next_at = self.next_at.min(now);
    }

    /// Record a failed survey; returns the delay until the retry: the
    /// backoff step plus up to 10% jitter.
    pub fn failed(&mut self, now: Instant) -> Duration {
        self.jitter ^= self.jitter << 13;
        self.jitter ^= self.jitter >> 7;
        self.jitter ^= self.jitter << 17;
        let permille = u32::try_from(self.jitter % 101).unwrap_or(0);
        self.failed_with(now, permille)
    }

    /// [`Refresh::failed`] with an explicit jitter in per-mille of the
    /// backoff step (0 gives the exact schedule: a quarter interval,
    /// doubling, capped at the maximum).
    pub fn failed_with(&mut self, now: Instant, jitter_permille: u32) -> Duration {
        let base = self.interval / 4;
        let step = base
            .checked_mul(1 << self.failures.min(16))
            .map_or(self.max_backoff, |d| d.min(self.max_backoff));
        let delay = step + step * jitter_permille / 1000;
        self.failures += 1;
        self.next_at = now + delay;
        delay
    }
}

/// What one [`refresh`] did.
#[derive(Debug, Default)]
pub struct RefreshReport {
    pub players: usize,
    pub households: usize,
    /// Players the previous model had that this survey did not find.
    pub missing: Vec<PlayerId>,
    /// Players whose `BootSeq` rose since last seen; their subscriptions
    /// were replaced.
    pub rebooted: Vec<PlayerId>,
    pub events: events::Report,
}

/// What one [`Reconciler::on_notify`] did.
#[derive(Debug, Default)]
pub struct NotifyReport {
    /// What the event changed in the playback state.
    pub changes: Changes,
    /// Players a topology event showed had rebooted (`BootSeq` rose); their
    /// subscriptions were replaced.
    pub rebooted: Vec<PlayerId>,
    /// Players a topology event dropped from their household (powered off,
    /// say): marked offline; the next survey settles their subscriptions.
    pub gone: Vec<PlayerId>,
    pub events: events::Report,
}

/// The reconcile loop's state: the household model, the GENA subscriptions,
/// player health, and the survey schedule.
#[derive(Debug)]
pub struct Reconciler {
    pub households: Vec<HouseholdState>,
    /// Players the households currently list as vanished, by room name: a
    /// command to one is to a powered-off speaker, not an unknown room.
    pub vanished: Vec<VanishedDevice>,
    pub subscriptions: Subscriptions,
    pub health: HealthBoard,
    pub schedule: Refresh,
    /// Last-known discovery hints, independent of the live room model.
    discovery: discovery::Discovery,
}

impl Reconciler {
    /// Survey every `interval`, backing off up to `max_backoff` after
    /// failures; the first survey is due right away.
    #[must_use]
    pub fn new(interval: Duration, max_backoff: Duration, now: Instant) -> Self {
        Self {
            households: Vec::new(),
            vanished: Vec::new(),
            subscriptions: Subscriptions::default(),
            health: HealthBoard::default(),
            schedule: Refresh::new(interval, max_backoff, now),
            discovery: discovery::Discovery::default(),
        }
    }

    /// One reconcile step: discover, probe known addresses for households
    /// discovery missed (including households absent for several passes),
    /// replace the live model, record health, and sync GENA.
    /// A failed or empty survey keeps the old model, backs the schedule off,
    /// and returns the error.
    pub fn refresh<L: Transport + Subscriber + ?Sized>(
        &mut self,
        lan: &L,
        seeds: &[IpAddr],
        callback_url: impl Fn(Service) -> String,
        now: Instant,
    ) -> Result<RefreshReport, CoreError> {
        self.discovery.observe(&self.households);
        let surveyed =
            inventory::survey_known(lan, seeds, self.discovery.households(), DISCOVERY_WAIT);
        let survey = match surveyed {
            Ok(s) if !s.households.is_empty() => s,
            Ok(_) => {
                let retry = self.schedule.failed(now);
                tracing::warn!(retry_in_s = retry.as_secs(), "survey found no players");
                return Err(CoreError::Proto(ProtoError::Network {
                    target: "survey".into(),
                    detail: "no players answered".into(),
                }));
            }
            Err(e) => {
                let retry = self.schedule.failed(now);
                tracing::warn!(retry_in_s = retry.as_secs(), error = %e, "survey failed");
                return Err(e);
            }
        };
        // A player that rebooted since last seen has forgotten its
        // subscriptions; drop them so the sync below subscribes afresh.
        let rebooted = self
            .subscriptions
            .reboots(survey.boot_seqs.iter().map(|(p, seq)| (p, *seq)));
        for p in &rebooted {
            let dropped = self.subscriptions.forget(p);
            tracing::info!(
                player = p.0.as_str(),
                cause = "BootSeq rose",
                action = "resubscribe",
                dropped,
                "player rebooted"
            );
        }
        let before: Vec<(PlayerId, IpAddr)> = self
            .households
            .iter()
            .flat_map(|h| h.players.iter().map(|p| (p.id.clone(), p.ip)))
            .collect();
        self.households = survey.households;
        self.discovery.observe(&self.households);
        self.vanished = survey.vanished;
        let mut report = RefreshReport {
            households: self.households.len(),
            rebooted,
            ..RefreshReport::default()
        };
        for p in self.households.iter().flat_map(|h| &h.players) {
            report.players += 1;
            // Listed in its household's topology, but its own address did
            // not answer the survey: not healthy, whatever its peers say.
            match survey
                .unreachable
                .iter()
                .find(|(addr, _)| addr.parse::<IpAddr>().ok() == Some(p.ip))
            {
                Some((_, error)) => self.health.failed(&p.id, error.clone()),
                None => self.health.ok(&p.id, now),
            }
        }
        let mut forgotten = 0;
        for (id, ip) in before {
            if self.households.iter().all(|h| h.player(&id).is_none()) {
                // Absent from the model either way; keep why, if the survey
                // saw its address fail to answer.
                let reason = survey
                    .unreachable
                    .iter()
                    .find(|(addr, _)| addr.parse::<IpAddr>().ok() == Some(ip))
                    .map_or_else(
                        || "not found by the last survey".to_string(),
                        |(_, error)| error.clone(),
                    );
                tracing::warn!(
                    player = id.0.as_str(),
                    reason = reason.as_str(),
                    "player missing"
                );
                self.health.missing(&id, reason);
                // Its subscriptions die with it; UNSUBSCRIBE would only time out.
                forgotten += self.subscriptions.forget(&id);
                report.missing.push(id);
            }
        }
        report.events =
            self.subscriptions
                .sync(lan, &events::wanted(&self.households), callback_url, now);
        report.events.dropped += forgotten;
        self.health.record(&report.events, now);
        self.schedule.succeeded(now);
        Ok(report)
    }

    /// Fold one NOTIFY from the event sink into the live state. A topology
    /// event that shows a player rebooted (its `BootSeq` rose) also replaces
    /// that player's subscriptions at once, instead of when their renewals
    /// fail; the fresh subscriptions' initial NOTIFYs bring its state back.
    /// Returns `None` for an unknown SID or a replay. Invalid events do not
    /// advance the sequence cursor and request a fresh full-state event;
    /// failed recovery uses the subscription engine's bounded retry.
    pub fn on_notify<S: Subscriber + ?Sized>(
        &mut self,
        lan: &S,
        playback: &mut Playback,
        n: &Notify,
        callback_url: impl Fn(Service) -> String,
        now: Instant,
    ) -> Result<Option<NotifyReport>, CoreError> {
        let Some((player, service)) = self.subscriptions.route(n).map(|(p, s)| (p.clone(), s))
        else {
            return Ok(None);
        };
        // A topology event rebuilds its household's players: note who was
        // there, so a player it drops is marked rather than just forgotten
        // (the next survey would not miss what the model no longer has).
        let before: Vec<PlayerId> = if service == Service::ZoneGroupTopology {
            self.discovery.observe(&self.households);
            self.households
                .iter()
                .flat_map(|h| h.players.iter().map(|p| p.id.clone()))
                .collect()
        } else {
            Vec::new()
        };
        let changes = initial::apply(&mut self.households, playback, &player, service, n, now)
            .inspect_err(|_| {
                // An unreadable event loses state just as a missing one does.
                // Keep its SEQ unconsumed: a corrected retry on the old SID
                // remains usable if the fresh subscription cannot be made.
                let recovery = self.subscriptions.refresh(lan, &n.sid, &callback_url, now);
                self.health.record(&recovery, now);
            })?;
        self.health.ok(&player, now);
        let missed = self.subscriptions.missed(n);
        let mut report = NotifyReport {
            changes,
            ..NotifyReport::default()
        };
        if missed {
            // Events were lost: what they changed is stale. A fresh
            // subscription's initial NOTIFY brings the whole state back.
            let r = self.subscriptions.refresh(lan, &n.sid, &callback_url, now);
            tracing::info!(
                player = player.0.as_str(),
                service = ?service,
                seq = n.seq,
                cause = "a NOTIFY was lost (SEQ skipped)",
                action = "resubscribe",
                resubscribed = r.resubscribed,
                failed = r.failed.len(),
                "events resynced"
            );
            self.health.record(&r, now);
            report.events.resubscribed += r.resubscribed;
            report.events.failed.extend(r.failed);
        }
        for id in before {
            if self.households.iter().all(|h| h.player(&id).is_none()) {
                tracing::warn!(
                    player = id.0.as_str(),
                    "player left its household's topology"
                );
                self.health
                    .missing(&id, "no longer in its household's topology");
                report.events.dropped += self.subscriptions.forget(&id);
                report.gone.push(id);
            }
        }
        if service == Service::ZoneGroupTopology
            && let Some(doc) = n.property("ZoneGroupState")
        {
            let zgs = parse_zone_group_state(doc)?;
            self.discovery.observe(&self.households);
            // This household's vanished players: drop those it lists again,
            // record (or refresh) those it now lists as vanished.
            self.vanished.retain(|v| {
                zgs.groups
                    .iter()
                    .flat_map(|g| &g.members)
                    .all(|m| m.uuid != v.uuid)
                    && zgs.vanished.iter().all(|now| now.uuid != v.uuid)
            });
            self.vanished.extend(zgs.vanished.iter().cloned());
            for p in self.subscriptions.reboots(zgs.boot_seqs()) {
                let r = self.subscriptions.resubscribe(lan, &p, &callback_url, now);
                tracing::info!(
                    player = p.0.as_str(),
                    cause = "BootSeq rose",
                    action = "resubscribe",
                    resubscribed = r.resubscribed,
                    failed = r.failed.len(),
                    "player rebooted"
                );
                self.health.record(&r, now);
                report.events.resubscribed += r.resubscribed;
                report.events.failed.extend(r.failed);
                report.rebooted.push(p);
            }
            // Grouping changed: a new coordinator needs its AVTransport and
            // group-volume subscriptions, a former one no longer does, and
            // a player that joined needs its own.
            let r =
                self.subscriptions
                    .sync(lan, &events::wanted(&self.households), &callback_url, now);
            self.health.record(&r, now);
            report.events.subscribed += r.subscribed;
            report.events.dropped += r.dropped;
            report.events.failed.extend(r.failed);
        }
        Ok(Some(report))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{RETRY, Want};
    use fsonos_proto::gena::Subscription;
    use std::cell::Cell;

    #[derive(Default)]
    struct TestSubscriber {
        attempts: Cell<u32>,
        fail: Cell<bool>,
    }

    impl Subscriber for TestSubscriber {
        fn subscribe(&self, _: &Want, _: &str) -> Result<Subscription, ProtoError> {
            self.attempts.set(self.attempts.get() + 1);
            if self.fail.get() {
                return Err(ProtoError::Network {
                    target: "test-player".into(),
                    detail: "subscription unavailable".into(),
                });
            }
            Ok(Subscription {
                sid: format!("uuid:reconcile-{}", self.attempts.get()),
                timeout_secs: 300,
            })
        }

        fn renew(&self, _: &Want, sid: &str) -> Result<Subscription, ProtoError> {
            Ok(Subscription {
                sid: sid.into(),
                timeout_secs: 300,
            })
        }

        fn unsubscribe(&self, _: &Want, _: &str) -> Result<(), ProtoError> {
            Ok(())
        }
    }

    fn player() -> PlayerId {
        PlayerId("test-player".into())
    }

    fn callback(service: Service) -> String {
        format!("http://127.0.0.1:3400/{}", service.tag())
    }

    fn start(s: &TestSubscriber, service: Service, now: Instant) -> Reconciler {
        let mut rec = Reconciler::new(RETRY, RETRY, now);
        let want = Want {
            player: player(),
            host: std::net::Ipv4Addr::LOCALHOST.into(),
            service,
        };
        assert_eq!(
            rec.subscriptions.sync(s, &[want], callback, now).subscribed,
            1
        );
        rec
    }

    fn avt(sid: &str, seq: u32, body: &str) -> Notify {
        Notify {
            sid: sid.into(),
            seq,
            path: "/AVTransport".into(),
            properties: vec![(
                "LastChange".into(),
                format!("<Event><InstanceID val=\"0\">{body}</InstanceID></Event>"),
            )],
        }
    }

    const PLAYING: &str =
        "<TransportState val=\"PLAYING\"/><CurrentTrackURI val=\"test:old-track\"/>";
    const PAUSED: &str =
        "<TransportState val=\"PAUSED_PLAYBACK\"/><CurrentTrackURI val=\"test:new-track\"/>";
    const MALFORMED: &str = "<TransportState val=\"STOPPED\"/>\
                             <CurrentTrackURI val=\"test:bad-track\"/>\
                             <CurrentTrackMetaData val=\"not-xml\"/>";

    #[test]
    fn malformed_event_requests_full_state_without_changing_playback() {
        let now = Instant::now();
        let s = TestSubscriber::default();
        let mut rec = start(&s, Service::AvTransport, now);
        let mut playback = Playback::default();
        let initial = avt("uuid:reconcile-1", 0, PLAYING);
        rec.on_notify(&s, &mut playback, &initial, callback, now)
            .unwrap();
        let before = playback.of(&player()).unwrap().clone();
        let malformed = avt("uuid:reconcile-1", 1, MALFORMED);
        assert!(
            rec.on_notify(&s, &mut playback, &malformed, callback, now)
                .is_err()
        );
        assert_eq!(playback.of(&player()), Some(&before));
        assert_eq!(s.attempts.get(), 2, "fresh full state was requested");
        assert!(rec.subscriptions.route(&malformed).is_none());
        let replacement = avt("uuid:reconcile-2", 0, PAUSED);
        let report = rec
            .on_notify(&s, &mut playback, &replacement, callback, now)
            .unwrap()
            .unwrap();
        assert!(report.changes.transport.is_some());
        assert_eq!(
            playback.of(&player()).unwrap().track_uri.as_deref(),
            Some("test:new-track")
        );
    }

    #[test]
    fn failed_recovery_keeps_a_corrected_same_sequence_event_usable() {
        let now = Instant::now();
        let s = TestSubscriber::default();
        let mut rec = start(&s, Service::AvTransport, now);
        let mut playback = Playback::default();
        let initial = avt("uuid:reconcile-1", 0, PLAYING);
        rec.on_notify(&s, &mut playback, &initial, callback, now)
            .unwrap();
        s.fail.set(true);
        let bad_at = now + Duration::from_secs(1);
        let malformed = avt("uuid:reconcile-1", 1, MALFORMED);
        assert!(
            rec.on_notify(&s, &mut playback, &malformed, callback, bad_at)
                .is_err()
        );
        assert_eq!(rec.health.of(&player()).unwrap().last_ok, Some(now));
        assert_eq!(rec.subscriptions.next_due(), Some(bad_at + RETRY));
        let corrected = avt("uuid:reconcile-1", 1, PAUSED);
        assert!(rec.subscriptions.route(&corrected).is_some());
        let report = rec
            .on_notify(&s, &mut playback, &corrected, callback, bad_at)
            .unwrap()
            .unwrap();
        assert!(report.changes.transport.is_some());
        assert!(report.changes.track.is_some());
        assert_eq!(s.attempts.get(), 2, "no unnecessary second recovery call");
        assert!(
            rec.on_notify(&s, &mut playback, &corrected, callback, bad_at)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn malformed_initial_events_retry_with_backoff_for_each_xml_service() {
        for service in [
            Service::AvTransport,
            Service::RenderingControl,
            Service::ZoneGroupTopology,
        ] {
            let now = Instant::now();
            let s = TestSubscriber::default();
            let mut rec = start(&s, service, now);
            let mut playback = Playback::default();
            s.fail.set(true);
            let property = if service == Service::ZoneGroupTopology {
                "ZoneGroupState"
            } else {
                "LastChange"
            };
            let bad = Notify {
                sid: "uuid:reconcile-1".into(),
                seq: 0,
                path: format!("/{}", service.tag()),
                properties: vec![(property.into(), "not-xml".into())],
            };
            for second in 0..30 {
                assert!(
                    rec.on_notify(
                        &s,
                        &mut playback,
                        &bad,
                        callback,
                        now + Duration::from_secs(second),
                    )
                    .is_err()
                );
                assert_eq!(s.attempts.get(), 2, "backoff for {service:?}");
                assert_eq!(rec.subscriptions.next_due(), Some(now + RETRY));
                assert!(playback.of(&player()).is_none());
            }
            s.fail.set(false);
            let recovered = rec.subscriptions.renew_due(&s, callback, now + RETRY);
            assert_eq!(recovered.resubscribed, 1);
            assert_eq!(s.attempts.get(), 3);
            assert!(rec.subscriptions.route(&bad).is_none());
        }
    }

    #[test]
    fn valid_forward_gaps_still_recover_but_malformed_replays_do_not() {
        let now = Instant::now();
        let s = TestSubscriber::default();
        let mut rec = start(&s, Service::AvTransport, now);
        let mut playback = Playback::default();
        let initial = avt("uuid:reconcile-1", 0, PLAYING);
        rec.on_notify(&s, &mut playback, &initial, callback, now)
            .unwrap();
        let replay = avt("uuid:reconcile-1", 0, MALFORMED);
        assert!(
            rec.on_notify(&s, &mut playback, &replay, callback, now)
                .unwrap()
                .is_none()
        );
        assert_eq!(s.attempts.get(), 1);
        let gap = avt("uuid:reconcile-1", 3, PAUSED);
        let report = rec
            .on_notify(&s, &mut playback, &gap, callback, now)
            .unwrap()
            .unwrap();
        assert!(report.changes.track.is_some());
        assert_eq!(report.events.resubscribed, 1);
        assert_eq!(s.attempts.get(), 2);
    }
}
