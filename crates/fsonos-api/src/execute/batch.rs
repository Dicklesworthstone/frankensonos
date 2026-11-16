//! A batch receipt keeps each planned destination and its actual result.
//! Partial completion is data, not an instruction to replay the request.

use fastapi::{JsonSchema, fastapi_openapi};
use serde::{Deserialize, Serialize};

use super::OutcomeDto;
use crate::guard::Note;
use crate::{ApiError, NoteCode};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ControlBatchDto {
    /// Every planned command returned a successful outcome.
    pub complete: bool,
    /// In original plan order, including commands not attempted after a failure.
    pub results: Vec<ControlTargetDto>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ControlTargetDto {
    /// The household label at admission, such as S1 or S2.
    pub household: String,
    /// The originally addressed player UUID. Recovery is described in notes.
    pub target: String,
    /// That player's room name at admission.
    pub zone: String,
    pub result: ControlResultDto,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ControlResultDto {
    Succeeded {
        done: String,
        changed: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        volume: Option<u8>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        notes: Vec<Note>,
    },
    Failed {
        error: ApiError,
    },
    /// A preceding failure ended sequential execution before this target.
    NotAttempted,
}

impl ControlResultDto {
    #[must_use]
    pub fn complete(&self) -> bool {
        matches!(self, Self::Succeeded { notes, .. } if !notes.iter().any(|note| note.code == NoteCode::Skipped))
    }
}

impl OutcomeDto {
    /// Read completion from structured results. Older daemon responses still
    /// carry typed SKIPPED notes; benign clamp/heal/logging notes are successful.
    #[must_use]
    pub fn incomplete_targets(&self) -> usize {
        let skipped = self
            .notes
            .iter()
            .filter(|note| note.code == NoteCode::Skipped)
            .count();
        let Some(batch) = &self.batch else {
            return skipped;
        };
        let failed = batch
            .results
            .iter()
            .filter(|target| !target.result.complete())
            .count();
        // A contradictory receipt must never make an incomplete call successful.
        failed
            .max(usize::from(!batch.complete || batch.results.is_empty()))
            .max(skipped)
    }

    #[must_use]
    pub fn complete(&self) -> bool {
        self.incomplete_targets() == 0
    }
}

// The pinned schema derive does not honor serde's tag on data enums.
impl fastapi_openapi::JsonSchema for ControlResultDto {
    fn schema() -> fastapi_openapi::Schema {
        use fastapi_openapi::{ObjectSchema, Schema};

        let variant = |status: &str, required: &[&str], fields: Vec<(&str, Schema)>| {
            let mut properties: std::collections::HashMap<_, _> = fields
                .into_iter()
                .map(|(name, schema)| (name.to_owned(), schema))
                .collect();
            properties.insert("status".into(), Schema::string_enum(vec![status.into()]));
            Schema::Object(ObjectSchema {
                title: None,
                description: None,
                properties,
                required: required.iter().map(|name| (*name).to_owned()).collect(),
                additional_properties: None,
            })
        };
        Schema::one_of(vec![
            variant(
                "succeeded",
                &["status", "done", "changed"],
                vec![
                    ("done", String::schema()),
                    ("changed", bool::schema()),
                    ("volume", Option::<u8>::schema()),
                    ("notes", Vec::<Note>::schema()),
                ],
            ),
            variant(
                "failed",
                &["status", "error"],
                vec![("error", ApiError::schema())],
            ),
            variant("not_attempted", &["status"], Vec::new()),
        ])
    }

    fn schema_name() -> Option<&'static str> {
        Some("ControlResultDto")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ErrorCode, Failure};

    #[test]
    fn legacy_single_target_responses_keep_their_shape_and_benign_notes_succeed() {
        let mut response: OutcomeDto = serde_json::from_value(serde_json::json!({
            "done": "volume is 20", "changed": true
        }))
        .unwrap();
        for code in [
            NoteCode::VolumeClamped,
            NoteCode::Healed,
            NoteCode::NotLogged,
        ] {
            response.notes.push(Note {
                code,
                detail: "detail".into(),
            });
        }
        assert!(response.complete());
        assert!(
            serde_json::to_value(&response)
                .unwrap()
                .get("batch")
                .is_none()
        );
        response.notes.push(Note {
            code: NoteCode::Skipped,
            detail: "legacy failure".into(),
        });
        assert_eq!(response.incomplete_targets(), 1);
    }

    #[test]
    fn structured_failure_is_incomplete_even_without_notes_or_with_a_wrong_summary_flag() {
        let response: OutcomeDto = serde_json::from_value(serde_json::json!({
            "done": "one room accepted", "changed": true,
            "batch": {"complete": true, "results": [{
                "household": "S1", "target": "RINCON_TEST", "zone": "Study",
                "result": {"status": "failed", "error": Failure::new(ErrorCode::OutcomeUncertain, "reply lost").body()}
            }]}
        }))
        .unwrap();
        assert_eq!(response.incomplete_targets(), 1);
        let serialized = serde_json::to_value(response).unwrap();
        assert_eq!(
            serialized["batch"]["results"][0]["result"]["error"]["code"],
            "OUTCOME_UNCERTAIN"
        );
    }

    #[test]
    fn schema_uses_the_wire_status_for_every_result() {
        let schema =
            serde_json::to_value(<ControlResultDto as fastapi_openapi::JsonSchema>::schema())
                .unwrap();
        let variants = schema["oneOf"].as_array().unwrap();
        assert_eq!(variants.len(), 3);
        for (variant, status) in variants
            .iter()
            .zip(["succeeded", "failed", "not_attempted"])
        {
            assert_eq!(variant["properties"]["status"]["enum"][0], status);
            assert!(
                variant["required"]
                    .as_array()
                    .unwrap()
                    .contains(&serde_json::json!("status"))
            );
        }
    }
}
