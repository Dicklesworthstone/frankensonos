//! HTTP `Expect: 100-continue` handling.
//!
//! This module provides support for the HTTP `Expect: 100-continue` mechanism
//! as defined in [RFC 7231 Section 5.1.1](https://tools.ietf.org/html/rfc7231#section-5.1.1).
//!
//! # Overview
//!
//! When a client sends a request with `Expect: 100-continue`, it is indicating
//! that it will wait for a `100 Continue` interim response before sending the
//! request body. This allows the server to:
//!
//! - Validate request headers before receiving potentially large body data
//! - Reject unauthorized requests without reading the body
//! - Check Content-Type and Content-Length before accepting uploads
//!
//! # Example
//!
//! ```ignore
//! use fastapi_http::expect::{ExpectHandler, ExpectValidation, CONTINUE_RESPONSE};
//!
//! // Check for Expect: 100-continue
//! if let Some(validation) = ExpectHandler::check_expect(&request) {
//!     // Run pre-body validation (auth, content-type, etc.)
//!     if !validate_auth(&request) {
//!         return validation.reject_unauthorized("Invalid credentials");
//!     }
//!     if !validate_content_type(&request) {
//!         return validation.reject_unsupported_media_type("Expected application/json");
//!     }
//!
//!     // Validation passed - send 100 Continue
//!     stream.write_all(CONTINUE_RESPONSE).await?;
//! }
//!
//! // Now proceed to read body and handle request
//! ```
//!
//! # Error Responses
//!
//! When pre-body validation fails, the server should NOT send `100 Continue`.
//! Instead, it should send an appropriate error response:
//!
//! - `417 Expectation Failed` - The expectation cannot be met
//! - `401 Unauthorized` - Authentication required
//! - `403 Forbidden` - Authorization failed
//! - `413 Payload Too Large` - Content-Length exceeds limits
//! - `415 Unsupported Media Type` - Content-Type not accepted
//!
//! # Wire Format
//!
//! The `100 Continue` response is a simple interim response:
//!
//! ```text
//! HTTP/1.1 100 Continue\r\n
//! \r\n
//! ```
//!
//! After sending this, the server proceeds to read the request body.

use fastapi_core::{Request, Response, ResponseBody, StatusCode};
use std::sync::Arc;

/// The raw bytes for an HTTP/1.1 100 Continue response.
///
/// This is the minimal valid 100 Continue response:
/// ```text
/// HTTP/1.1 100 Continue\r\n
/// \r\n
/// ```
pub const CONTINUE_RESPONSE: &[u8] = b"HTTP/1.1 100 Continue\r\n\r\n";

/// The Expect header value that triggers 100-continue handling.
pub const EXPECT_100_CONTINUE: &str = "100-continue";

/// Result of checking the Expect header.
#[derive(Debug, Clone)]
pub enum ExpectResult {
    /// No Expect header present - proceed normally without waiting
    NoExpectation,
    /// Expect: 100-continue present - must validate before reading body
    ExpectsContinue,
    /// Unknown expectation - should return 417 Expectation Failed
    UnknownExpectation(String),
}

/// Handler for HTTP Expect header processing.
#[derive(Debug, Clone, Default)]
pub struct ExpectHandler {
    /// Maximum Content-Length to accept (0 = unlimited)
    pub max_content_length: usize,
    /// Required Content-Type prefix (empty = any)
    pub required_content_type: Option<String>,
}

impl ExpectHandler {
    /// Create a new ExpectHandler with default settings.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the maximum Content-Length to accept.
    #[must_use]
    pub fn with_max_content_length(mut self, max: usize) -> Self {
        self.max_content_length = max;
        self
    }

    /// Set the required Content-Type prefix.
    #[must_use]
    pub fn with_required_content_type(mut self, content_type: impl Into<String>) -> Self {
        self.required_content_type = Some(content_type.into());
        self
    }

    /// Check if a request has an Expect header and what it contains.
    ///
    /// Returns:
    /// - `ExpectResult::NoExpectation` - No Expect header, proceed normally
    /// - `ExpectResult::ExpectsContinue` - Expect: 100-continue present
    /// - `ExpectResult::UnknownExpectation` - Unknown expectation value
    #[must_use]
    pub fn check_expect(request: &Request) -> ExpectResult {
        match request.headers().get("expect") {
            None => ExpectResult::NoExpectation,
            Some(value) => {
                let value_str = match std::str::from_utf8(value) {
                    Ok(s) => s.trim().to_ascii_lowercase(),
                    Err(_) => return ExpectResult::UnknownExpectation(String::new()),
                };

                let mut saw_continue = false;
                for token in value_str.split(',').map(str::trim) {
                    if token.is_empty() {
                        return ExpectResult::UnknownExpectation(value_str);
                    }
                    if token == EXPECT_100_CONTINUE {
                        saw_continue = true;
                    } else {
                        return ExpectResult::UnknownExpectation(value_str);
                    }
                }

                if saw_continue {
                    ExpectResult::ExpectsContinue
                } else {
                    ExpectResult::UnknownExpectation(value_str)
                }
            }
        }
    }

    /// Check if the request expects 100-continue.
    ///
    /// This is a convenience method that returns true only for valid
    /// `Expect: 100-continue` headers.
    #[must_use]
    pub fn expects_continue(request: &Request) -> bool {
        matches!(Self::check_expect(request), ExpectResult::ExpectsContinue)
    }

    /// Validate Content-Length against maximum limit.
    ///
    /// Returns `Ok(())` if Content-Length is within limits or not specified,
    /// or `Err(Response)` with 413 Payload Too Large if exceeded.
    pub fn validate_content_length(&self, request: &Request) -> Result<(), Response> {
        if self.max_content_length == 0 {
            return Ok(()); // No limit
        }

        if let Some(value) = request.headers().get("content-length")
            && let Ok(len_str) = std::str::from_utf8(value)
            && let Ok(len) = len_str.trim().parse::<usize>()
            && len > self.max_content_length
        {
            return Err(Self::payload_too_large(format!(
                "Content-Length {} exceeds maximum {}",
                len, self.max_content_length
            )));
        }

        Ok(())
    }

    /// Validate Content-Type against required type.
    ///
    /// Returns `Ok(())` if Content-Type matches or no requirement is set,
    /// or `Err(Response)` with 415 Unsupported Media Type if mismatched.
    pub fn validate_content_type(&self, request: &Request) -> Result<(), Response> {
        let required = match &self.required_content_type {
            Some(ct) => ct,
            None => return Ok(()),
        };

        match request.headers().get("content-type") {
            None => Err(Self::unsupported_media_type(format!(
                "Content-Type required: {required}"
            ))),
            Some(value) => {
                let content_type = std::str::from_utf8(value)
                    .map(|s| s.trim().to_ascii_lowercase())
                    .unwrap_or_default();

                if content_type.starts_with(&required.to_ascii_lowercase()) {
                    Ok(())
                } else {
                    Err(Self::unsupported_media_type(format!(
                        "Expected Content-Type: {required}, got: {content_type}"
                    )))
                }
            }
        }
    }

    /// Run all configured validations.
    ///
    /// Returns `Ok(())` if all validations pass, or the first error response.
    pub fn validate_all(&self, request: &Request) -> Result<(), Response> {
        self.validate_content_length(request)?;
        self.validate_content_type(request)?;
        Ok(())
    }

    /// Create a 417 Expectation Failed response.
    #[must_use]
    pub fn expectation_failed(detail: impl Into<String>) -> Response {
        let detail = detail.into();
        let body = format!("417 Expectation Failed: {detail}");
        // StatusCode::EXPECTATION_FAILED is 417
        Response::with_status(StatusCode::from_u16(417))
            .header("content-type", b"text/plain; charset=utf-8".to_vec())
            .header("connection", b"close".to_vec())
            .body(ResponseBody::Bytes(body.into_bytes()))
    }

    /// Create a 401 Unauthorized response.
    #[must_use]
    pub fn unauthorized(detail: impl Into<String>) -> Response {
        let detail = detail.into();
        let body = format!("401 Unauthorized: {detail}");
        Response::with_status(StatusCode::UNAUTHORIZED)
            .header("content-type", b"text/plain; charset=utf-8".to_vec())
            .header("connection", b"close".to_vec())
            .body(ResponseBody::Bytes(body.into_bytes()))
    }

    /// Create a 403 Forbidden response.
    #[must_use]
    pub fn forbidden(detail: impl Into<String>) -> Response {
        let detail = detail.into();
        let body = format!("403 Forbidden: {detail}");
        Response::with_status(StatusCode::FORBIDDEN)
            .header("content-type", b"text/plain; charset=utf-8".to_vec())
            .header("connection", b"close".to_vec())
            .body(ResponseBody::Bytes(body.into_bytes()))
    }

    /// Create a 413 Payload Too Large response.
    #[must_use]
    pub fn payload_too_large(detail: impl Into<String>) -> Response {
        let detail = detail.into();
        let body = format!("413 Payload Too Large: {detail}");
        Response::with_status(StatusCode::PAYLOAD_TOO_LARGE)
            .header("content-type", b"text/plain; charset=utf-8".to_vec())
            .header("connection", b"close".to_vec())
            .body(ResponseBody::Bytes(body.into_bytes()))
    }

    /// Create a 415 Unsupported Media Type response.
    #[must_use]
    pub fn unsupported_media_type(detail: impl Into<String>) -> Response {
        let detail = detail.into();
        let body = format!("415 Unsupported Media Type: {detail}");
        Response::with_status(StatusCode::UNSUPPORTED_MEDIA_TYPE)
            .header("content-type", b"text/plain; charset=utf-8".to_vec())
            .header("connection", b"close".to_vec())
            .body(ResponseBody::Bytes(body.into_bytes()))
    }
}

/// Trait for pre-body validation hooks.
///
/// Implement this trait to add custom validation that runs before
/// sending 100 Continue and reading the request body.
pub trait PreBodyValidator: Send + Sync {
    /// Validate the request headers before body is read.
    ///
    /// Returns `Ok(())` if validation passes, or `Err(Response)` with
    /// an appropriate error response if validation fails.
    fn validate(&self, request: &Request) -> Result<(), Response>;

    /// Optional name for debugging/logging.
    fn name(&self) -> &'static str {
        "PreBodyValidator"
    }
}

/// A collection of pre-body validators.
#[derive(Default, Clone)]
pub struct PreBodyValidators {
    validators: Vec<Arc<dyn PreBodyValidator>>,
}

impl std::fmt::Debug for PreBodyValidators {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreBodyValidators")
            .field("len", &self.validators.len())
            .field(
                "validators",
                &self.validators.iter().map(|v| v.name()).collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl PreBodyValidators {
    /// Create a new empty validator collection.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a validator to the collection.
    pub fn add<V: PreBodyValidator + 'static>(&mut self, validator: V) {
        self.validators.push(Arc::new(validator));
    }

    /// Add a validator and return self for chaining.
    #[must_use]
    pub fn with<V: PreBodyValidator + 'static>(mut self, validator: V) -> Self {
        self.add(validator);
        self
    }

    /// Run all validators in order.
    ///
    /// Returns `Ok(())` if all pass, or the first error response.
    pub fn validate_all(&self, request: &Request) -> Result<(), Response> {
        for validator in &self.validators {
            validator.validate(request)?;
        }
        Ok(())
    }

    /// Returns true if there are no validators.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.validators.is_empty()
    }

    /// Returns the number of validators.
    #[must_use]
    pub fn len(&self) -> usize {
        self.validators.len()
    }
}

/// A simple function-based pre-body validator.
pub struct FnValidator<F> {
    name: &'static str,
    validate_fn: F,
}

impl<F> FnValidator<F>
where
    F: Fn(&Request) -> Result<(), Response> + Send + Sync,
{
    /// Create a new function validator.
    pub fn new(name: &'static str, validate_fn: F) -> Self {
        Self { name, validate_fn }
    }
}

impl<F> PreBodyValidator for FnValidator<F>
where
    F: Fn(&Request) -> Result<(), Response> + Send + Sync,
{
    fn validate(&self, request: &Request) -> Result<(), Response> {
        (self.validate_fn)(request)
    }

    fn name(&self) -> &'static str {
        self.name
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use fastapi_core::Method;

    fn request_with_expect(value: &str) -> Request {
        let mut req = Request::new(Method::Post, "/upload");
        req.headers_mut()
            .insert("expect".to_string(), value.as_bytes().to_vec());
        req
    }

    fn request_with_headers(headers: &[(&str, &str)]) -> Request {
        let mut req = Request::new(Method::Post, "/upload");
        for (name, value) in headers {
            req.headers_mut()
                .insert(name.to_string(), value.as_bytes().to_vec());
        }
        req
    }

    #[test]
    fn check_expect_none() {
        let req = Request::new(Method::Get, "/");
        assert!(matches!(
            ExpectHandler::check_expect(&req),
            ExpectResult::NoExpectation
        ));
    }

    #[test]
    fn check_expect_100_continue() {
        let req = request_with_expect("100-continue");
        assert!(matches!(
            ExpectHandler::check_expect(&req),
            ExpectResult::ExpectsContinue
        ));
    }

    #[test]
    fn check_expect_100_continue_case_insensitive() {
        let req = request_with_expect("100-Continue");
        assert!(matches!(
            ExpectHandler::check_expect(&req),
            ExpectResult::ExpectsContinue
        ));

        let req = request_with_expect("100-CONTINUE");
        assert!(matches!(
            ExpectHandler::check_expect(&req),
            ExpectResult::ExpectsContinue
        ));
    }

    #[test]
    fn check_expect_100_continue_token_list() {
        let req = request_with_expect("100-continue, 100-continue");
        assert!(matches!(
            ExpectHandler::check_expect(&req),
            ExpectResult::ExpectsContinue
        ));
    }

    #[test]
    fn check_expect_unknown() {
        let req = request_with_expect("something-else");
        let result = ExpectHandler::check_expect(&req);
        assert!(matches!(result, ExpectResult::UnknownExpectation(_)));
        if let ExpectResult::UnknownExpectation(val) = result {
            assert_eq!(val, "something-else");
        }
    }

    #[test]
    fn expects_continue_helper() {
        let req_yes = request_with_expect("100-continue");
        assert!(ExpectHandler::expects_continue(&req_yes));

        let req_no = Request::new(Method::Get, "/");
        assert!(!ExpectHandler::expects_continue(&req_no));
    }

    #[test]
    fn check_expect_mixed_token_list_is_unknown() {
        let req = request_with_expect("100-continue, custom");
        let result = ExpectHandler::check_expect(&req);
        assert!(matches!(result, ExpectResult::UnknownExpectation(_)));
        if let ExpectResult::UnknownExpectation(val) = result {
            assert_eq!(val, "100-continue, custom");
        }
    }

    #[test]
    fn check_expect_empty_token_is_unknown() {
        let req = request_with_expect("100-continue,");
        let result = ExpectHandler::check_expect(&req);
        assert!(matches!(result, ExpectResult::UnknownExpectation(_)));
        if let ExpectResult::UnknownExpectation(val) = result {
            assert_eq!(val, "100-continue,");
        }
    }

    #[test]
    fn validate_content_length_no_limit() {
        let handler = ExpectHandler::new();
        let req = request_with_headers(&[("content-length", "1000000")]);
        assert!(handler.validate_content_length(&req).is_ok());
    }

    #[test]
    fn validate_content_length_within_limit() {
        let handler = ExpectHandler::new().with_max_content_length(1024);
        let req = request_with_headers(&[("content-length", "500")]);
        assert!(handler.validate_content_length(&req).is_ok());
    }

    #[test]
    fn validate_content_length_exceeds_limit() {
        let handler = ExpectHandler::new().with_max_content_length(1024);
        let req = request_with_headers(&[("content-length", "2048")]);
        let result = handler.validate_content_length(&req);
        assert!(result.is_err());
        let response = result.unwrap_err();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[test]
    fn validate_content_type_no_requirement() {
        let handler = ExpectHandler::new();
        let req = request_with_headers(&[("content-type", "text/plain")]);
        assert!(handler.validate_content_type(&req).is_ok());
    }

    #[test]
    fn validate_content_type_matches() {
        let handler = ExpectHandler::new().with_required_content_type("application/json");
        let req = request_with_headers(&[("content-type", "application/json; charset=utf-8")]);
        assert!(handler.validate_content_type(&req).is_ok());
    }

    #[test]
    fn validate_content_type_missing() {
        let handler = ExpectHandler::new().with_required_content_type("application/json");
        let req = Request::new(Method::Post, "/upload");
        let result = handler.validate_content_type(&req);
        assert!(result.is_err());
        let response = result.unwrap_err();
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    #[test]
    fn validate_content_type_mismatch() {
        let handler = ExpectHandler::new().with_required_content_type("application/json");
        let req = request_with_headers(&[("content-type", "text/plain")]);
        let result = handler.validate_content_type(&req);
        assert!(result.is_err());
        let response = result.unwrap_err();
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    #[test]
    fn validate_all_passes() {
        let handler = ExpectHandler::new()
            .with_max_content_length(1024)
            .with_required_content_type("application/json");
        let req = request_with_headers(&[
            ("content-length", "100"),
            ("content-type", "application/json"),
        ]);
        assert!(handler.validate_all(&req).is_ok());
    }

    #[test]
    fn validate_all_fails_on_first_error() {
        let handler = ExpectHandler::new()
            .with_max_content_length(100)
            .with_required_content_type("application/json");
        let req = request_with_headers(&[
            ("content-length", "1000"),     // Exceeds limit
            ("content-type", "text/plain"), // Wrong type
        ]);
        let result = handler.validate_all(&req);
        assert!(result.is_err());
        // Should fail on content-length first
        let response = result.unwrap_err();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[test]
    fn error_responses() {
        let resp = ExpectHandler::expectation_failed("test");
        assert_eq!(resp.status().as_u16(), 417);

        let resp = ExpectHandler::unauthorized("test");
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        let resp = ExpectHandler::forbidden("test");
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        let resp = ExpectHandler::payload_too_large("test");
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);

        let resp = ExpectHandler::unsupported_media_type("test");
        assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    #[test]
    fn continue_response_format() {
        let expected = b"HTTP/1.1 100 Continue\r\n\r\n";
        assert_eq!(CONTINUE_RESPONSE, expected);
    }

    #[test]
    fn pre_body_validators() {
        let mut validators = PreBodyValidators::new();
        assert!(validators.is_empty());
        assert_eq!(validators.len(), 0);

        // Add a validator that checks for Authorization header
        validators.add(FnValidator::new("auth_check", |req: &Request| {
            if req.headers().get("authorization").is_some() {
                Ok(())
            } else {
                Err(ExpectHandler::unauthorized("Missing Authorization header"))
            }
        }));

        assert!(!validators.is_empty());
        assert_eq!(validators.len(), 1);

        // Test with missing auth
        let req_no_auth = Request::new(Method::Post, "/upload");
        let result = validators.validate_all(&req_no_auth);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().status(), StatusCode::UNAUTHORIZED);

        // Test with auth
        let req_with_auth = request_with_headers(&[("authorization", "Bearer token")]);
        assert!(validators.validate_all(&req_with_auth).is_ok());
    }

    #[test]
    fn pre_body_validators_chain() {
        let validators = PreBodyValidators::new()
            .with(FnValidator::new("auth", |req: &Request| {
                if req.headers().get("authorization").is_some() {
                    Ok(())
                } else {
                    Err(ExpectHandler::unauthorized("Missing auth"))
                }
            }))
            .with(FnValidator::new("content_type", |req: &Request| {
                if let Some(ct) = req.headers().get("content-type")
                    && ct.starts_with(b"application/json")
                {
                    return Ok(());
                }
                Err(ExpectHandler::unsupported_media_type("Expected JSON"))
            }));

        assert_eq!(validators.len(), 2);

        // Both pass
        let req = request_with_headers(&[
            ("authorization", "Bearer token"),
            ("content-type", "application/json"),
        ]);
        assert!(validators.validate_all(&req).is_ok());

        // First fails
        let req = request_with_headers(&[("content-type", "application/json")]);
        let result = validators.validate_all(&req);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().status(), StatusCode::UNAUTHORIZED);
    }
}
