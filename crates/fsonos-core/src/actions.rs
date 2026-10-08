//! The action log and undo.
//!
//! Every mutating request a surface carries out is logged with who asked
//! (the house-policy client), the surface, the policy's decision, what
//! happened, and snapshots of the zones it touched taken just before — denied
//! requests too, with nothing to undo. An action that steers the DJ also
//! carries the zones' DJ session rows as they were ([`SessionBefore`]).
//! [`undo_last`] puts the newest undoable action's zones back the way they
//! were (via [`snapshot::restore`]), puts its DJ sessions back in the store,
//! and logs the undo itself, which is never undoable (no redo).
//!
//! The surfaces own the flow (authorize, plan, bound by the policy, capture,
//! execute, [`record`]); this module owns the parts every surface shares, so
//! they all log and undo alike. Retention keeps the newest [`KEEP_ACTIONS`]
//! rows and nothing older than [`KEEP_SECS`].

use crate::policy::Client;
use crate::snapshot::{self, RestoreReport, ZoneSnapshot};
use crate::store::{Action, DjSession, Store, StoreError};
use crate::{CoreError, HouseholdState};
use fsonos_proto::Transport;
use fsonos_types::PlayerId;
use serde::{Deserialize, Serialize};

/// Retention: the newest this many actions are kept…
pub const KEEP_ACTIONS: usize = 10_000;
/// …and none older than 30 days.
pub const KEEP_SECS: i64 = 30 * 24 * 60 * 60;

fn store_err(e: &StoreError) -> CoreError {
    CoreError::Store(e.to_string())
}

/// Snapshot the zones led by `coordinators` (duplicates ignored) before an
/// action changes them. A zone that cannot be read is left out, with why: the
/// action still runs, it just cannot be fully undone.
pub fn capture_zones<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinators: &[PlayerId],
    at: i64,
) -> (Vec<ZoneSnapshot>, Vec<(PlayerId, String)>) {
    let mut snaps = Vec::new();
    let mut missed = Vec::new();
    let mut seen: Vec<&PlayerId> = Vec::new();
    for coordinator in coordinators {
        if seen.contains(&coordinator) {
            continue;
        }
        seen.push(coordinator);
        match snapshot::capture(t, households, coordinator, at) {
            Ok(snap) => snaps.push(snap),
            Err(e) => missed.push((coordinator.clone(), e.to_string())),
        }
    }
    (snaps, missed)
}

/// A zone's DJ session as it was before an action: undo saves `row` back, or
/// deletes the zone's session when there was none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionBefore {
    /// The coordinator the session is keyed by.
    pub coordinator: String,
    /// `None`: the zone had no session row (unsteered).
    pub row: Option<DjSession>,
}

/// Everything an action's undo puts back, as parsed from its log row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct BeforeState {
    #[serde(default)]
    pub zones: Vec<ZoneSnapshot>,
    #[serde(default)]
    pub dj_sessions: Vec<SessionBefore>,
}

impl BeforeState {
    /// Parse a logged before-state in either form: a bare array of zone
    /// snapshots (every row logged before sessions could be undone, and
    /// zones-only rows since) or `{"zones":[…],"dj_sessions":[…]}`.
    ///
    /// # Errors
    /// The JSON is neither form.
    pub fn parse(json: &str) -> Result<Self, serde_json::Error> {
        let value: serde_json::Value = serde_json::from_str(json)?;
        if value.is_array() {
            return Ok(Self {
                zones: serde_json::from_value(value)?,
                dj_sessions: Vec::new(),
            });
        }
        serde_json::from_value(value)
    }
}

/// The DJ sessions of the zones led by `coordinators` (duplicates ignored)
/// before an action changes them, for [`before_state_with`].
///
/// # Errors
/// The store cannot be read.
pub fn capture_sessions<S: Store + ?Sized>(
    store: &S,
    coordinators: &[PlayerId],
) -> Result<Vec<SessionBefore>, CoreError> {
    let mut sessions: Vec<SessionBefore> = Vec::new();
    for coordinator in coordinators {
        if sessions.iter().any(|s| s.coordinator == coordinator.0) {
            continue;
        }
        let row = store
            .dj_session(&coordinator.0)
            .map_err(|e| store_err(&e))?;
        sessions.push(SessionBefore {
            coordinator: coordinator.0.clone(),
            row,
        });
    }
    Ok(sessions)
}

/// Snapshots as an action's before-state; `None` when there are none, so the
/// action is not undoable.
#[must_use]
pub fn before_state(snaps: &[ZoneSnapshot]) -> Option<String> {
    before_state_with(snaps, &[])
}

/// Snapshots and DJ sessions as an action's before-state; `None` when both
/// are empty, so the action is not undoable. Zones alone keep the bare-array
/// form older rows use.
#[must_use]
pub fn before_state_with(snaps: &[ZoneSnapshot], sessions: &[SessionBefore]) -> Option<String> {
    if sessions.is_empty() {
        if snaps.is_empty() {
            return None;
        }
        return serde_json::to_string(snaps).ok();
    }
    Some(serde_json::json!({ "zones": snaps, "dj_sessions": sessions }).to_string())
}

/// Log `action`, then apply the retention policy. Returns the action's id.
pub fn record<S: Store + ?Sized>(store: &mut S, action: &Action) -> Result<i64, CoreError> {
    let id = store.record_action(action).map_err(|e| store_err(&e))?;
    store
        .prune_actions(KEEP_ACTIONS, action.at.saturating_sub(KEEP_SECS))
        .map_err(|e| store_err(&e))?;
    Ok(id)
}

/// What [`undo_last`] did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UndoReport {
    /// The id of the action that was reversed, and what it was.
    pub undone: i64,
    pub intent: String,
    /// The id of the undo's own log entry.
    pub undo_id: i64,
    /// Per zone (by coordinator): what came back and what could not.
    pub zones: Vec<(PlayerId, RestoreReport)>,
    /// Zones that could not be restored at all, with why.
    pub failures: Vec<(PlayerId, String)>,
    /// DJ sessions put back (by coordinator); a zone that had none is
    /// unsteered again.
    pub sessions: Vec<PlayerId>,
    /// DJ sessions that could not be put back, with why.
    pub session_failures: Vec<(PlayerId, String)>,
    /// One line for people.
    pub summary: String,
}

/// Undo the newest undoable action — only `only`'s when given — on behalf of
/// `by` through `surface`: restore the zones it touched and the DJ sessions
/// it changed, log the undo, and report. `Ok(None)` when there is nothing to
/// undo.
///
/// The zones go back to exactly the state captured before that action, even
/// if later actions changed them since; undoing again steps further back.
pub fn undo_last<T: Transport + ?Sized, S: Store + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    store: &mut S,
    only: Option<&Client>,
    by: &Client,
    surface: &str,
    at: i64,
) -> Result<Option<UndoReport>, CoreError> {
    let Some(target) = store
        .last_undoable_action(only.map(Client::key))
        .map_err(|e| store_err(&e))?
    else {
        return Ok(None);
    };
    let before = BeforeState::parse(target.action.before_state.as_deref().unwrap_or("[]"))
        .map_err(|e| {
            CoreError::Store(format!(
                "action {} has an unreadable before-state: {e}",
                target.id
            ))
        })?;
    let coordinators: Vec<PlayerId> = before.zones.iter().map(|s| s.coordinator.clone()).collect();
    let (now, _) = capture_zones(t, households, &coordinators, at);
    let steered: Vec<PlayerId> = before
        .dj_sessions
        .iter()
        .map(|s| PlayerId(s.coordinator.clone()))
        .collect();
    let now_sessions = capture_sessions(store, &steered)?;

    let mut zones = Vec::new();
    let mut failures = Vec::new();
    for snap in &before.zones {
        match snapshot::restore(t, households, snap) {
            Ok(report) => zones.push((snap.coordinator.clone(), report)),
            Err(e) => failures.push((snap.coordinator.clone(), e.to_string())),
        }
    }
    let mut sessions = Vec::new();
    let mut session_failures = Vec::new();
    for session in &before.dj_sessions {
        let put_back = match &session.row {
            Some(row) => store.save_dj_session(row),
            None => store.delete_dj_session(&session.coordinator),
        };
        let coordinator = PlayerId(session.coordinator.clone());
        match put_back {
            Ok(()) => sessions.push(coordinator),
            Err(e) => session_failures.push((coordinator, e.to_string())),
        }
    }
    let summary = summarize(
        target.id,
        &target.action.intent,
        &zones,
        &failures,
        &named(households, &sessions),
        &session_failures,
    );
    let undo_id = record(
        store,
        &Action {
            at,
            client: by.key().to_string(),
            surface: surface.to_string(),
            intent: format!("undo #{}: {}", target.id, target.action.intent),
            decision: "allow".into(),
            result: summary.clone(),
            before_state: before_state_with(&now, &now_sessions),
            undo_of: Some(target.id),
        },
    )?;
    Ok(Some(UndoReport {
        undone: target.id,
        intent: target.action.intent,
        undo_id,
        zones,
        failures,
        sessions,
        session_failures,
        summary,
    }))
}

/// Each coordinator's room name, or its id when the households no longer
/// know it.
fn named(households: &[HouseholdState], coordinators: &[PlayerId]) -> Vec<String> {
    coordinators
        .iter()
        .map(|id| {
            households
                .iter()
                .find_map(|h| h.player(id))
                .map_or_else(|| id.0.clone(), |p| p.room_name.clone())
        })
        .collect()
}

fn summarize(
    id: i64,
    intent: &str,
    zones: &[(PlayerId, RestoreReport)],
    failures: &[(PlayerId, String)],
    sessions: &[String],
    session_failures: &[(PlayerId, String)],
) -> String {
    let mut parts = Vec::new();
    let steering = !sessions.is_empty() || !session_failures.is_empty();
    if !zones.is_empty() || !failures.is_empty() || !steering {
        parts.push(format!(
            "restored {} of {} zone(s)",
            zones.len(),
            zones.len() + failures.len()
        ));
    }
    for (_, report) in zones {
        for (aspect, why) in &report.skipped {
            parts.push(format!("{aspect:?} not restored: {why}").to_lowercase());
        }
    }
    for (coordinator, why) in failures {
        parts.push(format!("{} failed: {why}", coordinator.0));
    }
    if !sessions.is_empty() {
        parts.push(format!("steering for {} put back", sessions.join(", ")));
    }
    for (coordinator, why) in session_failures {
        parts.push(format!(
            "steering for {} not put back: {why}",
            coordinator.0
        ));
    }
    format!("undid #{id} ({intent}): {}", parts.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{Aspect, SnapshotSource};
    use crate::store::{ActionFilter, MemStore};
    use fsonos_types::TransportState;

    fn snap(coordinator: &str) -> ZoneSnapshot {
        ZoneSnapshot {
            household: None,
            coordinator: PlayerId(coordinator.into()),
            members: vec![PlayerId(coordinator.into())],
            levels: Vec::new(),
            group_volume: Some(20),
            transport_state: TransportState::Playing,
            source: SnapshotSource::Uri {
                uri: "x-rincon-mp3radio://example.invalid/stream".into(),
                metadata: String::new(),
                position_secs: None,
            },
            captured_at: 5,
        }
    }

    fn action(at: i64, before: Option<String>) -> Action {
        Action {
            at,
            client: "tag:agent".into(),
            surface: "mcp".into(),
            intent: "volume Kitchen 90".into(),
            decision: "clamp: Kitchen is capped at 70".into(),
            result: "Kitchen volume 70".into(),
            before_state: before,
            undo_of: None,
        }
    }

    #[test]
    fn before_state_round_trips_and_is_none_when_empty() {
        assert_eq!(before_state(&[]), None);
        let json = before_state(&[snap("RINCON_A"), snap("RINCON_B")]).unwrap();
        let back: Vec<ZoneSnapshot> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, [snap("RINCON_A"), snap("RINCON_B")]);
    }

    #[test]
    fn record_prunes_by_age_and_count() {
        let mut store = MemStore::default();
        let old = record(&mut store, &action(0, None)).unwrap();
        let fresh = record(&mut store, &action(KEEP_SECS + 10, None)).unwrap();
        let log = store.recent_actions(&ActionFilter::default()).unwrap();
        assert_eq!(log.iter().map(|a| a.id).collect::<Vec<_>>(), [fresh]);
        assert!(fresh > old);
    }

    #[test]
    fn nothing_to_undo_is_none() {
        struct NoLan;
        impl Transport for NoLan {
            fn soap_post(
                &self,
                _: std::net::IpAddr,
                _: &str,
                _: &str,
                _: &str,
            ) -> Result<String, fsonos_proto::ProtoError> {
                Err(fsonos_proto::ProtoError::NotWired("no LAN"))
            }
        }
        let mut store = MemStore::default();
        // A denial has no before-state: nothing to undo.
        record(&mut store, &action(1, None)).unwrap();
        let done = undo_last(&NoLan, &[], &mut store, None, &Client::Cli, "cli", 2).unwrap();
        assert_eq!(done, None);
    }

    #[test]
    fn summary_names_what_did_not_come_back() {
        let report = RestoreReport {
            restored: vec![Aspect::Volume],
            skipped: vec![(Aspect::Position, "the queue changed".into())],
        };
        let text = summarize(
            7,
            "volume Kitchen 90",
            &[(PlayerId("RINCON_A".into()), report)],
            &[(PlayerId("RINCON_B".into()), "unknown player".into())],
            &[],
            &[],
        );
        assert_eq!(
            text,
            "undid #7 (volume Kitchen 90): restored 1 of 2 zone(s); position not restored: \
             the queue changed; RINCON_B failed: unknown player"
        );
    }

    #[test]
    fn summary_of_a_pure_steer_names_the_steering() {
        let text = summarize(
            8,
            "dj steer Kitchen calm",
            &[],
            &[],
            &["Kitchen".into()],
            &[(PlayerId("RINCON_B".into()), "disk full".into())],
        );
        assert_eq!(
            text,
            "undid #8 (dj steer Kitchen calm): steering for Kitchen put back; \
             steering for RINCON_B not put back: disk full"
        );
    }

    fn session(coordinator: &str, row: Option<&str>) -> SessionBefore {
        SessionBefore {
            coordinator: coordinator.into(),
            row: row.map(|mood| DjSession {
                coordinator: coordinator.into(),
                mood: Some(mood.into()),
                constraints: Some(r#"{"energy":[0.2,0.5]}"#.into()),
                expires: 900,
            }),
        }
    }

    #[test]
    fn sessions_make_the_object_form_and_zones_alone_stay_a_bare_array() {
        assert_eq!(before_state_with(&[], &[]), None);
        let zones_only = before_state_with(&[snap("RINCON_A")], &[]).unwrap();
        assert!(zones_only.starts_with('['), "{zones_only}");
        assert_eq!(
            BeforeState::parse(&zones_only).unwrap(),
            BeforeState {
                zones: vec![snap("RINCON_A")],
                dj_sessions: Vec::new(),
            }
        );

        let sessions = [session("RINCON_A", Some("calm")), session("RINCON_B", None)];
        let json = before_state_with(&[snap("RINCON_A")], &sessions).unwrap();
        assert!(json.starts_with('{'), "{json}");
        assert_eq!(
            BeforeState::parse(&json).unwrap(),
            BeforeState {
                zones: vec![snap("RINCON_A")],
                dj_sessions: sessions.to_vec(),
            }
        );
        let steer_only = before_state_with(&[], &sessions[1..]).unwrap();
        assert_eq!(
            BeforeState::parse(&steer_only).unwrap().dj_sessions,
            sessions[1..]
        );
        assert!(BeforeState::parse(r#""zones""#).is_err());
    }
}
