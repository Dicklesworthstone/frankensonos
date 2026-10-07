//! The action log as every surface shows it (`GET /actions`, the
//! `recent_actions` tool, `fsonos log`), and what an undo reports.

use fsonos_core::actions::UndoReport;
use fsonos_core::store::{ActionFilter, LoggedAction};
use serde::{Deserialize, Serialize};

/// One logged action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionDto {
    pub id: i64,
    /// Unix seconds.
    pub at: i64,
    /// The policy client that asked (`cli`, `mcp-stdio`, a tailnet login, ...).
    pub client: String,
    /// The surface that carried it.
    pub surface: String,
    /// What was asked.
    pub intent: String,
    /// `allow`, `clamp: <reason>` or `deny: <reason>`.
    pub decision: String,
    /// What happened.
    pub result: String,
    /// Whether an undo can restore the state before it.
    pub undoable: bool,
    /// For an undo: the action it reversed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undo_of: Option<i64>,
}

impl From<&LoggedAction> for ActionDto {
    fn from(logged: &LoggedAction) -> Self {
        let a = &logged.action;
        Self {
            id: logged.id,
            at: a.at,
            client: a.client.clone(),
            surface: a.surface.clone(),
            intent: a.intent.clone(),
            decision: a.decision.clone(),
            result: a.result.clone(),
            undoable: a.before_state.is_some() && a.undo_of.is_none(),
            undo_of: a.undo_of,
        }
    }
}

/// Which actions to list: `GET /actions?client=&since=&limit=`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionsQuery {
    /// Only this client's actions.
    #[serde(default)]
    pub client: Option<String>,
    /// Only actions at or after this time (unix seconds).
    #[serde(default)]
    pub since: Option<i64>,
    /// At most this many, newest first (default 20).
    #[serde(default)]
    pub limit: Option<usize>,
}

impl ActionsQuery {
    /// The store filter.
    #[must_use]
    pub fn filter(&self) -> ActionFilter {
        ActionFilter {
            client: self.client.clone(),
            since: self.since,
            limit: self.limit.unwrap_or(20),
        }
    }
}

/// `POST /undo` body. `own_only` (default `true`) undoes the caller's own
/// newest action; `false` undoes the newest action of anyone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UndoRequest {
    #[serde(default = "yes")]
    pub own_only: bool,
}

fn yes() -> bool {
    true
}

/// What an undo did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndoDto {
    /// One line for people (`nothing to undo` when the log has nothing).
    pub summary: String,
    /// The action reversed, when there was one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undone: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<String>,
    /// Zones that could not be restored, with why.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<String>,
}

impl From<Option<UndoReport>> for UndoDto {
    fn from(report: Option<UndoReport>) -> Self {
        match report {
            None => Self {
                summary: "nothing to undo".to_string(),
                undone: None,
                intent: None,
                failures: Vec::new(),
            },
            Some(r) => Self {
                summary: r.summary,
                undone: Some(r.undone),
                intent: Some(r.intent),
                failures: r
                    .failures
                    .into_iter()
                    .map(|(zone, why)| format!("{}: {why}", zone.0))
                    .collect(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsonos_core::store::Action;

    #[test]
    fn actions_say_whether_they_can_be_undone() {
        let logged = |before: Option<&str>, undo_of: Option<i64>| LoggedAction {
            id: 7,
            action: Action {
                at: 1_700_000_000,
                client: "mcp-stdio".into(),
                surface: "mcp".into(),
                intent: "set_volume: ...".into(),
                decision: "clamp: room cap".into(),
                result: "Kitchen volume is 70".into(),
                before_state: before.map(str::to_string),
                undo_of,
            },
        };
        assert!(ActionDto::from(&logged(Some("{}"), None)).undoable);
        assert!(!ActionDto::from(&logged(None, None)).undoable);
        let undo = ActionDto::from(&logged(Some("{}"), Some(6)));
        assert!(!undo.undoable);
        assert_eq!(serde_json::to_value(&undo).unwrap()["undo_of"], 6);
    }

    #[test]
    fn an_empty_log_has_nothing_to_undo() {
        let dto = UndoDto::from(None);
        assert_eq!(dto.summary, "nothing to undo");
        assert_eq!(
            serde_json::to_value(&dto).unwrap(),
            serde_json::json!({ "summary": "nothing to undo" })
        );
    }

    #[test]
    fn the_default_listing_is_the_last_twenty() {
        let q: ActionsQuery = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(q.filter().limit, 20);
        assert!(serde_json::from_value::<ActionsQuery>(serde_json::json!({ "who": "x" })).is_err());
    }
}
