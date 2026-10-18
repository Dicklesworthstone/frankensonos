//! Reconcile desired leases without doing network I/O on the model thread.
//! New roles share the renewal workers; obsolete SIDs stop routing immediately.

use super::{Job, Lease, MAX_WORKERS, Provider, Report, Subscriptions, Want};
use fsonos_types::PlayerId;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Instant;

/// A pathological regrouping burst must not retain unlimited obsolete leases.
/// Retirement is best effort; a lease not retained here expires at the player.
const MAX_RETIRED: usize = 256;

#[derive(Debug)]
struct Retiring {
    lease: Lease,
    thread: JoinHandle<()>,
}

#[derive(Debug, Default)]
pub(super) struct Retirements {
    queue: VecDeque<(Lease, Instant)>,
    active: Vec<Retiring>,
}

impl Retirements {
    pub(super) fn len(&self) -> usize {
        self.active.len()
    }

    pub(super) fn due(&self) -> Option<Instant> {
        self.queue.front().map(|(_, at)| *at)
    }

    pub(super) fn busy(&self, player: &PlayerId) -> bool {
        self.active.iter().any(|job| job.lease.0.player == *player)
    }

    fn push(&mut self, lease: Lease, now: Instant) {
        if self.queue.iter().any(|(old, _)| old == &lease)
            || self.active.iter().any(|job| job.lease == lease)
        {
            return;
        }
        if self.queue.len() == MAX_RETIRED {
            tracing::warn!("lease retirement queue full; old subscription will expire");
            return;
        }
        self.queue.push_back((lease, now));
    }

    pub(super) fn reap(&mut self) {
        let mut index = 0;
        while index < self.active.len() {
            if self.active[index].thread.is_finished() {
                let job = self.active.swap_remove(index);
                if job.thread.join().is_err() {
                    tracing::warn!("lease retirement worker panicked; subscription will expire");
                }
            } else {
                index += 1;
            }
        }
    }

    /// Creation and renewal take priority over old-SID cleanup. Cleanup shares
    /// their four slots and player exclusion, including while an old call hangs.
    pub(super) fn start(
        &mut self,
        subs: &Subscriptions,
        jobs: &[Job],
        provider: &Provider,
        stop: &Arc<AtomicBool>,
    ) {
        while jobs.len() + self.active.len() < MAX_WORKERS && !stop.load(Ordering::Acquire) {
            let index = self.queue.iter().position(|((want, _), _)| {
                !self.busy(&want.player)
                    && !jobs.iter().any(|job| job.plan.want.player == want.player)
            });
            let Some((lease, _)) = index.and_then(|index| self.queue.remove(index)) else {
                break;
            };
            // A provider may reuse a SID. Never retire one the live table has
            // since adopted again at this service's address.
            if subs.active.iter().any(|active| {
                active.sid == lease.1
                    && active.want.host == lease.0.host
                    && active.want.service == lease.0.service
            }) {
                continue;
            }
            let subscriber = Arc::clone(provider);
            let stopping = Arc::clone(stop);
            let (want, sid) = lease.clone();
            match std::thread::Builder::new()
                .name("fsonos-lease-retire".into())
                .spawn(move || {
                    if !stopping.load(Ordering::Acquire) {
                        let _ = subscriber.unsubscribe(&want, &sid);
                    }
                })
            {
                Ok(thread) => self.active.push(Retiring { lease, thread }),
                Err(error) => {
                    tracing::warn!(%error, "cannot retire old lease; subscription will expire");
                }
            }
        }
    }
}

impl Subscriptions {
    /// Opt the live controller into queued topology and reboot maintenance.
    /// Standalone callers retain the synchronous public API's behavior.
    pub(crate) fn defer_network(&mut self) {
        self.deferred_refresh = true;
        self.deferred_sync = true;
    }

    pub(in crate::events) fn sync_deferred(&mut self, wanted: &[Want], now: Instant) -> Report {
        let (keep, removed): (Vec<_>, Vec<_>) = std::mem::take(&mut self.active)
            .into_iter()
            .partition(|active| wanted.contains(&active.want));
        self.active = keep;
        let report = Report {
            dropped: removed.len(),
            ..Report::default()
        };
        for active in removed {
            self.maintenance.retire.push((active.want, active.sid), now);
        }
        self.pending.retain(|(want, _)| wanted.contains(want));
        // Invalidate while removed requests are absent: re-adding the same
        // Want at the same Instant cannot revive an older in-flight request.
        // Unchanged requests keep their epoch, deadline and occupied worker.
        self.invalidate_renewals();
        for want in wanted {
            if !self.active.iter().any(|active| active.want == *want)
                && !self.pending.iter().any(|(pending, _)| pending == want)
            {
                self.pending.push((want.clone(), now));
            }
        }
        report
    }

    pub(in crate::events) fn resubscribe_deferred(
        &mut self,
        player: &PlayerId,
        now: Instant,
    ) -> Report {
        let (removed, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut self.active)
            .into_iter()
            .partition(|active| active.want.player == *player);
        self.active = keep;
        let mut wants: Vec<_> = removed.into_iter().map(|active| active.want).collect();
        self.pending.retain(|(want, _)| {
            if want.player == *player {
                wants.push(want.clone());
                false
            } else {
                true
            }
        });
        // A reboot invalidates every older attempt for this player, but must
        // not cancel another player's unrelated initial subscription.
        self.invalidate_renewals();
        for want in wants {
            if !self.pending.iter().any(|(pending, _)| *pending == want) {
                self.pending.push((want, now));
            }
        }
        // The rebooted player already forgot its old SIDs. Nothing has been
        // subscribed yet, and no UNSUBSCRIBE for those SIDs is necessary.
        Report::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{Service, Subscriber, active};
    use fsonos_proto::ProtoError;
    use fsonos_proto::gena::Subscription;

    fn want() -> Want {
        Want {
            player: PlayerId("retirement-test".into()),
            host: std::net::Ipv4Addr::LOCALHOST.into(),
            service: Service::RenderingControl,
        }
    }

    #[test]
    fn retirement_storage_is_bounded_and_duplicate_requests_keep_their_deadline() {
        let mut retire = Retirements::default();
        let now = Instant::now();
        for index in 0..MAX_RETIRED + 10 {
            retire.push((want(), format!("uuid:old-{index}")), now);
        }
        assert_eq!(retire.queue.len(), MAX_RETIRED);
        for _ in 0..100 {
            retire.push((want(), "uuid:old-0".into()), Instant::now());
        }
        assert_eq!(retire.queue.len(), MAX_RETIRED);
        assert_eq!(retire.due(), Some(now));
    }

    struct NoNetwork;

    impl Subscriber for NoNetwork {
        fn subscribe(&self, _: &Want, _: &str) -> Result<Subscription, ProtoError> {
            panic!("no subscription expected");
        }

        fn renew(&self, _: &Want, _: &str) -> Result<Subscription, ProtoError> {
            panic!("no renewal expected");
        }

        fn unsubscribe(&self, _: &Want, _: &str) -> Result<(), ProtoError> {
            panic!("an adopted SID must not be retired");
        }
    }

    #[test]
    fn retirement_does_not_unsubscribe_a_sid_adopted_again_at_the_same_service() {
        let now = Instant::now();
        let mut retire = Retirements::default();
        retire.push((want(), "uuid:reused".into()), now);
        let mut subs = Subscriptions::default();
        subs.active.push(active(
            want(),
            Subscription {
                sid: "uuid:reused".into(),
                timeout_secs: 300,
            },
            now,
        ));
        let provider: Provider = Arc::new(NoNetwork);
        retire.start(&subs, &[], &provider, &Arc::new(AtomicBool::new(false)));
        assert!(retire.queue.is_empty());
        assert_eq!(retire.len(), 0);
    }
}
