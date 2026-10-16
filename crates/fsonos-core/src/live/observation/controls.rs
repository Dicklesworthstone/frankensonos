//! Repair missing RenderingControl and transport-settings notifications.
//! Reuse the topology survey's cadence, never the fast track-end watchdog.

use super::{Engine, LiveEvent, Model, coordinators};
use crate::events::{Service, wanted};
use crate::playback::Changes;
use crate::reconcile::Health;
use fsonos_proto::control::{get_group_volume, get_mute, get_volume};
use fsonos_proto::play_mode::get_play_mode;
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::PlayerId;
use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::atomic::Ordering;

pub(in crate::live) fn room_levels<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
    stopping: impl Fn() -> bool,
) -> Result<Option<(u8, bool)>, ProtoError> {
    if stopping() {
        return Ok(None);
    }
    let volume = get_volume(transport, host)?;
    if stopping() {
        return Ok(None);
    }
    let mute = get_mute(transport, host)?;
    if stopping() {
        return Ok(None);
    }
    Ok(Some((volume, mute)))
}

fn record(events: &mut Vec<LiveEvent>, player: &PlayerId, changes: Changes) {
    if !changes.is_empty() {
        events.push(LiveEvent::Playback {
            player: player.clone(),
            changes,
        });
    }
}

/// Room levels belong to each renderer, not just the group's coordinator.
/// A failed service leaves its old state intact without suppressing successful
/// observations from other services or rooms. No control writes are issued.
pub(super) fn survey(engine: &Engine, model: &mut Model, events: &mut Vec<LiveEvent>) {
    let stopping = || engine.stop.load(Ordering::Acquire);
    let mut seen = HashSet::new();
    // Use exactly the rendering roles the subscription engine uses. Bridges
    // and topology-only devices must not acquire invented playback state.
    for want in wanted(&model.rec.households) {
        if want.service != Service::RenderingControl
            || model
                .rec
                .health
                .of(&want.player)
                .is_some_and(|h| h.health == Health::Offline)
            || !seen.insert(want.player.clone())
        {
            continue;
        }
        match room_levels(&engine.via(), want.host, stopping) {
            Ok(Some((volume, mute))) => record(
                events,
                &want.player,
                model.playback.apply_room_levels(&want.player, volume, mute),
            ),
            Ok(None) => return,
            Err(error) => {
                tracing::debug!(%error, "room levels read-back failed; retaining live state");
            }
        }
    }
    for (player, host) in coordinators(model) {
        if stopping() {
            return;
        }
        let volume = get_group_volume(&engine.via(), host);
        if stopping() {
            return;
        }
        match volume {
            Ok(volume) => record(
                events,
                &player,
                model.playback.apply_group_volume(&player, volume),
            ),
            Err(error) => {
                tracing::debug!(%error, "group volume read-back failed; retaining live state");
            }
        }
        if stopping() {
            return;
        }
        let mode = get_play_mode(&engine.via(), host);
        if stopping() {
            return;
        }
        match mode {
            Ok(mode) => record(
                events,
                &player,
                model.playback.apply_play_mode(&player, mode),
            ),
            Err(error) => {
                tracing::debug!(%error, "play mode read-back failed; retaining live state");
            }
        }
    }
}

#[cfg(test)]
mod tests;
