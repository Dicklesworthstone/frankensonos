//! Bounded, host-driven orchestration for final-protocol sampling tool loops.
//!
//! This is not a model client, tool executor, or source of authorization. The
//! host admits the peer's sampling capabilities, approves each disclosure and
//! executes tools in its own request context. This controller retains the exact
//! final wire types and validates a whole tool batch before exposing any calls.
//! It never retries a model request or a tool side effect automatically.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::{self, Write};

use serde::Serialize;
use serde_json::Value;

use crate::common_types::SamplingContentBlock;
use crate::{
    AdmittedSchema, FinalCreateMessageResult, FinalEmbeddedCreateMessageParams,
    FinalSamplingMessage, FinalSamplingMessageContent, FinalToolChoiceMode, RawJsonTopLevel,
    Role, admit_final_schema, admit_raw_json_document,
};

/// Immutable local work limits. These restrict orchestration, not model billing
/// or the duration/side effects of a host's tool implementation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SamplingToolLoopLimits {
    max_rounds: usize,
    max_tool_calls: usize,
    byte_ceiling: usize,
}

impl SamplingToolLoopLimits {
    /// Constructs limits within absolute ceilings of 1,024 model rounds, 4,096
    /// total tool calls, and 16 MiB per retained request/result representation.
    /// Zero tool calls is useful for a text/image/audio-only conversation.
    pub fn new(
        max_rounds: usize,
        max_tool_calls: usize,
        byte_ceiling: usize,
    ) -> Result<Self, SamplingToolLoopError> {
        if !(1..=1_024).contains(&max_rounds)
            || max_tool_calls > 4_096
            || !(1..=16 * 1024 * 1024).contains(&byte_ceiling)
        {
            return Err(SamplingToolLoopError::InvalidLimits);
        }
        Ok(Self { max_rounds, max_tool_calls, byte_ceiling })
    }
}

impl Default for SamplingToolLoopLimits {
    fn default() -> Self {
        Self { max_rounds: 16, max_tool_calls: 128, byte_ceiling: 4 * 1024 * 1024 }
    }
}

/// Sanitized orchestration errors. No model text, arguments, schema, tool
/// names/IDs, credentials, or raw validator diagnostics are retained.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SamplingToolLoopError {
    InvalidLimits,
    InvalidRequest,
    InvalidSchema,
    InvalidHistory,
    InvalidResponse,
    InvalidToolResults,
    UnknownTool,
    InvalidToolInput,
    InvalidToolOutput,
    RepeatedToolId,
    ToolChoiceViolation,
    RoundLimit,
    ToolCallLimit,
    ByteLimit,
    WrongPhase,
    Closed,
}

impl fmt::Display for SamplingToolLoopError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sampling tool loop: {self:?}")
    }
}

impl std::error::Error for SamplingToolLoopError {}

/// The next host action after admitting one model response.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SamplingToolLoopStep {
    /// Review and execute the calls exposed by `pending_tool_calls`.
    Tools { count: usize },
    /// Read the unmodified model response using `result`.
    Complete,
}

struct ToolSchemas {
    input: AdmittedSchema,
    output: Option<AdmittedSchema>,
}

enum Phase {
    Ready,
    Tools(Vec<(String, String)>),
    Complete(Box<FinalCreateMessageResult>),
    Closed,
}

/// A final sampling conversation with bounded rounds, exact tool correlation,
/// and atomic batch admission. Intentionally not Clone: an executing host must
/// not accidentally fork a pending side effect into multiple orchestration owners.
///
/// The initial history must have balanced tool-use/result pairs. Tool IDs are
/// never reused within this controller, including IDs in the initial history.
/// All local admission failures leave the state unchanged and correctable;
/// after an uncertain external execution the host must `close`, not replay it.
/// Schema checking uses the existing shared admission/validation service and
/// makes no stronger Draft 2020-12 support claim than that service.
pub struct SamplingToolLoop {
    request: Option<FinalEmbeddedCreateMessageParams>,
    schemas: BTreeMap<String, ToolSchemas>,
    used_ids: BTreeSet<String>,
    phase: Phase,
    rounds: usize,
    limits: SamplingToolLoopLimits,
}

impl fmt::Debug for SamplingToolLoop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SamplingToolLoop")
            .field("rounds", &self.rounds)
            .field("tool_calls", &self.used_ids.len())
            .field("closed", &matches!(self.phase, Phase::Closed))
            .finish_non_exhaustive()
    }
}

impl SamplingToolLoop {
    /// Admits the complete request, schemas and conversation before any model
    /// call. Wire ingress must still perform duplicate-aware decoding before
    /// constructing these typed values; lost duplicate members cannot be recovered.
    pub fn new(
        request: FinalEmbeddedCreateMessageParams,
        limits: SamplingToolLoopLimits,
    ) -> Result<Self, SamplingToolLoopError> {
        bounded_wire(&request, limits.byte_ceiling)?;
        if request.messages.is_empty()
            || request.temperature.is_some_and(|value| !value.is_finite())
        {
            return Err(SamplingToolLoopError::InvalidRequest);
        }
        let mut schemas = BTreeMap::new();
        for tool in request.tools.iter().flatten() {
            if tool.name.is_empty() || schemas.contains_key(&tool.name) {
                return Err(SamplingToolLoopError::InvalidRequest);
            }
            if tool.input_schema.get("type").and_then(Value::as_str) != Some("object")
                || tool.output_schema.as_ref().is_some_and(|schema| !schema.is_object())
            {
                return Err(SamplingToolLoopError::InvalidSchema);
            }
            let input = admit_final_schema(tool.input_schema.clone())
                .map_err(|_| SamplingToolLoopError::InvalidSchema)?;
            let output = tool.output_schema.clone().map(admit_final_schema).transpose()
                .map_err(|_| SamplingToolLoopError::InvalidSchema)?;
            schemas.insert(tool.name.clone(), ToolSchemas { input, output });
        }
        if choice(&request) == Some(FinalToolChoiceMode::Required)
            && (schemas.is_empty() || limits.max_tool_calls == 0)
        {
            return Err(SamplingToolLoopError::ToolChoiceViolation);
        }
        let mut used_ids = BTreeSet::new();
        let mut pending = Vec::new();
        for message in &request.messages {
            if pending.is_empty() {
                if blocks(&message.content).iter().any(|block| {
                    matches!(block, SamplingContentBlock::ToolResult { .. })
                }) {
                    return Err(SamplingToolLoopError::InvalidHistory);
                }
                let calls = admit_calls(&message.content, &schemas, &used_ids)?;
                if !calls.is_empty() && message.role != Role::Assistant {
                    return Err(SamplingToolLoopError::InvalidHistory);
                }
                if used_ids.len().saturating_add(calls.len()) > limits.max_tool_calls {
                    return Err(SamplingToolLoopError::ToolCallLimit);
                }
                used_ids.extend(calls.iter().map(|(id, _)| id.clone()));
                pending = calls;
            } else {
                if message.role != Role::User {
                    return Err(SamplingToolLoopError::InvalidHistory);
                }
                admit_results(blocks(&message.content), &pending, &schemas)?;
                pending.clear();
            }
        }
        if !pending.is_empty() {
            return Err(SamplingToolLoopError::InvalidHistory);
        }
        Ok(Self { request: Some(request), schemas, used_ids, phase: Phase::Ready, rounds: 0, limits })
    }

    /// The exact next request. None means input is pending, complete, or closed.
    /// This borrow conveys neither capability admission nor consent to send it.
    pub fn request(&self) -> Option<&FinalEmbeddedCreateMessageParams> {
        matches!(self.phase, Phase::Ready).then_some(self.request.as_ref()).flatten()
    }

    /// Number of model responses accepted by this controller.
    pub fn round_count(&self) -> usize { self.rounds }

    /// Total admitted calls, including balanced calls in initial history.
    pub fn tool_call_count(&self) -> usize { self.used_ids.len() }

    /// Admits a whole assistant response before exposing any tool for execution.
    /// Unknown stop reasons remain forward-open. `toolUse` without calls is
    /// rejected, but a response containing calls does not require that optional hint.
    pub fn accept_response(
        &mut self,
        response: FinalCreateMessageResult,
    ) -> Result<SamplingToolLoopStep, SamplingToolLoopError> {
        self.require_ready()?;
        if self.rounds >= self.limits.max_rounds {
            return Err(SamplingToolLoopError::RoundLimit);
        }
        bounded_wire(&response, self.limits.byte_ceiling)?;
        if response.role != Role::Assistant || blocks(&response.content).is_empty() {
            return Err(SamplingToolLoopError::InvalidResponse);
        }
        let calls = admit_calls(&response.content, &self.schemas, &self.used_ids)?;
        let request = self.request.as_ref().ok_or(SamplingToolLoopError::Closed)?;
        if (calls.is_empty() && choice(request) == Some(FinalToolChoiceMode::Required))
            || (!calls.is_empty() && choice(request) == Some(FinalToolChoiceMode::None))
        {
            return Err(SamplingToolLoopError::ToolChoiceViolation);
        }
        if calls.is_empty() {
            if response.stop_reason.as_deref() == Some("toolUse") {
                return Err(SamplingToolLoopError::InvalidResponse);
            }
            self.rounds += 1;
            self.phase = Phase::Complete(Box::new(response));
            return Ok(SamplingToolLoopStep::Complete);
        }
        // Refuse before tool execution when no model round remains to consume
        // the results. The host can instead request a final non-tool response.
        if self.rounds + 1 >= self.limits.max_rounds {
            return Err(SamplingToolLoopError::RoundLimit);
        }
        if self.used_ids.len().saturating_add(calls.len()) > self.limits.max_tool_calls {
            return Err(SamplingToolLoopError::ToolCallLimit);
        }
        let mut next = request.clone();
        next.messages.push(FinalSamplingMessage {
            role: response.role, content: response.content, meta: response.meta,
        });
        bounded_wire(&next, self.limits.byte_ceiling)?;
        let count = calls.len();
        self.request = Some(next);
        self.used_ids.extend(calls.iter().map(|(id, _)| id.clone()));
        self.rounds += 1;
        self.phase = Phase::Tools(calls);
        Ok(SamplingToolLoopStep::Tools { count })
    }

    /// Exact admitted tool-use blocks, in model order. Text, audio, image and
    /// metadata remain in the retained assistant message without being flattened.
    pub fn pending_tool_calls(&self) -> impl Iterator<Item = &SamplingContentBlock> {
        let content = if matches!(self.phase, Phase::Tools(_)) {
            self.request.as_ref().and_then(|r| r.messages.last()).map(|m| &m.content)
        } else { None };
        content.into_iter().flat_map(blocks).filter(|block| {
            matches!(block, SamplingContentBlock::ToolUse { .. })
        })
    }

    /// Appends one user message consisting solely of results for the entire
    /// pending batch. Missing, duplicate, foreign and mixed-content results are
    /// refused atomically. Host completion order need not equal model order.
    /// Explicit JSON null structured content is retained, not treated as absent.
    pub fn submit_tool_results(
        &mut self,
        results: Vec<SamplingContentBlock>,
    ) -> Result<(), SamplingToolLoopError> {
        let pending = match &self.phase {
            Phase::Tools(pending) => pending,
            Phase::Closed => return Err(SamplingToolLoopError::Closed),
            _ => return Err(SamplingToolLoopError::WrongPhase),
        };
        bounded_wire(&results, self.limits.byte_ceiling)?;
        admit_results(&results, pending, &self.schemas)?;
        let mut next = self.request.as_ref().ok_or(SamplingToolLoopError::Closed)?.clone();
        next.messages.push(FinalSamplingMessage {
            role: Role::User, content: FinalSamplingMessageContent::Blocks(results), meta: None,
        });
        bounded_wire(&next, self.limits.byte_ceiling)?;
        self.request = Some(next);
        self.phase = Phase::Ready;
        Ok(())
    }

    /// The complete model response, without changing its content shape or hints.
    pub fn result(&self) -> Option<&FinalCreateMessageResult> {
        match &self.phase { Phase::Complete(result) => Some(result), _ => None }
    }

    /// Retires this owner and releases retained conversation data. This does not
    /// roll back or cancel external effects; the host owns their cancellation.
    pub fn close(&mut self) {
        self.phase = Phase::Closed;
        self.request = None;
        self.schemas.clear();
        self.used_ids.clear();
    }

    fn require_ready(&self) -> Result<(), SamplingToolLoopError> {
        match self.phase {
            Phase::Ready => Ok(()),
            Phase::Closed => Err(SamplingToolLoopError::Closed),
            _ => Err(SamplingToolLoopError::WrongPhase),
        }
    }
}

fn choice(request: &FinalEmbeddedCreateMessageParams) -> Option<FinalToolChoiceMode> {
    request.tool_choice.as_ref().and_then(|choice| choice.mode)
}

fn blocks(content: &FinalSamplingMessageContent) -> &[SamplingContentBlock] {
    match content {
        FinalSamplingMessageContent::Block(block) => std::slice::from_ref(block),
        FinalSamplingMessageContent::Blocks(blocks) => blocks,
    }
}

fn admit_calls(
    content: &FinalSamplingMessageContent,
    schemas: &BTreeMap<String, ToolSchemas>,
    used: &BTreeSet<String>,
) -> Result<Vec<(String, String)>, SamplingToolLoopError> {
    let mut ids = BTreeSet::new();
    let mut calls = Vec::new();
    for block in blocks(content) {
        match block {
            SamplingContentBlock::ToolUse { id, name, input, .. } => {
                if id.is_empty() || used.contains(id) || !ids.insert(id) {
                    return Err(SamplingToolLoopError::RepeatedToolId);
                }
                let schema = schemas.get(name).ok_or(SamplingToolLoopError::UnknownTool)?;
                schema.input.validate(&Value::Object(input.clone()))
                    .map_err(|_| SamplingToolLoopError::InvalidToolInput)?;
                calls.push((id.clone(), name.clone()));
            }
            SamplingContentBlock::ToolResult { .. } => {
                return Err(SamplingToolLoopError::InvalidResponse);
            }
            _ => {}
        }
    }
    Ok(calls)
}

fn admit_results(
    results: &[SamplingContentBlock],
    pending: &[(String, String)],
    schemas: &BTreeMap<String, ToolSchemas>,
) -> Result<(), SamplingToolLoopError> {
    if results.len() != pending.len() || results.is_empty() {
        return Err(SamplingToolLoopError::InvalidToolResults);
    }
    let mut seen = BTreeSet::new();
    for result in results {
        let SamplingContentBlock::ToolResult { tool_use_id, structured_content, is_error, .. } = result
        else { return Err(SamplingToolLoopError::InvalidToolResults); };
        let (_, name) = pending.iter().find(|(id, _)| id == tool_use_id)
            .ok_or(SamplingToolLoopError::InvalidToolResults)?;
        if !seen.insert(tool_use_id) {
            return Err(SamplingToolLoopError::InvalidToolResults);
        }
        let schema = schemas.get(name).ok_or(SamplingToolLoopError::UnknownTool)?;
        if *is_error != Some(true) {
            if let Some(output) = &schema.output {
                let value = structured_content.as_ref().ok_or(SamplingToolLoopError::InvalidToolOutput)?;
                output.validate(value).map_err(|_| SamplingToolLoopError::InvalidToolOutput)?;
            }
        }
    }
    Ok(())
}

// Never serialize an unbounded intermediate String just to measure its size.
fn bounded_wire(value: &impl Serialize, max: usize) -> Result<(), SamplingToolLoopError> {
    struct Buffer { bytes: Vec<u8>, max: usize, exceeded: bool }
    impl Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.max.saturating_sub(self.bytes.len()) {
                self.exceeded = true;
                return Err(io::Error::other("sampling byte limit"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }
    let mut buffer = Buffer { bytes: Vec::new(), max, exceeded: false };
    if serde_json::to_writer(&mut buffer, value).is_err() {
        return Err(if buffer.exceeded { SamplingToolLoopError::ByteLimit }
            else { SamplingToolLoopError::InvalidRequest });
    }
    admit_raw_json_document(&buffer.bytes, max, RawJsonTopLevel::AnyValue)
        .map_err(|_| SamplingToolLoopError::InvalidRequest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request() -> FinalEmbeddedCreateMessageParams {
        serde_json::from_value(json!({
            "messages":[{"role":"user","content":{"type":"text","text":"weather"}}],
            "maxTokens":100,
            "tools":[{"name":"weather","inputSchema":{"type":"object",
                "properties":{"city":{"type":"string"}},"required":["city"]}}],
            "metadata":{"private":"keep"}
        })).unwrap()
    }
    fn response(content: Value) -> FinalCreateMessageResult {
        serde_json::from_value(json!({"role":"assistant","model":"model","content":content})).unwrap()
    }
    fn call(id: &str) -> Value {
        json!({"type":"tool_use","id":id,"name":"weather","input":{"city":"Paris"}})
    }
    fn answer(id: &str) -> SamplingContentBlock {
        serde_json::from_value(json!({"type":"tool_result","toolUseId":id,
            "content":[{"type":"text","text":"sunny"}],"structuredContent":null})).unwrap()
    }
    fn ready() -> SamplingToolLoop { SamplingToolLoop::new(request(), SamplingToolLoopLimits::default()).unwrap() }

    #[test]
    fn two_round_sampling_preserves_content_metadata_and_null() {
        let mut run = ready();
        let generated = response(json!([{"type":"text","text":"checking"},call("a"),call("b")]));
        let expected = generated.content.clone();
        assert_eq!(run.accept_response(generated).unwrap(), SamplingToolLoopStep::Tools { count: 2 });
        assert!(run.request().is_none());
        assert_eq!(run.pending_tool_calls().count(), 2);
        run.submit_tool_results(vec![answer("b"),answer("a")]).unwrap();
        let next = run.request().unwrap();
        assert_eq!(next.messages[1].content, expected);
        assert_eq!(next.metadata.as_ref().unwrap()["private"], "keep");
        let wire = serde_json::to_value(next).unwrap();
        assert!(wire["messages"][2]["content"][0]["structuredContent"].is_null());
        assert!(wire["messages"][2]["content"][0].get("structuredContent").is_some());
        let mut final_reply = response(json!({"type":"text","text":"done"}));
        final_reply.stop_reason = Some("futureProviderReason".to_owned());
        assert_eq!(run.accept_response(final_reply.clone()).unwrap(), SamplingToolLoopStep::Complete);
        assert_eq!(run.result(), Some(&final_reply));
        assert_eq!(run.round_count(), 2);
        assert!(run.request().is_none());
    }

    #[test]
    fn invalid_batch_exposes_no_partial_calls_and_keeps_request_unchanged() {
        for content in [json!([call("a"),call("a")]),
            json!([call("a"),{"type":"tool_use","id":"b","name":"unknown","input":{}}]),
            json!([call("a"),{"type":"tool_use","id":"b","name":"weather","input":{"city":7}}])]
        {
            let mut run = ready();
            let before = serde_json::to_value(run.request().unwrap()).unwrap();
            assert!(run.accept_response(response(content)).is_err());
            assert_eq!(serde_json::to_value(run.request().unwrap()).unwrap(), before);
            assert_eq!(run.pending_tool_calls().count(), 0);
            assert_eq!(run.round_count(), 0);
            assert_eq!(run.tool_call_count(), 0);
        }
    }

    #[test]
    fn wrong_results_are_atomic_and_correctable() {
        let mut run = ready();
        run.accept_response(response(json!([call("a"),call("b")]))).unwrap();
        for invalid in [vec![answer("a")],vec![answer("a"),answer("a")],
            vec![answer("a"),answer("foreign")],vec![answer("a"),serde_json::from_value(json!({"type":"text","text":"mixed"})).unwrap()]]
        {
            assert_eq!(run.submit_tool_results(invalid), Err(SamplingToolLoopError::InvalidToolResults));
            assert_eq!(run.pending_tool_calls().count(), 2);
            assert!(run.request().is_none());
        }
        run.submit_tool_results(vec![answer("b"),answer("a")]).unwrap();
        assert_eq!(run.accept_response(response(call("a"))), Err(SamplingToolLoopError::RepeatedToolId));
    }

    #[test]
    fn round_and_call_limits_refuse_before_tool_execution() {
        for (limits, error) in [
            (SamplingToolLoopLimits::new(1, 10, 4096).unwrap(), SamplingToolLoopError::RoundLimit),
            (SamplingToolLoopLimits::new(3, 0, 4096).unwrap(), SamplingToolLoopError::ToolCallLimit),
        ] {
            let mut run = SamplingToolLoop::new(request(), limits).unwrap();
            assert_eq!(run.accept_response(response(call("a"))), Err(error));
            assert_eq!(run.pending_tool_calls().count(), 0);
            assert_eq!(run.round_count(), 0);
            run.accept_response(response(json!({"type":"text","text":"done"}))).unwrap();
        }
    }

    #[test]
    fn tool_choice_and_response_role_are_enforced() {
        for mode in ["none","required"] {
            let mut req = request();
            req.tool_choice = Some(serde_json::from_value(json!({"mode":mode})).unwrap());
            let mut run = SamplingToolLoop::new(req, SamplingToolLoopLimits::default()).unwrap();
            let content = if mode == "none" { call("a") } else { json!({"type":"text","text":"no tool"}) };
            assert_eq!(run.accept_response(response(content)), Err(SamplingToolLoopError::ToolChoiceViolation));
        }
        let mut reply = response(call("a"));
        reply.role = Role::User;
        assert_eq!(ready().accept_response(reply), Err(SamplingToolLoopError::InvalidResponse));
    }

    #[test]
    fn output_schema_is_enforced_but_error_results_remain_deliverable() {
        let mut req = request();
        req.tools.as_mut().unwrap()[0].output_schema = Some(json!({"type":"number"}));
        let mut run = SamplingToolLoop::new(req, SamplingToolLoopLimits::default()).unwrap();
        run.accept_response(response(call("a"))).unwrap();
        assert_eq!(run.submit_tool_results(vec![answer("a")]), Err(SamplingToolLoopError::InvalidToolOutput));
        let result = serde_json::from_value(json!({"type":"tool_result","toolUseId":"a",
            "content":[],"isError":true})).unwrap();
        run.submit_tool_results(vec![result]).unwrap();
    }

    #[test]
    fn initial_history_must_be_balanced_and_ids_cannot_be_replayed() {
        let mut first = ready();
        first.accept_response(response(call("a"))).unwrap();
        let unbalanced = first.request.as_ref().unwrap().clone();
        assert!(matches!(SamplingToolLoop::new(unbalanced, SamplingToolLoopLimits::default()),
            Err(SamplingToolLoopError::InvalidHistory)));
        first.submit_tool_results(vec![answer("a")]).unwrap();
        let mut restored = SamplingToolLoop::new(first.request().unwrap().clone(), SamplingToolLoopLimits::default()).unwrap();
        assert_eq!(restored.accept_response(response(call("a"))), Err(SamplingToolLoopError::RepeatedToolId));
    }

    #[test]
    fn cumulative_bytes_close_and_redacted_diagnostics() {
        let req = request();
        let bytes = serde_json::to_vec(&req).unwrap().len();
        let mut run = SamplingToolLoop::new(req, SamplingToolLoopLimits::new(4, 8, bytes).unwrap()).unwrap();
        assert_eq!(run.accept_response(response(call("private-id"))), Err(SamplingToolLoopError::ByteLimit));
        assert_eq!(run.round_count(), 0);
        assert!(!format!("{run:?}").contains("private"));
        run.close();
        run.close();
        assert!(run.request().is_none());
        assert_eq!(run.accept_response(response(call("a"))), Err(SamplingToolLoopError::Closed));
        assert_eq!(run.submit_tool_results(vec![answer("a")]), Err(SamplingToolLoopError::Closed));
    }
}
