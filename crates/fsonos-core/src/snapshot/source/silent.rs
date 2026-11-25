//! Keep inactive snapshots silent even on renderers that retain transport state
//! when selecting a source. Pausing is authorized separately from selecting it.

use super::{Anchor, Target, unproven};
use crate::snapshot::RestoreOp;
use crate::{CoreError, HouseholdState, control};
use fsonos_proto::control as soap;
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::{PlayerId, TransportState};
use std::cell::Cell;
use std::net::IpAddr;

fn inactive(state: TransportState) -> bool {
    matches!(state, TransportState::Paused | TransportState::Stopped)
}

fn same_source<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    coordinator: &PlayerId,
    before: &Anchor,
) -> Result<Anchor, CoreError> {
    let current = Anchor::read(t, host, coordinator)?;
    if !before.same_source(&current)
        || !crate::snapshot::same_media(&before.media, &soap::get_media_info(t, host)?)
    {
        return Err(unproven(
            "source or group changed while preparing a silent restoration",
        ));
    }
    Ok(current)
}

fn unchanged<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    coordinator: &PlayerId,
    before: &Anchor,
) -> Result<Anchor, CoreError> {
    let current = same_source(t, host, coordinator, before)?;
    if !before.agrees(&current) {
        return Err(unproven(
            "playback occurrence or state changed while preparing a silent restoration",
        ));
    }
    Ok(current)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Write {
    Pause,
    Stop,
}

/// Only the existing pause helper's Pause and fallback Stop cross this guard.
/// Its preparatory reads cannot authorize pausing another app's newer source.
struct Pause<'a, T: ?Sized> {
    inner: &'a T,
    host: IpAddr,
    coordinator: &'a PlayerId,
    before: &'a Anchor,
    vet: &'a dyn Fn(&RestoreOp) -> Result<RestoreOp, String>,
    rejected: Cell<bool>,
    written: Cell<Option<Write>>,
}

impl<T: Transport + ?Sized> Transport for Pause<'_, T> {
    fn http_get(&self, url: &str) -> Result<String, ProtoError> {
        self.inner.http_get(url)
    }

    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let (written, name) = if action.ends_with("#Pause\"") {
            (Write::Pause, "Pause")
        } else if action.ends_with("#Stop\"") {
            (Write::Stop, "Stop")
        } else {
            return self.inner.soap_post(host, path, action, body);
        };
        if self.rejected.get()
            || host != self.host
            || !(self.vet)(&RestoreOp::Stop).is_ok_and(|op| op == RestoreOp::Stop)
        {
            self.rejected.set(true);
            return Err(ProtoError::Malformed(
                "silent restoration pause was not authorized".into(),
            ));
        }
        if let Err(error) = unchanged(self.inner, host, self.coordinator, self.before) {
            self.rejected.set(true);
            return Err(ProtoError::Malformed(error.to_string()));
        }
        self.written.set(Some(written));
        let reply = self.inner.soap_post(host, path, action, body);
        let refused = match &reply {
            Err(ProtoError::SoapFault { .. }) => true,
            Ok(body) => fsonos_proto::soap::parse_response(body, name)
                .is_err_and(|error| matches!(error, ProtoError::SoapFault { .. })),
            _ => false,
        };
        if refused {
            // A definite refusal cannot own a later manual pause, even if the
            // shared helper observes that state before attempting its fallback.
            self.written.set(None);
        }
        reply
    }
}

pub(super) fn prepare<T: Transport + ?Sized>(
    t: &T,
    household: &HouseholdState,
    host: IpAddr,
    coordinator: &PlayerId,
    before: Anchor,
    vet: &dyn Fn(&RestoreOp) -> Result<RestoreOp, String>,
) -> Result<Anchor, CoreError> {
    if inactive(before.state) {
        return Ok(before);
    }
    if before.state == TransportState::Unknown {
        return Err(unproven(
            "playback state is unknown before silent restoration",
        ));
    }
    let guarded = Pause {
        inner: t,
        host,
        coordinator,
        before: &before,
        vet,
        rejected: Cell::new(false),
        written: Cell::new(None),
    };
    let paused = control::pause_confirmed(&guarded, std::slice::from_ref(household), coordinator);
    if guarded.rejected.get() {
        // A local veto is not an uncertain device write. The shared helper's
        // inactive read-back must not turn another controller's pause into ours.
        return Err(unproven("silent restoration pause preflight was rejected"));
    }
    paused?;
    let Some(written) = guarded.written.get() else {
        return Err(unproven(
            "playback became inactive without an authorized restoration write",
        ));
    };
    let after = same_source(t, host, coordinator, &before)?;
    if !inactive(after.state) || (written == Write::Stop && after.state != TransportState::Stopped)
    {
        return Err(unproven(
            "playback did not retain the authorized inactive state",
        ));
    }
    let same_occurrence = before.track == after.track && before.track_uri == after.track_uri;
    let cleared_by_stop = written == Write::Stop
        && (after.track == 0 || after.track == before.track)
        && (matches!(after.track_uri.trim(), "" | "NOT_IMPLEMENTED")
            || after.track_uri == before.track_uri);
    if !same_occurrence && !cleared_by_stop {
        return Err(unproven(
            "playback occurrence changed while preparing a silent restoration",
        ));
    }
    // An authorized Stop can clear the selected position. Only this dispatched
    // write permits that relaxation; its new anchor governs source selection.
    Ok(after)
}

pub(super) fn before_selection<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    coordinator: &PlayerId,
    before: &Anchor,
) -> Result<(), CoreError> {
    let current = unchanged(t, host, coordinator, before)?;
    if !inactive(current.state) || soap::get_transport_info(t, host)?.state != before.state {
        return Err(unproven(
            "playback is no longer inactive before source restoration",
        ));
    }
    Ok(())
}

pub(super) fn after_selection<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    coordinator: &PlayerId,
    target: &Target,
    members: &[PlayerId],
) -> Result<(), CoreError> {
    if !inactive(soap::get_transport_info(t, host)?.state)
        || !target.confirmed(t, host, coordinator, members)?
        || !inactive(soap::get_transport_info(t, host)?.state)
    {
        return Err(unproven(
            "restored source was not confirmed inactive; no seek or resume was sent",
        ));
    }
    Ok(())
}
