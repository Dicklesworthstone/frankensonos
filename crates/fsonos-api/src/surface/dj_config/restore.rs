//! Validate the mood names a reviewed undo would bring back before restoring.

use std::collections::BTreeSet;

use fsonos_core::actions::BeforeState;
use fsonos_core::scenes::{Scene, SceneSource};
use fsonos_core::schedule::{Schedule, ScheduleAction};
use fsonos_core::store::{LoggedAction, StoredSchedule};

use crate::failure::{ErrorCode, Failure};
use crate::surface::Surface;

use super::MoodDefinitionDto;

struct Reference {
    mood: Option<String>,
    origin: String,
    min_work_minutes: Option<u32>,
    max_work_minutes: Option<u32>,
}

/// The caller holds the configuration gate and the undo-selection mutex.
/// This runs before taking the restoring lock or touching speakers, and the
/// subsequent checked undo must retain this exact action ID.
pub(in crate::surface) fn check(
    surface: &Surface,
    target: &LoggedAction,
    now: i64,
) -> Result<(), Failure> {
    let Some(dj) = &surface.dj else {
        return Ok(());
    };
    let before = BeforeState::parse(target.action.before_state.as_deref().unwrap_or("[]"))
        .map_err(|error| {
            Failure::invalid(format!(
                "action #{} has an unreadable before-state: {error}",
                target.id
            ))
        })?;
    let referenced = references(&before, now);
    if referenced.as_ref().is_ok_and(Vec::is_empty) {
        return Ok(());
    }
    let configuration = match dj.configuration() {
        Ok(configuration) => configuration,
        Err(failure) if failure.code == ErrorCode::NotImplemented => return Ok(()),
        Err(failure) => return Err(failure),
    };
    let known: BTreeSet<String> = configuration
        .moods
        .keys()
        .map(|mood| mood.to_lowercase())
        .collect();
    let mut missing = BTreeSet::new();
    let mut incompatible = BTreeSet::new();
    let automatic: BTreeSet<&str> = configuration
        .programs
        .iter()
        .map(|program| program.mood.as_str())
        .collect();
    for reference in referenced? {
        let choices: Vec<&str> = match &reference.mood {
            Some(mood) => vec![mood.as_str()],
            None => automatic.iter().copied().collect(),
        };
        if reference.mood.is_none()
            && choices.is_empty()
            && let Some(conflict) = bounds(
                &reference,
                &MoodDefinitionDto::default(),
                "automatic steering",
            )
        {
            incompatible.insert(conflict);
        }
        for mood in choices {
            let Some(definition) = configuration.moods.get(&mood.to_lowercase()) else {
                missing.insert(format!("{mood:?} in {}", reference.origin));
                continue;
            };
            let selected = if reference.mood.is_some() {
                format!("{mood:?}")
            } else {
                format!("automatic program mood {mood:?}")
            };
            if let Some(conflict) = bounds(&reference, definition, &selected) {
                incompatible.insert(conflict);
            }
        }
    }
    if !missing.is_empty() {
        return Err(Failure::new(
            ErrorCode::UnknownMood,
            format!(
                "undo of action #{} would restore missing DJ moods: {}",
                target.id,
                missing.into_iter().collect::<Vec<_>>().join("; ")
            ),
        )
        .with_hint("Restore those moods in the DJ configuration before undoing this action. No action was restored.")
        .with_suggestions(known));
    }
    if !incompatible.is_empty() {
        return Err(Failure::invalid(format!(
            "undo of action #{} would restore directions that clash with current DJ moods: {}",
            target.id,
            incompatible.into_iter().collect::<Vec<_>>().join("; ")
        ))
        .with_hint(
            "Restore compatible mood bounds before undoing this action. No action was restored.",
        ));
    }
    Ok(())
}

fn bounds(reference: &Reference, definition: &MoodDefinitionDto, selected: &str) -> Option<String> {
    // Explicit direction bounds replace the corresponding mood bounds,
    // exactly as the DJ's constraints merge does before the next pick.
    let minimum = reference.min_work_minutes.or(definition.min_work_minutes);
    let maximum = reference.max_work_minutes.or(definition.max_work_minutes);
    match (minimum, maximum) {
        (Some(minimum), Some(maximum)) if minimum > maximum => Some(format!(
            "{selected} in {}: minimum {minimum} minutes exceeds maximum {maximum}",
            reference.origin
        )),
        _ => None,
    }
}

fn retain(reference: &Reference) -> bool {
    reference.mood.is_some()
        || reference.min_work_minutes.is_some()
        || reference.max_work_minutes.is_some()
}

fn references(before: &BeforeState, now: i64) -> Result<Vec<Reference>, Failure> {
    let mut references = Vec::new();
    for session in &before.dj_sessions {
        if let Some(row) = &session.row
            && (row.expires == i64::MAX || now < row.expires)
        {
            let constraints: MoodDefinitionDto = row
                .constraints
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .map_err(|error| {
                    Failure::invalid(format!(
                        "undo cannot read saved steering for {:?}: {error}",
                        session.coordinator
                    ))
                })?
                .unwrap_or_default();
            references.push(Reference {
                mood: row.mood.clone(),
                origin: format!("session for {:?}", session.coordinator),
                min_work_minutes: constraints.min_work_minutes,
                max_work_minutes: constraints.max_work_minutes,
            });
        }
    }
    for scene in &before.scenes {
        let parsed: Scene = serde_json::from_str(&scene.was.spec).map_err(|error| {
            Failure::invalid(format!(
                "undo cannot read saved scene {:?}: {error}",
                scene.was.name
            ))
            .with_hint("Repair the saved scene before restoring it. No action was restored.")
        })?;
        for group in parsed.groups {
            if let SceneSource::Dj { mood, direction } = group.source {
                references.push(Reference {
                    mood,
                    origin: format!("scene {:?}", scene.was.name),
                    min_work_minutes: direction
                        .as_ref()
                        .and_then(|d| d.constraints.min_work_minutes),
                    max_work_minutes: direction
                        .as_ref()
                        .and_then(|d| d.constraints.max_work_minutes),
                });
            }
        }
    }
    for schedule in &before.schedules {
        let was = &schedule.was;
        let parsed = Schedule::from_stored(&StoredSchedule {
            id: was.id,
            spec: was.spec.clone(),
            action: was.action.clone(),
            creator: was.creator.clone(),
            enabled: was.enabled,
            created: was.created,
            last_fired: was.last_fired,
        })
        .map_err(|error| {
            Failure::invalid(format!(
                "undo cannot read saved schedule #{}: {error}",
                was.id
            ))
            .with_hint("Repair the saved schedule before restoring it. No action was restored.")
        })?;
        if let ScheduleAction::DjStart {
            mood, direction, ..
        } = parsed.action
        {
            references.push(Reference {
                mood,
                origin: format!("schedule #{}", was.id),
                min_work_minutes: direction
                    .as_ref()
                    .and_then(|d| d.constraints.min_work_minutes),
                max_work_minutes: direction
                    .as_ref()
                    .and_then(|d| d.constraints.max_work_minutes),
            });
        }
    }
    references.retain(retain);
    Ok(references)
}

#[cfg(test)]
mod tests;
