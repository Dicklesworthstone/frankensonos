//! Reviewed undo endpoints. The existing POST /undo remains unchanged.

use crate::http::{Ctx, LOG, Op, answer, body};
use crate::log::{ActionDto, UndoDto};
use fastapi::core::RouteEntry;
use fastapi::{JsonSchema, fastapi_openapi};
use serde::Deserialize;

/// Only a reviewed id: callers cannot widen the scope or supply a snapshot.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CheckedUndoRequest {
    expected_id: i64,
}

pub(super) fn routes(cx: &Ctx<'_>) -> Vec<RouteEntry> {
    vec![
        cx.route(
            &Op::get(
                "/undo/preview",
                "preview_undo",
                LOG,
                "The caller's newest undoable action, or null; no speaker changes",
            ),
            |s, c, _| answer(s.preview_undo(c)),
        )
        .response_schema::<Option<ActionDto>>(200, "The caller's action, or null"),
        cx.route(
            &Op::post(
                "/undo/checked",
                "undo_checked",
                LOG,
                "Undo the reviewed action only if it is still the caller's newest eligible action",
            ),
            |s, c, req| {
                answer(body::<CheckedUndoRequest>(req).and_then(|r| s.undo_checked(c, r.expected_id)))
            },
        )
        .request_schema::<CheckedUndoRequest>(true)
        .response_schema::<UndoDto>(200, "The exact action restored and any partial failures"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn checked_undo_requires_an_id_and_never_accepts_a_wider_scope() {
        let request: CheckedUndoRequest = serde_json::from_value(json!({"expected_id": 7})).unwrap();
        assert_eq!(request.expected_id, 7);
        for value in [
            json!({}),
            json!({"expected_id": "7"}),
            json!({"expected_id": 7, "own_only": false}),
            json!({"expected_id": 7, "client": "cli"}),
            json!({"expected_id": 7, "before_state": "[]"}),
        ] {
            assert!(serde_json::from_value::<CheckedUndoRequest>(value).is_err());
        }
    }
}
