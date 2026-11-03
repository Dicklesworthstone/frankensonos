//! URL clients follow the daemon's bounded JSON event reads over HTTP or HTTPS.
//! Only read failures are retried; one cursor covers quiet polls and reconnects.

use super::Event;
use fsonos_api::events::EventsDto;
use fsonos_api::{ErrorCode, Failure};
use serde_json::json;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(15);
const CHECK_STOP: Duration = Duration::from_millis(100);
const RETRY: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];

/// The request worker owns no playback. A stopped reader need not wait for
/// its one bounded GET to finish before returning to the terminal.
pub(super) fn run(
    poll: impl FnMut(Option<u64>, Duration) -> Result<EventsDto, Failure> + Send + 'static,
    since: Option<u64>,
    output: impl FnMut(Event) -> io::Result<bool>,
) -> anyhow::Result<()> {
    let stop = Arc::new(AtomicBool::new(false));
    let _signals = Signals::new(&stop)?;
    follow(poll, since, stop, output, &RETRY)
}

struct Signals(Vec<signal_hook::SigId>);

impl Signals {
    fn new(stop: &Arc<AtomicBool>) -> io::Result<Self> {
        let mut registered = Self(Vec::new());
        for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
            registered
                .0
                .push(signal_hook::flag::register(signal, Arc::clone(stop))?);
        }
        Ok(registered)
    }
}

impl Drop for Signals {
    fn drop(&mut self) {
        for signal in self.0.drain(..) {
            signal_hook::low_level::unregister(signal);
        }
    }
}

struct Reader {
    ask: mpsc::SyncSender<Option<u64>>,
    replies: mpsc::Receiver<Result<EventsDto, Failure>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Reader {
    fn start(
        mut poll: impl FnMut(Option<u64>, Duration) -> Result<EventsDto, Failure> + Send + 'static,
        stop: Arc<AtomicBool>,
    ) -> io::Result<Self> {
        let (ask, requests) = mpsc::sync_channel::<Option<u64>>(1);
        let (answers, replies) = mpsc::sync_channel(1);
        let stopping = Arc::clone(&stop);
        let worker = thread::Builder::new()
            .name("fsonos-events".into())
            .spawn(move || {
                while let Ok(since) = requests.recv() {
                    if stopping.load(Ordering::Acquire) {
                        break;
                    }
                    // Establish the initial cursor without a waiting response
                    // whose loss could silently discard the first changes.
                    let wait = if since.is_some() {
                        WAIT
                    } else {
                        Duration::ZERO
                    };
                    let result = poll(since, wait);
                    if stopping.load(Ordering::Acquire) || answers.send(result).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self {
            ask,
            replies,
            stop,
            worker: Some(worker),
        })
    }

    fn next(&self, since: Option<u64>) -> Result<Option<EventsDto>, Failure> {
        if self.stop.load(Ordering::Acquire) {
            return Ok(None);
        }
        self.ask.send(since).map_err(|_| worker_ended())?;
        loop {
            if self.stop.load(Ordering::Acquire) {
                return Ok(None);
            }
            match self.replies.recv_timeout(CHECK_STOP) {
                Ok(reply) => {
                    return if self.stop.load(Ordering::Acquire) {
                        Ok(None)
                    } else {
                        reply.map(Some)
                    };
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err(worker_ended()),
            }
        }
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take()
            && worker.is_finished()
        {
            let _ = worker.join();
        }
    }
}

fn worker_ended() -> Failure {
    Failure::new(
        ErrorCode::Internal,
        "the daemon event reader stopped unexpectedly",
    )
}

fn follow(
    poll: impl FnMut(Option<u64>, Duration) -> Result<EventsDto, Failure> + Send + 'static,
    mut since: Option<u64>,
    stop: Arc<AtomicBool>,
    mut output: impl FnMut(Event) -> io::Result<bool>,
    retry: &[Duration],
) -> anyhow::Result<()> {
    let reader = Reader::start(poll, stop)?;
    let mut failed = 0;
    loop {
        let batch = match reader.next(since) {
            Ok(Some(batch)) => batch,
            Ok(None) => return Ok(()),
            Err(failure) if failure.code == ErrorCode::NotReady && failed < retry.len() => {
                let delay = retry[failed];
                failed += 1;
                if !wait(delay, &reader.stop) {
                    return Ok(());
                }
                continue;
            }
            Err(mut failure) => {
                if failure.code == ErrorCode::NotReady {
                    failure.hint = since.map_or_else(
                        || "Check the daemon URL and tailnet connection, then run fsonos events again.".into(),
                        |id| format!(
                            "Check the daemon URL and tailnet connection; run fsonos events --since {id} with the same --daemon-url to continue."
                        ),
                    );
                }
                return Err(failure.into());
            }
        };
        validate(&batch, since)?;
        failed = 0;
        if let Some(oldest) = batch.reset
            && !output(Event {
                id: None,
                kind: "events.reset".into(),
                data: json!({ "oldest": oldest }),
            })?
        {
            return Ok(());
        }
        for event in batch.events {
            if reader.stop.load(Ordering::Acquire)
                || !output(Event {
                    id: Some(event.id),
                    kind: event.event,
                    data: event.data,
                })?
            {
                return Ok(());
            }
        }
        // Empty polls advance the initial cursor too. A reset may move it
        // backwards after a restart; keeping the old id would reset forever.
        since = Some(batch.last_id);
    }
}

/// A malformed answer must not replay events or move a valid cursor backwards
/// without the reset that tells the listener its history was lost.
fn validate(batch: &EventsDto, since: Option<u64>) -> Result<(), Failure> {
    let bad = || {
        Failure::new(
            ErrorCode::Internal,
            "the daemon returned an inconsistent event cursor",
        )
    };
    let mut after = match batch.reset {
        Some(0) => return Err(bad()),
        Some(oldest) => oldest - 1,
        None => since.unwrap_or_default(),
    };
    for event in &batch.events {
        if event.id <= after {
            return Err(bad());
        }
        after = event.id;
    }
    if (since.is_some() || batch.reset.is_some() || !batch.events.is_empty())
        && after != batch.last_id
    {
        return Err(bad());
    }
    Ok(())
}

fn wait(for_time: Duration, stop: &AtomicBool) -> bool {
    let until = Instant::now() + for_time;
    loop {
        if stop.load(Ordering::Acquire) {
            return false;
        }
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return true;
        }
        thread::sleep(left.min(CHECK_STOP));
    }
}

#[cfg(test)]
mod tests;
