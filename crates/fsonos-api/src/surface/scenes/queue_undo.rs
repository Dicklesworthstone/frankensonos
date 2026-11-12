//! Retain one complete restoration proof for every queue a scene replaces.
//! Unsupported or uncertain replacements make the whole action a barrier.

use fsonos_core::HouseholdState;
use fsonos_core::actions::{self, PendingSceneQueueUndo};
use fsonos_core::control::QueueReplacement;
use fsonos_core::scenes::{SceneOp, ScenePlan, SceneReport};
use fsonos_proto::didl::SpotifyContainer;
use fsonos_types::PlayerId;
use std::cell::RefCell;

use crate::Surface;
use crate::source::is_catalog_source;

#[derive(Default)]
pub(super) struct Recording {
    catalog: bool,
    pending: RefCell<Vec<(PlayerId, PendingSceneQueueUndo)>>,
    attempts: RefCell<Vec<PlayerId>>,
    receipts: RefCell<Vec<(PlayerId, QueueReplacement)>>,
}

impl Recording {
    /// Caps have already been applied. The saved queue and deliberate scene
    /// level fields therefore preserve those reductions when restored.
    pub(super) fn capture(
        surface: &Surface,
        households: &[HouseholdState],
        plan: &ScenePlan,
    ) -> Self {
        let mut pending = Vec::new();
        if surface.log.is_some() {
            for op in &plan.ops {
                let coordinator = match op {
                    SceneOp::Favorite { coordinator, .. }
                    | SceneOp::SonosPlaylist { coordinator, .. } => coordinator,
                    SceneOp::Uri {
                        coordinator, uri, ..
                    } if SpotifyContainer::of(uri).is_some() || is_catalog_source(uri) => {
                        coordinator
                    }
                    _ => continue,
                };
                if pending.iter().any(|(id, _)| id == coordinator) {
                    continue;
                }
                if let Ok(saved) = actions::capture_scene_queue_undo(
                    &*surface.transport,
                    households,
                    coordinator,
                    plan,
                    surface.now(),
                ) {
                    pending.push((coordinator.clone(), saved));
                }
            }
        }
        Self {
            catalog: plan
                .ops
                .iter()
                .any(|op| matches!(op, SceneOp::Uri { uri, .. } if is_catalog_source(uri))),
            pending: RefCell::new(pending),
            ..Self::default()
        }
    }

    pub(super) fn started(&self, coordinator: &PlayerId) -> bool {
        let mut attempts = self.attempts.borrow_mut();
        attempts.push(coordinator.clone());
        attempts.iter().filter(|id| *id == coordinator).count() == 1
            && self
                .pending
                .borrow()
                .iter()
                .any(|(id, _)| id == coordinator)
    }

    pub(super) fn finished(&self, coordinator: &PlayerId, receipt: QueueReplacement) {
        self.receipts
            .borrow_mut()
            .push((coordinator.clone(), receipt));
    }

    /// Return the action's before-state and whether its queue restoration is
    /// unavailable. A multi-group scene is never partially queue-undoable.
    pub(super) fn attach(
        &self,
        surface: &Surface,
        households: &[HouseholdState],
        report: &SceneReport,
        before: Option<String>,
    ) -> (Option<String>, bool) {
        let attempts = self.attempts.borrow();
        if attempts.is_empty() {
            return (before, false);
        }
        let barrier = || {
            (
                Some(actions::before_state_not_undoable(if self.catalog {
                    "scene catalog playback replaced queue contents without complete restoration proof"
                } else {
                    "scene queue replacement has no complete restoration proof"
                })),
                true,
            )
        };
        let receipts = self.receipts.borrow();
        if !report.is_complete() || attempts.len() != receipts.len() {
            return barrier();
        }
        let mut pending = self.pending.borrow_mut();
        let mut queues = Vec::new();
        for coordinator in attempts.iter() {
            let matching: Vec<_> = receipts
                .iter()
                .filter(|(id, _)| id == coordinator)
                .collect();
            let [(_, receipt)] = matching.as_slice() else {
                return barrier();
            };
            if attempts.iter().filter(|id| *id == coordinator).count() != 1 {
                return barrier();
            }
            let Some(index) = pending.iter().position(|(id, _)| id == coordinator) else {
                return barrier();
            };
            let (_, saved) = pending.remove(index);
            let Ok(queue) = actions::finish_scene_queue_replacement(
                &*surface.transport,
                households,
                saved,
                receipt,
                report,
                surface.now(),
            ) else {
                return barrier();
            };
            queues.push(queue);
        }
        match actions::with_queue_undo(before, &queues) {
            Some(before) => (Some(before), false),
            None => barrier(),
        }
    }
}
