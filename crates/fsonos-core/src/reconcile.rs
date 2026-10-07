//! Keep the inventory current and know which players are healthy.
//!
//! GENA events ([`crate::events`]) keep state fresh between surveys; a
//! periodic survey catches what events cannot: a player that joined, left,
//! or moved to a new address. [`Refresh`] schedules surveys and backs off
//! while they keep failing. [`HealthBoard`] tracks each player from the
//! outcome of every survey, subscription and call. [`Reconciler::refresh`] is one step:
//! survey, replace the household model, record health, and bring the GENA
//! subscriptions in line with the new model.

use crate::events::{self, Service, Subscriber, Subscriptions};
use crate::inventory::{self, DISCOVERY_WAIT};
use crate::{CoreError, HouseholdState};
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
#[derive(Debug, Default)]
pub struct HealthBoard {
    players: HashMap<PlayerId, PlayerHealth>,
}

impl HealthBoard {
    #[must_use]
    pub fn of(&self, player: &PlayerId) -> Option<&PlayerHealth> {
        self.players.get(player)
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

    /// Record a failed survey; returns the delay until the retry.
    pub fn failed(&mut self, now: Instant) -> Duration {
        let base = self.interval / 4;
        let delay = base
            .checked_mul(1 << self.failures.min(16))
            .map_or(self.max_backoff, |d| d.min(self.max_backoff));
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
                self.schedule.failed(now);
                return Err(CoreError::Proto(ProtoError::Network {
                    target: "survey".into(),
                    detail: "no players answered".into(),
                }));
            }
            Err(e) => {
                self.schedule.failed(now);
                return Err(e);
            }
        };
        let before: Vec<(PlayerId, IpAddr)> = self
            .households
            .iter()
            .flat_map(|h| h.players.iter().map(|p| (p.id.clone(), p.ip)))
            .collect();
        self.households = survey.households;
        let mut report = RefreshReport {
            households: self.households.len(),
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
}
