//! Release local authority immediately; network retirement is best effort.

use super::super::Workers;
use super::{Provider, Subscriptions};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Stop admitting teardown work after this interval. Calls already in flight
/// retain their transport timeout; the remote device expires remaining leases.
const CLEANUP_WINDOW: Duration = Duration::from_secs(30);

impl Subscriptions {
    /// Sleep and route changes leave cleanup on the normal bounded workers.
    /// Final shutdown hands those same slots to one non-network supervisor;
    /// the live thread never waits for a player to answer UNSUBSCRIBE.
    pub(crate) fn release_background(&mut self, provider: Provider, stopping: bool) {
        if !self.deferred_sync {
            self.unsubscribe_all(&*provider);
            return;
        }
        let _ = self.sync_deferred(&[], Instant::now());
        self.maintenance.cancel_all();
        self.pending_epoch = Arc::new(());
        self.maintenance.early.clear();
        self.maintenance.early_bytes = 0;
        if !stopping {
            return;
        }
        let workers = std::mem::take(&mut self.maintenance);
        if idle(&workers) {
            return;
        }
        let deadline = Instant::now() + CLEANUP_WINDOW;
        if let Err(error) = std::thread::Builder::new()
            .name("fsonos-lease-shutdown".into())
            .spawn(move || drain(workers, &provider, deadline))
        {
            tracing::warn!(%error, "cannot finish lease cleanup; subscriptions will expire");
        }
    }
}

fn idle(workers: &Workers) -> bool {
    workers.jobs.is_empty() && workers.retire.len() == 0 && workers.retirement_due().is_none()
}

fn drain(mut workers: Workers, provider: &Provider, deadline: Instant) {
    let mut empty = Subscriptions::default();
    // This flag admits only old-SID retirement, never renewal or SUBSCRIBE.
    // The original stop flag and cancellation stay set on the lease jobs.
    let cleanup_stop = Arc::new(AtomicBool::new(false));
    while Instant::now() < deadline {
        let _ = workers.reap(&mut empty, true);
        if idle(&workers) {
            break;
        }
        workers
            .retire
            .start(&empty, &workers.jobs, provider, &cleanup_stop);
        std::thread::sleep(Duration::from_millis(10));
    }
    // A spawned thread not yet scheduled must not start a late cleanup call.
    cleanup_stop.store(true, Ordering::Release);
    // Dropping Workers cancels unfinished lease jobs and disconnects their
    // cleanup replies. They can retire their own unadopted SID, but cannot
    // issue another renewal or subscription. No unfinished handle is joined.
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{Service, Subscriber, Want};
    use fsonos_proto::ProtoError;
    use fsonos_proto::gena::Subscription;
    use fsonos_types::PlayerId;
    use std::sync::atomic::AtomicUsize;

    #[derive(Default)]
    struct Calls(AtomicUsize);

    impl Subscriber for Calls {
        fn subscribe(&self, _: &Want, _: &str) -> Result<Subscription, ProtoError> {
            panic!("shutdown cannot subscribe");
        }

        fn renew(&self, _: &Want, _: &str) -> Result<Subscription, ProtoError> {
            panic!("shutdown cannot renew");
        }

        fn unsubscribe(&self, _: &Want, _: &str) -> Result<(), ProtoError> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    #[test]
    fn an_expired_shutdown_budget_does_not_admit_queued_network_calls() {
        let mut workers = Workers::default();
        workers.retire.push(
            (
                Want {
                    player: PlayerId("shutdown-test".into()),
                    host: std::net::Ipv4Addr::LOCALHOST.into(),
                    service: Service::RenderingControl,
                },
                "uuid:retire-after-stop".into(),
            ),
            Instant::now(),
        );
        let calls = Arc::new(Calls::default());
        let provider: Provider = calls.clone();
        drain(workers, &provider, Instant::now());
        assert_eq!(calls.0.load(Ordering::Relaxed), 0);
        assert_eq!(Arc::strong_count(&calls), 2);
    }
}
