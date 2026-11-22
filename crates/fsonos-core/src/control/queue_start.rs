//! Complete an explicit queue start without replaying completed stages.
//! Source selection, ordered mode, track selection and Play are sent once.
//! Starts confirm endpoint ownership before writing; ordered starts set NORMAL after
//! selecting the queue and before seeking or playing. Dependent writes recheck
//! the retained selection even after acknowledged stages. Ambiguous failures
//! permit read-back only; a confirmed stage advances to the next one rather
//! than resetting the source or seeking again. Definite SOAP refusals retain
//! their original meaning (including render-parameter recovery upstream).

use super::source_start::wait_for_playing;
use crate::CoreError;
use fsonos_proto::content::{BrowseSnapshot, QUEUE, browse_range_snapshot};
use fsonos_proto::control as soap;
use fsonos_proto::didl::spotify_uri_from_renderer_uri;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::{PlayMode, PlayerId, TransportState};
use std::net::IpAddr;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Source,
    Mode,
    Track,
    Play,
}

impl Stage {
    fn name(self) -> &'static str {
        match self {
            Self::Source => "SetAVTransportURI",
            Self::Mode => "SetPlayMode",
            Self::Track => "Seek",
            Self::Play => "Play",
        }
    }
}

pub(super) fn play<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
    coordinator: &PlayerId,
    position: u32,
) -> Result<(), CoreError> {
    start(transport, host, coordinator, position, false, true).map(drop)
}

pub(super) fn play_in_order<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
    coordinator: &PlayerId,
    position: u32,
) -> Result<(), CoreError> {
    start(transport, host, coordinator, position, true, true).map(drop)
}

/// Select a queue slot while stopped. Retain the pre-load revision so a
/// shifted or replaced queue cannot be reported as the requested selection.
pub(super) fn load<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
    coordinator: &PlayerId,
    position: u32,
    in_order: bool,
) -> Result<(), CoreError> {
    let offset = position
        .checked_sub(1)
        .ok_or_else(|| ProtoError::Malformed("queue positions start at one".into()))?;
    let before = start(transport, host, coordinator, position, in_order, false)?;
    if !confirmed(
        transport,
        host,
        coordinator,
        position,
        Stage::Track,
        &before,
    )? || !matches!(
        soap::get_transport_info(transport, host)?.state,
        TransportState::Paused | TransportState::Stopped
    ) || before != browse_range_snapshot(transport, host, QUEUE, offset, 1)?
    {
        return Err(ProtoError::Malformed(
            "the requested queue slot was not confirmed loaded without playback".into(),
        )
        .into());
    }
    Ok(())
}

fn start<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
    coordinator: &PlayerId,
    position: u32,
    in_order: bool,
    autoplay: bool,
) -> Result<BrowseSnapshot, CoreError> {
    if position == 0 {
        return Err(ProtoError::Malformed("queue positions start at one".into()).into());
    }
    super::require_coordinator(transport, host, coordinator)?;
    // Retain the selected occurrence before changing the source. A later
    // read must not silently adopt a replacement queue at the same position.
    let before = browse_range_snapshot(transport, host, QUEUE, position - 1, 1)?;
    let [item] = before.objects.as_slice() else {
        return Err(ProtoError::Malformed("the requested queue slot is absent".into()).into());
    };
    if item
        .res
        .as_ref()
        .is_none_or(|res| res.uri.trim().is_empty() || res.uri == "NOT_IMPLEMENTED")
    {
        return Err(ProtoError::Malformed("the queue slot has no identity".into()).into());
    }
    // The bounded queue read can outlive the endpoint's group membership.
    // A mismatch before our first write may still use ordinary target recovery.
    super::require_coordinator(transport, host, coordinator)?;
    for stage in [Stage::Source, Stage::Mode, Stage::Track, Stage::Play] {
        if (stage == Stage::Mode && !in_order) || (stage == Stage::Play && !autoplay) {
            continue;
        }
        if stage != Stage::Source {
            require_previous_stage(
                transport,
                host,
                coordinator,
                position,
                stage,
                &before,
                in_order,
            )?;
        }
        let result = match stage {
            Stage::Source => soap::play_from_queue(transport, host, coordinator),
            Stage::Mode => soap::set_normal_play_mode(transport, host),
            Stage::Track => soap::seek_track(transport, host, position),
            Stage::Play => soap::play(transport, host),
        };
        if let Err(error) = result {
            // Each ambiguous reply needs evidence for its own stage. Mode
            // recovery reads the setting explicitly; being on the queue or
            // already playing cannot prove NORMAL. Definite faults stop here.
            if matches!(
                &error,
                ProtoError::Network { .. } | ProtoError::Malformed(_)
            ) && confirmed(transport, host, coordinator, position, stage, &before)
                .unwrap_or(false)
            {
                tracing::warn!(
                    action = stage.name(),
                    cause = %error,
                    "queue start stage confirmed by read-back; command not replayed"
                );
            } else {
                // Preserve the original failure and never execute a later
                // stage without proof. A read failure is not permission to
                // retry a mutation or roll playback back.
                return Err(error.into());
            }
        }
        if !autoplay {
            let selected = if stage == Stage::Mode {
                fsonos_proto::play_mode::get_play_mode(transport, host)?
                    == fsonos_types::PlayMode::Normal
            } else {
                confirmed(transport, host, coordinator, position, stage, &before)?
            };
            if !selected
                || !matches!(
                    soap::get_transport_info(transport, host)?.state,
                    TransportState::Paused | TransportState::Stopped
                )
            {
                return Err(ProtoError::Malformed(format!(
                    "{} did not establish the requested inactive queue state",
                    stage.name()
                ))
                .into());
            }
        }
    }
    Ok(before)
}

/// An acknowledgement proves a completed write, not continued ownership of
/// the source. Bind each dependent write to the retained selection and stage.
fn require_previous_stage<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
    coordinator: &PlayerId,
    position: u32,
    next: Stage,
    before: &BrowseSnapshot,
    in_order: bool,
) -> Result<(), CoreError> {
    let previous = match next {
        Stage::Mode => Stage::Source,
        Stage::Track if in_order => Stage::Mode,
        Stage::Track => Stage::Source,
        Stage::Play => Stage::Track,
        Stage::Source => return Ok(()),
    };
    let current = (|| {
        if in_order
            && matches!(next, Stage::Track | Stage::Play)
            && fsonos_proto::play_mode::get_play_mode(transport, host)? != PlayMode::Normal
        {
            return Ok(false);
        }
        // An acknowledged mode write need not leave playback stopped: a
        // native queue reselection may remain active or transitioning. Only
        // ambiguous mode replies need the stricter stopped/stable proof.
        let proof = if previous == Stage::Mode {
            Stage::Source
        } else {
            previous
        };
        confirmed(transport, host, coordinator, position, proof, before)
    })();
    match current {
        Ok(true) => Ok(()),
        Ok(false) => Err(after_selection(
            "the source, queue, coordinator or completed stage changed",
        )),
        Err(error) => Err(after_selection(error)),
    }
}

fn after_selection(error: impl std::fmt::Display) -> CoreError {
    CoreError::Uncertain(format!(
        "queue source selection completed but a later stage was not sent: \
         {error}; completed writes were not replayed; read the current queue before trying again"
    ))
}

/// Check only the state promised by this stage. Reads are bounded, and no
/// mutation is retried. This is an observed result, not a transaction with
/// other controllers: later user changes remain possible.
fn confirmed<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
    coordinator: &PlayerId,
    position: u32,
    stage: Stage,
    before: &BrowseSnapshot,
) -> Result<bool, ProtoError> {
    let own_queue = format!("x-rincon-queue:{}#0", coordinator.0);
    if soap::get_media_info(transport, host)?.uri != own_queue {
        return Ok(false);
    }
    if !crate::topology::coordinator_at(&get_zone_group_state(transport, host)?, host, coordinator)?
    {
        return Ok(false);
    }
    if stage == Stage::Source {
        return Ok(
            *before == browse_range_snapshot(transport, host, QUEUE, position - 1, 1)?
                && soap::get_media_info(transport, host)?.uri == own_queue,
        );
    }
    // Keep the original pre-selection revision through every observation,
    // including ordinary acknowledged stages and lost-reply recovery.
    let [item] = before.objects.as_slice() else {
        return Ok(false);
    };
    let Some(resource) = &item.res else {
        return Ok(false);
    };
    if stage == Stage::Mode {
        if resource.uri.trim().is_empty() || resource.uri == "NOT_IMPLEMENTED" {
            return Ok(false);
        }
        return mode_confirmed(transport, host, coordinator, position, before);
    }
    let unchanged = || {
        let current = soap::get_position_info(transport, host)?;
        if current.track != position || !same_track(&current.uri, &resource.uri) {
            return Ok(false);
        }
        if soap::get_media_info(transport, host)?.uri != own_queue {
            return Ok(false);
        }
        let topology = get_zone_group_state(transport, host)?;
        if !crate::topology::coordinator_at(&topology, host, coordinator)? {
            return Ok(false);
        }
        // Topology reads can outlive a listener's source or occurrence
        // selection. Bracket that read before allowing the dependent write;
        // elapsed playback time is deliberately not part of identity.
        if *before != browse_range_snapshot(transport, host, QUEUE, position - 1, 1)? {
            return Ok(false);
        }
        let current = soap::get_position_info(transport, host)?;
        Ok(current.track == position
            && same_track(&current.uri, &resource.uri)
            && soap::get_media_info(transport, host)?.uri == own_queue)
    };
    if stage == Stage::Play {
        wait_for_playing(transport, host, unchanged)
    } else {
        unchanged()
    }
}

/// Seek has not been sent yet. Confirm NORMAL against the still-stopped
/// selected queue, not against a target position we have not selected. Retain
/// the original read-back revision and current occurrence across these reads.
/// This observes the requested result; it cannot attribute it to a controller
/// or detect a change completed before the first read or after the last one.
fn mode_confirmed<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
    coordinator: &PlayerId,
    position: u32,
    before: &BrowseSnapshot,
) -> Result<bool, ProtoError> {
    let current = soap::get_position_info(transport, host)?;
    if current.track == 0
        || current.uri.trim().is_empty()
        || current.uri == "NOT_IMPLEMENTED"
        || soap::get_transport_info(transport, host)?.state != TransportState::Stopped
        || fsonos_proto::play_mode::get_play_mode(transport, host)? != PlayMode::Normal
    {
        return Ok(false);
    }
    // An owner's seek, including one within the same track, invalidates the
    // observation. Do not reset or play over that change to finish our start.
    let after = soap::get_position_info(transport, host)?;
    if current.track != after.track
        || current.position_secs != after.position_secs
        || !same_track(&current.uri, &after.uri)
        || *before != browse_range_snapshot(transport, host, QUEUE, position - 1, 1)?
    {
        return Ok(false);
    }
    let own_queue = format!("x-rincon-queue:{}#0", coordinator.0);
    Ok(
        crate::topology::coordinator_at(
            &get_zone_group_state(transport, host)?,
            host,
            coordinator,
        )? && soap::get_media_info(transport, host)?.uri == own_queue
            && fsonos_proto::play_mode::get_play_mode(transport, host)? == PlayMode::Normal
            && soap::get_transport_info(transport, host)?.state == TransportState::Stopped,
    )
}

fn same_track(actual: &str, expected: &str) -> bool {
    if actual.is_empty() || expected.is_empty() {
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
