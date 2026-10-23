//! Recovery for read-only Web API requests. Token POSTs are deliberately outside
//! the retry loop. One deadline includes connection/body reads, refresh and waits.

use super::{BRIEF_WAIT_SECS, REQUEST_TIMEOUT, Session, SpotifyError, unix_now};
use crate::client::api_error;
use asupersync::http::h1::{ClientError, HttpError};
use asupersync::time::{sleep, timeout, wall_now};
use asupersync::{Cx, Time};
use chrono::DateTime;
use std::io::ErrorKind;
use std::time::{Duration, Instant};

const MAX_ATTEMPTS: u32 = 4;
const READ_TIMEOUT: Duration = Duration::from_secs(90);
const BRIEF_TIMEOUT: Duration = Duration::from_secs(35);
const CANCEL_POLL: Duration = Duration::from_millis(100);

/// One session's Web API rate limit, retained even if a read returns early or
/// its future is dropped during backoff. This is process-local, not persisted.
/// Instant remains monotonic if the session is used with a different Cx later.
#[derive(Default)]
pub(super) struct Cooldown {
    limited: Option<(Instant, Duration)>,
}

impl Cooldown {
    fn record(&mut self, header: Option<&str>, unix: i64) {
        let delay = retry_after(header, unix)
            .unwrap_or(Some(Duration::MAX))
            .unwrap_or(Duration::from_secs(1));
        self.limited = (!delay.is_zero()).then(|| (Instant::now(), delay));
    }

    fn remaining_at(&self, now: Instant) -> Duration {
        self.limited.map_or(Duration::ZERO, |(started, delay)| {
            delay.saturating_sub(now.saturating_duration_since(started))
        })
    }

    pub(super) fn remaining(&self) -> Duration {
        self.remaining_at(Instant::now())
    }

    async fn admit(
        &mut self,
        cx: &Cx,
        max_wait_secs: u64,
        budget: Duration,
    ) -> Result<(), SpotifyError> {
        let started = Instant::now();
        loop {
            checkpoint(cx)?;
            let left = self.remaining_at(Instant::now());
            if left.is_zero() {
                self.limited = None;
                return Ok(());
            }
            if max_wait_secs == 0
                || left > Duration::from_secs(max_wait_secs)
                || left >= budget.saturating_sub(started.elapsed())
            {
                let seconds = left
                    .as_secs()
                    .saturating_add(u64::from(left.subsec_nanos() != 0));
                return Err(SpotifyError::Api {
                    status: 429,
                    body: format!("Web API rate limit still active; retry in {seconds} seconds"),
                });
            }
            // Do not clear the limit before waiting: dropping this future is
            // not permission for the next caller to retry early.
            wait(cx, left).await?;
        }
    }
}

fn now(cx: &Cx) -> Time {
    cx.timer_driver()
        .or_else(|| Cx::with_current(Cx::timer_driver).flatten())
        .map_or_else(wall_now, |timer| timer.now())
}

fn checkpoint(cx: &Cx) -> Result<(), SpotifyError> {
    cx.checkpoint()
        .map_err(|_| SpotifyError::Http("Spotify read cancelled".into()))
}

fn transient_io(kind: ErrorKind) -> bool {
    matches!(
        kind,
        ErrorKind::ConnectionRefused
            | ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::NotConnected
            | ErrorKind::AddrNotAvailable
            | ErrorKind::BrokenPipe
            | ErrorKind::TimedOut
            | ErrorKind::UnexpectedEof
            | ErrorKind::Interrupted
            | ErrorKind::WouldBlock
    )
}

fn transient_transport(error: &ClientError) -> bool {
    match error {
        ClientError::DnsError(error) => {
            transient_io(error.kind()) || error.kind() == ErrorKind::Other
        }
        ClientError::ConnectError(error)
        | ClientError::Io(error)
        | ClientError::HttpError(HttpError::Io(error)) => transient_io(error.kind()),
        // Cancellation, exhausted deadlines, TLS/configuration failures and
        // malformed/oversized protocol messages cannot be repaired by replay.
        _ => false,
    }
}

fn transient_status(status: u16) -> bool {
    matches!(status, 408 | 429 | 500 | 502 | 503 | 504)
}

/// `Err` means an integer delay too large even to represent. Never turn it into
/// a short default delay. Malformed dates/values otherwise use normal backoff.
fn retry_after(header: Option<&str>, unix: i64) -> Result<Option<Duration>, ()> {
    let Some(value) = header.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    if value.bytes().all(|byte| byte.is_ascii_digit()) {
        return value
            .parse::<u64>()
            .map(|secs| Some(Duration::from_secs(secs)))
            .map_err(|_| ());
    }
    Ok(DateTime::parse_from_rfc2822(value).ok().map(|at| {
        Duration::from_secs(u64::try_from(at.timestamp().saturating_sub(unix)).unwrap_or(0))
    }))
}

/// `attempt` is zero-based. Both rate limits and other transient errors consume
/// the same finite attempt budget; alternating failures cannot reset it.
fn retry_delay(
    attempt: u32,
    status: Option<u16>,
    header: Option<&str>,
    unix: i64,
    max_wait_secs: u64,
    remaining: Duration,
) -> Option<Duration> {
    if attempt >= MAX_ATTEMPTS - 1 || max_wait_secs == 0 {
        return None;
    }
    if status.is_some_and(|status| !transient_status(status)) {
        return None;
    }
    let backoff = Duration::from_millis(250 * (1_u64 << attempt));
    let fallback = if status == Some(429) {
        Duration::from_secs(1)
    } else {
        Duration::ZERO
    };
    let required = retry_after(header, unix).ok()?.unwrap_or(fallback);
    let delay = backoff.max(required);
    // Leave time for the next attempt. Refuse a wait that cannot fit, rather
    // than truncating Retry-After and hitting Spotify before permission to retry.
    (delay <= Duration::from_secs(max_wait_secs) && delay < remaining).then_some(delay)
}

async fn wait(cx: &Cx, delay: Duration) -> Result<(), SpotifyError> {
    let started = now(cx);
    loop {
        checkpoint(cx)?;
        let elapsed = Duration::from_nanos(now(cx).duration_since(started));
        let left = delay.saturating_sub(elapsed);
        if left.is_zero() {
            return Ok(());
        }
        // Sleep does not itself take a Cx. Bound cancellation latency even when
        // Retry-After asks for a long wait, without busy-spinning.
        sleep(now(cx), left.min(CANCEL_POLL)).await;
    }
}

pub(super) async fn get(
    session: &mut Session,
    cx: &Cx,
    url: &str,
    max_wait_secs: u64,
) -> Result<Vec<u8>, SpotifyError> {
    let limit = match max_wait_secs {
        0 => REQUEST_TIMEOUT,
        1..=BRIEF_WAIT_SECS => BRIEF_TIMEOUT,
        _ => READ_TIMEOUT,
    };
    get_with_limit(session, cx, url, max_wait_secs, limit).await
}

async fn get_with_limit(
    session: &mut Session,
    cx: &Cx,
    url: &str,
    max_wait_secs: u64,
    limit: Duration,
) -> Result<Vec<u8>, SpotifyError> {
    if !session.endpoints.is_api_url(url) {
        return Err(SpotifyError::Config(format!(
            "refusing to send the Spotify token outside the Web API: {url}"
        )));
    }
    checkpoint(cx)?;
    let started = now(cx);
    let remaining = cx
        .budget()
        .deadline
        .map(|deadline| Duration::from_nanos(deadline.duration_since(started)));
    let limit = remaining.map_or(limit, |remaining| limit.min(remaining));
    if limit.is_zero() {
        return Err(deadline_error());
    }
    let read = attempts(session, cx, url, max_wait_secs, started, limit);
    timeout(started, limit, std::pin::pin!(read))
        .await
        .map_err(|_| deadline_error())?
}

fn deadline_error() -> SpotifyError {
    SpotifyError::Http("Spotify read deadline exceeded (including retries and refresh)".into())
}

async fn attempts(
    session: &mut Session,
    cx: &Cx,
    url: &str,
    max_wait_secs: u64,
    started: Time,
    limit: Duration,
) -> Result<Vec<u8>, SpotifyError> {
    let remaining = || limit.saturating_sub(Duration::from_nanos(now(cx).duration_since(started)));
    let mut refreshed = false;
    for attempt in 0..MAX_ATTEMPTS {
        checkpoint(cx)?;
        if remaining().is_zero() {
            return Err(deadline_error());
        }
        session
            .api_cooldown
            .admit(cx, max_wait_secs, remaining())
            .await?;
        if remaining().is_zero() {
            return Err(deadline_error());
        }
        let token = session.access_token(cx).await?;
        checkpoint(cx)?;
        let left = remaining();
        if left.is_zero() {
            return Err(deadline_error());
        }
        let response = session
            .http
            .get(url)
            .bearer_auth(&token)
            .header("Accept", "application/json")
            .timeout(REQUEST_TIMEOUT.min(left))
            .send(cx)
            .await;
        if let Ok(response) = &response
            && response.status == 429
        {
            // Remember the limit even on the final attempt or a cancelled read.
            session
                .api_cooldown
                .record(response.header_value("Retry-After"), unix_now());
        }
        checkpoint(cx)?;
        if remaining().is_zero() {
            return Err(deadline_error());
        }
        match response {
            Ok(response) => {
                if response.is_success() {
                    return Ok(response.body);
                }
                if response.status == 401 && !refreshed && attempt + 1 < MAX_ATTEMPTS {
                    refreshed = true;
                    // At most one reactive refresh; never retry a token POST.
                    session.refresh(cx).await?;
                    continue;
                }
                let delay = retry_delay(
                    attempt,
                    Some(response.status),
                    response.header_value("Retry-After"),
                    unix_now(),
                    max_wait_secs,
                    remaining(),
                );
                let Some(delay) = delay else {
                    return Err(api_error(response.status, &response.body));
                };
                wait(cx, delay).await?;
            }
            Err(error) => {
                let delay = transient_transport(&error)
                    .then(|| {
                        retry_delay(attempt, None, None, unix_now(), max_wait_secs, remaining())
                    })
                    .flatten();
                let Some(delay) = delay else {
                    return Err(SpotifyError::Http(error.to_string()));
                };
                wait(cx, delay).await?;
            }
        }
    }
    // Every last-attempt failure returns its original typed error above.
    Err(deadline_error())
}

#[cfg(test)]
mod tests;
