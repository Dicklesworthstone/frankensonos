//! Confirm a lost native timer reply without extending or clearing it twice.
//! Countdown alone cannot identify a write: retain the player's generation,
//! physical group and boot epoch across one bounded read-back.

use crate::CoreError;
use crate::control::SourceDestination;
use fsonos_proto::control::{self as soap, SleepTimerInfo};
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::PlayerId;
use std::net::IpAddr;
use std::time::{Duration, Instant};

const PREPARE_BUDGET: Duration = Duration::from_millis(750);
const CONFIRM_BUDGET: Duration = Duration::from_secs(3);

struct Reads<'a, T: ?Sized> {
    inner: &'a T,
    until: Instant,
}

impl<T: Transport + ?Sized> Transport for Reads<'_, T> {
    fn http_get(&self, url: &str) -> Result<String, ProtoError> {
        if Instant::now() >= self.until {
            return Err(ProtoError::NotWired(
                "sleep timer observation budget elapsed",
            ));
        }
        self.inner.http_get(url)
    }

    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if Instant::now() >= self.until {
            return Err(ProtoError::NotWired(
                "sleep timer observation budget elapsed",
            ));
        }
        self.inner.soap_post(host, path, action, body)
    }
}

pub(super) fn configure<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    coordinator: &PlayerId,
    after: Option<u32>,
) -> Result<(), CoreError> {
    // Endpoint identity is mandatory even when optional timer-generation
    // evidence is unavailable. A refusal here precedes every native write,
    // so callers keep their existing armed timer and restoration ownership.
    crate::control::require_coordinator(t, host, coordinator)?;
    let preparation = Reads {
        inner: t,
        until: Instant::now() + PREPARE_BUDGET,
    };
    let proof = Observation::read(&preparation, host, coordinator)
        .ok()
        .flatten()
        .filter(|_| Instant::now() < preparation.until);
    // Preparation can itself observe a changed endpoint. Optional evidence
    // must never turn that refusal into permission to write at the old address.
    crate::control::require_coordinator(t, host, coordinator)?;
    let sent = Instant::now();
    let Err(error) = soap::configure_sleep_timer(t, host, after) else {
        return Ok(());
    };
    // A cancelled or failed worker does not prove the write was never sent.
    // Only a native refusal preserves the old local timer without read-back.
    if !matches!(
        &error,
        ProtoError::Network { .. } | ProtoError::Malformed(_) | ProtoError::NotWired(_)
    ) {
        return Err(error.into());
    }
    let confirmation = Reads {
        inner: t,
        until: Instant::now() + CONFIRM_BUDGET,
    };
    if proof.is_some_and(|proof| {
        proof
            .confirmed(&confirmation, host, coordinator, after, sent)
            .unwrap_or(false)
            && Instant::now() < confirmation.until
    }) {
        tracing::warn!(cause = %error, "sleep timer confirmed without replaying its write");
        Ok(())
    } else {
        Err(CoreError::Uncertain(format!(
            "the sleep timer change was not confirmed: {error}; \
             the request was not replayed; read the current timer before trying again"
        )))
    }
}

/// Clear a fallback once, preserving the cleanup caller's original reply
/// semantics. A changed or unreadable endpoint never authorizes the clear.
pub(super) fn clear<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    coordinator: &PlayerId,
) -> Result<(), CoreError> {
    crate::control::require_coordinator(t, host, coordinator)?;
    Ok(soap::configure_sleep_timer(t, host, None)?)
}

/// A successful leave can be witnessed while the player's own membership
/// remains stale. Verify that narrow case before the one cleanup write;
/// a failed native clear never enters another observation or write attempt.
pub(super) fn clear_after_leave<T: Transport + ?Sized>(
    t: &T,
    households: &[crate::HouseholdState],
    coordinator: &PlayerId,
) -> Result<(), CoreError> {
    let host = super::ip(households, coordinator)?;
    crate::control::require_coordinator_after_leave(t, households, host, coordinator)?;
    Ok(soap::configure_sleep_timer(t, host, None)?)
}

struct Observation {
    destination: SourceDestination,
    next_generation: u32,
}

impl Observation {
    fn read<T: Transport + ?Sized>(
        t: &T,
        host: IpAddr,
        coordinator: &PlayerId,
    ) -> Result<Option<Self>, ProtoError> {
        let destination = SourceDestination::read(t, host, coordinator)?;
        if !destination.has_boot_identity() {
            return Ok(None);
        }
        let timer = soap::get_sleep_timer_info(t, host)?;
        Ok(timer
            .generation
            .and_then(|generation| generation.checked_add(1))
            .map(|next_generation| Self {
                destination,
                next_generation,
            }))
    }

    fn confirmed<T: Transport + ?Sized>(
        &self,
        t: &T,
        host: IpAddr,
        coordinator: &PlayerId,
        after: Option<u32>,
        sent: Instant,
    ) -> Result<bool, ProtoError> {
        let first = soap::get_sleep_timer_info(t, host)?;
        if !self.matches(first, after, sent.elapsed())
            || !self
                .destination
                .same_group(&SourceDestination::read(t, host, coordinator)?)
        {
            return Ok(false);
        }
        let last = soap::get_sleep_timer_info(t, host)?;
        Ok(
            self.matches(last, after, sent.elapsed())
                && last.remaining_secs <= first.remaining_secs,
        )
    }

    fn matches(&self, timer: SleepTimerInfo, after: Option<u32>, age: Duration) -> bool {
        timer.generation == Some(self.next_generation)
            && countdown_matches(after, timer.remaining_secs, age)
    }
}

fn countdown_matches(expected: Option<u32>, observed: Option<u32>, age: Duration) -> bool {
    match (expected.map(|seconds| seconds.min(86_399)), observed) {
        (None | Some(0), None | Some(0)) => true,
        (Some(expected), Some(observed)) => {
            observed <= expected
                && u64::from(expected - observed) <= age.as_secs().saturating_add(1)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_requested_countdown_or_its_elapsed_seconds_match() {
        assert!(countdown_matches(Some(600), Some(600), Duration::ZERO));
        assert!(countdown_matches(Some(600), Some(599), Duration::ZERO));
        assert!(!countdown_matches(Some(600), Some(598), Duration::ZERO));
        assert!(countdown_matches(
            Some(600),
            Some(597),
            Duration::from_secs(2)
        ));
        assert!(!countdown_matches(
            Some(600),
            Some(601),
            Duration::from_secs(2)
        ));
        assert!(!countdown_matches(Some(600), None, Duration::from_secs(2)));
        assert!(countdown_matches(
            Some(u32::MAX),
            Some(86_399),
            Duration::ZERO
        ));
        assert!(countdown_matches(None, None, Duration::ZERO));
        assert!(countdown_matches(None, Some(0), Duration::ZERO));
        assert!(!countdown_matches(None, Some(1), Duration::ZERO));
    }
}
