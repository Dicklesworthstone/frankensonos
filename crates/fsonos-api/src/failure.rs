//! The one error model every surface reports.
//!
//! A [`Failure`] is a stable [`ErrorCode`] plus an agent-readable `detail`, a
//! one-sentence `hint` on what to do next, and `suggestions` to retry with
//! (for example the nearest room names). The code fixes the HTTP status, the
//! CLI exit code, and whether retrying unchanged can succeed, so the same
//! failure reads the same everywhere:
//!
//! * HTTP: [`Failure::http_response`], the status plus an [`ApiError`] body
//!   (`{"detail", "code", "hint", "suggestions", "retryable"}`);
//! * MCP: [`Failure::tool_text`] as the tool-error text, with the same JSON as
//!   structured content;
//! * CLI: [`Failure::cli_text`] on stderr and [`Failure::exit_code`].
//!
//! `docs/ERRORS.md` documents every code; a test keeps the two in step.

use fastapi::fastapi_openapi::{JsonSchema, Schema};
use fastapi::{Response, ResponseBody, StatusCode};
use fsonos_core::CoreError;
use fsonos_core::favorites::FavoriteError;
use fsonos_core::rooms::suggest_rooms;
use fsonos_proto::ProtoError;
use serde::{Deserialize, Serialize};
use std::fmt;

use crate::ApiError;

/// Stable, documented failure codes. Never rename one; add new ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    /// A request field is missing, malformed or out of range.
    InvalidArgument,
    /// No room matches the name given.
    UnknownRoom,
    /// The name matches rooms in more than one place.
    AmbiguousRoom,
    /// No household matches the label or id given.
    UnknownHousehold,
    /// The rooms are in different households, which can never share a group.
    CrossHouseholdGroup,
    /// Nothing has been discovered yet.
    NotReady,
    /// A speaker did not answer.
    PlayerUnreachable,
    /// The group changed under the command; its coordinator moved.
    NotCoordinator,
    /// A speaker answered with a UPnP fault (`upnp_code`) or an unreadable
    /// response.
    UpnpFault,
    /// The household's Sonos app has no linked Spotify account.
    SpotifyNotLinked,
    /// The household's Spotify render parameters have not been learned.
    RenderParamsMissing,
    /// The household's Spotify render parameters are stale: the player
    /// refused the render with UPnP 800 even after relearning them once.
    RenderParamsStale,
    /// The daemon has no valid Spotify sign-in.
    SpotifyAuthRequired,
    /// The house policy forbids the request.
    PolicyDenied,
    /// No DJ mood has that name.
    UnknownMood,
    /// No DJ session runs in that zone.
    NoDjSession,
    /// A fault inside the daemon.
    Internal,
    /// The request is understood but this build cannot carry it out yet.
    NotImplemented,
    /// No favorite in that household matches the name given.
    UnknownFavorite,
    /// The name matches more than one favorite.
    AmbiguousFavorite,
    /// The favorite is a shortcut with nothing to play.
    UnplayableFavorite,
    /// The request came from a web page that is not one of the daemon's own.
    UntrustedOrigin,
    /// A control request whose body is not `application/json`.
    UnsupportedMediaType,
    /// A library search found nothing to play.
    NoMatch,
    /// No saved scene has that name.
    UnknownScene,
    /// No schedule has that id.
    UnknownSchedule,
}

impl ErrorCode {
    /// Every code, in documentation order.
    pub const ALL: [Self; 25] = [
        Self::InvalidArgument,
        Self::UnknownRoom,
        Self::AmbiguousRoom,
        Self::UnknownHousehold,
        Self::CrossHouseholdGroup,
        Self::NotReady,
        Self::PlayerUnreachable,
        Self::NotCoordinator,
        Self::UpnpFault,
        Self::SpotifyNotLinked,
        Self::RenderParamsMissing,
        Self::RenderParamsStale,
        Self::SpotifyAuthRequired,
        Self::PolicyDenied,
        Self::UnknownMood,
        Self::NoDjSession,
        Self::Internal,
        Self::NotImplemented,
        Self::UnknownFavorite,
        Self::AmbiguousFavorite,
        Self::UnplayableFavorite,
        Self::UntrustedOrigin,
        Self::UnsupportedMediaType,
        Self::NoMatch,
        Self::UnknownScene,
        Self::UnknownSchedule,
    ];

    /// The wire name, e.g. `UNKNOWN_ROOM`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidArgument => "INVALID_ARGUMENT",
            Self::UnknownRoom => "UNKNOWN_ROOM",
            Self::AmbiguousRoom => "AMBIGUOUS_ROOM",
            Self::UnknownHousehold => "UNKNOWN_HOUSEHOLD",
            Self::CrossHouseholdGroup => "CROSS_HOUSEHOLD_GROUP",
            Self::NotReady => "NOT_READY",
            Self::PlayerUnreachable => "PLAYER_UNREACHABLE",
            Self::NotCoordinator => "NOT_COORDINATOR",
            Self::UpnpFault => "UPNP_FAULT",
            Self::SpotifyNotLinked => "SPOTIFY_NOT_LINKED",
            Self::RenderParamsStale => "RENDER_PARAMS_STALE",
            Self::SpotifyAuthRequired => "SPOTIFY_AUTH_REQUIRED",
            Self::PolicyDenied => "POLICY_DENIED",
            Self::UnknownMood => "UNKNOWN_MOOD",
            Self::NoDjSession => "NO_DJ_SESSION",
            Self::Internal => "INTERNAL",
            Self::NotImplemented => "NOT_IMPLEMENTED",
            Self::UnknownFavorite => "UNKNOWN_FAVORITE",
            Self::NoMatch => "NO_MATCH",
            Self::AmbiguousFavorite => "AMBIGUOUS_FAVORITE",
            Self::UnplayableFavorite => "UNPLAYABLE_FAVORITE",
            Self::UntrustedOrigin => "UNTRUSTED_ORIGIN",
            Self::UnsupportedMediaType => "UNSUPPORTED_MEDIA_TYPE",
            Self::UnknownScene => "UNKNOWN_SCENE",
            Self::UnknownSchedule => "UNKNOWN_SCHEDULE",
        }
    }

    /// The HTTP status the API answers with.
    #[must_use]
    pub fn status(self) -> u16 {
        match self {
            Self::InvalidArgument | Self::CrossHouseholdGroup | Self::UnplayableFavorite => 422,
            Self::UnknownRoom
            | Self::UnknownHousehold
            | Self::UnknownMood
            | Self::NoDjSession
            | Self::UnknownFavorite
            | Self::NoMatch
            | Self::UnknownScene
            | Self::UnknownSchedule => 404,
            Self::AmbiguousRoom
            | Self::NotCoordinator
            | Self::SpotifyNotLinked
            | Self::RenderParamsMissing
            | Self::RenderParamsStale
            | Self::SpotifyAuthRequired
            | Self::AmbiguousFavorite => 409,
            Self::PolicyDenied | Self::UntrustedOrigin => 403,
            Self::UnsupportedMediaType => 415,
            Self::NotReady | Self::PlayerUnreachable => 503,
            Self::Internal => 500,
            Self::NotImplemented => 501,
        }
    }

    /// The `fsonos` exit code: 2 usage, 3 not found, 4 unreachable or not
    /// ready, 5 policy, 1 anything else.
    #[must_use]
    pub fn exit_code(self) -> u8 {
        match self {
            Self::InvalidArgument
            | Self::AmbiguousRoom
            | Self::CrossHouseholdGroup
            | Self::AmbiguousFavorite
            | Self::UnplayableFavorite
            | Self::UnsupportedMediaType => 2,
            Self::UnknownRoom
            | Self::UnknownHousehold
            | Self::UnknownMood
            | Self::NoDjSession
            | Self::UnknownFavorite
            | Self::NoMatch
            | Self::UnknownScene
            | Self::UnknownSchedule => 3,
            Self::NotReady | Self::PlayerUnreachable | Self::NotCoordinator => 4,
            Self::PolicyDenied | Self::UntrustedOrigin => 5,
            Self::UpnpFault
            | Self::SpotifyNotLinked
            | Self::RenderParamsMissing
            | Self::RenderParamsStale
            | Self::SpotifyAuthRequired
            | Self::Internal
            | Self::NotImplemented => 1,
        }
    }

    /// Whether the same request, retried unchanged shortly, can succeed.
    #[must_use]
    pub fn retryable(self) -> bool {
        matches!(
            self,
            Self::NotReady | Self::PlayerUnreachable | Self::NotCoordinator
        )
    }

    /// The hint a failure carries unless it supplies a sharper one.
    #[must_use]
    pub fn default_hint(self) -> &'static str {
        match self {
            Self::InvalidArgument => "Fix the field the detail names and send the request again.",
            Self::UnknownRoom => {
                "Use a suggested room, or list rooms with list_zones (GET /zones)."
            }
            Self::AmbiguousRoom => {
                "Repeat the request with one of the suggestions; Room@S1 or Room@S2 picks the household."
            }
            Self::UnknownHousehold => {
                "List rooms with list_zones (GET /zones) to see the households."
            }
            Self::CrossHouseholdGroup => {
                "Group rooms within one household; S1 and S2 players can never share a group."
            }
            Self::NotReady => "Discovery is still running; retry in a few seconds.",
            Self::PlayerUnreachable => {
                "Check the speaker is powered and on the network, then retry."
            }
            Self::NotCoordinator => "The group changed while the command ran; retry it.",
            Self::UpnpFault => {
                "The speaker refused the command in its current state; check it and retry."
            }
            Self::SpotifyNotLinked => {
                "Link Spotify in that household's Sonos app once, then retry."
            }
            Self::RenderParamsMissing => {
                "Add any Spotify track to My Sonos in that household's app, then retry."
            }
            Self::RenderParamsStale => {
                "Re-link Spotify in that household's Sonos app (or remove and re-add a Spotify \
                 track favorite), then retry."
            }
            Self::PolicyDenied => "The house policy forbids this; ask the owner to change it.",
            Self::UnknownMood => "Use one of the suggested moods.",
            Self::NoDjSession => "Start the DJ in that zone first (dj_start).",
            Self::Internal => "Retry once; if it persists, check the daemon log.",
            Self::NotImplemented => "Use what the detail suggests until this lands.",
            Self::UnknownFavorite => {
                "Use a suggested favorite, or list them with list_favorites (GET /favorites)."
            }
            Self::AmbiguousFavorite => "Repeat the request with one of the suggested titles.",
            Self::NoMatch => {
                "Try fewer or other words: a composer's surname, a performer, or a catalog number (bwv 988)."
            }
            Self::UnplayableFavorite => "Pick a favorite that is a track, a station or a playlist.",
            Self::UntrustedOrigin => {
                "Call the API from the CLI, an agent, or the daemon's own pages."
            }
            Self::UnsupportedMediaType => {
                "Send the request body as JSON with Content-Type: application/json."
            }
            Self::UnknownScene => {
                "List the scenes with list_scenes (GET /scenes), or use a suggested name."
            }
            Self::UnknownSchedule => "Use an id that list_schedules (GET /schedules) shows.",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Stable codes for notes a successful response can carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum NoteCode {
    /// The requested volume exceeded the house policy and was lowered.
    VolumeClamped,
    /// The speakers had changed under the request (a player at a new
    /// address, a new group coordinator) and it was retried once there.
    Healed,
}

impl NoteCode {
    /// Every note code, in documentation order.
    pub const ALL: [Self; 2] = [Self::VolumeClamped, Self::Healed];

    /// The wire name, e.g. `VOLUME_CLAMPED`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::VolumeClamped => "VOLUME_CLAMPED",
            Self::Healed => "HEALED",
        }
    }
}

// By hand: the derive would name the variants, not their wire names.
impl JsonSchema for ErrorCode {
    fn schema() -> Schema {
        Schema::string_enum(Self::ALL.iter().map(|c| c.as_str().to_string()).collect())
    }

    fn schema_name() -> Option<&'static str> {
        Some("ErrorCode")
    }
}

impl JsonSchema for NoteCode {
    fn schema() -> Schema {
        Schema::string_enum(Self::ALL.iter().map(|c| c.as_str().to_string()).collect())
    }

    fn schema_name() -> Option<&'static str> {
        Some("NoteCode")
    }
}

/// A request that could not be carried out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub code: ErrorCode,
    /// What went wrong, specifically.
    pub detail: String,
    /// One sentence on what to do next.
    pub hint: String,
    /// Values to retry with, best first (room names, moods, ...).
    pub suggestions: Vec<String>,
    /// The UPnP error code, for [`ErrorCode::UpnpFault`].
    pub upnp_code: Option<u16>,
    /// The room the request named, for [`ErrorCode::UnknownRoom`] (a surface
    /// with a live model checks it against the vanished players).
    pub room: Option<String>,
}

impl Failure {
    /// A failure with `code`'s default hint and no suggestions.
    #[must_use]
    pub fn new(code: ErrorCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
            hint: code.default_hint().to_string(),
            suggestions: Vec::new(),
            upnp_code: None,
            room: None,
        }
    }

    /// The request itself is malformed or out of range
    /// ([`ErrorCode::InvalidArgument`]).
    #[must_use]
    pub fn invalid(detail: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidArgument, detail)
    }

    /// Replace the default hint.
    #[must_use]
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = hint.into();
        self
    }

    /// Attach values to retry with, best first.
    #[must_use]
    pub fn with_suggestions<S: Into<String>>(
        mut self,
        suggestions: impl IntoIterator<Item = S>,
    ) -> Self {
        self.suggestions = suggestions.into_iter().map(Into::into).collect();
        self
    }

    /// The HTTP status.
    #[must_use]
    pub fn status(&self) -> u16 {
        self.code.status()
    }

    /// The `fsonos` exit code.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        self.code.exit_code()
    }

    /// Whether retrying unchanged can succeed.
    #[must_use]
    pub fn retryable(&self) -> bool {
        self.code.retryable()
    }

    /// Whether the caller's input is at fault (a 4xx), as opposed to the
    /// daemon or a speaker (a 5xx).
    #[must_use]
    pub fn is_client_error(&self) -> bool {
        (400..500).contains(&self.status())
    }

    /// The JSON body both the API and MCP structured content carry.
    #[must_use]
    pub fn body(&self) -> ApiError {
        ApiError {
            detail: self.detail.clone(),
            code: self.code,
            hint: self.hint.clone(),
            suggestions: self.suggestions.clone(),
            retryable: self.retryable(),
            upnp_code: self.upnp_code,
        }
    }

    /// The HTTP response: the code's status and the [`ApiError`] body.
    #[must_use]
    pub fn http_response(&self) -> Response {
        let body = serde_json::to_vec(&self.body()).expect("ApiError serializes");
        Response::with_status(StatusCode::from_u16(self.status()))
            .header("content-type", b"application/json".to_vec())
            .body(ResponseBody::Bytes(body))
    }

    /// One line for an MCP tool error:
    /// `CODE: detail. Hint: ... Did you mean: a, b?`
    #[must_use]
    pub fn tool_text(&self) -> String {
        let mut text = format!("{}: {}", self.code, self.detail.trim_end_matches('.'));
        text.push_str(". Hint: ");
        text.push_str(&self.hint);
        if !self.suggestions.is_empty() {
            text.push_str(" Did you mean: ");
            text.push_str(&self.suggestions.join(", "));
            text.push('?');
        }
        text
    }

    /// The CLI's stderr report.
    #[must_use]
    pub fn cli_text(&self) -> String {
        let mut text = format!(
            "error[{}]: {}\n  hint: {}",
            self.code, self.detail, self.hint
        );
        if !self.suggestions.is_empty() {
            text.push_str("\n  did you mean: ");
            text.push_str(&self.suggestions.join(", "));
        }
        text
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for Failure {}

impl From<Failure> for ApiError {
    fn from(failure: Failure) -> Self {
        failure.body()
    }
}

/// Core errors carry their own retry-able text (known rooms, qualified
/// candidates); the mapping picks the code and the suggestions.
impl From<CoreError> for Failure {
    fn from(err: CoreError) -> Self {
        let detail = err.to_string();
        match err {
            CoreError::UnknownRoom { name, known } => Self {
                room: Some(name.clone()),
                ..Self::new(ErrorCode::UnknownRoom, detail)
                    .with_suggestions(suggest_rooms(&name, &known))
            },
            CoreError::AmbiguousRoom { candidates, .. } => {
                Self::new(ErrorCode::AmbiguousRoom, detail).with_suggestions(candidates)
            }
            CoreError::UnknownHousehold(_) => Self::new(ErrorCode::UnknownHousehold, detail),
            CoreError::UnknownPlayer(id) => Self::new(
                ErrorCode::PlayerUnreachable,
                format!("player {id} is not reachable; it may be offline or have a new address"),
            ),
            CoreError::Proto(err) => proto_failure(&err, detail),
            // Store errors can carry a DB path or engine internals; keep them
            // out of the client response. The caller logs the full error.
            CoreError::Store(_) => Self::new(ErrorCode::Internal, "internal error"),
            CoreError::NoSpotifyFavorite => Self::new(ErrorCode::RenderParamsMissing, detail),
            CoreError::RenderParamsStale => Self::new(ErrorCode::RenderParamsStale, detail),
        }
    }
}

impl From<FavoriteError> for Failure {
    fn from(err: FavoriteError) -> Self {
        let detail = err.to_string();
        match err {
            FavoriteError::Unknown { suggestions, .. } => {
                Self::new(ErrorCode::UnknownFavorite, detail).with_suggestions(suggestions)
            }
            FavoriteError::Ambiguous { candidates, .. } => {
                Self::new(ErrorCode::AmbiguousFavorite, detail).with_suggestions(candidates)
            }
            FavoriteError::Unplayable { .. } => Self::new(ErrorCode::UnplayableFavorite, detail),
            FavoriteError::Core(core) => Self::from(core),
        }
    }
}

/// A speaker-side failure: a UPnP fault or unreadable answer is the speaker
/// refusing; a network failure (refused, timed out, an unexpected HTTP
/// status) means it did not answer.
fn proto_failure(err: &ProtoError, detail: String) -> Failure {
    match err {
        ProtoError::SoapFault { code, .. } => Failure {
            upnp_code: Some(*code),
            ..Failure::new(ErrorCode::UpnpFault, detail)
        },
        ProtoError::Malformed(_) => Failure::new(ErrorCode::UpnpFault, detail),
        ProtoError::Network { .. } => Failure::new(ErrorCode::PlayerUnreachable, detail),
        ProtoError::NotWired(_) => Failure::new(ErrorCode::Internal, detail),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn wire_names_match_serde() {
        for code in ErrorCode::ALL {
            assert_eq!(serde_json::to_value(code).unwrap(), json!(code.as_str()));
        }
        for note in NoteCode::ALL {
            assert_eq!(serde_json::to_value(note).unwrap(), json!(note.as_str()));
        }
    }

    #[test]
    fn statuses_exit_codes_and_retryability() {
        let expect = [
            (ErrorCode::InvalidArgument, 422, 2, false),
            (ErrorCode::UnknownRoom, 404, 3, false),
            (ErrorCode::AmbiguousRoom, 409, 2, false),
            (ErrorCode::PlayerUnreachable, 503, 4, true),
            (ErrorCode::NotCoordinator, 409, 4, true),
            (ErrorCode::NotReady, 503, 4, true),
            (ErrorCode::UpnpFault, 502, 1, false),
            (ErrorCode::PolicyDenied, 403, 5, false),
            (ErrorCode::Internal, 500, 1, false),
            (ErrorCode::NoMatch, 404, 3, false),
            (ErrorCode::UnknownSchedule, 404, 3, false),
        ];
        for (code, status, exit, retryable) in expect {
            let f = Failure::new(code, "x");
            assert_eq!(
                (f.status(), f.exit_code(), f.retryable()),
                (status, exit, retryable),
                "{code}"
            );
        }
    }

    #[test]
    fn every_code_has_a_one_sentence_hint() {
        for code in ErrorCode::ALL {
            let hint = code.default_hint();
            assert!(hint.ends_with('.') && hint.len() < 100, "{code}: {hint:?}");
        }
    }

    #[test]
    fn json_shape_is_stable() {
        let f = Failure::new(ErrorCode::UnknownRoom, "unknown room \"Kichen\"")
            .with_suggestions(["Kitchen@S1"]);
        assert_eq!(
            serde_json::to_value(f.body()).unwrap(),
            json!({
                "detail": "unknown room \"Kichen\"",
                "code": "UNKNOWN_ROOM",
                "hint": "Use a suggested room, or list rooms with list_zones (GET /zones).",
                "suggestions": ["Kitchen@S1"],
                "retryable": false
            })
        );
        let fault = Failure {
            upnp_code: Some(701),
            ..Failure::new(
                ErrorCode::UpnpFault,
                "soap fault 701: Transition not available",
            )
        };
        assert_eq!(
            serde_json::to_value(fault.body()).unwrap()["upnp_code"],
            701
        );
    }

    #[test]
    fn http_response_carries_status_and_body() {
        let f = Failure::new(ErrorCode::PolicyDenied, "volume 90 is above the cap");
        let (status, headers, body) = f.http_response().into_parts();
        assert_eq!(status.as_u16(), 403);
        assert!(
            headers
                .iter()
                .any(|(k, v)| k == "content-type" && v == b"application/json")
        );
        let ResponseBody::Bytes(bytes) = body else {
            panic!("expected a byte body")
        };
        let parsed: ApiError = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed, f.body());
    }

    #[test]
    fn tool_and_cli_text() {
        let f = Failure::new(ErrorCode::AmbiguousRoom, "room \"Den\" is ambiguous.")
            .with_suggestions(["Den@S1", "Den@S2"]);
        assert_eq!(
            f.tool_text(),
            "AMBIGUOUS_ROOM: room \"Den\" is ambiguous. Hint: Repeat the request with one of the \
             suggestions; Room@S1 or Room@S2 picks the household. Did you mean: Den@S1, Den@S2?"
        );
        assert_eq!(
            f.cli_text(),
            "error[AMBIGUOUS_ROOM]: room \"Den\" is ambiguous.\n  hint: Repeat the request with \
             one of the suggestions; Room@S1 or Room@S2 picks the household.\n  did you mean: \
             Den@S1, Den@S2"
        );
        let plain =
            Failure::invalid("volume must be 0 to 100, got 140").with_hint("Send 0 to 100.");
        assert_eq!(
            plain.tool_text(),
            "INVALID_ARGUMENT: volume must be 0 to 100, got 140. Hint: Send 0 to 100."
        );
    }

    #[test]
    fn core_errors_map_to_codes() {
        let known: Vec<String> = ["Kitchen@S1", "Den@S1", "Kitchen@S2", "Patio@S2"]
            .map(String::from)
            .into();
        let unknown = Failure::from(CoreError::UnknownRoom {
            name: "kichen".into(),
            known: known.clone(),
        });
        assert_eq!(
            (unknown.code, unknown.status()),
            (ErrorCode::UnknownRoom, 404)
        );
        assert_eq!(unknown.suggestions, ["Kitchen@S1", "Kitchen@S2"]);
        assert!(unknown.detail.contains("Patio@S2"), "{unknown}");

        let ambiguous = Failure::from(CoreError::AmbiguousRoom {
            name: "Kitchen".into(),
            candidates: vec!["Kitchen@S1".into(), "Kitchen@S2".into()],
        });
        assert_eq!(ambiguous.code, ErrorCode::AmbiguousRoom);
        assert_eq!(ambiguous.suggestions, ["Kitchen@S1", "Kitchen@S2"]);

        let soap = Failure::from(CoreError::Proto(ProtoError::SoapFault {
            code: 701,
            reason: "Transition not available".into(),
        }));
        assert_eq!(
            (soap.code, soap.upnp_code, soap.status()),
            (ErrorCode::UpnpFault, Some(701), 502)
        );

        let garbled = Failure::from(CoreError::Proto(ProtoError::Malformed("no body".into())));
        assert_eq!(
            (garbled.code, garbled.upnp_code),
            (ErrorCode::UpnpFault, None)
        );
        let silent = Failure::from(CoreError::Proto(ProtoError::Network {
            target: "192.0.2.10:1400".into(),
            detail: "connection refused".into(),
        }));
        assert_eq!(
            (silent.code, silent.status()),
            (ErrorCode::PlayerUnreachable, 503)
        );
        assert!(silent.retryable());
        let unwired = Failure::from(CoreError::Proto(ProtoError::NotWired("ssdp")));
        assert_eq!(unwired.code, ErrorCode::Internal);

        let gone = Failure::from(CoreError::UnknownPlayer("RINCON_X".into()));
        assert_eq!(gone.code, ErrorCode::PlayerUnreachable);
        assert!(gone.retryable() && gone.detail.contains("RINCON_X"));

        let store = Failure::from(CoreError::Store("/secret/path/db: disk full".into()));
        assert_eq!(
            (store.code, store.detail.as_str()),
            (ErrorCode::Internal, "internal error")
        );
    }

    /// `docs/ERRORS.md` lists every code with the status, exit code and
    /// retryability the code reports, and nothing else.
    #[test]
    fn errors_doc_matches_the_codes() {
        let doc = include_str!("../../../docs/ERRORS.md");
        let rows: Vec<Vec<&str>> = doc
            .lines()
            .filter(|l| l.starts_with("| `"))
            .map(|l| l.trim_matches('|').split('|').map(str::trim).collect())
            .collect();
        let documented: Vec<&str> = rows.iter().map(|r| r[0].trim_matches('`')).collect();
        let mut expected: Vec<&str> = ErrorCode::ALL.iter().map(|c| c.as_str()).collect();
        expected.extend(NoteCode::ALL.iter().map(|n| n.as_str()));
        assert_eq!(
            documented, expected,
            "docs/ERRORS.md codes differ from ErrorCode::ALL + NoteCode::ALL"
        );
        for (row, code) in rows.iter().zip(ErrorCode::ALL) {
            let want = [
                code.status().to_string(),
                code.exit_code().to_string(),
                if code.retryable() { "yes" } else { "no" }.to_string(),
            ];
            assert_eq!(&row[1..4], want.as_slice(), "docs/ERRORS.md row for {code}");
            assert!(
                row[5].contains(code.default_hint()),
                "docs/ERRORS.md hint for {code}"
            );
        }
    }
}
