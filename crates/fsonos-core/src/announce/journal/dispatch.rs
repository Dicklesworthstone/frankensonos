//! A restart claim keeps source authority through its own source transitions.
//! Observed ownership loss is latched; even a later matching URI cannot revive
//! that claim. Ambiguous writes remain attempted and can only be read back.

use super::AnnouncementRecovery;
use crate::snapshot::{SnapshotSource, ZoneSnapshot};
use fsonos_proto::didl::{equivalent_metadata, xml_escape};
use fsonos_proto::{ProtoError, Transport, control as soap};
use fsonos_types::{PlayerId, TransportState};
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
    /// The temporary lead is first. Other entries are original coordinators
    /// whose retained sources reappear when they leave the temporary group.
    sources: Vec<OwnedSource>,
}

struct OwnedSource {
    player: PlayerId,
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

impl OwnedSource {
    fn check<T: Transport + ?Sized>(
        &mut self,
        t: &T,
        record: &AnnouncementRecovery,
    ) -> Result<(), ProtoError> {
        if let Some(why) = &self.revoked {
            return Err(refused(why.clone()));
        }
        let host = record
            .proof
            .host(&self.player)
            .map_err(|error| refused(error.to_string()))?;
        if let Err(error) = self.expected.check(t, host) {
            self.revoked = Some(error.to_string());
            return Err(error);
        }
        Ok(())
    }
}

impl Fence {
    pub(super) fn new(record: &AnnouncementRecovery) -> Self {
        let mut sources = vec![OwnedSource {
            player: record.plan.lead.clone(),
            expected: Source {
                uri: record.clip_url.clone(),
                metadata: Some(record.clip_metadata.clone()),
                allow_paused: false,
            },
            revoked: None,
        }];
        for snapshot in &record.snapshots {
            if snapshot.coordinator != record.plan.lead
                && record.selected.contains(&snapshot.coordinator)
                && !record
                    .effects
                    .continuations
                    .iter()
                    .any(|entry| entry.coordinator == snapshot.coordinator)
            {
                // Native partial-group handbacks retain their own stronger
                // live Playback proofs. Ordinary returning coordinators need
                // a separate guard: the temporary lead cannot speak for them.
                sources.push(OwnedSource {
                    player: snapshot.coordinator.clone(),
                    expected: Source {
                        uri: format!("x-rincon:{}", record.plan.lead.0),
                        metadata: None,
                        allow_paused: true,
                    },
                    revoked: None,
                });
            }
        }
        Self(Mutex::new(State {
            attempted: HashSet::new(),
            sources,
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
    ) -> Result<bool, ProtoError> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| refused("announcement recovery's dispatch fence is unavailable"))?;
        let lead = record
            .proof
            .host(&record.plan.lead)
            .map_err(|error| refused(error.to_string()))?;
        state.sources[0].check(t, record)?;
        let player = record
            .before
            .players
            .iter()
            .find(|player| player.ip == host)
            .ok_or_else(|| refused("announcement recovery cannot identify the requested room"))?;
        let zone = record
            .before
            .rooms
            .iter()
            .find(|room| room.players.contains(&player.id))
            .map(|room| &room.coordinator);
        if let Some(owner) = state
            .sources
            .iter_mut()
            .skip(1)
            .find(|owner| Some(&owner.player) == zone)
        {
            owner.check(t, record)?;
        }
        if !state.attempted.insert(Dispatch {
            host,
            path: path.into(),
            action: action.into(),
            body: body.into(),
        }) {
            // An originally idle snapshot also asks for Stop. The first
            // recovery Stop already satisfied that effect if fresh reads
            // still confirm it; report the proof without another dispatch.
            if host == lead
                && action.contains("#Stop\"")
                && soap::get_transport_info(t, host)?.state == TransportState::Stopped
            {
                return Ok(false);
            }
            return Err(refused(
                "an announcement recovery mutation was already attempted; it was not replayed",
            ));
        }
        // Advance only to the source described by our impending dispatch,
        // never to whichever source happens to answer a later fresh read.
        if action.contains("#SetAVTransportURI") {
            if let Some(owner) = state
                .sources
                .iter_mut()
                .find(|owner| owner.player == player.id)
                && let Some(next) = next_source(record, &player.id, body)?
            {
                owner.expected = next;
            }
        } else if action.contains("#DelegateGroupCoordinationTo") {
            if let Some(next) = delegated_source(t, record, host, body, &state.sources[0].expected)?
            {
                state.sources[0].expected = next;
            }
        } else if action.contains("#Pause") {
            // Silent snapshot restoration can explicitly pause before or
            // after source selection. Only that issued write grants this.
            if let Some(owner) = state
                .sources
                .iter_mut()
                .find(|owner| owner.player == player.id)
            {
                owner.expected.allow_paused = true;
            }
        } else if action.contains("#Play\"")
            && let Some(owner) = state
                .sources
                .iter_mut()
                .find(|owner| owner.player == player.id)
        {
            owner.expected.allow_paused = false;
        }
        Ok(true)
    }
}

fn delegated_source<T: Transport + ?Sized>(
    t: &T,
    record: &AnnouncementRecovery,
    host: IpAddr,
    body: &str,
    current: &Source,
) -> Result<Option<Source>, ProtoError> {
    let entry = record
        .effects
        .continuations
        .iter()
        .find(|entry| {
            argument(body, "NewCoordinator", &entry.coordinator.0)
                && entry
                    .survivor()
                    .is_some_and(|survivor| record.proof.host(survivor).ok() == Some(host))
        })
        .ok_or_else(|| {
            refused("announcement recovery refused an unproved coordination transfer")
        })?;
    if !argument(body, "RejoinGroup", "1") {
        return Err(refused(
            "announcement recovery cannot detach the live survivor",
        ));
    }
    if entry.coordinator == record.plan.lead {
        let media = soap::get_media_info(t, host)?;
        let uri = if media.uri.starts_with("x-rincon-queue:") {
            format!("x-rincon-queue:{}#0", record.plan.lead.0)
        } else {
            media.uri
        };
        return Ok(Some(Source {
            uri,
            metadata: Some(media.uri_metadata),
            allow_paused: true,
        }));
    }
    // The temporary lead can itself be an original member, not the returning
    // coordinator. It follows the same native handoff without taking ownership
    // of the survivor's queue or replaying that queue from its own snapshot.
    if entry
        .survivor()
        .is_some_and(|survivor| current.uri == format!("x-rincon:{}", survivor.0))
    {
        return Ok(Some(Source {
            uri: format!("x-rincon:{}", entry.coordinator.0),
            metadata: None,
            allow_paused: true,
        }));
    }
    Ok(None)
}

fn next_source(
    record: &AnnouncementRecovery,
    player: &PlayerId,
    body: &str,
) -> Result<Option<Source>, ProtoError> {
    for snapshot in &record.snapshots {
        if snapshot.coordinator != *player {
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
    for destination in &record.before.players {
        let uri = format!("x-rincon:{}", destination.id.0);
        if argument(body, "CurrentURI", &uri) && argument(body, "CurrentURIMetaData", "") {
            // A self-join leaves a group but retains the renderer's own source.
            if destination.id == *player {
                if player == &record.plan.lead {
                    return Ok(None);
                }
                let snapshot = record
                    .snapshots
                    .iter()
                    .find(|snapshot| snapshot.coordinator == *player)
                    .ok_or_else(|| {
                        refused("a returning coordinator has no retained source evidence")
                    })?;
                return Ok(Some(retained_source(snapshot)));
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

fn retained_source(snapshot: &ZoneSnapshot) -> Source {
    let (uri, metadata) = match &snapshot.source {
        SnapshotSource::Queue { .. } => {
            (format!("x-rincon-queue:{}#0", snapshot.coordinator.0), None)
        }
        SnapshotSource::Uri { uri, metadata, .. } => (uri.clone(), Some(metadata.clone())),
        SnapshotSource::Nothing => (String::new(), Some(String::new())),
    };
    Source {
        uri,
        metadata,
        allow_paused: snapshot.transport_state == TransportState::Paused,
    }
}
