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
use fsonos_proto::topology::parse_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::PlayerId;
use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

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

    /// Record the failures an [`events`] pass reported.
    pub fn record(&mut self, report: &events::Report) {
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
    /// Survey every `interval`; after failures, retry sooner at first and
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
    pub subscriptions: Subscriptions,
    pub health: HealthBoard,
    pub schedule: Refresh,
}

impl Reconciler {
    /// Survey every `interval`, backing off up to `max_backoff` after
    /// failures; the first survey is due right away.
    #[must_use]
    pub fn new(interval: Duration, max_backoff: Duration, now: Instant) -> Self {
        Self {
            households: Vec::new(),
            subscriptions: Subscriptions::default(),
            health: HealthBoard::default(),
            schedule: Refresh::new(interval, max_backoff, now),
        }
    }

    /// One reconcile step: survey (SSDP plus `seeds`), replace the household
    /// model, record health, and sync the subscriptions with the new model.
    /// A failed or empty survey keeps the old model, backs the schedule off,
    /// and returns the error.
    pub fn refresh<L: Transport + Subscriber + ?Sized>(
        &mut self,
        lan: &L,
        seeds: &[IpAddr],
        callback_url: impl Fn(Service) -> String,
        now: Instant,
    ) -> Result<RefreshReport, CoreError> {
        let survey = match inventory::survey(lan, seeds, DISCOVERY_WAIT) {
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
        let mut report = RefreshReport {
            households: self.households.len(),
            rebooted,
            ..RefreshReport::default()
        };
        for p in self.households.iter().flat_map(|h| &h.players) {
            self.health.ok(&p.id, now);
            report.players += 1;
        }
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
                report.missing.push(id);
            }
        }
        report.events =
            self.subscriptions
                .sync(lan, &events::wanted(&self.households), callback_url, now);
        self.health.record(&report.events);
        self.schedule.succeeded(now);
        Ok(report)
    }

    /// Fold one NOTIFY from the event sink into the live state. A topology
    /// event that shows a player rebooted (its `BootSeq` rose) also replaces
    /// that player's subscriptions at once, instead of when their renewals
    /// fail; the fresh subscriptions' initial NOTIFYs bring its state back.
    /// Returns `None` for a NOTIFY no subscription owns.
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
        self.health.ok(&player, now);
        // A topology event rebuilds its household's players: note who was
        // there, so a player it drops is marked rather than just forgotten
        // (the next survey would not miss what the model no longer has).
        let before: Vec<PlayerId> = if service == Service::ZoneGroupTopology {
            self.households
                .iter()
                .flat_map(|h| h.players.iter().map(|p| p.id.clone()))
                .collect()
        } else {
            Vec::new()
        };
        let mut report = NotifyReport {
            changes: events::apply(&mut self.households, playback, &player, service, n, now)?,
            ..NotifyReport::default()
        };
        for id in before {
            if self.households.iter().all(|h| h.player(&id).is_none()) {
                tracing::warn!(
                    player = id.0.as_str(),
                    "player left its household's topology"
                );
                self.health
                    .missing(&id, "no longer in its household's topology");
                report.gone.push(id);
            }
        }
        if service == Service::ZoneGroupTopology
            && let Some(doc) = n.property("ZoneGroupState")
        {
            let zgs = parse_zone_group_state(doc)?;
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
                self.health.record(&r);
                report.events.resubscribed += r.resubscribed;
                report.events.failed.extend(r.failed);
                report.rebooted.push(p);
            }
        }
        Ok(Some(report))
    }
}
