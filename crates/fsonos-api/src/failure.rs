//! The one failure shape both surfaces report.
//!
//! A [`Failure`] is an HTTP status plus a human/agent-readable `detail`. The
//! HTTP routes send it as FastAPI-style `{"detail": ...}` with that status; the
//! MCP tools send the `detail` as the tool error text. Details are written for
//! an agent to act on: say what was wrong and what would work instead.

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

#[cfg(test)]
mod tests {
    use super::*;

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
