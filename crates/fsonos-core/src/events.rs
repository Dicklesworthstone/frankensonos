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
use fsonos_proto::topology::parse_zone_group_state;
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
    /// A failed replacement must retry SUBSCRIBE, not renew a stale stream.
    refresh_pending: bool,
    /// The last accepted SEQ; event keys wrap from u32::MAX to 1, not 0.
    last_seq: Option<u32>,
}

/// What one [`Subscriptions`] pass did, for logs and health.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub subscribed: usize,
    pub renewed: usize,
    /// Subscriptions replaced after renewal failure, lost events, or reboot.
    pub resubscribed: usize,
    pub dropped: usize,
    /// Calls that failed outright: (player, service, error).
    pub failed: Vec<(PlayerId, Service, String)>,
    /// Players that answered a SUBSCRIBE or renewal in this pass.
    pub answered: Vec<PlayerId>,
}

/// How soon a SUBSCRIBE or renewal that failed is tried again.
pub const RETRY: Duration = Duration::from_secs(30);

/// The active subscriptions.
#[derive(Debug, Default)]
pub struct Subscriptions {
    active: Vec<Active>,
    /// Wanted subscriptions whose SUBSCRIBE failed, and when to try again.
    pending: Vec<(Want, Instant)>,
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
    /// An unchanged failed subscription keeps its retry deadline, even when
    /// topology events arrive faster than the backoff.
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
        self.pending.retain(|(w, _)| wanted.contains(w));
        for want in wanted {
            if self.active.iter().any(|a| a.want == *want)
                || self.pending.iter().any(|(w, at)| w == want && *at > now)
            {
                continue;
            }
            self.pending.retain(|(w, _)| w != want);
            match s.subscribe(want, &callback_url(want.service)) {
                Ok(sub) => {
                    self.active.push(active(want.clone(), sub, now));
                    report.subscribed += 1;
                    report.answered.push(want.player.clone());
                }
                Err(e) => {
                    self.pending.push((want.clone(), now + RETRY));
                    report
                        .failed
                        .push((want.player.clone(), want.service, e.to_string()));
                }
            }
        }
        report
    }

    /// Renew every subscription that is due at `now`. A failed renewal (a
    /// rebooted player answers 412) is replaced by a fresh subscription.
    /// Failed replacements retry here too, without needing another NOTIFY.
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
            if a.refresh_pending {
                refresh_active(s, a, &callback_url, now, &mut report);
                continue;
            }
            match s.renew(&a.want, &a.sid) {
                Ok(sub) => {
                    // Renewing the same SID extends its lifetime, not its event stream.
                    let last_seq = if sub.sid == a.sid { a.last_seq } else { None };
                    *a = active(a.want.clone(), sub, now);
                    a.last_seq = last_seq;
                    report.renewed += 1;
                    report.answered.push(a.want.player.clone());
                }
                Err(renewal) => match s.subscribe(&a.want, &callback_url(a.want.service)) {
                    Ok(sub) => {
                        tracing::info!(
                            player = a.want.player.0.as_str(),
                            service = ?a.want.service,
                            cause = %renewal,
                            action = "resubscribe",
                            outcome = "ok",
                            "renewal failed; subscribed afresh"
                        );
                        *a = active(a.want.clone(), sub, now);
                        report.resubscribed += 1;
                        report.answered.push(a.want.player.clone());
                    }
                    Err(e) => {
                        tracing::warn!(
                            player = a.want.player.0.as_str(),
                            service = ?a.want.service,
                            cause = %renewal,
                            action = "resubscribe",
                            outcome = %e,
                            "renewal failed and so did a fresh subscription; retrying in 30 s"
                        );
                        a.refresh_pending = true;
                        a.renew_at = now + RETRY;
                        report
                            .failed
                            .push((a.want.player.clone(), a.want.service, e.to_string()));
                    }
                },
            }
        }
        // Subscriptions that never took: try them again.
        let (due, later): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|(_, at)| *at <= now);
        self.pending = later;
        for (want, _) in due {
            match s.subscribe(&want, &callback_url(want.service)) {
                Ok(sub) => {
                    report.answered.push(want.player.clone());
                    self.active.push(active(want, sub, now));
                    report.subscribed += 1;
                }
                Err(e) => {
                    report
                        .failed
                        .push((want.player.clone(), want.service, e.to_string()));
                    self.pending.push((want, now + RETRY));
                }
            }
        }
        report
    }

    /// When the next renewal or recovery retry is due, for the daemon's timer.
    #[must_use]
    pub fn next_due(&self) -> Option<Instant> {
        self.active
            .iter()
            .map(|a| a.renew_at)
            .chain(self.pending.iter().map(|(_, at)| *at))
            .min()
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

    /// Note `n`'s SEQ on its subscription: whether NOTIFYs were lost before
    /// it. After the initial one (SEQ 0, the full state) a player sends only
    /// what changed, so a lost one leaves that state stale until replaced
    /// ([`Subscriptions::refresh`]). A subscription's first NOTIFY is no gap
    /// only when it is the initial one (SEQ 0): a later one first means the
    /// full state was lost. Duplicate or older event keys are ignored;
    /// ordering follows the GENA wrap from u32::MAX to 1 (0 is initial only).
    pub fn missed(&mut self, n: &Notify) -> bool {
        let Some(a) = self.active.iter_mut().find(|a| a.sid == n.sid) else {
            return false;
        };
        let Some(gap) = sequence_gap(a.last_seq, n.seq) else {
            return false;
        };
        a.last_seq = Some(n.seq);
        gap
    }

    /// Replace subscription `sid` with a fresh one, whose initial NOTIFY
    /// carries the whole state again (after [`Subscriptions::missed`]). The
    /// old one is unsubscribed once the new one stands: the player still
    /// has it. A refused SUBSCRIBE keeps the old one and schedules a fresh
    /// attempt after [`RETRY`]; further gaps do not bypass that backoff.
    pub fn refresh<S: Subscriber + ?Sized>(
        &mut self,
        s: &S,
        sid: &str,
        callback_url: impl Fn(Service) -> String,
        now: Instant,
    ) -> Report {
        let mut report = Report::default();
        let Some(a) = self.active.iter_mut().find(|a| a.sid == sid) else {
            return report;
        };
        if a.refresh_pending && a.renew_at > now {
            return report;
        }
        refresh_active(s, a, &callback_url, now, &mut report);
        report
    }

    /// Players whose `BootSeq` rose since last seen, from `(player,
    /// BootSeq)` pairs (a topology event's or a survey's): they rebooted, so
    /// their subscriptions are gone even if no renewal has failed yet. The
    /// first sighting of a player only records it.
    pub fn reboots<'a>(
        &mut self,
        seen: impl IntoIterator<Item = (&'a PlayerId, u32)>,
    ) -> Vec<PlayerId> {
        let mut rebooted = Vec::new();
        for (player, seq) in seen {
            if let Some(prev) = self.boot_seqs.insert(player.clone(), seq)
                && seq > prev
            {
                rebooted.push(player.clone());
            }
        }
        rebooted
    }

    /// Drop every subscription to `player` without UNSUBSCRIBE (it rebooted
    /// and has already forgotten them); the next [`Subscriptions::sync`]
    /// subscribes afresh. Returns how many were dropped.
    pub fn forget(&mut self, player: &PlayerId) -> usize {
        let before = self.active.len();
        self.active.retain(|a| a.want.player != *player);
        self.pending.retain(|(w, _)| w.player != *player);
        before - self.active.len()
    }

    /// Replace every subscription to `player` with a fresh one (after a
    /// reboot). The old SIDs are dropped without UNSUBSCRIBE: the player has
    /// already forgotten them. Failed replacements become pending, never
    /// routable under a SID belonging to the previous boot.
    pub fn resubscribe<S: Subscriber + ?Sized>(
        &mut self,
        s: &S,
        player: &PlayerId,
        callback_url: impl Fn(Service) -> String,
        now: Instant,
    ) -> Report {
        let mut report = Report::default();
        let (replace, keep): (Vec<Active>, Vec<Active>) = std::mem::take(&mut self.active)
            .into_iter()
            .partition(|a| a.want.player == *player);
        self.active = keep;
        for old in replace {
            self.pending.retain(|(w, _)| *w != old.want);
            match s.subscribe(&old.want, &callback_url(old.want.service)) {
                Ok(sub) => {
                    report.answered.push(old.want.player.clone());
                    self.active.push(active(old.want, sub, now));
                    report.resubscribed += 1;
                }
                Err(e) => {
                    report
                        .failed
                        .push((old.want.player.clone(), old.want.service, e.to_string()));
                    self.pending.push((old.want, now + RETRY));
                }
            }
        }
        report
    }

    /// Unsubscribe everything (daemon shutdown). Best effort.
    pub fn unsubscribe_all<S: Subscriber + ?Sized>(&mut self, s: &S) {
        self.pending.clear();
        for a in self.active.drain(..) {
            let _ = s.unsubscribe(&a.want, &a.sid);
        }
    }
}

fn refresh_active<S: Subscriber + ?Sized>(
    s: &S,
    a: &mut Active,
    callback_url: &impl Fn(Service) -> String,
    now: Instant,
    report: &mut Report,
) {
    match s.subscribe(&a.want, &callback_url(a.want.service)) {
        Ok(sub) => {
            let fresh = active(a.want.clone(), sub, now);
            let old = std::mem::replace(a, fresh);
            if old.sid != a.sid {
                let _ = s.unsubscribe(&old.want, &old.sid);
            }
            report.resubscribed += 1;
            report.answered.push(a.want.player.clone());
        }
        Err(e) => {
            a.refresh_pending = true;
            a.renew_at = now + RETRY;
            report
                .failed
                .push((a.want.player.clone(), a.want.service, e.to_string()));
        }
    }
}

/// `None` is a duplicate/old key; otherwise whether a forward event skipped keys.
fn sequence_gap(last: Option<u32>, seq: u32) -> Option<bool> {
    let Some(last) = last else {
        return Some(seq != 0);
    };
    if seq == 0 || seq == last {
        return None;
    }
    if last == 0 {
        return Some(seq != 1);
    }
    // GENA cycles through 1..=u32::MAX. Compare within half that ring so
    // delayed pre-wrap events cannot move the sequence backwards after wrap.
    let distance = if seq > last {
        seq - last
    } else {
        u32::MAX - last + seq
    };
    (distance <= u32::MAX / 2).then_some(distance > 1)
}

fn active(want: Want, sub: Subscription, now: Instant) -> Active {
    let half = Duration::from_secs(u64::from(sub.timeout_secs.max(2) / 2));
    Active {
        want,
        sid: sub.sid,
        renew_at: now + half,
        refresh_pending: false,
        last_seq: None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[derive(Default)]
    struct TestSubscriber {
        renewal_sid: Option<&'static str>,
    }

    impl Subscriber for TestSubscriber {
        fn subscribe(&self, _: &Want, _: &str) -> Result<Subscription, ProtoError> {
            Ok(Subscription {
                sid: "uuid:events-test".into(),
                timeout_secs: 300,
            })
        }

        fn renew(&self, _: &Want, sid: &str) -> Result<Subscription, ProtoError> {
            Ok(Subscription {
                sid: self.renewal_sid.unwrap_or(sid).into(),
                timeout_secs: 300,
            })
        }

        fn unsubscribe(&self, _: &Want, _: &str) -> Result<(), ProtoError> {
            Ok(())
        }
    }

    fn want() -> Want {
        Want {
            player: PlayerId("test-player".into()),
            host: std::net::Ipv4Addr::LOCALHOST.into(),
            service: Service::AvTransport,
        }
    }

    fn callback(_: Service) -> String {
        "http://127.0.0.1:3400/AVTransport".into()
    }

    fn notify(sid: &str, seq: u32) -> Notify {
        Notify {
            sid: sid.into(),
            seq,
            path: "/AVTransport".into(),
            properties: Vec::new(),
        }
    }

    #[test]
    fn renewal_preserves_sequence_and_still_detects_real_loss() {
        let subscriber = TestSubscriber::default();
        let mut subs = Subscriptions::default();
        let now = Instant::now();
        assert_eq!(subs.sync(&subscriber, &[want()], callback, now).subscribed, 1);
        assert!(!subs.missed(&notify("uuid:events-test", 0)));
        assert!(!subs.missed(&notify("uuid:events-test", 1)));
        let report = subs.renew_due(&subscriber, callback, now + Duration::from_secs(150));
        assert_eq!(report.renewed, 1);
        assert!(!subs.missed(&notify("uuid:events-test", 2)));
        assert!(subs.missed(&notify("uuid:events-test", 4)));
    }

    #[test]
    fn changed_renewal_sid_starts_a_new_event_stream() {
        let subscriber = TestSubscriber {
            renewal_sid: Some("uuid:replacement"),
        };
        let mut subs = Subscriptions::default();
        let now = Instant::now();
        subs.sync(&subscriber, &[want()], callback, now);
        assert!(subs.missed(&notify("uuid:events-test", 10)));
        subs.renew_due(&subscriber, callback, now + Duration::from_secs(150));
        assert!(subs.route(&notify("uuid:events-test", 11)).is_none());
        assert!(!subs.missed(&notify("uuid:replacement", 0)));
        assert!(!subs.missed(&notify("uuid:replacement", 1)));
        assert!(subs.missed(&notify("uuid:replacement", 3)));
    }

    #[test]
    fn missing_initial_state_is_detected_even_after_renewal() {
        let subscriber = TestSubscriber::default();
        let mut subs = Subscriptions::default();
        let now = Instant::now();
        subs.sync(&subscriber, &[want()], callback, now);
        subs.renew_due(&subscriber, callback, now + Duration::from_secs(150));
        assert!(subs.missed(&notify("uuid:events-test", 1)));
    }

    #[test]
    fn wrap_skips_zero_and_keeps_detecting_gaps() {
        let subscriber = TestSubscriber::default();
        let mut subs = Subscriptions::default();
        subs.sync(&subscriber, &[want()], callback, Instant::now());
        for (seq, gap) in [
            (u32::MAX - 1, true),
            (u32::MAX, false),
            (1, false),
            (u32::MAX, false), // Delayed pre-wrap event.
            (2, false),
            (4, true),
            (3, false), // Delayed event cannot rewind the cursor.
            (0, false), // A repeated initial event is not a wrap.
            (5, false),
        ] {
            assert_eq!(subs.missed(&notify("uuid:events-test", seq)), gap, "{seq}");
        }
    }

    #[test]
    fn sequence_order_covers_initial_duplicates_backwards_and_wrap_gaps() {
        for (last, seq, expected) in [
            (None, 0, Some(false)),
            (None, 2, Some(true)),
            (Some(0), 1, Some(false)),
            (Some(0), 2, Some(true)),
            (Some(10), 10, None),
            (Some(10), 9, None),
            (Some(10), 0, None),
            (Some(10), 11, Some(false)),
            (Some(10), 12, Some(true)),
            (Some(u32::MAX), 1, Some(false)),
            (Some(u32::MAX), 2, Some(true)),
            (Some(u32::MAX - 1), 1, Some(true)),
            (Some(1), u32::MAX, None),
        ] {
            assert_eq!(sequence_gap(last, seq), expected, "{last:?} -> {seq}");
        }
    }

    #[derive(Default)]
    struct RetrySubscriber {
        calls: Cell<usize>,
        failures: Cell<usize>,
        renewals: Cell<usize>,
        unsubscribed: RefCell<Vec<String>>,
    }

    impl Subscriber for RetrySubscriber {
        fn subscribe(&self, _: &Want, _: &str) -> Result<Subscription, ProtoError> {
            self.calls.set(self.calls.get() + 1);
            if self.failures.get() > 0 {
                self.failures.set(self.failures.get() - 1);
                return Err(ProtoError::Network {
                    target: "test-player".into(),
                    detail: "SUBSCRIBE: HTTP 503".into(),
                });
            }
            Ok(Subscription {
                sid: format!("uuid:retry-{}", self.calls.get()),
                timeout_secs: 300,
            })
        }

        fn renew(&self, _: &Want, sid: &str) -> Result<Subscription, ProtoError> {
            self.renewals.set(self.renewals.get() + 1);
            Ok(Subscription {
                sid: sid.into(),
                timeout_secs: 300,
            })
        }

        fn unsubscribe(&self, _: &Want, sid: &str) -> Result<(), ProtoError> {
            self.unsubscribed.borrow_mut().push(sid.into());
            Ok(())
        }
    }

    #[test]
    fn sync_honors_backoff_and_deduplicates_failed_wants() {
        let subscriber = RetrySubscriber::default();
        subscriber.failures.set(1);
        let mut subs = Subscriptions::default();
        let now = Instant::now();
        let wants = [want(), want()];
        let first = subs.sync(&subscriber, &wants, callback, now);
        assert_eq!(first.failed.len(), 1);
        assert_eq!(subscriber.calls.get(), 1);
        assert_eq!(subs.next_due(), Some(now + RETRY));
        for second in 1..30 {
            let report = subs.sync(
                &subscriber,
                &wants,
                callback,
                now + Duration::from_secs(second),
            );
            assert_eq!(report, Report::default());
        }
        assert_eq!(subscriber.calls.get(), 1);
        let due = subs.sync(&subscriber, &wants, callback, now + RETRY);
        assert_eq!(due.subscribed, 1);
        assert_eq!(subscriber.calls.get(), 2);
        assert_eq!(subs.len(), 1);
        assert_eq!(
            subs.renew_due(&subscriber, callback, now + RETRY),
            Report::default()
        );
    }

    #[test]
    fn changed_address_retries_immediately_and_cancels_old_pending_want() {
        let subscriber = RetrySubscriber::default();
        subscriber.failures.set(1);
        let mut subs = Subscriptions::default();
        let now = Instant::now();
        subs.sync(&subscriber, &[want()], callback, now);
        let moved = Want {
            host: std::net::Ipv4Addr::new(192, 0, 2, 10).into(),
            ..want()
        };
        let report = subs.sync(
            &subscriber,
            &[moved],
            callback,
            now + Duration::from_secs(1),
        );
        assert_eq!(report.subscribed, 1);
        assert_eq!(subscriber.calls.get(), 2);
        assert_eq!(
            subs.renew_due(&subscriber, callback, now + RETRY),
            Report::default()
        );
        assert_eq!(subs.len(), 1);
        assert!(subs.pending.is_empty());
    }

    #[test]
    fn failed_gap_refresh_retries_until_full_state_can_be_requested() {
        let subscriber = RetrySubscriber::default();
        let mut subs = Subscriptions::default();
        let now = Instant::now();
        subs.sync(&subscriber, &[want()], callback, now);
        assert!(!subs.missed(&notify("uuid:retry-1", 0)));
        assert!(subs.missed(&notify("uuid:retry-1", 3)));
        subscriber.failures.set(2);
        let gap_at = now + Duration::from_secs(10);
        let failed = subs.refresh(&subscriber, "uuid:retry-1", callback, gap_at);
        assert_eq!(failed.failed.len(), 1);
        assert_eq!(subs.next_due(), Some(gap_at + RETRY));
        assert!(subs.route(&notify("uuid:retry-1", 4)).is_some());
        let early = gap_at + Duration::from_secs(1);
        assert_eq!(
            subs.refresh(&subscriber, "uuid:retry-1", callback, early),
            Report::default()
        );
        assert_eq!(
            subs.renew_due(&subscriber, callback, early),
            Report::default()
        );
        assert_eq!(subscriber.calls.get(), 2);
        let failed_again = subs.renew_due(&subscriber, callback, gap_at + RETRY);
        assert_eq!(failed_again.failed.len(), 1);
        assert_eq!(subs.next_due(), Some(gap_at + RETRY * 2));
        let recovered = subs.renew_due(&subscriber, callback, gap_at + RETRY * 2);
        assert_eq!(recovered.resubscribed, 1);
        assert_eq!(recovered.answered, vec![want().player]);
        assert_eq!(subscriber.calls.get(), 4);
        assert_eq!(subscriber.renewals.get(), 0, "renewing cannot restore state");
        assert!(subs.route(&notify("uuid:retry-1", 5)).is_none());
        assert!(!subs.missed(&notify("uuid:retry-4", 0)));
        assert_eq!(
            subscriber.unsubscribed.borrow().as_slice(),
            ["uuid:retry-1"]
        );
    }

    #[test]
    fn failed_reboot_replacement_drops_the_old_sid_and_uses_backoff() {
        let subscriber = RetrySubscriber::default();
        let mut subs = Subscriptions::default();
        let now = Instant::now();
        subs.sync(&subscriber, &[want()], callback, now);
        subscriber.failures.set(1);
        let failed = subs.resubscribe(&subscriber, &want().player, callback, now);
        assert_eq!(failed.failed.len(), 1);
        assert!(subs.is_empty());
        assert!(subs.route(&notify("uuid:retry-1", 1)).is_none());
        assert_eq!(subs.next_due(), Some(now + RETRY));
        assert_eq!(
            subs.sync(&subscriber, &[want()], callback, now),
            Report::default()
        );
        assert_eq!(subscriber.calls.get(), 2);
        let recovered = subs.renew_due(&subscriber, callback, now + RETRY);
        assert_eq!(recovered.subscribed, 1);
        assert_eq!(subs.len(), 1);
        assert_eq!(subscriber.renewals.get(), 0);
        assert!(subscriber.unsubscribed.borrow().is_empty());
        assert!(subs.route(&notify("uuid:retry-3", 0)).is_some());
    }

    #[test]
    fn shutdown_or_forgetting_cancels_failed_refresh_retries() {
        for shutdown in [false, true] {
            let subscriber = RetrySubscriber::default();
            let mut subs = Subscriptions::default();
            let now = Instant::now();
            subs.sync(&subscriber, &[want()], callback, now);
            subscriber.failures.set(1);
            subs.refresh(&subscriber, "uuid:retry-1", callback, now);
            if shutdown {
                subs.unsubscribe_all(&subscriber);
            } else {
                assert_eq!(subs.forget(&want().player), 1);
            }
            assert_eq!(subs.next_due(), None);
            assert_eq!(
                subs.renew_due(&subscriber, callback, now + RETRY),
                Report::default()
            );
            assert_eq!(subscriber.calls.get(), 2);
        }
    }
}
