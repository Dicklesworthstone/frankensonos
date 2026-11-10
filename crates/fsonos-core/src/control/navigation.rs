//! Reconcile a lost navigation reply without skipping a second time.
//! Only a distinct adjacent occurrence in NORMAL mode is deterministic here.
//! Optional preflight reads retain that occurrence and its queue/group/source;
//! an unavailable proof never prevents the original one-shot command.

use super::SourceDestination;
use fsonos_proto::content::{self, BrowseFlag, BrowsePage, QUEUE};
use fsonos_proto::control::{self as soap, PositionInfo};
use fsonos_proto::didl::{DidlKind, DidlObject, spotify_uri_from_renderer_uri};
use fsonos_proto::play_mode::get_play_mode;
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::{PlayMode, PlayerId, TransportState};
use std::net::IpAddr;
use std::time::{Duration, Instant};

const MAX_OBSERVATION_AGE: Duration = Duration::from_secs(30);
const PREPARE_BUDGET: Duration = Duration::from_millis(750);
const CONFIRM_BUDGET: Duration = Duration::from_secs(3);

/// Do not compound several slow probes into several full network timeouts.
/// An in-flight read still has the transport's timeout; subsequent reads
/// cannot start after this optional observation's aggregate budget expires.
struct Reads<'a, T: ?Sized> {
    inner: &'a T,
    until: Instant,
}

impl<T: Transport + ?Sized> Transport for Reads<'_, T> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if Instant::now() >= self.until {
            return Err(ProtoError::NotWired(
                "navigation observation budget elapsed",
            ));
        }
        self.inner.soap_post(host, path, action, body)
    }
}

#[derive(Clone, Copy)]
pub(super) enum Step {
    Next,
    Previous,
}

impl Step {
    fn target(self, current: u32) -> Option<u32> {
        if current == 0 {
            return None;
        }
        match self {
            Self::Next => current.checked_add(1),
            Self::Previous => current.checked_sub(1).filter(|track| *track > 0),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Next => "Next",
            Self::Previous => "Previous",
        }
    }
}

pub(super) fn step<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    coordinator: &PlayerId,
    step: Step,
) -> Result<(), ProtoError> {
    let preparation = Reads {
        inner: t,
        until: Instant::now() + PREPARE_BUDGET,
    };
    let proof = Observation::read(&preparation, host, coordinator, step)
        .ok()
        .flatten()
        .filter(|_| Instant::now() < preparation.until);
    let sent = Instant::now();
    let result = match step {
        Step::Next => soap::next(t, host),
        Step::Previous => soap::previous(t, host),
    };
    let Err(error) = result else {
        return Ok(());
    };
    if matches!(
        &error,
        ProtoError::Network { .. } | ProtoError::Malformed(_)
    ) && proof.is_some_and(|proof| {
        let confirmation = Reads {
            inner: t,
            until: Instant::now() + CONFIRM_BUDGET,
        };
        proof
            .confirmed(&confirmation, host, coordinator, sent)
            .unwrap_or(false)
            && Instant::now() < confirmation.until
    }) {
        tracing::warn!(
            action = step.name(),
            cause = %error,
            "queue navigation observed without repeating its relative step"
        );
        Ok(())
    } else {
        // A refusal, an unchanged or different occurrence, and unavailable
        // read-back all retain the original write error, without any replay.
        Err(error)
    }
}

struct Observation {
    destination: SourceDestination,
    page: BrowsePage,
    offset: u32,
    before: PositionInfo,
    target: u32,
    state: TransportState,
    observed: Instant,
}

impl Observation {
    fn read<T: Transport + ?Sized>(
        t: &T,
        host: IpAddr,
        coordinator: &PlayerId,
        step: Step,
    ) -> Result<Option<Self>, ProtoError> {
        let observed = Instant::now();
        let before = soap::get_position_info(t, host)?;
        let Some(target) = step.target(before.track) else {
            return Ok(None);
        };
        if before.position_secs.is_none()
            || !known_uri(&before.uri)
            || get_play_mode(t, host)? != PlayMode::Normal
        {
            return Ok(None);
        }
        let destination = SourceDestination::read(t, host, coordinator)?;
        if !destination.is_queue(coordinator) || !destination.has_boot_identity() {
            return Ok(None);
        }
        // A single two-entry page, independent of the complete queue's size.
        // A short page simply cannot supply recovery evidence for this step.
        let offset = before.track.min(target) - 1;
        let page = adjacent(t, host, offset)?;
        if page.update_id == 0
            || page.number_returned != 2
            || page.objects.len() != 2
            || page.total_matches > content::MAX_BROWSE_OBJECTS
            || offset
                .checked_add(2)
                .is_none_or(|end| end > page.total_matches)
            || !same_track(
                &before.uri,
                resource(&page.objects[usize::from(before.track > target)]),
            )
            || !known_uri(resource(&page.objects[usize::from(target > before.track)]))
        {
            return Ok(None);
        }
        let transport = soap::get_transport_info(t, host)?;
        if !matches!(
            transport.state,
            TransportState::Playing | TransportState::Paused | TransportState::Stopped
        ) || transport.status == "ERROR_OCCURRED"
        {
            return Ok(None);
        }
        let last = soap::get_position_info(t, host)?;
        if !same_occurrence(&before, &last)
            || !clock_progress(&before, &last, transport.state, observed.elapsed())
        {
            return Ok(None);
        }
        Ok(Some(Self {
            destination,
            page,
            offset,
            before,
            target,
            state: transport.state,
            observed,
        }))
    }

    fn selected(&self, position: &PositionInfo, sent: Instant) -> bool {
        let age = self.observed.elapsed();
        let Some(before_secs) = self.before.position_secs else {
            return false;
        };
        if age > MAX_OBSERVATION_AGE {
            return false;
        }
        // An automatic end-of-track transition is not proof of Next. A
        // missing duration cannot rule it out while the original track plays.
        if self.target > self.before.track
            && self.state == TransportState::Playing
            && self.before.duration_secs.is_none_or(|duration| {
                duration == 0 || u64::from(before_secs) + age.as_secs() + 1 >= u64::from(duration)
            })
        {
            return false;
        }
        let item = &self.page.objects[usize::from(self.target > self.before.track)];
        position.track == self.target
            && same_track(&position.uri, resource(item))
            && position.position_secs.is_some_and(|seconds| {
                // Navigation starts the adjacent occurrence at its beginning.
                // A manual seek deep into it cannot confirm this request.
                u64::from(seconds) <= sent.elapsed().as_secs() + 1
                    && position
                        .duration_secs
                        .is_none_or(|duration| duration == 0 || seconds < duration)
            })
    }

    fn unchanged<T: Transport + ?Sized>(
        &self,
        t: &T,
        host: IpAddr,
        coordinator: &PlayerId,
    ) -> Result<bool, ProtoError> {
        if self.observed.elapsed() > MAX_OBSERVATION_AGE
            || self.page != adjacent(t, host, self.offset)?
            || get_play_mode(t, host)? != PlayMode::Normal
        {
            return Ok(false);
        }
        self.destination.confirm(t, host, coordinator)?;
        let transport = soap::get_transport_info(t, host)?;
        Ok(transport.state == self.state && transport.status != "ERROR_OCCURRED")
    }

    fn confirmed<T: Transport + ?Sized>(
        &self,
        t: &T,
        host: IpAddr,
        coordinator: &PlayerId,
        sent: Instant,
    ) -> Result<bool, ProtoError> {
        if self.observed.elapsed() > MAX_OBSERVATION_AGE {
            return Ok(false);
        }
        let first_at = Instant::now();
        let first = soap::get_position_info(t, host)?;
        if !self.selected(&first, sent) || !self.unchanged(t, host, coordinator)? {
            return Ok(false);
        }
        let last = soap::get_position_info(t, host)?;
        Ok(self.selected(&last, sent)
            && same_occurrence(&first, &last)
            && clock_progress(&first, &last, self.state, first_at.elapsed()))
    }
}

fn adjacent<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    offset: u32,
) -> Result<BrowsePage, ProtoError> {
    content::browse(t, host, QUEUE, BrowseFlag::DirectChildren, offset, 2)
}

fn resource(item: &DidlObject) -> &str {
    if item.kind != DidlKind::Item {
        return "";
    }
    item.res
        .as_ref()
        .map_or("", |resource| resource.uri.as_str())
}

fn known_uri(uri: &str) -> bool {
    !uri.trim().is_empty() && uri != "NOT_IMPLEMENTED"
}

fn same_track(actual: &str, expected: &str) -> bool {
    if !known_uri(actual) || !known_uri(expected) {
        return false;
    }
    match (
        spotify_uri_from_renderer_uri(actual),
        spotify_uri_from_renderer_uri(expected),
    ) {
        (Some(actual), Some(expected)) => actual == expected,
        _ => actual == expected,
    }
}

fn same_occurrence(before: &PositionInfo, after: &PositionInfo) -> bool {
    before.track == after.track
        && before.duration_secs == after.duration_secs
        && same_track(&before.uri, &after.uri)
}

fn clock_progress(
    before: &PositionInfo,
    after: &PositionInfo,
    state: TransportState,
    age: Duration,
) -> bool {
    let (Some(before), Some(after)) = (before.position_secs, after.position_secs) else {
        return false;
    };
    age <= MAX_OBSERVATION_AGE
        && match state {
            TransportState::Playing => {
                after >= before && u64::from(after - before) <= age.as_secs() + 1
            }
            TransportState::Paused | TransportState::Stopped => after == before,
            TransportState::Transitioning | TransportState::Unknown => false,
        }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    struct Count(Cell<usize>);

    impl Transport for Count {
        fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
            self.0.set(self.0.get() + 1);
            Ok(String::new())
        }
    }

    #[test]
    fn exhausted_optional_read_budget_never_starts_another_probe() {
        let inner = Count(Cell::new(0));
        let reads = Reads {
            inner: &inner,
            until: Instant::now(),
        };
        assert!(matches!(
            soap::get_position_info(&reads, "192.0.2.1".parse().unwrap()),
            Err(ProtoError::NotWired(_))
        ));
        assert_eq!(inner.0.get(), 0);
    }
}
