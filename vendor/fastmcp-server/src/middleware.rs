//! Middleware hooks for request/response interception.
//!
//! This provides a minimal, synchronous middleware system for MCP requests.
//! Middleware can short-circuit requests, transform responses, and rewrite errors.
//!
//! # Ordering Semantics
//!
//! - `on_request` runs **in registration order** (first registered, first called).
//! - `on_response` runs **in reverse order** for middleware whose `on_request` ran.
//! - `on_error` runs **in reverse order** for middleware whose `on_request` ran.
//! - Server-generated failures before `on_request` entry (for example auth errors)
//!   may still be passed through `on_error` for the registered middleware stack.
//!
//! If a middleware returns `Respond` from `on_request`, the response is still
//! passed through `on_response` for the already-entered middleware stack.
//! If any `on_request` or `on_response` returns an error, `on_error` is invoked
//! for the entered middleware stack to allow error rewriting.

/// Encrypted terminal-result recovery for exact modern continuation retries.
pub mod continuation_replay;

pub(crate) mod seal {
    /// Cannot be named outside this crate, so only in-crate middleware can
    /// override or call [`super::Middleware::replays_router_minted_continuation`].
    #[derive(Debug, Clone, Copy)]
    pub struct Sealed;
}

use fastmcp_core::{McpContext, McpError, McpResult};
use fastmcp_protocol::JsonRpcRequest;

use std::sync::Arc;

/// Result of middleware request interception.
#[derive(Debug, Clone)]
pub enum MiddlewareDecision {
    /// Continue normal dispatch.
    Continue,
    /// Short-circuit dispatch and return this JSON value as the result.
    Respond(serde_json::Value),
}

/// Middleware hook trait for request/response interception.
///
/// This is intentionally minimal: synchronous hooks only, with simple
/// short-circuit and response transform capabilities. See the module-level
/// documentation for ordering semantics.
pub trait Middleware: Send + Sync {
    /// Invoked before routing the request.
    ///
    /// Return `Respond` to skip normal dispatch and return a custom result.
    fn on_request(
        &self,
        _ctx: &McpContext,
        _request: &JsonRpcRequest,
    ) -> McpResult<MiddlewareDecision> {
        Ok(MiddlewareDecision::Continue)
    }

    /// Invoked after a successful handler result is produced.
    ///
    /// Middleware can transform the response value (or return an error).
    fn on_response(
        &self,
        _ctx: &McpContext,
        _request: &JsonRpcRequest,
        response: serde_json::Value,
    ) -> McpResult<serde_json::Value> {
        Ok(response)
    }

    /// Invoked when a handler or middleware returns an error.
    ///
    /// Middleware may rewrite the error before it is sent to the client.
    fn on_error(&self, _ctx: &McpContext, _request: &JsonRpcRequest, error: McpError) -> McpError {
        error
    }

    /// Whether a `Respond` from this middleware may replay a router-minted
    /// `input_required` reply. Generic middleware, caches included, may only
    /// complete a final request; only the successor-recovering
    /// [`continuation_replay::ContinuationReplayMiddleware`] answers `true`.
    #[doc(hidden)]
    fn replays_router_minted_continuation(&self, _: seal::Sealed) -> bool {
        false
    }
}

impl<T> Middleware for Arc<T>
where
    T: Middleware + ?Sized,
{
    fn on_request(
        &self,
        ctx: &McpContext,
        request: &JsonRpcRequest,
    ) -> McpResult<MiddlewareDecision> {
        (**self).on_request(ctx, request)
    }

    fn on_response(
        &self,
        ctx: &McpContext,
        request: &JsonRpcRequest,
        response: serde_json::Value,
    ) -> McpResult<serde_json::Value> {
        (**self).on_response(ctx, request, response)
    }

    fn on_error(&self, ctx: &McpContext, request: &JsonRpcRequest, error: McpError) -> McpError {
        (**self).on_error(ctx, request, error)
    }

    fn replays_router_minted_continuation(&self, seal: seal::Sealed) -> bool {
        (**self).replays_router_minted_continuation(seal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::Cx;

    fn make_ctx() -> McpContext {
        McpContext::new(Cx::for_testing(), 1)
    }

    fn make_request() -> JsonRpcRequest {
        JsonRpcRequest::new("tools/call", None, 1i64)
    }

    // ── MiddlewareDecision ───────────────────────────────────────────

    #[test]
    fn middleware_decision_continue_debug() {
        let d = MiddlewareDecision::Continue;
        let debug = format!("{:?}", d);
        assert!(debug.contains("Continue"));
    }

    #[test]
    fn middleware_decision_respond_debug() {
        let d = MiddlewareDecision::Respond(serde_json::json!({"ok": true}));
        let debug = format!("{:?}", d);
        assert!(debug.contains("Respond"));
    }

    #[test]
    fn middleware_decision_clone() {
        let d = MiddlewareDecision::Respond(serde_json::json!(42));
        let cloned = d.clone();
        match cloned {
            MiddlewareDecision::Respond(v) => assert_eq!(v, 42),
            _ => panic!("expected Respond"),
        }
    }

    // ── Default trait methods ────────────────────────────────────────

    struct NoopMiddleware;
    impl Middleware for NoopMiddleware {}

    #[test]
    fn default_on_request_returns_continue() {
        let mw = NoopMiddleware;
        let ctx = make_ctx();
        let req = make_request();
        let decision = mw.on_request(&ctx, &req).unwrap();
        matches!(decision, MiddlewareDecision::Continue);
    }

    #[test]
    fn default_on_response_passes_through() {
        let mw = NoopMiddleware;
        let ctx = make_ctx();
        let req = make_request();
        let input = serde_json::json!({"data": "hello"});
        let output = mw.on_response(&ctx, &req, input.clone()).unwrap();
        assert_eq!(output, input);
    }

    #[test]
    fn default_on_error_passes_through() {
        let mw = NoopMiddleware;
        let ctx = make_ctx();
        let req = make_request();
        let err = McpError::internal_error("test error");
        let result = mw.on_error(&ctx, &req, err);
        assert!(result.message.contains("test error"));
    }

    // ── Custom middleware ─────────────────────────────────────────────

    struct BlockingMiddleware;
    impl Middleware for BlockingMiddleware {
        fn on_request(
            &self,
            _ctx: &McpContext,
            _request: &JsonRpcRequest,
        ) -> McpResult<MiddlewareDecision> {
            Ok(MiddlewareDecision::Respond(
                serde_json::json!({"blocked": true}),
            ))
        }
    }

    #[test]
    fn custom_on_request_can_short_circuit() {
        let mw = BlockingMiddleware;
        let ctx = make_ctx();
        let req = make_request();
        let decision = mw.on_request(&ctx, &req).unwrap();
        match decision {
            MiddlewareDecision::Respond(v) => assert_eq!(v["blocked"], true),
            _ => panic!("expected Respond"),
        }
    }

    struct ErrorRewritingMiddleware;
    impl Middleware for ErrorRewritingMiddleware {
        fn on_error(
            &self,
            _ctx: &McpContext,
            _request: &JsonRpcRequest,
            _error: McpError,
        ) -> McpError {
            McpError::internal_error("rewritten")
        }
    }

    #[test]
    fn custom_on_error_can_rewrite() {
        let mw = ErrorRewritingMiddleware;
        let ctx = make_ctx();
        let req = make_request();
        let original = McpError::internal_error("original");
        let rewritten = mw.on_error(&ctx, &req, original);
        assert!(rewritten.message.contains("rewritten"));
    }

    // ── Arc delegation ───────────────────────────────────────────────

    #[test]
    fn arc_middleware_delegates_on_request() {
        let mw: Arc<dyn Middleware> = Arc::new(BlockingMiddleware);
        let ctx = make_ctx();
        let req = make_request();
        let decision = mw.on_request(&ctx, &req).unwrap();
        match decision {
            MiddlewareDecision::Respond(v) => assert_eq!(v["blocked"], true),
            _ => panic!("expected Respond"),
        }
    }

    #[test]
    fn arc_middleware_delegates_on_response() {
        let mw: Arc<dyn Middleware> = Arc::new(NoopMiddleware);
        let ctx = make_ctx();
        let req = make_request();
        let input = serde_json::json!("hello");
        let output = mw.on_response(&ctx, &req, input.clone()).unwrap();
        assert_eq!(output, input);
    }

    #[test]
    fn arc_middleware_delegates_on_error() {
        let mw: Arc<dyn Middleware> = Arc::new(ErrorRewritingMiddleware);
        let ctx = make_ctx();
        let req = make_request();
        let err = McpError::internal_error("x");
        let result = mw.on_error(&ctx, &req, err);
        assert!(result.message.contains("rewritten"));
    }

    // ── Additional coverage ─────────────────────────────────────────

    struct TransformResponseMiddleware;
    impl Middleware for TransformResponseMiddleware {
        fn on_response(
            &self,
            _ctx: &McpContext,
            _request: &JsonRpcRequest,
            mut response: serde_json::Value,
        ) -> McpResult<serde_json::Value> {
            response["transformed"] = serde_json::json!(true);
            Ok(response)
        }
    }

    #[test]
    fn custom_on_response_can_transform() {
        let mw = TransformResponseMiddleware;
        let ctx = make_ctx();
        let req = make_request();
        let input = serde_json::json!({"data": 1});
        let output = mw.on_response(&ctx, &req, input).unwrap();
        assert_eq!(output["data"], 1);
        assert_eq!(output["transformed"], true);
    }

    #[test]
    fn on_request_can_return_error() {
        struct RejectMiddleware;
        impl Middleware for RejectMiddleware {
            fn on_request(
                &self,
                _ctx: &McpContext,
                _request: &JsonRpcRequest,
            ) -> McpResult<MiddlewareDecision> {
                Err(McpError::internal_error("rejected"))
            }
        }

        let mw = RejectMiddleware;
        let ctx = make_ctx();
        let req = make_request();
        let err = mw.on_request(&ctx, &req).unwrap_err();
        assert!(err.message.contains("rejected"));
    }

    #[test]
    fn on_response_can_return_error() {
        struct FailResponseMiddleware;
        impl Middleware for FailResponseMiddleware {
            fn on_response(
                &self,
                _ctx: &McpContext,
                _request: &JsonRpcRequest,
                _response: serde_json::Value,
            ) -> McpResult<serde_json::Value> {
                Err(McpError::internal_error("response-fail"))
            }
        }

        let mw = FailResponseMiddleware;
        let ctx = make_ctx();
        let req = make_request();
        let err = mw
            .on_response(&ctx, &req, serde_json::json!({}))
            .unwrap_err();
        assert!(err.message.contains("response-fail"));
    }

    #[test]
    fn middleware_decision_continue_clone() {
        let d = MiddlewareDecision::Continue;
        let cloned = d.clone();
        assert!(matches!(cloned, MiddlewareDecision::Continue));
    }

    #[test]
    fn arc_middleware_delegates_transforming_on_response() {
        let mw: Arc<dyn Middleware> = Arc::new(TransformResponseMiddleware);
        let ctx = make_ctx();
        let req = make_request();
        let input = serde_json::json!({"x": 2});
        let output = mw.on_response(&ctx, &req, input).unwrap();
        assert_eq!(output["x"], 2);
        assert_eq!(output["transformed"], true);
    }
}
