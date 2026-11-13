//! A successful queue replacement is undoable only with a bounded original
//! backup and the native operation's exact receipt. Unproved attempts remain
//! action-history barriers rather than reference-only playback snapshots.

use fsonos_core::HouseholdState;
use fsonos_core::actions::{self, PendingQueueUndo, QueueUndo};
use fsonos_core::control::QueueReplacement;
use fsonos_core::favorites::FavoriteKind;
use fsonos_proto::didl::SpotifyContainer;
use fsonos_types::PlayerId;

use super::Surface;
use crate::plan::Command;

pub(super) fn replaces(command: &Command) -> bool {
    match command {
        Command::Play { source_uri, .. } => SpotifyContainer::of(source_uri).is_some(),
        Command::PlayTracks { .. } | Command::PlaySelection { .. } => true,
        Command::PlayFavorite { favorite, .. } => favorite.kind == FavoriteKind::Container,
        _ => false,
    }
}

pub(super) fn capture(
    surface: &Surface,
    households: &[HouseholdState],
    command: &Command,
) -> Option<PendingQueueUndo> {
    surface.log.as_ref()?;
    if !replaces(command) {
        return None;
    }
    actions::capture_queue_undo(
        &*surface.transport,
        households,
        command.addressed()?,
        surface.now(),
    )
    .ok()
}

pub(super) fn confirm(
    surface: &Surface,
    households: &[HouseholdState],
    pending: Option<PendingQueueUndo>,
    caps: &[(PlayerId, u8)],
) -> Option<PendingQueueUndo> {
    let pending = pending?.volume_caps(caps).ok()?;
    pending
        .confirm(&*surface.transport, households, surface.now())
        .ok()?;
    Some(pending)
}

pub(super) fn finish(
    surface: &Surface,
    households: &[HouseholdState],
    pending: Option<PendingQueueUndo>,
    receipt: Option<&QueueReplacement>,
) -> Option<QueueUndo> {
    actions::finish_queue_replacement(
        &*surface.transport,
        households,
        pending?,
        receipt?,
        surface.now(),
    )
    .ok()
}

pub(super) fn attach(
    before: Option<String>,
    commands: &[Command],
    queues: &[QueueUndo],
) -> Option<String> {
    let replacements: Vec<_> = commands
        .iter()
        .filter(|command| replaces(command))
        .collect();
    if replacements.is_empty() {
        return before;
    }
    // A mixed or partially failed set is not partially undoable. Two writes
    // to the same queue also cannot share one coherent original/result proof.
    let complete = replacements.len() == queues.len()
        && replacements.iter().all(|command| {
            command.addressed().is_some_and(|coordinator| {
                queues
                    .iter()
                    .filter(|queue| queue.coordinator() == coordinator)
                    .count()
                    == 1
                    && replacements
                        .iter()
                        .filter(|command| command.addressed() == Some(coordinator))
                        .count()
                        == 1
            })
        });
    if !complete {
        let label = match commands {
            [Command::PlayTracks { selection, .. }] => selection.kind.source_label(),
            [Command::PlaySelection { .. }] => "selected tracks",
            _ => "source",
        };
        return Some(actions::before_state_not_undoable(&format!(
            "{label} queue replacement"
        )));
    }
    actions::with_queue_undo(before, queues)
}
