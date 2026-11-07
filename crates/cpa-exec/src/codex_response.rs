//! Codex responses: Go's status and terminal-event error classification
//! (codex_executor_terminal.go, codex_websockets_errors.go), per-event SSE processing,
//! bootstrap buffering, and buffered (non-streaming) assembly.

use cpa_core::gostr::GoStr;
use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use cpa_core::exec::{ExecError, ExecStream, FailureScope};
use futures_util::StreamExt;
use gjson::Kind;
use http::HeaderMap;

use crate::codex_json::{set_raw, set_str};

pub(crate) const INCOMPLETE_MESSAGE: &str =
    "stream error: stream disconnected before completion: stream closed before response.completed";
pub(crate) const EMPTY_INCOMPLETE_MESSAGE: &str =
    "stream error: upstream terminated with incomplete empty response (0 tokens)";
/// `codexBootstrapMaxBufferedFrames` / `codexBootstrapMaxBufferedBytes`.
pub(crate) const BOOTSTRAP_MAX_FRAMES: usize = 48;
pub(crate) const BOOTSTRAP_MAX_BYTES: usize = 1 << 20;

fn lower(v: &gjson::Value<'_>) -> String {
    v.str().trim().to_ascii_lowercase()
}

fn text(body: &str, path: &str) -> String {
    gjson::get(body, path).str().to_owned()
}

/// `isCodexUsageLimitError`.
pub(crate) fn is_usage_limit(body: &str) -> bool {
    ["error.type", "type"].iter().any(|p| {
        gjson::get(body, p)
            .str()
            .trim()
            .eq_ignore_ascii_case("usage_limit_reached")
    })
}

/// `isCodexModelCapacityError`.
pub(crate) fn is_capacity(body: &str) -> bool {
    [text(body, "error.message"), text(body, "message"), body.to_owned()]
        .iter()
        .map(|c| c.trim().to_ascii_lowercase())
        .any(|l| {
            !l.is_empty()
                && (l.contains("model is at capacity")
                    || l.contains("model_at_capacity")
                    || l.contains("model_is_at_capacity")
                    || (l.contains("model") && l.contains("at capacity")))
        })
}

/// `codexStatusErrorClassification`.
pub(crate) fn classification(status: u16, body: &str) -> Option<(&'static str, &'static str)> {
    let mut message = lower(&gjson::get(body, "error.message"));
    if message.is_empty() {
        message = lower(&gjson::get(body, "message"));
    }
    let all = body.trim().to_ascii_lowercase();
    let code = lower(&gjson::get(body, "error.code"));
    let kind = lower(&gjson::get(body, "error.type"));
    let invalid = kind.is_empty() || kind == "invalid_request_error";
    if status == 413
        || code == "context_length_exceeded"
        || code == "context_too_large"
        || (invalid
            && ["context length", "context_length", "maximum context", "too many tokens"]
                .iter()
                .any(|s| message.contains(s)))
    {
        return Some(("context_too_large", "invalid_request_error"));
    }
    if all.contains("invalid signature in thinking block") || all.contains("invalid_encrypted_content") {
        return Some(("thinking_signature_invalid", "invalid_request_error"));
    }
    if code == "previous_response_not_found"
        || all.contains("previous_response_not_found")
        || (all.contains("previous_response_id") && all.contains("not found"))
    {
        return Some(("previous_response_not_found", "invalid_request_error"));
    }
    if status == 401
        || kind == "authentication_error"
        || code == "invalid_api_key"
        || all.contains("invalid or expired token")
        || all.contains("refresh_token_reused")
    {
        return Some(("auth_unavailable", "authentication_error"));
    }
    None
}

/// `classifyCodexStatusError`: rewrites recognised failures into an OpenAI error object.
fn classify(status: u16, body: &str) -> String {
    let Some((code, kind)) = classification(status, body) else {
        return body.to_owned();
    };
    let mut message = text(body, "error.message");
    if message.is_empty() {
        message = text(body, "message");
    }
    if message.is_empty() {
        message = body.trim().to_owned();
    }
    if message.is_empty() {
        message = http::StatusCode::from_u16(status)
            .ok()
            .and_then(|s| s.canonical_reason())
            .unwrap_or_default()
            .to_owned();
    }
    let out = set_str(r#"{"error":{}}"#, "error.message", &message);
    let out = set_str(&out, "error.type", kind);
    set_str(&out, "error.code", code)
}

/// `parseCodexRetryAfter`: only quota exhaustion carries a reset hint.
fn usage_retry_after(status: u16, body: &str, now: SystemTime) -> Option<Duration> {
    if status != 429 || body.is_empty() {
        return None;
    }
    let root = gjson::parse(body);
    for quota in [root.get("error"), gjson::parse(body)] {
        if !quota
            .get("type")
            .str()
            .trim()
            .eq_ignore_ascii_case("usage_limit_reached")
        {
            continue;
        }
        let at = quota.get("resets_at").i64();
        if at > 0 {
            let reset = UNIX_EPOCH + Duration::from_secs(at as u64);
            if let Ok(wait) = reset.duration_since(now)
                && !wait.is_zero()
            {
                return Some(wait);
            }
        }
        let secs = quota.get("resets_in_seconds").i64();
        if secs > 0 {
            return Some(Duration::from_secs(secs as u64));
        }
    }
    None
}

/// `clienterror.IsRequestFault`: the caller's request is wrong; no credential can help.
pub(crate) fn request_fault(status: u16, body: &str) -> bool {
    if status == 402 || status == 429 {
        return false;
    }
    let field = |paths: &[&str]| {
        paths
            .iter()
            .map(|p| gjson::get(body, p).str().trim().to_ascii_lowercase())
            .collect::<Vec<_>>()
    };
    let valid = gjson::valid(body.trim());
    let types = if valid {
        field(&["error.type", "type", "response.error.type", "body.error.type"])
    } else {
        Vec::new()
    };
    let codes = if valid {
        field(&["error.code", "code", "response.error.code", "body.error.code"])
    } else {
        Vec::new()
    };
    if status == 401 && types.iter().any(|t| t == "authentication_error") {
        return false;
    }
    if codes
        .iter()
        .any(|c| c == "model_not_found" || c == "model_not_found_error")
    {
        return false;
    }
    const CODES: [&str; 9] = [
        "cyber_policy",
        "context_length_exceeded",
        "message_too_big",
        "string_above_max_length",
        "invalid_prompt",
        "invalid_value",
        "unsupported_value",
        "invalid_request_error",
        "previous_response_not_found",
    ];
    const TYPES: [&str; 4] = [
        "invalid_request",
        "invalid_request_error",
        "bad_request_error",
        "invalid_prompt",
    ];
    if codes.iter().any(|c| CODES.contains(&c.as_str())) || types.iter().any(|t| TYPES.contains(&t.as_str())) {
        return true;
    }
    let l = body.to_ascii_lowercase();
    if l.contains("item with id")
        && l.contains("not found")
        && l.contains("items are not persisted when `store` is set to false")
    {
        return true;
    }
    matches!(status, 400 | 409 | 413 | 422)
}

/// `newCodexStatusErrWithCooling` as an [`ExecError`]: usage-limit exhaustion is
/// credential-wide unless model-level cooling is on; capacity and usage limits are 429.
pub(crate) fn status_error(status: u16, body: &[u8], headers: HeaderMap, model_level_cooling: bool) -> ExecError {
    let raw = String::from_utf8_lossy(body);
    let code = if is_usage_limit(&raw) || is_capacity(&raw) {
        429
    } else {
        status
    };
    let classified = classify(code, &raw);
    status_error_raw(code, &classified, headers, model_level_cooling)
}

/// A plain `statusErr{code, msg}` with Go's retry hint and credential scope.
pub(crate) fn status_error_raw(code: u16, message: &str, headers: HeaderMap, model_level_cooling: bool) -> ExecError {
    let retry_after = usage_retry_after(code, message, SystemTime::now());
    let scope = if is_usage_limit(message) && !model_level_cooling {
        FailureScope::Credential
    } else if request_fault(code, message) {
        FailureScope::Request
    } else {
        match code {
            429 | 404 => FailureScope::Model,
            _ => FailureScope::Credential,
        }
    };
    let message = if message.is_empty() {
        format!("status {code}")
    } else {
        message.to_owned()
    };
    ExecError {
        status: code,
        scope,
        body: Bytes::from(message),
        headers: Box::new(headers),
        retry_after,
        direct: false,
        addon: None,
    }
}

/// Request-scoped stream failures (`codexIncompleteStreamError` and friends).
pub(crate) fn request_scoped(status: u16, message: &str) -> ExecError {
    ExecError::local(status, FailureScope::Request, message)
}

fn error_body(event: &str, path: &str) -> Option<String> {
    let found = gjson::get(event, path);
    if !found.exists() {
        return None;
    }
    let mut body = String::from(r#"{"error":{}}"#);
    if matches!(found.kind(), Kind::Object | Kind::Array) {
        body = set_raw(&body, "error", found.json());
    } else if !found.str().trim().is_empty() {
        body = set_str(&body, "error.message", found.str().trim());
    }
    let missing = |b: &str| gjson::get(b, "error.message").str().trim().is_empty();
    if missing(&body) {
        let m = text(event, "response.error.message");
        if !m.trim().is_empty() {
            body = set_str(&body, "error.message", m.trim());
        }
    }
    for fallback in ["error.code", "error.type"] {
        if missing(&body) {
            let v = text(&body, fallback);
            if !v.trim().is_empty() {
                body = set_str(&body, "error.message", v.trim());
            }
        }
    }
    Some(body)
}

fn top_level_error_body(event: &str) -> Option<String> {
    let get = |p: &str| text(event, p).trim().to_owned();
    let (message, code, kind, param) = (get("message"), get("code"), get("error_type"), get("param"));
    if message.is_empty() && code.is_empty() && kind.is_empty() && param.is_empty() {
        return None;
    }
    let mut body = String::from(r#"{"error":{}}"#);
    for (key, value) in [
        ("error.message", &message),
        ("error.code", &code),
        ("error.type", &kind),
        ("error.param", &param),
    ] {
        if !value.is_empty() {
            body = set_str(&body, key, value);
        }
    }
    if message.is_empty() {
        if !code.is_empty() {
            body = set_str(&body, "error.message", &code);
        } else if !kind.is_empty() {
            body = set_str(&body, "error.message", &kind);
        }
    }
    Some(body)
}

/// `codexTerminalFailureBody`: the error object of an `error` or `response.failed` event.
pub(crate) fn terminal_failure_body(event: &str) -> Option<String> {
    let body = match gjson::get(event, "type").str() {
        "error" => error_body(event, "error").or_else(|| top_level_error_body(event)),
        "response.failed" => error_body(event, "response.error").or_else(|| error_body(event, "error")),
        _ => return None,
    };
    let mut body = body
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| r#"{"error":{"message":"upstream stream failed without error details"}}"#.into());
    let seq = gjson::get(event, "sequence_number");
    if seq.exists() {
        body = set_raw(&body, "sequence_number", &seq.i64().to_string());
    }
    Some(body)
}

fn context_length(body: &str) -> bool {
    let code = lower(&gjson::get(body, "error.code"));
    let message = lower(&gjson::get(body, "error.message"));
    code == "context_length_exceeded"
        || code == "context_too_large"
        || ["context window", "context length", "too many tokens"]
            .iter()
            .any(|s| message.contains(s))
}

/// `codexTerminalFailureStatus`.
fn terminal_status(body: &str) -> u16 {
    for path in ["error.status_code", "error.status"] {
        let status = gjson::get(body, path).i64();
        if (400..=599).contains(&status) {
            return status as u16;
        }
    }
    let kind = lower(&gjson::get(body, "error.type"));
    let code = lower(&gjson::get(body, "error.code"));
    match () {
        _ if code == "cyber_policy" => 400,
        _ if kind == "not_found_error" || code == "not_found" || code == "model_not_found" => 404,
        _ if kind == "authentication_error" || code == "invalid_api_key" || code == "unauthorized" => 401,
        _ if kind == "permission_error" || code == "forbidden" || code == "permission_denied" => 403,
        _ if kind == "rate_limit_error" || code == "rate_limit_exceeded" => 429,
        _ if kind == "invalid_request_error" || kind == "bad_request_error" => 400,
        _ => 502,
    }
}

/// `codexTerminalFailureErrWithCooling`: an in-stream failure event as an error, with the
/// body it was built from.
pub(crate) fn terminal_failure(event: &str, model_level_cooling: bool) -> Option<(ExecError, String)> {
    let body = terminal_failure_body(event)?;
    let stream_handled = context_length(&body)
        || is_usage_limit(&body)
        || is_capacity(&body)
        || classification(400, &body).is_some_and(|(code, _)| code == "thinking_signature_invalid");
    let status = if stream_handled { 400 } else { terminal_status(&body) };
    Some((
        status_error(status, body.as_bytes(), HeaderMap::new(), model_level_cooling),
        body,
    ))
}

/// `isCodexOverloadBootstrapFailure`: transient capacity rejections another credential may serve.
pub(crate) fn is_overload(body: &str) -> bool {
    if is_capacity(body) {
        return true;
    }
    let kind = lower(&gjson::get(body, "error.type"));
    let code = lower(&gjson::get(body, "error.code"));
    let mut message = lower(&gjson::get(body, "error.message"));
    if message.is_empty() {
        message = lower(&gjson::get(body, "message"));
    }
    kind == "service_unavailable_error"
        || code == "server_is_overloaded"
        || kind == "rate_limit_error"
        || code == "rate_limit_exceeded"
        || ((kind == "server_error" || code == "server_error") && message.contains("you can retry your request"))
}

/// `newCodexBootstrapOverloadErr`: the 503 the upstream refused to put on the wire.
pub(crate) fn bootstrap_overload(body: &str) -> ExecError {
    status_error(503, body.as_bytes(), HeaderMap::new(), false)
}

/// `HasMeaningfulCodexOutputDelta`.
pub(crate) fn meaningful_delta(payload: &str) -> bool {
    matches!(
        gjson::get(payload, "type").str(),
        "response.output_text.delta"
            | "response.reasoning_text.delta"
            | "response.reasoning_summary_text.delta"
            | "response.function_call_arguments.delta"
    ) && !gjson::get(payload, "delta").str().trim().is_empty()
}

/// `IsCodexTerminalEmptyIncomplete`: `response.incomplete` with literally zero output,
/// an upstream silent abort. Unlike Go, an incomplete response that names why it stopped
/// (`interrupted`, `content_filter`, `max_output_tokens`) is that response, not an
/// abort: a cancellation before the first token produces exactly this shape, and current
/// Codex reads those reasons itself (docs/DIFFERENCES-FROM-GO.md).
pub(crate) fn empty_incomplete(payload: &str, items: usize, saw_delta: bool) -> bool {
    if gjson::get(payload, "type").str() != "response.incomplete" || saw_delta || items > 0 {
        return false;
    }
    if matches!(
        gjson::get(payload, "response.incomplete_details.reason").str(),
        "interrupted" | "content_filter" | "max_output_tokens"
    ) {
        return false;
    }
    let output = gjson::get(payload, "response.output");
    if output.kind() == Kind::Array && !output.array().is_empty() {
        return false;
    }
    let tokens = gjson::get(payload, "response.usage.output_tokens");
    tokens.kind() == Kind::Number && tokens.json().trim() == "0"
}

/// `isCodexBootstrapBufferableEvent`: frames that show the client nothing yet.
pub(crate) fn bufferable(payload: &str) -> bool {
    if payload.trim().is_empty() {
        return true;
    }
    let empty_list = |list: gjson::Value<'_>| {
        list.array().iter().all(|entry| match entry.get("type").str() {
            "output_text" | "summary_text" | "text" | "reasoning_text" => entry.get("text").str().is_empty(),
            "refusal" => entry.get("refusal").str().is_empty(),
            _ => false,
        })
    };
    let empty_part = |payload: &str| {
        let part = gjson::get(payload, "part");
        match part.get("type").str() {
            "output_text" | "summary_text" | "text" | "reasoning_text" => part.get("text").str().is_empty(),
            "refusal" => part.get("refusal").str().is_empty(),
            _ => false,
        }
    };
    match gjson::get(payload, "type").str() {
        "response.created" | "response.in_progress" | "codex.rate_limits" | "codex.response.metadata" | "keepalive" => {
            true
        }
        "response.output_item.added" => {
            let item = gjson::get(payload, "item");
            match item.get("type").str() {
                "message" => empty_list(item.get("content")),
                "reasoning" => {
                    item.get("encrypted_content").str().is_empty()
                        && empty_list(item.get("summary"))
                        && empty_list(item.get("content"))
                }
                "function_call" => item.get("arguments").str().is_empty(),
                "custom_tool_call" => item.get("input").str().is_empty(),
                _ => false,
            }
        }
        "response.content_part.added" | "response.reasoning_summary_part.added" => empty_part(payload),
        _ => false,
    }
}

/// `normalizeCodexWebsocketCompletion`: `response.done` is reported as `response.completed`.
pub(crate) fn normalize_completion(payload: String) -> String {
    if gjson::get(&payload, "type").str().trim() == "response.done" {
        set_str(&payload, "type", "response.completed")
    } else {
        payload
    }
}

/// `EnsureResponsesUsageDetails` for one JSON object payload.
pub(crate) fn ensure_usage_details(payload: String) -> String {
    let trimmed = payload.trim();
    if !trimmed.starts_with('{') || gjson::get(trimmed, "object").str() == "response.compaction" {
        return payload;
    }
    let mut out = trimmed.to_owned();
    for path in ["response.usage", "usage"] {
        let usage = gjson::get(&out, path);
        if usage.kind() != Kind::Object {
            continue;
        }
        let (output, input) = (usage.get("output_tokens_details"), usage.get("input_tokens_details"));
        let output = (
            output.exists(),
            output.kind(),
            output.get("reasoning_tokens").exists() && output.get("reasoning_tokens").kind() != Kind::Null,
        );
        let input = (
            input.exists(),
            input.kind(),
            input.get("cached_tokens").exists() && input.get("cached_tokens").kind() != Kind::Null,
        );
        out = match output {
            (false, ..) => set_raw(&out, &format!("{path}.output_tokens_details.reasoning_tokens"), "0"),
            (true, kind, _) if kind != Kind::Object => set_raw(
                &out,
                &format!("{path}.output_tokens_details"),
                r#"{"reasoning_tokens":0}"#,
            ),
            (true, _, false) => set_raw(&out, &format!("{path}.output_tokens_details.reasoning_tokens"), "0"),
            _ => out,
        };
        out = match input {
            (false, ..) => set_raw(&out, &format!("{path}.input_tokens_details.cached_tokens"), "0"),
            (true, kind, _) if kind != Kind::Object => {
                set_raw(&out, &format!("{path}.input_tokens_details"), r#"{"cached_tokens":0}"#)
            }
            (true, _, false) => set_raw(&out, &format!("{path}.input_tokens_details.cached_tokens"), "0"),
            _ => out,
        };
    }
    if out == trimmed { payload } else { out }
}

/// Go `http.StatusText` (where the `http` crate's reason phrases differ).
pub(crate) fn go_status_text(status: u16) -> &'static str {
    match status {
        103 => "Early Hints",
        413 => "Request Entity Too Large",
        414 => "Request URI Too Long",
        416 => "Requested Range Not Satisfiable",
        418 => "I'm a teapot",
        422 => "Unprocessable Entity",
        425 => "Too Early",
        s => http::StatusCode::from_u16(s)
            .ok()
            .and_then(|s| s.canonical_reason())
            .unwrap_or_default(),
    }
}

/// `EnsureResponsesUsageDetails` over one stream chunk: a JSON object, or SSE `data:`
/// lines (Go applies it to every chunk sent to an OpenAI Responses client).
pub(crate) fn ensure_usage_details_chunk(chunk: Bytes) -> Bytes {
    let text = String::from_utf8_lossy(&chunk);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return chunk;
    }
    if trimmed.starts_with('{') {
        let out = ensure_usage_details(trimmed.to_owned());
        return if out == trimmed { chunk } else { Bytes::from(out) };
    }
    if !text.contains("data:") {
        return chunk;
    }
    let mut modified = false;
    let lines: Vec<String> = text
        .split('\n')
        .map(|line| {
            if !line.trim().starts_with("data:") {
                return line.to_owned();
            }
            let prefix = if line.starts_with("data: ") {
                "data: "
            } else if line.starts_with("data:") {
                "data:"
            } else {
                return line.to_owned();
            };
            let data = line[prefix.len()..].trim();
            if !data.starts_with('{') {
                return line.to_owned();
            }
            let updated = ensure_usage_details(data.to_owned());
            if updated == data {
                return line.to_owned();
            }
            modified = true;
            format!("{prefix}{updated}")
        })
        .collect();
    if modified { Bytes::from(lines.join("\n")) } else { chunk }
}

/// Output items seen in `response.output_item.done`, used to patch an empty
/// `response.completed.response.output` (`collectCodexOutputItemDone`,
/// `patchCodexCompletedOutput`).
#[derive(Default)]
pub(crate) struct OutputItems {
    by_index: BTreeMap<i64, String>,
    fallback: Vec<String>,
}

impl OutputItems {
    pub fn len(&self) -> usize {
        self.by_index.len() + self.fallback.len()
    }

    pub fn collect(&mut self, payload: &str) {
        let item = gjson::get(payload, "item");
        if !matches!(item.kind(), Kind::Object | Kind::Array) {
            return;
        }
        let index = gjson::get(payload, "output_index");
        if index.exists() {
            self.by_index.insert(index.i64(), item.json().to_owned());
        } else {
            self.fallback.push(item.json().to_owned());
        }
    }

    pub fn patch(&self, payload: String) -> String {
        let output = gjson::get(&payload, "response.output");
        let outputs = output.array();
        if output.kind() == Kind::Array && !outputs.is_empty() {
            // hydrateCodexCompletedOutputItemIDs: fill missing ids from done items.
            let mut patched = payload.clone();
            for (i, item) in outputs.iter().enumerate() {
                let id = item.get("id");
                let has_id = id.exists()
                    && id.kind() != Kind::Null
                    && (id.kind() != Kind::String || !id.str().trim().is_empty());
                if has_id {
                    continue;
                }
                let Some(done) = self.by_index.get(&(i as i64)) else {
                    continue;
                };
                let done_id = gjson::get(done, "id");
                if done_id.kind() == Kind::String && !done_id.str().trim().is_empty() {
                    patched = set_raw(&patched, &format!("response.output.{i}.id"), done_id.json());
                }
            }
            return patched;
        }
        if self.len() == 0 {
            return payload;
        }
        let items: Vec<&str> = self
            .by_index
            .values()
            .chain(self.fallback.iter())
            .map(String::as_str)
            .collect();
        set_raw(&payload, "response.output", &format!("[{}]", items.join(",")))
    }
}

/// Go's 50 MB scanner line limit on the streaming path.
const SCANNER_MAX_LINE: usize = 52_428_800;

/// Processed lines as one SSE frame; `None` for no lines.
fn framed(lines: &[String]) -> Option<Bytes> {
    if lines.is_empty() {
        return None;
    }
    let mut out = lines.join("\n");
    out.push_str("\n\n");
    Some(Bytes::from(out))
}

/// What one upstream event means for the stream.
pub(crate) enum Step {
    /// Forward this (normalised) event.
    Event(Bytes),
    /// Forward this event; it ends the response.
    Terminal(Bytes),
    /// The upstream rejected or broke the response; nothing more is forwarded after
    /// `before`, the event's lines that preceded the failure (Go sends each line as read).
    Fail {
        error: ExecError,
        body: Option<String>,
        before: Option<Bytes>,
    },
}

/// Per-response SSE processing shared by streaming and buffered paths.
pub(crate) struct Processor {
    items: OutputItems,
    saw_delta: bool,
    preserve_native: bool,
    model_level_cooling: bool,
    /// The last completed payload (response.completed/incomplete), for buffered callers.
    pub completed: Option<String>,
    /// The request renamed the `collaboration` namespace (multi-agent v2).
    restore: bool,
    /// Claude clients' reasoning replay: cached from completed turns, cleared on an
    /// invalid-signature failure.
    replay: Option<(std::sync::Arc<crate::codex_replay::Cache>, crate::codex_replay::Scope)>,
    /// Usage records see every upstream payload in Codex format (`observeCodexTokenEvent`).
    usage: cpa_core::exec::UsageSink,
    /// Grok Build clients get keepalive events as SSE comments (`grokbuild`).
    grok_keepalive: bool,
    /// The attempt's wire capture: every scanned line and each stream failure.
    wire: crate::codex_capture::Wire,
    /// Joins an event's data split across `data:` lines (`CodexSSEScanner`, 50 MB lines).
    sse: crate::codex_sse::DataLines,
}

impl Processor {
    pub fn new(preserve_native: bool, model_level_cooling: bool) -> Self {
        Self {
            items: OutputItems::default(),
            saw_delta: false,
            preserve_native,
            model_level_cooling,
            completed: None,
            restore: false,
            replay: None,
            usage: Default::default(),
            grok_keepalive: false,
            wire: Default::default(),
            sse: crate::codex_sse::DataLines::new(SCANNER_MAX_LINE),
        }
    }

    /// Records every scanned line and each stream failure (Go `AppendAPIResponseChunk`
    /// and `RecordAPIResponseError` in the stream loops).
    pub fn capturing(mut self, wire: crate::codex_capture::Wire) -> Self {
        self.wire = wire;
        self
    }

    /// `RecordAPIResponseError` for a failure the stream reports.
    fn record(&self, error: &ExecError) {
        self.wire.exec_error(error);
    }

    /// `grokbuild.TransformKeepaliveSSELine` for a Grok Build client (`User-Agent` with
    /// `grok-pager` or `grok-shell`).
    pub fn grok_keepalive(mut self, headers: &http::HeaderMap) -> Self {
        self.grok_keepalive = is_grok_client(headers);
        self
    }

    /// Reports each upstream payload to the attempt's usage record.
    // ponytail: Go also publishes `response.tool_usage.image_gen` as a second record under
    // the request's image_generation tool model (`publishCodexImageToolUsage`, default
    // gpt-image-2); `UsageSink` has no per-model record yet (a Server contract change).
    pub fn reporting(mut self, usage: cpa_core::exec::UsageSink) -> Self {
        self.usage = usage;
        self
    }

    /// Caches completed turns and clears on invalid signatures for this replay scope.
    pub fn replaying(
        mut self,
        cache: std::sync::Arc<crate::codex_replay::Cache>,
        scope: crate::codex_replay::Scope,
    ) -> Self {
        self.replay = Some((cache, scope));
        self
    }

    fn replay_failure(&self, status: u16, body: &str) {
        if let Some((cache, scope)) = &self.replay {
            crate::codex_replay::clear_on_invalid_signature(cache, scope, status, body.as_bytes());
        }
    }

    fn replay_completed(&self, payload: &str) {
        if let Some((cache, scope)) = &self.replay {
            crate::codex_replay::cache_completed(cache, scope, payload.as_bytes());
        }
    }

    /// Restores the client's `collaboration` names in every payload before anything
    /// reads it, as Go does right after trimming each `data:` line.
    pub fn restoring(mut self, restore: bool) -> Self {
        self.restore = restore;
        self
    }

    /// One framed SSE event in. Go scans lines: `data:` payloads are trimmed and
    /// re-prefixed with `data: `, other lines pass through, CR is dropped.
    pub fn event(&mut self, event: &[u8]) -> Step {
        self.event_with(event, None)
    }

    fn event_with(&mut self, event: &[u8], gate: Option<&mut Gate>) -> Step {
        let step = self.event_step(event, gate);
        if let Step::Fail { error, .. } = &step {
            self.record(error);
        }
        step
    }

    /// Processes `event` line by line, in Go's order. While `gate` holds the stream, each
    /// delivered line spends its budgets before the next is read, and the gate releases
    /// at the line that passes them; the rest of the event is processed as streamed.
    fn event_step(&mut self, event: &[u8], mut gate: Option<&mut Gate>) -> Step {
        let mut lines = Vec::new();
        let mut terminal = false;
        // Go writes each keepalive comment as its own SSE frame.
        let mut after_comment = false;
        let mut physical = event.split(|b| *b == b'\n');
        let mut scanned = Vec::new();
        let mut read_all = false;
        'read: while !read_all {
            match physical.next() {
                Some(raw) => {
                    let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
                    // Go records each physical line as read, and none after a terminal.
                    self.wire.chunk(raw);
                    self.sse.push(raw, &mut scanned);
                    let held = self.sse.holding();
                    if held.0 > 0 && raw.starts_with(b"data:") {
                        // Go's budget check inside the scanner, on a data line it holds.
                        if let Some(gate) = gate.as_deref_mut() {
                            gate.assembling(held);
                        }
                    }
                }
                None => {
                    self.sse.finish(&mut scanned);
                    read_all = true;
                }
            }
            let held = self.sse.holding();
            for item in scanned.drain(..) {
                let line = String::from_utf8_lossy(&item.line);
                let line = line.as_ref();
                if line.is_empty() {
                    continue;
                }
                let before = lines.len();
                if after_comment {
                    lines.push(String::new());
                }
                after_comment = self.grok_keepalive && is_keepalive_line(line);
                let handshake = if after_comment {
                    lines.push(": keepalive".to_owned());
                    true
                } else if let Some(data) = line.strip_prefix("data:") {
                    let payload = restore(data.trim(), self.restore);
                    let payload = payload.as_ref();
                    if self.usage.enabled() {
                        self.usage
                            .response_line(cpa_core::format::Format::Codex, payload.as_bytes());
                    }
                    if let Some((error, body)) = terminal_failure(payload, self.model_level_cooling) {
                        self.replay_failure(error.status, &body);
                        return Step::Fail {
                            error,
                            body: Some(body),
                            before: framed(&lines),
                        };
                    }
                    if meaningful_delta(payload) {
                        self.saw_delta = true;
                    }
                    if empty_incomplete(payload, self.items.len(), self.saw_delta) {
                        return Step::Fail {
                            error: request_scoped(502, EMPTY_INCOMPLETE_MESSAGE),
                            body: None,
                            before: framed(&lines),
                        };
                    }
                    let handshake = bufferable(payload);
                    let mut payload = payload.to_owned();
                    match gjson::get(&payload, "type").str() {
                        "response.output_item.done" => self.items.collect(&payload),
                        kind @ ("response.completed" | "response.incomplete" | "response.done") => {
                            let incomplete = kind == "response.incomplete";
                            terminal = true;
                            payload = normalize_completion(payload);
                            if !self.preserve_native {
                                payload = self.items.patch(payload);
                            }
                            if !incomplete {
                                self.replay_completed(&payload);
                            }
                            self.completed = Some(payload.clone());
                        }
                        _ => {}
                    }
                    lines.push(format!("data: {payload}"));
                    if terminal {
                        break 'read;
                    }
                    handshake
                } else {
                    lines.push(line.to_owned());
                    true
                };
                if let Some(gate) = gate.as_deref_mut() {
                    let translated = lines[before..].iter().map(String::len).sum();
                    gate.line(handshake, held, (item.raw_lines, item.raw_bytes), translated);
                }
            }
        }
        if !terminal && let Some(gate) = gate {
            // The blank line that ends the event (Go delivers it as a line too).
            gate.line(true, (0, 0), (1, 0), 0);
        }
        let out = framed(&lines).unwrap_or_else(|| Bytes::from_static(b"\n\n"));
        if terminal {
            Step::Terminal(out)
        } else {
            Step::Event(out)
        }
    }

    /// Buffered (`Execute`) handling of one event's data payloads: always patches output.
    pub fn buffered(&mut self, event: &[u8]) -> Result<Option<String>, ExecError> {
        // Go scans the whole body with a line limit one past its length.
        let mut sse = crate::codex_sse::DataLines::new(event.len() + 1);
        let mut physical = event.split(|b| *b == b'\n');
        let mut scanned = Vec::new();
        let mut read_all = false;
        while !read_all {
            match physical.next() {
                Some(raw) => sse.push(raw.strip_suffix(b"\r").unwrap_or(raw), &mut scanned),
                None => {
                    sse.finish(&mut scanned);
                    read_all = true;
                }
            }
            for line in scanned.drain(..) {
                let line = String::from_utf8_lossy(&line.line);
                let Some(data) = line.strip_prefix("data:") else {
                    continue;
                };
                let payload = restore(data.trim(), self.restore);
                let payload = payload.as_ref();
                if self.usage.enabled() {
                    self.usage
                        .response_line(cpa_core::format::Format::Codex, payload.as_bytes());
                }
                if meaningful_delta(payload) {
                    self.saw_delta = true;
                }
                if let Some((error, body)) = terminal_failure(payload, self.model_level_cooling) {
                    self.replay_failure(error.status, &body);
                    return Err(error);
                }
                match gjson::get(payload, "type").str() {
                    "response.output_item.done" => self.items.collect(payload),
                    kind @ ("response.completed" | "response.incomplete") => {
                        if empty_incomplete(payload, self.items.len(), self.saw_delta) {
                            return Err(request_scoped(502, EMPTY_INCOMPLETE_MESSAGE));
                        }
                        let completed = self.items.patch(payload.to_owned());
                        if kind == "response.completed" {
                            self.replay_completed(&completed);
                        }
                        return Ok(Some(completed));
                    }
                    _ => {}
                }
            }
        }
        Ok(None)
    }
}

/// `grokbuild.IsGrokClientHeaders`: any `User-Agent` value naming Grok Pager or Shell.
pub(crate) fn is_grok_client(headers: &http::HeaderMap) -> bool {
    headers.get_all(http::header::USER_AGENT).iter().any(|v| {
        let ua = String::from_utf8_lossy(v.as_bytes()).go_lower();
        ua.contains("grok-pager") || ua.contains("grok-shell")
    })
}

/// `grokbuild.IsKeepaliveSSELine`: `event: keepalive` or a `keepalive` data payload.
fn is_keepalive_line(line: &str) -> bool {
    let trimmed = line.trim();
    if let Some(name) = trimmed.strip_prefix("event:") {
        return name.trim() == "keepalive";
    }
    trimmed
        .strip_prefix("data:")
        .is_some_and(|data| gjson::get(data.trim(), "type").str() == "keepalive")
}

/// `RestoreCodexMultiAgentV2Response` on one upstream payload when the request was
/// optimized.
pub(crate) fn restore(payload: &str, optimized: bool) -> std::borrow::Cow<'_, str> {
    if !optimized {
        return std::borrow::Cow::Borrowed(payload);
    }
    match String::from_utf8(cpa_common::codex_client::restore_response(payload.as_bytes(), true)) {
        Ok(restored) => std::borrow::Cow::Owned(restored),
        Err(_) => std::borrow::Cow::Borrowed(payload),
    }
}

/// Upstream events → processed client events. Stops after a terminal event or the first
/// error; a clean EOF before any terminal event is Go's request-scoped 408.
pub(crate) fn processed(upstream: ExecStream, processor: Processor, emitted: usize) -> ExecStream {
    struct State {
        upstream: ExecStream,
        processor: Processor,
        emitted: usize,
        done: bool,
        /// A failure to deliver after the lines that preceded it in its event.
        failed: Option<ExecError>,
    }
    futures_util::stream::unfold(
        State {
            upstream,
            processor,
            emitted,
            done: false,
            failed: None,
        },
        |mut st| async move {
            if let Some(error) = st.failed.take() {
                st.done = true;
                return Some((Err(error), st));
            }
            if st.done {
                return None;
            }
            let item = match st.upstream.next().await {
                Some(Ok(event)) => match st.processor.event(&event) {
                    Step::Event(out) => {
                        st.emitted += 1;
                        Ok(out)
                    }
                    Step::Terminal(out) => {
                        st.done = true;
                        Ok(out)
                    }
                    Step::Fail {
                        error,
                        before: Some(before),
                        ..
                    } => {
                        st.emitted += 1;
                        st.failed = Some(error);
                        Ok(before)
                    }
                    Step::Fail { error, .. } => {
                        st.done = true;
                        Err(error)
                    }
                },
                Some(Err(error)) => {
                    st.done = true;
                    // Go records the scan error, then how the stream ended.
                    st.processor.record(&error);
                    st.processor.record(&ended(st.emitted));
                    Err(error)
                }
                None => {
                    st.done = true;
                    let end = ended(st.emitted);
                    st.processor.record(&end);
                    // Go ends silently when nothing at all was emitted.
                    if st.emitted == 0 {
                        return None;
                    }
                    Err(end)
                }
            };
            Some((item, st))
        },
    )
    .boxed()
}

/// How a stream that ended without a terminal event is reported: Go's 502 "upstream
/// stream closed before first payload" when nothing was emitted, else the 408.
fn ended(emitted: usize) -> ExecError {
    if emitted == 0 {
        request_scoped(502, "upstream stream closed before first payload")
    } else {
        request_scoped(408, INCOMPLETE_MESSAGE)
    }
}

/// Result of holding back the frames that precede generation.
pub(crate) enum Bootstrap {
    /// Failed before anything was shown: the attempt may fail over.
    Reject(ExecError),
    /// The stream to deliver: held frames first, then the rest.
    Stream(ExecStream),
}

/// Go's bootstrap budgets (codex_executor_stream.go): the lines held so far, their raw
/// and translated bytes, and whether the stream has been released.
pub(crate) struct Gate {
    frames: usize,
    bytes: usize,
    timeout: Option<Duration>,
    started: Instant,
    released: bool,
}

impl Gate {
    fn new(timeout: Option<Duration>, started: Instant) -> Self {
        Self {
            frames: 0,
            bytes: 0,
            timeout,
            started,
            released: false,
        }
    }

    fn timed_out(&self) -> bool {
        self.timeout.is_some_and(|t| self.started.elapsed() >= t)
    }

    /// The scanner's budget check while it holds `held` (lines, bytes) of an unfinished
    /// event: the stream is released at the limit itself.
    fn assembling(&mut self, held: (usize, usize)) {
        if !self.released
            && (self.timed_out()
                || self.frames + held.0 >= BOOTSTRAP_MAX_FRAMES
                || self.bytes + held.1 >= BOOTSTRAP_MAX_BYTES)
        {
            self.released = true;
        }
    }

    /// One delivered line read from `raw` (lines, bytes) and `translated` bytes out, with
    /// `held` (lines, bytes) still collected: a handshake within the budgets, which it may
    /// fill exactly, stays held; anything else releases the stream.
    // ponytail: `translated` is the Codex-format line: Go's translator output for a Codex
    // client and for Responses events it leaves alone (it inserts a missing
    // `response.model`); Claude and Chat sizes differ. That matters only when a total
    // straddles 1 MiB. Exact needs client translation inside bootstrap.
    fn line(&mut self, handshake: bool, held: (usize, usize), raw: (usize, usize), translated: usize) {
        if self.released {
            return;
        }
        let frames = self.frames + held.0 + raw.0;
        let bytes = self.bytes + held.1 + raw.1 + translated;
        if handshake && !self.timed_out() && frames <= BOOTSTRAP_MAX_FRAMES && bytes <= BOOTSTRAP_MAX_BYTES {
            self.frames += raw.0;
            self.bytes += raw.1 + translated;
        } else {
            self.released = true;
        }
    }
}

/// Codex stream bootstrap buffering (`codex.stream-bootstrap-buffering`, M5-0025): hold
/// handshake frames (created, rate limits, empty `*.added`, heartbeats) up to 48 frames,
/// 1 MiB and the optional timeout, so an overload/rate-limit rejection smuggled into a 200
/// stream fails over before headers are committed. Lines count as frames, as in Go.
pub(crate) async fn bootstrap(
    mut upstream: ExecStream,
    mut processor: Processor,
    timeout: Option<Duration>,
    started: Instant,
) -> Bootstrap {
    let mut held: VecDeque<Result<Bytes, ExecError>> = VecDeque::new();
    let mut gate = Gate::new(timeout, started);
    loop {
        let event = match upstream.next().await {
            Some(Ok(event)) => event,
            Some(Err(error)) => {
                processor.record(&error);
                return Bootstrap::Reject(error);
            }
            None if held.is_empty() => {
                // "upstream stream closed before first payload": an empty stream.
                processor.record(&ended(0));
                return Bootstrap::Stream(futures_util::stream::empty().boxed());
            }
            None => {
                let error = request_scoped(408, INCOMPLETE_MESSAGE);
                processor.record(&error);
                return Bootstrap::Reject(error);
            }
        };
        match processor.event_with(&event, Some(&mut gate)) {
            Step::Fail { error, body, before } => {
                // Once a line has released the stream, the overload is delivered in-stream.
                if !gate.released && !gate.timed_out() && body.as_deref().is_some_and(is_overload) {
                    return Bootstrap::Reject(bootstrap_overload(body.as_deref().unwrap_or_default()));
                }
                held.extend(before.map(Ok));
                held.push_back(Err(error));
                return Bootstrap::Stream(futures_util::stream::iter(held).boxed());
            }
            Step::Terminal(out) => {
                held.push_back(Ok(out));
                return Bootstrap::Stream(futures_util::stream::iter(held).boxed());
            }
            Step::Event(out) => {
                held.push_back(Ok(out));
                if gate.released {
                    let emitted = held.len();
                    let rest = processed(upstream, processor, emitted);
                    return Bootstrap::Stream(futures_util::stream::iter(held).chain(rest).boxed());
                }
            }
        }
    }
}

#[cfg(test)]
mod empty_incomplete_tests {
    use super::empty_incomplete;

    fn incomplete(details: &str) -> String {
        format!(
            r#"{{"type":"response.incomplete","response":{{"id":"r1","output":[],{details}"usage":{{"output_tokens":0}}}}}}"#
        )
    }

    /// An explained stop is a response; an unexplained zero-output one stays an abort.
    #[test]
    fn named_reasons_are_not_silent_aborts() {
        for reason in ["interrupted", "content_filter", "max_output_tokens"] {
            let payload = incomplete(&format!(r#""incomplete_details":{{"reason":"{reason}"}},"#));
            assert!(!empty_incomplete(&payload, 0, false), "{reason}");
        }
        for details in [
            "",
            r#""incomplete_details":null,"#,
            r#""incomplete_details":{"reason":"unknown"},"#,
        ] {
            assert!(empty_incomplete(&incomplete(details), 0, false), "{details}");
        }
    }
}
