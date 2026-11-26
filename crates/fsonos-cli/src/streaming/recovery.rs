//! Restore only the playback a temporary stream still owns. This is bounded
//! read-before-write protection, not an atomic transaction with other clients.

use fsonos_core::snapshot::{self, Aspect, RestoreOp, RestoreReport, SnapshotSource, ZoneSnapshot};
use fsonos_core::{CoreError, HouseholdState};
use fsonos_proto::content::{self, BrowseFlag};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport, control as soap};
use fsonos_types::TransportState;
use std::cell::Cell;
use std::net::IpAddr;
use std::time::Duration;

const PLAYBACK: [Aspect; 3] = [Aspect::Source, Aspect::Position, Aspect::Transport];

/// The shared restorer vets PlayQueue both before selecting the queue and
/// afterward, before restoring its mode. Only our own dispatched selection can
/// advance that ownership check. Read-back in the shared restorer must still
/// confirm the original queue, revision and group before its later writes.
struct QueueSelection<'a, T: ?Sized> {
    inner: &'a T,
    host: IpAddr,
    argument: Option<String>,
    dispatched: Cell<bool>,
}

impl<T: Transport + ?Sized> Transport for QueueSelection<'_, T> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let selection = host == self.host
            && path == fsonos_proto::soap::AV_TRANSPORT.control_path
            && action
                == fsonos_proto::soap::soap_action_header(
                    &fsonos_proto::soap::AV_TRANSPORT,
                    "SetAVTransportURI",
                )
            && self
                .argument
                .as_ref()
                .is_some_and(|argument| body.contains(argument));
        let reply = self.inner.soap_post(host, path, action, body);
        if selection {
            let dispatched = match &reply {
                Ok(body) => !fsonos_proto::soap::parse_response(body, "SetAVTransportURI")
                    .is_err_and(|error| matches!(error, ProtoError::SoapFault { .. })),
                Err(ProtoError::Network { .. } | ProtoError::Malformed(_)) => true,
                Err(_) => false,
            };
            // A definite refusal retains temporary-source ownership for the
            // shared helper's bounded 402 retry. Ambiguous writes advance only
            // this phase marker; they still need the helper's effect proof.
            self.dispatched.set(dispatched);
        }
        reply
    }

    fn http_get(&self, url: &str) -> Result<String, ProtoError> {
        self.inner.http_get(url)
    }

    fn http_get_many(
        &self,
        urls: &[String],
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Vec<Result<String, ProtoError>> {
        self.inner.http_get_many(urls, cancelled)
    }

    fn http_get_bytes(&self, url: &str) -> Result<fsonos_proto::HttpBody, ProtoError> {
        self.inner.http_get_bytes(url)
    }

    fn ssdp_search(
        &self,
        mx_secs: u8,
        wait: Duration,
    ) -> Result<Vec<fsonos_proto::ssdp::Advert>, ProtoError> {
        self.inner.ssdp_search(mx_secs, wait)
    }

    fn mdns_search(
        &self,
        wait: Duration,
    ) -> Result<Vec<fsonos_proto::mdns::SonosAdvert>, ProtoError> {
        self.inner.mdns_search(wait)
    }

    fn discover(&self, mx_secs: u8, wait: Duration) -> fsonos_proto::Discovered {
        self.inner.discover(mx_secs, wait)
    }
}

fn preserve(why: &str) -> RestoreReport {
    RestoreReport {
        restored: Vec::new(),
        skipped: PLAYBACK
            .into_iter()
            .map(|aspect| (aspect, why.to_owned()))
            .collect(),
    }
}

fn same_source(expected: &str, actual: &str) -> bool {
    expected == actual
        || expected
            .strip_prefix("x-rincon-mp3radio://")
            .zip(actual.strip_prefix("http://"))
            .is_some_and(|(expected, actual)| expected == actual)
        || actual
            .strip_prefix("x-rincon-mp3radio://")
            .zip(expected.strip_prefix("http://"))
            .is_some_and(|(actual, expected)| expected == actual)
}

fn check_group(now: &HouseholdState, before: &ZoneSnapshot) -> Result<(), String> {
    if now.id != before.household
        || now.coordinator_of(&before.coordinator) != Some(&before.coordinator)
    {
        return Err(
            "stream household or coordinator changed; newer grouping was left alone".into(),
        );
    }
    let actual: Vec<_> = now
        .rooms
        .iter()
        .filter(|room| room.coordinator == before.coordinator)
        .map(|room| &room.primary)
        .collect();
    if actual.len() != before.members.len()
        || !before.members.iter().all(|member| actual.contains(&member))
    {
        return Err("stream group membership changed; newer grouping was left alone".into());
    }
    Ok(())
}

fn refreshed<T: Transport + ?Sized>(
    transport: &T,
    household: &HouseholdState,
    before: &ZoneSnapshot,
) -> Result<HouseholdState, String> {
    let host = household
        .player(&before.coordinator)
        .ok_or_else(|| "stream coordinator disappeared".to_owned())?
        .ip;
    let topology = get_zone_group_state(transport, host).map_err(|error| error.to_string())?;
    let mut now = household.clone();
    now.apply_topology(&topology);
    check_group(&now, before)?;
    Ok(now)
}

fn original_uri(before: &ZoneSnapshot) -> String {
    match &before.source {
        SnapshotSource::Nothing => String::new(),
        SnapshotSource::Queue { .. } => format!("x-rincon-queue:{}#0", before.coordinator.0),
        SnapshotSource::Uri { uri, .. } => uri.clone(),
    }
}

fn check_queue<T: Transport + ?Sized>(
    transport: &T,
    host: std::net::IpAddr,
    before: &ZoneSnapshot,
) -> Result<(), String> {
    if let SnapshotSource::Queue { update_id, .. } = before.source {
        let queue = content::browse(
            transport,
            host,
            content::QUEUE,
            BrowseFlag::DirectChildren,
            0,
            1,
        )
        .map_err(|error| error.to_string())?;
        if queue.update_id != update_id {
            return Err("queue changed; refusing to replace, seek or resume it".into());
        }
    }
    Ok(())
}

/// The measured audio must still belong to the original room group. These
/// bounded reads run only around finite measurement windows, not continuously.
/// Accept the same radio/HTTP alias as cleanup, never a different URL or group.
#[cfg(unix)]
pub(super) fn verify_measurement<T: Transport + ?Sized>(
    transport: &T,
    households: &[HouseholdState],
    before: &ZoneSnapshot,
    source: &str,
    require_playing: bool,
) -> Result<(), String> {
    let household = households
        .iter()
        .find(|household| household.player(&before.coordinator).is_some())
        .ok_or_else(|| "stream coordinator disappeared during measurement".to_owned())?;
    let now = refreshed(transport, household, before)?;
    let host = now
        .player(&before.coordinator)
        .ok_or_else(|| "stream coordinator disappeared during measurement".to_owned())?
        .ip;
    let media = soap::get_media_info(transport, host).map_err(|error| error.to_string())?;
    if !same_source(source, &media.uri) {
        return Err("stream source changed; live latency measurement stopped".into());
    }
    let info = soap::get_transport_info(transport, host).map_err(|error| error.to_string())?;
    if info.status == "ERROR_OCCURRED" {
        return Err("stream renderer reported a playback error; measurement stopped".into());
    }
    let state = info.state;
    if state != TransportState::Playing
        && (require_playing || state != TransportState::Transitioning)
    {
        return Err("stream is no longer playing; live latency measurement stopped".into());
    }
    refreshed(transport, &now, before)?;
    let media = soap::get_media_info(transport, host).map_err(|error| error.to_string())?;
    if !same_source(source, &media.uri) {
        return Err("stream source changed during observation; measurement stopped".into());
    }
    Ok(())
}

/// Encoder startup can take time. Do not displace a new source/group that
/// appeared after the initial snapshot while capture was warming up.
pub(super) fn verify_start<T: Transport + ?Sized>(
    transport: &T,
    households: &[HouseholdState],
    before: &ZoneSnapshot,
) -> Result<(), String> {
    let household = households
        .iter()
        .find(|household| household.player(&before.coordinator).is_some())
        .ok_or_else(|| "stream coordinator disappeared before startup".to_owned())?;
    let now = refreshed(transport, household, before)?;
    let host = now
        .player(&before.coordinator)
        .ok_or_else(|| "stream coordinator disappeared before startup".to_owned())?
        .ip;
    let media = soap::get_media_info(transport, host).map_err(|error| error.to_string())?;
    if !same_source(&original_uri(before), &media.uri) {
        return Err("playback changed while preparing capture; stream was not started".into());
    }
    let state = soap::get_transport_info(transport, host).map_err(|error| error.to_string())?;
    if state.state != before.transport_state {
        return Err("transport changed while preparing capture; stream was not started".into());
    }
    check_queue(transport, host, before)
}

/// `started` means Play was acknowledged. A failed dispatch may nevertheless
/// have installed the stream URI, so callers must also invoke this after an
/// attempted but failed Play. Never restore grouping or member levels: stream
/// capture does not own them. Preserve a later pause/stop by not auto-resuming.
pub(super) fn restore<T: Transport + ?Sized>(
    transport: &T,
    households: &[HouseholdState],
    before: &ZoneSnapshot,
    temporary: &str,
    started: bool,
) -> Result<RestoreReport, CoreError> {
    let Some(household) = households
        .iter()
        .find(|household| household.player(&before.coordinator).is_some())
    else {
        return Ok(preserve(
            "stream coordinator disappeared; nothing was restored",
        ));
    };
    let now = match refreshed(transport, household, before) {
        Ok(now) => now,
        Err(why) => return Ok(preserve(&why)),
    };
    let host = now
        .player(&before.coordinator)
        .ok_or_else(|| CoreError::UnknownPlayer(before.coordinator.0.clone()))?
        .ip;
    let current = soap::get_media_info(transport, host)?;
    if !same_source(temporary, &current.uri) {
        return Ok(preserve(
            "stream source changed; newer playback was left alone",
        ));
    }
    let state = soap::get_transport_info(transport, host)?.state;
    let keep_silent = started && state != TransportState::Playing;
    let mut previous = before.clone();
    if keep_silent {
        previous.transport_state = state;
    }
    let original = original_uri(&previous);
    let selecting = QueueSelection {
        inner: transport,
        host,
        argument: matches!(&previous.source, SnapshotSource::Queue { .. })
            .then(|| fsonos_proto::soap::args_xml(&[("CurrentURI", &original)])),
        dispatched: Cell::new(false),
    };
    let mut report = snapshot::restore_vetted(
        &selecting,
        std::slice::from_ref(&now),
        &previous,
        &PLAYBACK,
        &|operation| {
            // Re-read at each write, not just once before the entire restore.
            let current = refreshed(transport, household, &previous)?;
            let host = current
                .player(&previous.coordinator)
                .ok_or_else(|| "stream coordinator disappeared".to_owned())?
                .ip;
            let expected = match operation {
                RestoreOp::PlayQueue if selecting.dispatched.get() => &original,
                RestoreOp::SetUri { .. } | RestoreOp::PlayQueue | RestoreOp::Stop => temporary,
                RestoreOp::SeekTrack(_) | RestoreOp::SeekPosition(_) | RestoreOp::Play => &original,
                _ => return Err("stream cleanup cannot change grouping, volume or mute".into()),
            };
            let media = soap::get_media_info(transport, host).map_err(|error| error.to_string())?;
            if !same_source(expected, &media.uri) {
                return Err(
                    "source changed during stream cleanup; no dependent write was sent".into(),
                );
            }
            check_queue(transport, host, &previous)?;
            Ok(operation.clone())
        },
    )?;
    if keep_silent && before.transport_state == TransportState::Playing {
        report.skipped.push((
            Aspect::Transport,
            "stream was no longer playing; the previous source was not auto-resumed".into(),
        ));
    }
    Ok(report)
}

#[cfg(test)]
mod tests;

#[cfg(all(unix, test))]
#[path = "recovery/measurement_tests.rs"]
mod measurement_tests;
