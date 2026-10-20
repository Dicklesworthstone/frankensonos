//! Run Sonos evidence reads on the stream's owner thread, never a second transport.

use std::sync::mpsc::{self, Receiver, SyncSender};
use std::time::{Duration, Instant};

use anyhow::{bail, ensure};

use super::Stop;

const POLL: Duration = Duration::from_millis(100);
const TIMEOUT: Duration = Duration::from_secs(60);

type Verdict = Result<(), String>;

struct Request {
    settled: bool,
    reply: SyncSender<Verdict>,
}

pub(super) struct Checks {
    pending: SyncSender<Request>,
    timeout: Duration,
}

pub(super) struct Requests {
    pending: Receiver<Request>,
}

pub(super) fn channel() -> (Checks, Requests) {
    let (send, receive) = mpsc::sync_channel(1);
    (
        Checks {
            pending: send,
            timeout: TIMEOUT,
        },
        Requests { pending: receive },
    )
}

impl Checks {
    /// One outstanding observation. Cancellation never waits for a late reply
    /// or permits that reply to authorize the next window.
    pub(super) fn verify(&self, settled: bool, stop: &Stop) -> anyhow::Result<bool> {
        if stop.requested() {
            return Ok(false);
        }
        let (send, receive) = mpsc::sync_channel(1);
        self.pending
            .try_send(Request {
                settled,
                reply: send,
            })
            .map_err(|_| anyhow::anyhow!("stream ownership verifier is unavailable"))?;
        let started = Instant::now();
        loop {
            if stop.requested() {
                return Ok(false);
            }
            let remaining = self.timeout.saturating_sub(started.elapsed());
            ensure!(
                !remaining.is_zero(),
                "stream ownership verification deadline exceeded"
            );
            match receive.recv_timeout(remaining.min(POLL)) {
                Ok(verdict) => {
                    if stop.requested() {
                        return Ok(false);
                    }
                    ensure!(
                        started.elapsed() < self.timeout,
                        "stream ownership verification reply arrived after its deadline"
                    );
                    verdict.map_err(anyhow::Error::msg)?;
                    return Ok(true);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    bail!("stream ownership verifier stopped without evidence");
                }
            }
        }
    }
}

impl Requests {
    /// Called from the existing stream supervisor. A failed observation is
    /// delivered to the correction worker, not replayed and not a stream write.
    pub(super) fn service(&self, verify: &mut impl FnMut(bool) -> anyhow::Result<()>) {
        if let Ok(request) = self.pending.try_recv() {
            let verdict = verify(request.settled).map_err(|error| format!("{error:#}"));
            let _ = request.reply.try_send(verdict);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    fn stop() -> Stop {
        Stop {
            session: Arc::new(AtomicBool::new(false)),
            stream: Arc::new(AtomicBool::new(false)),
            deadline: None,
        }
    }

    #[test]
    fn successful_verification_requires_a_fresh_reply_for_each_phase() {
        let (checks, requests) = channel();
        let worker = thread::spawn(move || {
            let stop = stop();
            assert!(checks.verify(false, &stop).unwrap());
            assert!(checks.verify(true, &stop).unwrap());
        });
        for settled in [false, true] {
            let request = requests
                .pending
                .recv_timeout(Duration::from_secs(2))
                .unwrap();
            assert_eq!(request.settled, settled);
            request.reply.send(Ok(())).unwrap();
        }
        worker.join().unwrap();
    }

    #[test]
    fn a_failed_observation_is_returned_without_another_request() {
        let (checks, requests) = channel();
        let worker = thread::spawn(move || checks.verify(true, &stop()));
        let request = requests
            .pending
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        request
            .reply
            .send(Err("room now plays another source".into()))
            .unwrap();
        let error = worker.join().unwrap().unwrap_err();
        assert!(error.to_string().contains("another source"));
        assert!(requests.pending.try_recv().is_err());
    }

    #[test]
    fn cancellation_unblocks_a_worker_even_without_a_supervisor_reply() {
        for cancel_session in [false, true] {
            let (checks, requests) = channel();
            let stop = stop();
            let cancel = Arc::clone(if cancel_session {
                &stop.session
            } else {
                &stop.stream
            });
            let worker = thread::spawn(move || checks.verify(false, &stop));
            let request = requests
                .pending
                .recv_timeout(Duration::from_secs(2))
                .unwrap();
            cancel.store(true, Ordering::Release);
            assert!(!worker.join().unwrap().unwrap());
            assert!(
                request.reply.send(Ok(())).is_err(),
                "late evidence is discarded"
            );
        }
    }

    #[test]
    fn cancellation_before_dispatch_does_not_request_an_observation() {
        let (checks, requests) = channel();
        let stop = stop();
        stop.session.store(true, Ordering::Release);
        assert!(!checks.verify(false, &stop).unwrap());
        assert!(requests.pending.try_recv().is_err());
    }

    #[test]
    fn a_stalled_or_disconnected_verifier_cannot_authorize_capture() {
        let (mut checks, requests) = channel();
        checks.timeout = Duration::from_millis(20);
        let started = Instant::now();
        assert!(checks.verify(false, &stop()).is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(requests);
        assert!(checks.verify(false, &stop()).is_err());
    }

    #[test]
    fn service_executes_only_on_demand_and_preserves_the_verifiers_failure() {
        let (mut checks, requests) = channel();
        checks.timeout = Duration::from_secs(1);
        let mut calls = Vec::new();
        requests.service(&mut |settled| {
            calls.push(settled);
            Ok(())
        });
        assert!(calls.is_empty());
        let worker = thread::spawn(move || checks.verify(true, &stop()));
        let deadline = Instant::now() + Duration::from_secs(2);
        while !worker.is_finished() && Instant::now() < deadline {
            requests.service(&mut |settled| {
                calls.push(settled);
                bail!("group membership changed");
            });
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            worker
                .join()
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("membership")
        );
        assert_eq!(calls, [true]);
    }
}
