//! Final MCP protocol-version header validation.
//!
//! This module models the `2026-07-28` HTTP rule only: every request carries
//! an `MCP-Protocol-Version` header whose exact value matches the body's
//! `io.modelcontextprotocol/protocolVersion` value. Transport code owns HTTP
//! header parsing and supplies the already-decoded field value here.

use serde_json::{Map, Value, json};

use crate::ClientCapabilities;

/// The final protocol version implemented by this narrow modern surface.
pub const FINAL_PROTOCOL_VERSION: &str = "2026-07-28";

/// The exact protocol versions admitted by this final-only surface.
pub const SUPPORTED_FINAL_PROTOCOL_VERSIONS: &[&str] = &[FINAL_PROTOCOL_VERSION];

/// The HTTP header that mirrors the body's protocol-version metadata.
pub const MCP_PROTOCOL_VERSION_HEADER: &str = "MCP-Protocol-Version";

/// The HTTP header that mirrors the JSON-RPC request method.
pub const MCP_METHOD_HEADER: &str = "Mcp-Method";

/// The HTTP header that mirrors a method-specific tool, resource, or prompt name.
pub const MCP_NAME_HEADER: &str = "Mcp-Name";

/// The final MCP JSON-RPC code for malformed, missing, or mismatched headers.
pub const HEADER_MISMATCH_ERROR_CODE: i32 = -32020;

/// The message FastMCP servers send with a pre-dispatch header mismatch.
///
/// A client compares it exactly, together with the code, request id and an
/// absent `data`, before treating a refusal as safe to retry.
pub const HEADER_MISMATCH_MESSAGE: &str = "MCP request headers do not match the JSON-RPC request";

/// The final MCP JSON-RPC code for a missing required client capability.
pub const MISSING_REQUIRED_CLIENT_CAPABILITY_ERROR_CODE: i32 = -32021;

/// The final MCP JSON-RPC code for a version the server does not support.
pub const UNSUPPORTED_PROTOCOL_VERSION_ERROR_CODE: i32 = -32022;

/// The maximum encoded size accepted for typed required-capabilities error data.
pub const MAX_REQUIRED_CAPABILITIES_ERROR_DATA_BYTES: usize = 64 * 1024;

/// A validated final protocol version.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FinalProtocolVersion;

impl FinalProtocolVersion {
    /// Returns the exact wire value for this final version.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        FINAL_PROTOCOL_VERSION
    }
}

/// The request metadata whose HTTP and JSON body values mirror each other.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RequestVersionMetadata<'a> {
    /// The already-decoded `MCP-Protocol-Version` HTTP header value.
    pub header_version: Option<&'a str>,
    /// The `io.modelcontextprotocol/protocolVersion` body metadata value.
    pub body_version: Option<&'a str>,
}

/// A request admitted through the final protocol-version boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FinalRequestAdmission {
    version: FinalProtocolVersion,
}

impl FinalRequestAdmission {
    /// Returns the version whose header/body mirror was admitted.
    #[must_use]
    pub const fn protocol_version(self) -> FinalProtocolVersion {
        self.version
    }
}

/// HTTP metadata mirrored from the final JSON-RPC request body.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FinalHttpRequestMetadata<'a> {
    /// The final protocol-version header/body mirror.
    pub version: RequestVersionMetadata<'a>,
    /// The `Mcp-Method` header value.
    pub header_method: Option<&'a str>,
    /// The JSON-RPC request method.
    pub body_method: Option<&'a str>,
    /// The conditional `Mcp-Name` header value.
    pub header_name: Option<&'a str>,
    /// The conditional name or URI value from the request body.
    pub body_name: Option<&'a str>,
}

/// The exact header/body condition that failed final request admission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HeaderMismatchReason {
    /// The required protocol-version header was absent.
    MissingHeader,
    /// The required body protocol-version field was absent.
    MissingBodyVersion,
    /// The header value was empty.
    EmptyHeader,
    /// The body value was empty.
    EmptyBodyVersion,
    /// The supplied header and body values were different.
    HeaderBodyVersionMismatch,
    /// The required `Mcp-Method` header was absent.
    MissingMethodHeader,
    /// The JSON-RPC request method was absent.
    MissingBodyMethod,
    /// The `Mcp-Method` header was empty.
    EmptyMethodHeader,
    /// The JSON-RPC request method was empty.
    EmptyBodyMethod,
    /// The supplied `Mcp-Method` header and JSON-RPC method differed.
    HeaderBodyMethodMismatch,
    /// A method that requires `Mcp-Name` omitted that header.
    MissingNameHeader,
    /// A method that requires `Mcp-Name` omitted the matching body value.
    MissingBodyName,
    /// The required `Mcp-Name` header was empty.
    EmptyNameHeader,
    /// The matching body name or URI was empty.
    EmptyBodyName,
    /// The supplied `Mcp-Name` header and matching body value differed.
    HeaderBodyNameMismatch,
}

/// Typed local detail for a final `HeaderMismatchError`.
///
/// The detail is for local routing and diagnostics only. Canonical peer-facing
/// header-mismatch emission has no required error-data shape.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HeaderMismatchError {
    reason: HeaderMismatchReason,
}

impl HeaderMismatchError {
    /// Returns the local typed reason without constructing peer error data.
    #[must_use]
    pub const fn reason(self) -> HeaderMismatchReason {
        self.reason
    }

    /// Returns the final MCP JSON-RPC header-mismatch code.
    #[must_use]
    pub const fn jsonrpc_error_code(self) -> i32 {
        HEADER_MISMATCH_ERROR_CODE
    }

    /// Returns the final HTTP status for a header mismatch.
    #[must_use]
    pub const fn http_status(self) -> u16 {
        400
    }

    /// Returns no canonical peer error data for a header mismatch.
    #[must_use]
    pub fn canonical_error_data(self) -> Option<Value> {
        None
    }
}

/// Typed data for a final unsupported-protocol-version response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnsupportedProtocolVersionError {
    requested: String,
}

impl UnsupportedProtocolVersionError {
    /// Returns the exact matching header/body value the server rejected.
    #[must_use]
    pub fn requested(&self) -> &str {
        &self.requested
    }

    /// Returns the exact final versions that this surface supports.
    #[must_use]
    pub const fn supported_versions(&self) -> &'static [&'static str] {
        SUPPORTED_FINAL_PROTOCOL_VERSIONS
    }

    /// Returns the final MCP JSON-RPC unsupported-version code.
    #[must_use]
    pub const fn jsonrpc_error_code(&self) -> i32 {
        UNSUPPORTED_PROTOCOL_VERSION_ERROR_CODE
    }

    /// Returns the final HTTP status for an unsupported version.
    #[must_use]
    pub const fn http_status(&self) -> u16 {
        400
    }

    /// Returns the exact final peer error-data object.
    #[must_use]
    pub fn canonical_error_data(&self) -> Value {
        json!({
            "supported": self.supported_versions(),
            "requested": self.requested(),
        })
    }
}

/// Why a caller could not construct required-capabilities error data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequiredCapabilitiesError {
    /// The protocol requires an object, not another JSON value kind.
    NotAnObject,
    /// The exact JSON encoding exceeded the bounded peer-data allowance.
    TooLarge,
    /// The JSON object could not be encoded for the peer-facing error shape.
    Encoding,
}

/// Typed final error data for a missing required client capability.
#[derive(Clone, Debug, PartialEq)]
pub struct MissingRequiredClientCapabilityError {
    required_capabilities: Map<String, Value>,
}

impl MissingRequiredClientCapabilityError {
    /// Constructs the error from the exact typed client-capabilities object.
    ///
    /// This is the safe local constructor for a capability-gated final request:
    /// it serializes the standard typed shape without accepting a separately
    /// assembled peer-data object.
    pub fn from_client_capabilities(
        required_capabilities: &ClientCapabilities,
    ) -> Result<Self, RequiredCapabilitiesError> {
        let required_capabilities = serde_json::to_value(required_capabilities)
            .map_err(|_| RequiredCapabilitiesError::Encoding)?;
        Self::new(required_capabilities)
    }

    /// Constructs the error from the exact required-capabilities object.
    ///
    /// Flattened diagnostic paths deliberately do not enter peer-facing data;
    /// the exact object is retained for the final error shape instead.
    pub fn new(required_capabilities: Value) -> Result<Self, RequiredCapabilitiesError> {
        let Value::Object(required_capabilities) = required_capabilities else {
            return Err(RequiredCapabilitiesError::NotAnObject);
        };
        let encoded_len = serde_json::to_vec(&required_capabilities)
            .map_err(|_| RequiredCapabilitiesError::Encoding)?
            .len();
        if encoded_len > MAX_REQUIRED_CAPABILITIES_ERROR_DATA_BYTES {
            return Err(RequiredCapabilitiesError::TooLarge);
        }
        Ok(Self {
            required_capabilities,
        })
    }

    /// Returns the exact required-capabilities object.
    #[must_use]
    pub const fn required_capabilities(&self) -> &Map<String, Value> {
        &self.required_capabilities
    }

    /// Returns the final MCP JSON-RPC missing-capability code.
    #[must_use]
    pub const fn jsonrpc_error_code(&self) -> i32 {
        MISSING_REQUIRED_CLIENT_CAPABILITY_ERROR_CODE
    }

    /// Returns the final HTTP status for a missing client capability.
    #[must_use]
    pub const fn http_status(&self) -> u16 {
        400
    }

    /// Returns the exact final peer error-data object.
    #[must_use]
    pub fn canonical_error_data(&self) -> Value {
        json!({"requiredCapabilities": self.required_capabilities})
    }
}

/// One rule in the frozen final request-admission precedence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FinalAdmissionRule {
    /// A header/body mirror condition, identified by its exact reason.
    HeaderMismatch(HeaderMismatchReason),
    /// The mirror was well formed but named an unsupported version.
    UnsupportedProtocolVersion,
}

/// The exact order in which final request admission evaluates its rules.
///
/// Precedence is wire-visible contract, not an implementation detail. A peer
/// that sends a request violating several rules at once must get the same
/// refusal from every conforming implementation, or it cannot diagnose itself:
/// a client told `EmptyHeader` when it also mismatched its method mirror will
/// fix the header, resend, and be refused again for a reason it was never told
/// about.
///
/// This order was always real, but until it was written down here it was
/// *emergent* — a consequence of the statement order inside
/// [`admit_final_request`] and [`admit_final_http_request`], unnamed, unpinned,
/// and silently reorderable by any refactor that moved a `?`. Naming it makes
/// a reorder a visible edit to a declared contract.
///
/// Reading order is evaluation order: rule `i` is evaluated strictly before
/// rule `j` for every `i < j`, so a request violating both is refused by `i`.
/// The three stages run version, then method, then name; the name stage is
/// reached only for a method that requires `Mcp-Name`.
pub const FINAL_ADMISSION_PRECEDENCE: &[FinalAdmissionRule] = &[
    // Version stage.
    FinalAdmissionRule::HeaderMismatch(HeaderMismatchReason::MissingHeader),
    FinalAdmissionRule::HeaderMismatch(HeaderMismatchReason::MissingBodyVersion),
    FinalAdmissionRule::HeaderMismatch(HeaderMismatchReason::EmptyHeader),
    FinalAdmissionRule::HeaderMismatch(HeaderMismatchReason::EmptyBodyVersion),
    FinalAdmissionRule::HeaderMismatch(HeaderMismatchReason::HeaderBodyVersionMismatch),
    FinalAdmissionRule::UnsupportedProtocolVersion,
    // Method stage.
    FinalAdmissionRule::HeaderMismatch(HeaderMismatchReason::MissingMethodHeader),
    FinalAdmissionRule::HeaderMismatch(HeaderMismatchReason::MissingBodyMethod),
    FinalAdmissionRule::HeaderMismatch(HeaderMismatchReason::EmptyMethodHeader),
    FinalAdmissionRule::HeaderMismatch(HeaderMismatchReason::EmptyBodyMethod),
    FinalAdmissionRule::HeaderMismatch(HeaderMismatchReason::HeaderBodyMethodMismatch),
    // Name stage, reached only when the method requires `Mcp-Name`.
    FinalAdmissionRule::HeaderMismatch(HeaderMismatchReason::MissingNameHeader),
    FinalAdmissionRule::HeaderMismatch(HeaderMismatchReason::MissingBodyName),
    FinalAdmissionRule::HeaderMismatch(HeaderMismatchReason::EmptyNameHeader),
    FinalAdmissionRule::HeaderMismatch(HeaderMismatchReason::EmptyBodyName),
    FinalAdmissionRule::HeaderMismatch(HeaderMismatchReason::HeaderBodyNameMismatch),
];

/// Typed failure from final request version admission.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RequestAdmissionError {
    /// Required metadata was missing, empty, malformed, or failed the mirror check.
    HeaderMismatch(HeaderMismatchError),
    /// The mirror was valid but selected a version this surface does not support.
    UnsupportedProtocolVersion(UnsupportedProtocolVersionError),
}

impl RequestAdmissionError {
    /// Returns the precedence rule that refused this request.
    ///
    /// This is the join between a refusal and
    /// [`FINAL_ADMISSION_PRECEDENCE`]: it lets a caller — and the acceptance
    /// harness — ask *which declared rule fired* rather than inferring it from
    /// the error's shape.
    #[must_use]
    pub const fn rule(&self) -> FinalAdmissionRule {
        match self {
            Self::HeaderMismatch(error) => FinalAdmissionRule::HeaderMismatch(error.reason()),
            Self::UnsupportedProtocolVersion(_) => FinalAdmissionRule::UnsupportedProtocolVersion,
        }
    }

    /// Returns the HTTP status required by this final request-admission failure.
    #[must_use]
    pub const fn http_status(&self) -> u16 {
        match self {
            Self::HeaderMismatch(error) => error.http_status(),
            Self::UnsupportedProtocolVersion(error) => error.http_status(),
        }
    }

    /// Returns the final MCP JSON-RPC error code for this failure.
    #[must_use]
    pub const fn jsonrpc_error_code(&self) -> i32 {
        match self {
            Self::HeaderMismatch(error) => error.jsonrpc_error_code(),
            Self::UnsupportedProtocolVersion(error) => error.jsonrpc_error_code(),
        }
    }
}

/// Why final protocol-version validation rejected a request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProtocolVersionError {
    /// The required header or body field was absent, empty, malformed, or differed.
    HeaderMismatch,
    /// Header and body agreed on a well-formed value that this final surface does not support.
    UnsupportedProtocolVersion { requested: String },
}

impl ProtocolVersionError {
    /// Returns the HTTP status required for either final version failure.
    #[must_use]
    pub const fn http_status(&self) -> u16 {
        400
    }

    /// Returns the final MCP JSON-RPC error code.
    #[must_use]
    pub const fn jsonrpc_error_code(&self) -> i32 {
        match self {
            Self::HeaderMismatch => HEADER_MISMATCH_ERROR_CODE,
            Self::UnsupportedProtocolVersion { .. } => UNSUPPORTED_PROTOCOL_VERSION_ERROR_CODE,
        }
    }
}

/// Validates the final HTTP header/body protocol-version mirror.
///
/// This deliberately performs no legacy fallback. A missing or empty header,
/// a missing or empty body value, and different values all use the final
/// header-mismatch classification. Matching non-final values are classified as
/// unsupported and retain the requested value for the caller's typed error
/// data.
pub fn validate_final_protocol_version(
    header_version: Option<&str>,
    body_version: Option<&str>,
) -> Result<FinalProtocolVersion, ProtocolVersionError> {
    admit_final_request(RequestVersionMetadata {
        header_version,
        body_version,
    })
    .map(|admission| admission.protocol_version())
    .map_err(|error| match error {
        RequestAdmissionError::HeaderMismatch(_) => ProtocolVersionError::HeaderMismatch,
        RequestAdmissionError::UnsupportedProtocolVersion(error) => {
            ProtocolVersionError::UnsupportedProtocolVersion {
                requested: error.requested,
            }
        }
    })
}

/// Admits final request metadata using the required error precedence.
///
/// Header/body validity is evaluated before version support. This prevents a
/// mismatched or malformed mirror from being misclassified as an unsupported
/// version and ensures callers never select policy from an untrusted header.
pub fn admit_final_request(
    metadata: RequestVersionMetadata<'_>,
) -> Result<FinalRequestAdmission, RequestAdmissionError> {
    let header_version = metadata
        .header_version
        .ok_or(RequestAdmissionError::HeaderMismatch(HeaderMismatchError {
            reason: HeaderMismatchReason::MissingHeader,
        }))?;
    let body_version = metadata
        .body_version
        .ok_or(RequestAdmissionError::HeaderMismatch(HeaderMismatchError {
            reason: HeaderMismatchReason::MissingBodyVersion,
        }))?;

    if header_version.is_empty() {
        return Err(RequestAdmissionError::HeaderMismatch(HeaderMismatchError {
            reason: HeaderMismatchReason::EmptyHeader,
        }));
    }
    if body_version.is_empty() {
        return Err(RequestAdmissionError::HeaderMismatch(HeaderMismatchError {
            reason: HeaderMismatchReason::EmptyBodyVersion,
        }));
    }
    if header_version != body_version {
        return Err(RequestAdmissionError::HeaderMismatch(HeaderMismatchError {
            reason: HeaderMismatchReason::HeaderBodyVersionMismatch,
        }));
    }
    if header_version != FINAL_PROTOCOL_VERSION {
        return Err(RequestAdmissionError::UnsupportedProtocolVersion(
            UnsupportedProtocolVersionError {
                requested: header_version.to_owned(),
            },
        ));
    }

    Ok(FinalRequestAdmission {
        version: FinalProtocolVersion,
    })
}

/// Admits the standard final HTTP header mirrors for a request.
///
/// Protocol-version admission occurs first. Once that mirror selects the
/// final protocol, `Mcp-Method` must exactly match the JSON-RPC method. The
/// `Mcp-Name` mirror is then required for name- or identifier-addressed
/// methods, including official Tasks operations; no extra header is inferred
/// for other methods.
pub fn admit_final_http_request(
    metadata: FinalHttpRequestMetadata<'_>,
) -> Result<FinalRequestAdmission, RequestAdmissionError> {
    let admission = admit_final_request(metadata.version)?;
    let method = exact_nonempty_mirror(
        metadata.header_method,
        metadata.body_method,
        HeaderMismatchReason::MissingMethodHeader,
        HeaderMismatchReason::MissingBodyMethod,
        HeaderMismatchReason::EmptyMethodHeader,
        HeaderMismatchReason::EmptyBodyMethod,
        HeaderMismatchReason::HeaderBodyMethodMismatch,
    )?;
    if requires_mcp_name(method) {
        let _ = exact_nonempty_mirror(
            metadata.header_name,
            metadata.body_name,
            HeaderMismatchReason::MissingNameHeader,
            HeaderMismatchReason::MissingBodyName,
            HeaderMismatchReason::EmptyNameHeader,
            HeaderMismatchReason::EmptyBodyName,
            HeaderMismatchReason::HeaderBodyNameMismatch,
        )?;
    }
    Ok(admission)
}

fn exact_nonempty_mirror<'a>(
    header: Option<&'a str>,
    body: Option<&'a str>,
    missing_header: HeaderMismatchReason,
    missing_body: HeaderMismatchReason,
    empty_header: HeaderMismatchReason,
    empty_body: HeaderMismatchReason,
    mismatch: HeaderMismatchReason,
) -> Result<&'a str, RequestAdmissionError> {
    let header = header.ok_or(RequestAdmissionError::HeaderMismatch(HeaderMismatchError {
        reason: missing_header,
    }))?;
    let body = body.ok_or(RequestAdmissionError::HeaderMismatch(HeaderMismatchError {
        reason: missing_body,
    }))?;
    if header.is_empty() {
        return Err(RequestAdmissionError::HeaderMismatch(HeaderMismatchError {
            reason: empty_header,
        }));
    }
    if body.is_empty() {
        return Err(RequestAdmissionError::HeaderMismatch(HeaderMismatchError {
            reason: empty_body,
        }));
    }
    if header != body {
        return Err(RequestAdmissionError::HeaderMismatch(HeaderMismatchError {
            reason: mismatch,
        }));
    }
    Ok(header)
}

fn requires_mcp_name(method: &str) -> bool {
    matches!(
        method,
        "tools/call"
            | "resources/read"
            | "prompts/get"
            | "tasks/get"
            | "tasks/update"
            | "tasks/cancel"
    )
}

#[cfg(test)]
mod tests {
    // The frozen `prt_03_a_*` / `prt_03_b_*` pairs live in the external
    // public-surface consumer at
    // `crates/fastmcp-protocol/tests/prt_03_contract.rs`. These keep the same
    // coverage as in-crate unit rehearsals under distinct names.
    //
    // Being inside a `mod` already qualifies these as
    // `protocol_version::tests::…`, so `--exact <bare name>` would not have
    // matched them. They are renamed anyway: the campaign invariant is one
    // definition per frozen ID workspace-wide, and singularity that depends on
    // a caller remembering to pass `--exact` is a property of the invocation,
    // not of the tree.
    use super::*;

    #[test]
    fn prt_03_a_unit_positive() {
        let admission = admit_final_http_request(FinalHttpRequestMetadata {
            version: RequestVersionMetadata {
                header_version: Some(FINAL_PROTOCOL_VERSION),
                body_version: Some(FINAL_PROTOCOL_VERSION),
            },
            header_method: Some("tools/call"),
            body_method: Some("tools/call"),
            header_name: Some("weather"),
            body_name: Some("weather"),
        })
        .expect("matching final standard headers and body values must be admitted");

        assert_eq!(
            admission.protocol_version().as_str(),
            FINAL_PROTOCOL_VERSION
        );
        assert_eq!(MCP_PROTOCOL_VERSION_HEADER, "MCP-Protocol-Version");
        assert_eq!(MCP_METHOD_HEADER, "Mcp-Method");
        assert_eq!(MCP_NAME_HEADER, "Mcp-Name");
    }

    #[test]
    fn prt_03_a_unit_planted_negative() {
        let body_name = Some("weather");
        let error = admit_final_http_request(FinalHttpRequestMetadata {
            version: RequestVersionMetadata {
                header_version: Some(FINAL_PROTOCOL_VERSION),
                body_version: Some(FINAL_PROTOCOL_VERSION),
            },
            header_method: Some("tools/call"),
            body_method: Some("tools/call"),
            header_name: Some("other-weather"),
            body_name,
        })
        .expect_err("changing only the name header must reject the request");

        assert_eq!(
            error,
            RequestAdmissionError::HeaderMismatch(HeaderMismatchError {
                reason: HeaderMismatchReason::HeaderBodyNameMismatch,
            })
        );
        assert_eq!(error.http_status(), 400);
        assert_eq!(error.jsonrpc_error_code(), HEADER_MISMATCH_ERROR_CODE);
        assert_eq!(body_name, Some("weather"));
    }

    #[test]
    fn official_tasks_methods_require_the_same_mcp_name_mirror() {
        for method in ["tasks/get", "tasks/update", "tasks/cancel"] {
            let admitted = admit_final_http_request(FinalHttpRequestMetadata {
                version: RequestVersionMetadata {
                    header_version: Some(FINAL_PROTOCOL_VERSION),
                    body_version: Some(FINAL_PROTOCOL_VERSION),
                },
                header_method: Some(method),
                body_method: Some(method),
                header_name: Some("task-42"),
                body_name: Some("task-42"),
            });
            assert!(
                admitted.is_ok(),
                "{method} accepts an exact task identifier mirror"
            );
        }

        let body_name = Some("task-42");
        let error = admit_final_http_request(FinalHttpRequestMetadata {
            version: RequestVersionMetadata {
                header_version: Some(FINAL_PROTOCOL_VERSION),
                body_version: Some(FINAL_PROTOCOL_VERSION),
            },
            header_method: Some("tasks/get"),
            body_method: Some("tasks/get"),
            header_name: Some("other-task"),
            body_name,
        })
        .expect_err("changing only a Tasks name mirror rejects final admission");
        assert_eq!(
            error,
            RequestAdmissionError::HeaderMismatch(HeaderMismatchError {
                reason: HeaderMismatchReason::HeaderBodyNameMismatch,
            })
        );
        assert_eq!(body_name, Some("task-42"));
    }

    #[test]
    fn matching_unsupported_version_reports_the_requested_value() {
        let error = validate_final_protocol_version(Some("2025-11-25"), Some("2025-11-25"))
            .expect_err("matching unsupported versions must not be accepted");

        assert_eq!(
            error,
            ProtocolVersionError::UnsupportedProtocolVersion {
                requested: "2025-11-25".to_owned(),
            }
        );
        assert_eq!(error.http_status(), 400);
        assert_eq!(
            error.jsonrpc_error_code(),
            UNSUPPORTED_PROTOCOL_VERSION_ERROR_CODE
        );
    }

    #[test]
    fn missing_or_empty_version_is_a_header_mismatch() {
        for (header, body) in [
            (None, Some(FINAL_PROTOCOL_VERSION)),
            (Some(FINAL_PROTOCOL_VERSION), None),
            (Some(""), Some(FINAL_PROTOCOL_VERSION)),
            (Some(FINAL_PROTOCOL_VERSION), Some("")),
        ] {
            assert_eq!(
                validate_final_protocol_version(header, body),
                Err(ProtocolVersionError::HeaderMismatch)
            );
        }
    }

    #[test]
    fn prt_03_b_unit_positive() {
        let admission = admit_final_request(RequestVersionMetadata {
            header_version: Some(FINAL_PROTOCOL_VERSION),
            body_version: Some(FINAL_PROTOCOL_VERSION),
        })
        .expect("matching supported header and body versions must admit the request");

        assert_eq!(
            admission.protocol_version().as_str(),
            FINAL_PROTOCOL_VERSION
        );
        assert_eq!(SUPPORTED_FINAL_PROTOCOL_VERSIONS, [FINAL_PROTOCOL_VERSION]);
    }

    #[test]
    fn prt_03_b_unit_planted_negative() {
        let body_version = Some(FINAL_PROTOCOL_VERSION);
        let changed_header_version = Some("2025-11-25");

        let error = admit_final_request(RequestVersionMetadata {
            header_version: changed_header_version,
            body_version,
        })
        .expect_err("changing only the header must retain header-mismatch precedence");

        assert_eq!(
            error,
            RequestAdmissionError::HeaderMismatch(HeaderMismatchError {
                reason: HeaderMismatchReason::HeaderBodyVersionMismatch,
            })
        );
        assert_eq!(error.jsonrpc_error_code(), HEADER_MISMATCH_ERROR_CODE);
        assert_eq!(error.http_status(), 400);
        assert_eq!(body_version, Some(FINAL_PROTOCOL_VERSION));
    }

    #[test]
    fn matching_unsupported_version_is_classified_after_the_mirror_check() {
        let error = admit_final_request(RequestVersionMetadata {
            header_version: Some("2025-11-25"),
            body_version: Some("2025-11-25"),
        })
        .expect_err("matching unsupported version must reject after mirror validation");

        let RequestAdmissionError::UnsupportedProtocolVersion(error) = error else {
            panic!("matching values must not use the header-mismatch error");
        };
        assert_eq!(error.requested(), "2025-11-25");
        assert_eq!(error.supported_versions(), [FINAL_PROTOCOL_VERSION]);
        assert_eq!(
            error.jsonrpc_error_code(),
            UNSUPPORTED_PROTOCOL_VERSION_ERROR_CODE
        );
        assert_eq!(error.http_status(), 400);
    }

    #[test]
    fn typed_errors_preserve_only_their_final_peer_data_shapes() {
        let mismatch = HeaderMismatchError {
            reason: HeaderMismatchReason::MissingHeader,
        };
        assert_eq!(mismatch.canonical_error_data(), None);

        let unsupported = UnsupportedProtocolVersionError {
            requested: "2025-11-25".to_owned(),
        };
        assert_eq!(
            unsupported.canonical_error_data(),
            json!({"supported": [FINAL_PROTOCOL_VERSION], "requested": "2025-11-25"})
        );

        let missing = MissingRequiredClientCapabilityError::new(json!({
            "roots": {"listChanged": true},
            "sampling": {"context": {}}
        }))
        .expect("bounded capability object is valid typed peer data");
        assert_eq!(missing.http_status(), 400);
        assert_eq!(
            missing.jsonrpc_error_code(),
            MISSING_REQUIRED_CLIENT_CAPABILITY_ERROR_CODE
        );
        assert_eq!(
            missing.canonical_error_data(),
            json!({
                "requiredCapabilities": {
                    "roots": {"listChanged": true},
                    "sampling": {"context": {}}
                }
            })
        );

        let typed_missing =
            MissingRequiredClientCapabilityError::from_client_capabilities(&ClientCapabilities {
                roots: Some(crate::RootsCapability { list_changed: true }),
                ..ClientCapabilities::default()
            })
            .expect("typed capabilities serialize as a bounded required-capabilities object");
        assert_eq!(
            typed_missing.canonical_error_data(),
            json!({"requiredCapabilities": {"roots": {"listChanged": true}}})
        );
    }
}
