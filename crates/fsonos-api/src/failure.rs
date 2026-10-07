//! The one failure shape both surfaces report.
//!
//! A [`Failure`] is an HTTP status plus a human/agent-readable `detail`. The
//! HTTP routes send it as FastAPI-style `{"detail": ...}` with that status; the
//! MCP tools send the `detail` as the tool error text. Details are written for
//! an agent to act on: say what was wrong and what would work instead.

use fsonos_core::CoreError;
use std::fmt;

/// A request that could not be carried out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// HTTP status code (`422` invalid input, `404` unknown, `409` ambiguous,
    /// `502` a speaker failed, `503` not ready yet, `500` daemon fault).
    pub status: u16,
    pub detail: String,
}

impl Failure {
    fn new(status: u16, detail: impl Into<String>) -> Self {
        Self {
            status,
            detail: detail.into(),
        }
    }

    /// The request itself is malformed or out of range (`422`).
    #[must_use]
    pub fn invalid(detail: impl Into<String>) -> Self {
        Self::new(422, detail)
    }

    /// The named room, player or resource does not exist (`404`).
    #[must_use]
    pub fn not_found(detail: impl Into<String>) -> Self {
        Self::new(404, detail)
    }

    /// The request matches more than one thing, or conflicts with current
    /// state (`409`).
    #[must_use]
    pub fn conflict(detail: impl Into<String>) -> Self {
        Self::new(409, detail)
    }

    /// A speaker refused or failed the command (`502`).
    #[must_use]
    pub fn bad_gateway(detail: impl Into<String>) -> Self {
        Self::new(502, detail)
    }

    /// The daemon has not learned enough yet (e.g. no players discovered);
    /// retrying shortly may succeed (`503`).
    #[must_use]
    pub fn unavailable(detail: impl Into<String>) -> Self {
        Self::new(503, detail)
    }

    /// A fault inside the daemon (`500`).
    #[must_use]
    pub fn internal(detail: impl Into<String>) -> Self {
        Self::new(500, detail)
    }

    /// Whether the caller's input is at fault (a 4xx), as opposed to the
    /// daemon or a speaker (a 5xx).
    #[must_use]
    pub fn is_client_error(&self) -> bool {
        (400..500).contains(&self.status)
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for Failure {}

impl From<Failure> for crate::ApiError {
    fn from(failure: Failure) -> Self {
        Self {
            detail: failure.detail,
        }
    }
}

/// Core errors carry their own retry-able text (known rooms, qualified
/// candidates); the mapping only picks the status.
impl From<CoreError> for Failure {
    fn from(err: CoreError) -> Self {
        let detail = err.to_string();
        match err {
            CoreError::UnknownRoom { .. }
            | CoreError::UnknownPlayer(_)
            | CoreError::UnknownHousehold(_) => Self::not_found(detail),
            CoreError::AmbiguousRoom { .. } => Self::conflict(detail),
            CoreError::Proto(_) => Self::bad_gateway(detail),
            // Store errors can carry a DB path or engine internals; keep them
            // out of the client response. The caller logs the full error.
            CoreError::Store(_) => Self::internal("internal error"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_errors_map_to_statuses() {
        let unknown = Failure::from(CoreError::UnknownRoom {
            name: "Garage".into(),
            known: vec!["Den@S2".into()],
        });
        assert_eq!(unknown.status, 404);
        assert!(unknown.detail.contains("Den@S2"), "{unknown}");
        let ambiguous = Failure::from(CoreError::AmbiguousRoom {
            name: "Den".into(),
            candidates: vec!["Den@S1".into(), "Den@S2".into()],
        });
        assert_eq!(ambiguous.status, 409);
        assert!(ambiguous.detail.contains("Den@S1, Den@S2"), "{ambiguous}");
        let soap = Failure::from(CoreError::Proto(fsonos_proto::ProtoError::SoapFault {
            code: 701,
            reason: "Transition not available".into(),
        }));
        assert_eq!(soap.status, 502);
        assert_eq!(
            Failure::from(CoreError::Store("disk full".into())).status,
            500
        );
        assert_eq!(
            Failure::from(CoreError::UnknownPlayer("RINCON_X".into())).status,
            404
        );
    }

    #[test]
    fn statuses_follow_their_constructor() {
        assert_eq!(Failure::invalid("x").status, 422);
        assert_eq!(Failure::not_found("x").status, 404);
        assert_eq!(Failure::conflict("x").status, 409);
        assert_eq!(Failure::bad_gateway("x").status, 502);
        assert_eq!(Failure::unavailable("x").status, 503);
        assert_eq!(Failure::internal("x").status, 500);
    }

    #[test]
    fn client_errors_are_the_4xx_ones() {
        assert!(Failure::invalid("x").is_client_error());
        assert!(Failure::conflict("x").is_client_error());
        assert!(!Failure::bad_gateway("x").is_client_error());
        assert!(!Failure::unavailable("x").is_client_error());
    }

    #[test]
    fn converts_to_the_detail_payload() {
        let body = crate::ApiError::from(Failure::not_found("no room named \"Den\""));
        assert_eq!(
            serde_json::to_value(body).unwrap(),
            serde_json::json!({ "detail": "no room named \"Den\"" })
        );
    }
}
