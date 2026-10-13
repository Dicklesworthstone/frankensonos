//! Finish an explicit queue start when a stage's reply, not its effect, was
//! lost. Source selection, track selection and Play are each sent once.
//! Ordinary starts keep the three-call path; ordered starts set NORMAL after
//! selecting the queue and before seeking or playing. Ambiguous failures
//! permit read-back only; a confirmed stage advances to the next one rather
//! than resetting the source or seeking again. Definite SOAP refusals retain
//! their original meaning (including render-parameter recovery upstream).

use super::source_start::wait_for_playing;
use fsonos_proto::content::{QUEUE, browse_range_snapshot};
use fsonos_proto::control as soap;
use fsonos_proto::didl::spotify_uri_from_renderer_uri;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::PlayerId;
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
) -> Result<(), ProtoError> {
    start(transport, host, coordinator, position, false)
}

pub(super) fn play_in_order<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
    coordinator: &PlayerId,
    position: u32,
) -> Result<(), ProtoError> {
    start(transport, host, coordinator, position, true)
}

fn start<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
    coordinator: &PlayerId,
    position: u32,
    in_order: bool,
) -> Result<(), ProtoError> {
    if position == 0 {
        return Err(ProtoError::Malformed("queue positions start at one".into()));
    }
    for stage in [Stage::Source, Stage::Mode, Stage::Track, Stage::Play] {
        if stage == Stage::Mode && !in_order {
            continue;
        }
        let result = match stage {
            Stage::Source => soap::play_from_queue(transport, host, coordinator),
            Stage::Mode => soap::set_normal_play_mode(transport, host),
            Stage::Track => soap::seek_track(transport, host, position),
            Stage::Play => soap::play(transport, host),
        };
        if let Err(error) = result {
            // Source/position/Playing prove nothing about a mode change.
            // A failed mode reply must not authorize Seek or Play, and no
            // earlier stage is replayed to try the mode again.
            if stage == Stage::Mode {
                return Err(error);
            }
            if matches!(
                &error,
                ProtoError::Network { .. } | ProtoError::Malformed(_)
            ) && confirmed(transport, host, coordinator, position, stage).unwrap_or(false)
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
                return Err(error);
            }
        }
    }
    Ok(())
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
) -> Result<bool, ProtoError> {
    let own_queue = format!("x-rincon-queue:{}#0", coordinator.0);
    if soap::get_media_info(transport, host)?.uri != own_queue {
        return Ok(false);
    }
    if !get_zone_group_state(transport, host)?
        .groups
        .iter()
        .any(|group| group.coordinator == *coordinator)
    {
        return Ok(false);
    }
    if stage == Stage::Source {
        return Ok(true);
    }
    // Retain this original revision through every transitioning observation.
    // Replacing it on each pass could accept an owner's newly edited queue.
    let before = browse_range_snapshot(transport, host, QUEUE, position - 1, 1)?;
    let [item] = before.objects.as_slice() else {
        return Ok(false);
    };
    let Some(resource) = &item.res else {
        return Ok(false);
    };
    let unchanged = || {
        let current = soap::get_position_info(transport, host)?;
        if current.track != position || !same_track(&current.uri, &resource.uri) {
            return Ok(false);
        }
        let after = browse_range_snapshot(transport, host, QUEUE, position - 1, 1)?;
        if before != after || soap::get_media_info(transport, host)?.uri != own_queue {
            return Ok(false);
        }
        let topology = get_zone_group_state(transport, host)?;
        Ok(topology
            .groups
            .iter()
            .any(|group| group.coordinator == *coordinator))
    };
    if stage == Stage::Play {
        wait_for_playing(transport, host, unchanged)
    } else {
        unchanged()
    }
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
