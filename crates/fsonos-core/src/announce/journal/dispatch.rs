//! A restart claim keeps source authority through its own source transitions.
//! Observed ownership loss is latched; even a later matching URI cannot revive
//! that claim. Ambiguous writes remain attempted and can only be read back.

use super::AnnouncementRecovery;
use crate::snapshot::SnapshotSource;
use fsonos_proto::didl::{equivalent_metadata, xml_escape};
use fsonos_proto::{ProtoError, Transport, control as soap};
use fsonos_types::TransportState;
use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::Mutex;

#[derive(PartialEq, Eq, Hash)]
struct Dispatch {
    host: IpAddr,
    path: String,
    action: String,
    body: String,
}

struct Source {
    uri: String,
    metadata: Option<String>,
    /// Native continuation carries the survivor's present paused state. An
    /// ordinary source install is stopped and cannot acquire a newer pause.
    allow_paused: bool,
}

struct State {
    attempted: HashSet<Dispatch>,
    expected: Source,
    revoked: Option<String>,
}

pub(super) struct Fence(Mutex<State>);

fn refused(why: impl Into<String>) -> ProtoError {
    ProtoError::Malformed(why.into())
}

fn argument(body: &str, name: &str, value: &str) -> bool {
    body.contains(&format!("<{name}>{}</{name}>", xml_escape(value)))
}

impl Source {
    fn check<T: Transport + ?Sized>(&self, t: &T, host: IpAddr) -> Result<(), ProtoError> {
        let check = || {
            let media = soap::get_media_info(t, host)?;
            if media.uri != self.uri
                || self.metadata.as_ref().is_some_and(|metadata| {
                    media.uri_metadata != *metadata
                        && !equivalent_metadata(&media.uri_metadata, metadata)
                })
            {
                return Err(refused(
                    "announcement recovery observed a newer source or native metadata; no further changes were sent",
                ));
            }
            Ok(())
        };
        check()?;
        if !self.allow_paused && soap::get_transport_info(t, host)?.state == TransportState::Paused
        {
            return Err(refused(
                "announcement playback was paused during recovery; no further changes were sent",
            ));
        }
        check()
    }
}

impl Fence {
    pub(super) fn new(record: &AnnouncementRecovery) -> Self {
        Self(Mutex::new(State {
            attempted: HashSet::new(),
            expected: Source {
                uri: record.clip_url.clone(),
                metadata: Some(record.clip_metadata.clone()),
                allow_paused: false,
            },
            revoked: None,
        }))
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn before<T: Transport + ?Sized>(
        &self,
        t: &T,
        record: &AnnouncementRecovery,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<(), ProtoError> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| refused("announcement recovery's dispatch fence is unavailable"))?;
        if let Some(why) = &state.revoked {
            return Err(refused(why.clone()));
        }
        let lead = record
            .proof
            .host(&record.plan.lead)
            .map_err(|error| refused(error.to_string()))?;
        if let Err(error) = state.expected.check(t, lead) {
            state.revoked = Some(error.to_string());
            return Err(error);
        }
        if !state.attempted.insert(Dispatch {
            host,
            path: path.into(),
            action: action.into(),
            body: body.into(),
        }) {
            return Err(refused(
                "an announcement recovery mutation was already attempted; it was not replayed",
            ));
        }
        // Advance only to the source described by our impending dispatch,
        // never to whichever source happens to answer a later fresh read.
        if host == lead && action.contains("#SetAVTransportURI") {
            let next = next_source(record, body)?;
            if let Some(next) = next {
                state.expected = next;
            }
        } else if action.contains("#DelegateGroupCoordinationTo")
            && argument(body, "NewCoordinator", &record.plan.lead.0)
        {
            let survivor = record.effects.continuations.iter().any(|entry| {
                entry
                    .survivor()
                    .is_some_and(|survivor| record.proof.host(survivor).ok() == Some(host))
            });
            if !survivor || !argument(body, "RejoinGroup", "1") {
                return Err(refused(
                    "announcement recovery refused an unproved coordination transfer",
                ));
            }
            let media = soap::get_media_info(t, host)?;
            let uri = if media.uri.starts_with("x-rincon-queue:") {
                format!("x-rincon-queue:{}#0", record.plan.lead.0)
            } else {
                media.uri
            };
            state.expected = Source {
                uri,
                metadata: Some(media.uri_metadata),
                allow_paused: true,
            };
        } else if host == lead && action.contains("#Pause") {
            // Silent snapshot restoration can explicitly pause before or
            // after source selection. Only that issued write grants this.
            state.expected.allow_paused = true;
        } else if host == lead && action.contains("#Play\"") {
            state.expected.allow_paused = false;
        }
        Ok(())
    }
}

fn next_source(record: &AnnouncementRecovery, body: &str) -> Result<Option<Source>, ProtoError> {
    for snapshot in &record.snapshots {
        if snapshot.coordinator != record.plan.lead {
            continue;
        }
        let (uri, metadata) = match &snapshot.source {
            SnapshotSource::Queue { .. } => (
                format!("x-rincon-queue:{}#0", snapshot.coordinator.0),
                String::new(),
            ),
            SnapshotSource::Uri { uri, metadata, .. } => (uri.clone(), metadata.clone()),
            SnapshotSource::Nothing => (String::new(), String::new()),
        };
        if argument(body, "CurrentURI", &uri) && argument(body, "CurrentURIMetaData", &metadata) {
            return Ok(Some(Source {
                uri,
                // Queue selection has no request metadata. Some renderers
                // synthesize their own queue DIDL; URI and the snapshot's
                // independently checked UpdateID bind this source.
                metadata: (!matches!(snapshot.source, SnapshotSource::Queue { .. }))
                    .then_some(metadata),
                allow_paused: false,
            }));
        }
    }
    for player in &record.before.players {
        let uri = format!("x-rincon:{}", player.id.0);
        if argument(body, "CurrentURI", &uri) && argument(body, "CurrentURIMetaData", "") {
            // A self-join leaves a group but retains the renderer's own source.
            if player.id == record.plan.lead {
                return Ok(None);
            }
            return Ok(Some(Source {
                uri,
                metadata: None,
                allow_paused: true,
            }));
        }
    }
    Err(refused(
        "announcement recovery refused a source transition outside its saved restoration",
    ))
}
