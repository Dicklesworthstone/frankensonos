//! The daemon's event engine: keep a GENA subscription alive for every
//! service the live state needs, and route each NOTIFY to its player.
//!
//! [`wanted`] says which subscriptions the households need: AVTransport and
//! GroupRenderingControl on each group coordinator, RenderingControl on every
//! renderer, and ZoneGroupTopology once per household. [`Subscriptions`]
//! reconciles the active set with that (subscribing what is missing,
//! dropping what is gone), renews before expiry, and resubscribes when a
//! renewal fails (a rebooted player answers 412). [`apply`] folds a routed
//! NOTIFY into [`Playback`] or, for topology events, into the household's
//! groups and rooms. The I/O goes through [`Subscriber`], which
//! `fsonos_proto::net::Lan` implements.

use crate::HouseholdState;
use crate::playback::{Changes, EventSource, Playback};
use fsonos_proto::ProtoError;
use fsonos_proto::gena::{Notify, Subscription};
use fsonos_proto::net::Lan;
use fsonos_proto::soap::{
    AV_TRANSPORT, GROUP_RENDERING_CONTROL, RENDERING_CONTROL, ZONE_GROUP_TOPOLOGY,
};
use fsonos_proto::topology::{ZoneGroupState, parse_zone_group_state};
use fsonos_types::PlayerId;
use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

/// The subscription timeout requested; renewals happen halfway through
/// whatever the player grants.
pub const TIMEOUT_SECS: u32 = 1800;

/// A service whose events the live state needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Service {
    AvTransport,
    RenderingControl,
    GroupRenderingControl,
    ZoneGroupTopology,
}

impl Service {
    #[must_use]
    pub fn event_path(self) -> &'static str {
        match self {
            Self::AvTransport => AV_TRANSPORT.event_path,
            Self::RenderingControl => RENDERING_CONTROL.event_path,
            Self::GroupRenderingControl => GROUP_RENDERING_CONTROL.event_path,
            Self::ZoneGroupTopology => ZONE_GROUP_TOPOLOGY.event_path,
        }
    }

    /// The callback path tag, so one sink serves every service.
    #[must_use]
    pub fn tag(self) -> &'static str {
        match self {
            Self::AvTransport => "AVTransport",
            Self::RenderingControl => "RenderingControl",
            Self::GroupRenderingControl => "GroupRenderingControl",
            Self::ZoneGroupTopology => "ZoneGroupTopology",
        }
    }
}

/// One subscription the live state needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Want {
    pub player: PlayerId,
    pub host: IpAddr,
    pub service: Service,
}

/// The subscriptions `households` need, in a stable order.
#[must_use]
pub fn wanted(households: &[HouseholdState]) -> Vec<Want> {
    let mut out = Vec::new();
    for h in households {
        let want = |id: &PlayerId, service| {
            h.player(id).map(|p| Want {
                player: id.clone(),
                host: p.ip,
                service,
            })
        };
        // Topology once per household, from its first addressable coordinator.
        out.extend(
            h.groups
                .iter()
                .find_map(|g| want(&g.coordinator, Service::ZoneGroupTopology)),
        );
        for g in &h.groups {
            out.extend(want(&g.coordinator, Service::AvTransport));
            out.extend(want(&g.coordinator, Service::GroupRenderingControl));
        }
        for p in &h.players {
            out.extend(want(&p.id, Service::RenderingControl));
        }
    }
    out
}

/// The subscription calls the engine makes. `Lan` implements it.
pub trait Subscriber {
    fn subscribe(&self, want: &Want, callback_url: &str) -> Result<Subscription, ProtoError>;
    fn renew(&self, want: &Want, sid: &str) -> Result<Subscription, ProtoError>;
    fn unsubscribe(&self, want: &Want, sid: &str) -> Result<(), ProtoError>;
}

impl Subscriber for Lan {
    fn subscribe(&self, want: &Want, callback_url: &str) -> Result<Subscription, ProtoError> {
        Lan::subscribe(
            self,
            want.host,
            want.service.event_path(),
            callback_url,
            TIMEOUT_SECS,
        )
    }

    fn renew(&self, want: &Want, sid: &str) -> Result<Subscription, ProtoError> {
        Lan::renew(
            self,
            want.host,
            want.service.event_path(),
            sid,
            TIMEOUT_SECS,
        )
    }

    fn unsubscribe(&self, want: &Want, sid: &str) -> Result<(), ProtoError> {
        Lan::unsubscribe(self, want.host, want.service.event_path(), sid)
    }
}

#[derive(Debug, Clone)]
struct Active {
    want: Want,
    sid: String,
    renew_at: Instant,
}

/// What one [`Subscriptions`] pass did, for logs and health.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub subscribed: usize,
    pub renewed: usize,
    /// Renewals that failed and were replaced by a fresh subscription.
    pub resubscribed: usize,
    pub dropped: usize,
    /// Calls that failed outright: (player, service, error).
    pub failed: Vec<(PlayerId, Service, String)>,
}

/// The active subscriptions.
#[derive(Debug, Default)]
pub struct Subscriptions {
    active: Vec<Active>,
    /// The last `BootSeq` each player reported in a topology.
    boot_seqs: HashMap<PlayerId, u32>,
}

impl Subscriptions {
    /// How many subscriptions are active.
    #[must_use]
    pub fn len(&self) -> usize {
        self.active.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.active.is_empty()
    }

    /// Make the active set match `wanted`: subscribe what is missing and
    /// unsubscribe what is no longer wanted (a player left, a group's
    /// coordinator changed). `callback_url` gives the sink URL per service.
    pub fn sync<S: Subscriber + ?Sized>(
        &mut self,
        s: &S,
        wanted: &[Want],
        callback_url: impl Fn(Service) -> String,
        now: Instant,
    ) -> Report {
        let mut report = Report::default();
        let (keep, drop): (Vec<Active>, Vec<Active>) = std::mem::take(&mut self.active)
            .into_iter()
            .partition(|a| wanted.contains(&a.want));
        for gone in drop {
            // Best effort: a player that left may not answer.
            let _ = s.unsubscribe(&gone.want, &gone.sid);
            report.dropped += 1;
        }
        self.active = keep;
        for want in wanted {
            if self.active.iter().any(|a| a.want == *want) {
                continue;
            }
            match s.subscribe(want, &callback_url(want.service)) {
                Ok(sub) => {
                    self.active.push(active(want.clone(), sub, now));
                    report.subscribed += 1;
                }
                Err(e) => report
                    .failed
                    .push((want.player.clone(), want.service, e.to_string())),
            }
        }
        report
    }

    /// Renew every subscription that is due at `now`. A failed renewal (a
    /// rebooted player answers 412) is replaced by a fresh subscription.
    pub fn renew_due<S: Subscriber + ?Sized>(
        &mut self,
        s: &S,
        callback_url: impl Fn(Service) -> String,
        now: Instant,
    ) -> Report {
        let mut report = Report::default();
        for a in &mut self.active {
            if a.renew_at > now {
                continue;
            }
            match s.renew(&a.want, &a.sid) {
                Ok(sub) => {
                    *a = active(a.want.clone(), sub, now);
                    report.renewed += 1;
                }
                Err(_) => match s.subscribe(&a.want, &callback_url(a.want.service)) {
                    Ok(sub) => {
                        *a = active(a.want.clone(), sub, now);
                        report.resubscribed += 1;
                    }
                    Err(e) => {
                        // Try again on the next pass.
                        a.renew_at = now + Duration::from_secs(30);
                        report
                            .failed
                            .push((a.want.player.clone(), a.want.service, e.to_string()));
                    }
                },
            }
        }
        report
    }

    /// When the next renewal is due, for the daemon's timer.
    #[must_use]
    pub fn next_due(&self) -> Option<Instant> {
        self.active.iter().map(|a| a.renew_at).min()
    }

    /// The player and service a NOTIFY belongs to; `None` for an unknown SID
    /// (a subscription already dropped, or a stray).
    #[must_use]
    pub fn route(&self, n: &Notify) -> Option<(&PlayerId, Service)> {
        self.active
            .iter()
            .find(|a| a.sid == n.sid)
            .map(|a| (&a.want.player, a.want.service))
    }

    /// Players whose `BootSeq` rose since the last topology seen: they
    /// rebooted, so their subscriptions are gone even if no renewal has
    /// failed yet. The first sighting of a player only records it.
    pub fn reboots(&mut self, zgs: &ZoneGroupState) -> Vec<PlayerId> {
        let mut rebooted = Vec::new();
        let members = zgs
            .groups
            .iter()
            .flat_map(|g| &g.members)
            .flat_map(|m| std::iter::once(m).chain(&m.satellites));
        for m in members {
            let Some(seq) = m.boot_seq else { continue };
            if let Some(prev) = self.boot_seqs.insert(m.uuid.clone(), seq)
                && seq > prev
            {
                rebooted.push(m.uuid.clone());
            }
        }
        rebooted
    }

    /// Replace every subscription to `player` with a fresh one (after a
    /// reboot). The old SIDs are dropped without UNSUBSCRIBE: the player has
    /// already forgotten them.
    pub fn resubscribe<S: Subscriber + ?Sized>(
        &mut self,
        s: &S,
        player: &PlayerId,
        callback_url: impl Fn(Service) -> String,
        now: Instant,
    ) -> Report {
        let mut report = Report::default();
        for a in self.active.iter_mut().filter(|a| a.want.player == *player) {
            match s.subscribe(&a.want, &callback_url(a.want.service)) {
                Ok(sub) => {
                    *a = active(a.want.clone(), sub, now);
                    report.resubscribed += 1;
                }
                Err(e) => {
                    a.renew_at = now;
                    report
                        .failed
                        .push((a.want.player.clone(), a.want.service, e.to_string()));
                }
            }
        }
        report
    }

    /// Unsubscribe everything (daemon shutdown). Best effort.
    pub fn unsubscribe_all<S: Subscriber + ?Sized>(&mut self, s: &S) {
        for a in self.active.drain(..) {
            let _ = s.unsubscribe(&a.want, &a.sid);
        }
    }
}

fn active(want: Want, sub: Subscription, now: Instant) -> Active {
    let half = Duration::from_secs(u64::from(sub.timeout_secs.max(2) / 2));
    Active {
        want,
        sid: sub.sid,
        renew_at: now + half,
    }
}

/// Fold a NOTIFY routed to (`player`, `service`) into the live state:
/// transport and volume events into `playback`, topology events into the
/// household `player` belongs to.
pub fn apply(
    households: &mut [HouseholdState],
    playback: &mut Playback,
    player: &PlayerId,
    service: Service,
    n: &Notify,
    now: Instant,
) -> Result<Changes, ProtoError> {
    let source = match service {
        Service::AvTransport => EventSource::AvTransport,
        Service::RenderingControl => EventSource::RenderingControl,
        Service::GroupRenderingControl => EventSource::GroupRenderingControl,
        Service::ZoneGroupTopology => {
            if let Some(doc) = n.property("ZoneGroupState") {
                let zgs = parse_zone_group_state(doc)?;
                if let Some(h) = households.iter_mut().find(|h| h.player(player).is_some()) {
                    h.apply_topology(&zgs);
                }
            }
            return Ok(Changes::default());
        }
    };
    playback.apply(player, source, n, now)
}
