//! Usage records for the usage queue, one per upstream attempt (Go
//! sdk/cliproxy/usage/accounting.go, internal/runtime/executor/helps/usage_helpers.go
//! and response_model.go, internal/redisqueue/plugin.go `HandleUsage`).
//!
//! Go's executors parse usage from the upstream response in its own format. Executors
//! that forward those payloads through `ExecRequest::usage` get the same parse here;
//! otherwise the dispatch loop parses the client-format response, which matches Go
//! whenever the upstream speaks the client's format.
//!
//! The queued record adds what Go's lacks, after Go's fields: where its `session_id`
//! came from, a [`prompt_prefix`] hash, per-stage [`Timings`] and byte counts
//! (docs/DIFFERENCES-FROM-GO.md).

use crate::format::Format;
use crate::protocol::json::{self as gj, Kind, Res};
use crate::text::GoStr;

/// Go `usage.TokenAccountingQuality`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Quality {
    Complete,
    Inconsistent,
    Unclassified,
}

/// Go `usage.TokenBreakdown` (schema version 2), always valid when present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Breakdown {
    quality: Quality,
    total: i64,
    /// total, uncached, cache read, cache write.
    input: [i64; 4],
    /// total, non-reasoning, reasoning.
    output: [i64; 3],
    unclassified: i64,
}

/// Go `usage.Detail`. `breakdown: None` is Go's zero (invalid) breakdown.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Detail {
    pub input: i64,
    pub output: i64,
    pub reasoning: i64,
    pub cached: i64,
    pub cache_read: i64,
    pub cache_creation: i64,
    pub cache_creation_5m: Option<i64>,
    pub cache_creation_1h: Option<i64>,
    pub total: i64,
    pub breakdown: Option<Breakdown>,
    pub response_service_tier: String,
    /// Claude `usage.server_tool_use` counts, when the provider sent them (U1, #99).
    pub web_search_requests: Option<i64>,
    pub web_fetch_requests: Option<i64>,
    /// xAI `usage.cost_in_usd_ticks` (10^10 ticks per dollar), when sent (#178).
    pub cost_in_usd_ticks: Option<i64>,
    /// xAI `usage.server_side_tool_usage_details`: the documented counters it sent, in
    /// [`XAI_TOOL_COUNTERS`] order (#178).
    pub server_side_tools: Vec<(&'static str, i64)>,
}

/// The counters xAI documents in `usage.server_side_tool_usage_details` (docs.x.ai API
/// reference, Responses `usage`, read 2026-11-14). Other members are not carried.
const XAI_TOOL_COUNTERS: [&str; 9] = [
    "code_interpreter_calls",
    "document_search_calls",
    "file_search_calls",
    "image_generation_calls",
    "mcp_calls",
    "web_search_calls",
    "x_search_calls",
    "x_posts_fetched",
    "x_users_fetched",
];

/// Go `nonNegativeSum`.
fn sum(values: &[i64]) -> Option<i64> {
    values
        .iter()
        .try_fold(0i64, |acc, v| if *v < 0 { None } else { acc.checked_add(*v) })
}

/// Go `resolveAccountingTotal`.
fn resolve_total(total: i64, expected: i64) -> (i64, bool) {
    if total < 0 || expected < 0 {
        (0, false)
    } else if total == 0 {
        (expected, true)
    } else {
        (total, total == expected)
    }
}

fn inconsistent(total: i64, fallback: i64) -> Breakdown {
    let mut resolved = if total <= 0 { fallback } else { total };
    if resolved < 0 {
        resolved = 0;
    }
    Breakdown {
        quality: Quality::Inconsistent,
        total: resolved,
        input: [0; 4],
        output: [0; 3],
        unclassified: resolved,
    }
}

fn complete(total: i64, input: [i64; 4], output: [i64; 3]) -> Breakdown {
    Breakdown {
        quality: Quality::Complete,
        total,
        input,
        output,
        unclassified: 0,
    }
}

/// Go `NewSubsetTokenBreakdown`: cache inside input, reasoning inside output.
fn subset(input: i64, cache_read: i64, cache_write: i64, output: i64, reasoning: i64, total: i64) -> Breakdown {
    let cache = sum(&[cache_read, cache_write]);
    let expected = sum(&[input, output]);
    let expected_or_zero = expected.unwrap_or(0);
    let Some(cache) = cache.filter(|c| expected.is_some() && reasoning >= 0 && *c <= input && reasoning <= output)
    else {
        return inconsistent(total, expected_or_zero);
    };
    let (resolved, ok) = resolve_total(total, expected_or_zero);
    if !ok {
        return inconsistent(total, expected_or_zero);
    }
    complete(
        resolved,
        [input, input - cache, cache_read, cache_write],
        [output, output - reasoning, reasoning],
    )
}

/// Go `NewPartialSubsetTokenBreakdown`: an authoritative remainder is unclassified.
fn partial_subset(input: i64, cache_read: i64, cache_write: i64, output: i64, reasoning: i64, total: i64) -> Breakdown {
    let cache = sum(&[cache_read, cache_write]);
    let expected = sum(&[input, output]);
    let expected_or_zero = expected.unwrap_or(0);
    let Some(cache) = cache.filter(|c| {
        expected.is_some()
            && input >= 0
            && output >= 0
            && reasoning >= 0
            && *c <= input
            && reasoning <= output
            && total >= 0
    }) else {
        return inconsistent(total, expected_or_zero);
    };
    let resolved = if total == 0 { expected_or_zero } else { total };
    if resolved < expected_or_zero {
        return inconsistent(total, expected_or_zero);
    }
    let unclassified = resolved - expected_or_zero;
    Breakdown {
        quality: if unclassified > 0 {
            Quality::Unclassified
        } else {
            Quality::Complete
        },
        total: resolved,
        input: [input, input - cache, cache_read, cache_write],
        output: [output, output - reasoning, reasoning],
        unclassified,
    }
}

/// Go `NewIndependentTokenBreakdown`: every bucket separate.
fn independent(
    uncached: i64,
    cache_read: i64,
    cache_write: i64,
    non_reasoning: i64,
    reasoning: i64,
    total: i64,
) -> Breakdown {
    let input = sum(&[uncached, cache_read, cache_write]);
    let output = sum(&[non_reasoning, reasoning]);
    let expected = sum(&[input.unwrap_or(0), output.unwrap_or(0)]);
    let expected_or_zero = expected.unwrap_or(0);
    let (Some(input), Some(output), Some(_)) = (input, output, expected) else {
        return inconsistent(total, expected_or_zero);
    };
    let (resolved, ok) = resolve_total(total, expected_or_zero);
    if !ok {
        return inconsistent(total, expected_or_zero);
    }
    complete(
        resolved,
        [input, uncached, cache_read, cache_write],
        [output, non_reasoning, reasoning],
    )
}

/// Go `NewSeparateReasoningTokenBreakdown`: cache inside input, reasoning separate.
fn separate_reasoning(
    input: i64,
    cache_read: i64,
    cache_write: i64,
    non_reasoning: i64,
    reasoning: i64,
    total: i64,
) -> Breakdown {
    let Some(cache) = sum(&[cache_read, cache_write]).filter(|c| input >= 0 && *c <= input) else {
        return inconsistent(total, 0);
    };
    let output = sum(&[non_reasoning, reasoning]);
    let expected = sum(&[input, output.unwrap_or(0)]);
    let expected_or_zero = expected.unwrap_or(0);
    let (Some(output), Some(_)) = (output, expected) else {
        return inconsistent(total, expected_or_zero);
    };
    let (resolved, ok) = resolve_total(total, expected_or_zero);
    if !ok {
        return inconsistent(total, expected_or_zero);
    }
    complete(
        resolved,
        [input, input - cache, cache_read, cache_write],
        [output, non_reasoning, reasoning],
    )
}

/// Go `NewUnclassifiedTokenBreakdown`.
fn unclassified(total: i64) -> Breakdown {
    if total <= 0 {
        let mut b = complete(0, [0; 4], [0; 3]);
        if total < 0 {
            b.quality = Quality::Inconsistent;
        }
        return b;
    }
    Breakdown {
        quality: Quality::Unclassified,
        total,
        input: [0; 4],
        output: [0; 3],
        unclassified: total,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Semantics {
    Unknown,
    Subset,
    Independent,
    SeparateReasoning,
}

/// Go `tokenAccountingSemanticsFor`.
fn semantics(provider: &str, executor_type: &str) -> Semantics {
    let provider = provider.trim().go_lower();
    let executor = executor_type.trim().go_lower();
    let value = format!("{provider} {executor}");
    let value = value.trim();
    if value.is_empty() || value == "unknown" || value == "unknown unknown" {
        return Semantics::Unknown;
    }
    if executor == "openaicompatexecutor"
        || provider == "openai-compatibility"
        || provider.starts_with("openai-compatible-")
    {
        return Semantics::Subset;
    }
    if value.contains("claude") || value.contains("anthropic") {
        return Semantics::Independent;
    }
    if ["gemini", "aistudio", "antigravity", "vertex", "interaction"]
        .iter()
        .any(|m| value.contains(m))
    {
        return Semantics::SeparateReasoning;
    }
    if [
        "openai",
        "codex",
        "xai",
        "grok",
        "kimi",
        "qwen",
        "deepseek",
        "openrouter",
    ]
    .iter()
    .any(|m| value.contains(m))
    {
        return Semantics::Subset;
    }
    Semantics::Unknown
}

/// Go `unclassifiedTokenLowerBound`.
fn lower_bound(d: &Detail) -> Option<i64> {
    let cache = sum(&[d.cache_read, d.cache_creation])?;
    if d.input < 0 || d.output < 0 || d.reasoning < 0 || d.cached < 0 {
        return None;
    }
    let input = d.input.max(cache).max(d.cached);
    let output = d.output.max(d.reasoning);
    sum(&[input, output])
}

/// Go `tokenBreakdownForSemantics`.
fn breakdown_for(d: &Detail, s: Semantics) -> Breakdown {
    if d.total == 0 && d.input == 0 && d.output == 0 {
        match lower_bound(d) {
            None => return inconsistent(d.total, 0),
            Some(total)
                if total > 0
                    && (matches!(s, Semantics::Unknown | Semantics::Subset)
                        || (s == Semantics::SeparateReasoning
                            && (d.cache_read > 0 || d.cache_creation > 0 || d.cached > 0))) =>
            {
                return unclassified(total);
            }
            Some(_) => {}
        }
    }
    match s {
        Semantics::Subset => subset(d.input, d.cache_read, d.cache_creation, d.output, d.reasoning, d.total),
        Semantics::Independent => independent(d.input, d.cache_read, d.cache_creation, d.output, d.reasoning, d.total),
        Semantics::SeparateReasoning => {
            separate_reasoning(d.input, d.cache_read, d.cache_creation, d.output, d.reasoning, d.total)
        }
        Semantics::Unknown => {
            let total = if d.total == 0 {
                match lower_bound(d) {
                    Some(t) => t,
                    None => return inconsistent(d.total, 0),
                }
            } else {
                d.total
            };
            unclassified(total)
        }
    }
}

/// Go `EnsureTokenBreakdownForProvider`.
pub fn ensure_breakdown(mut d: Detail, provider: &str, executor_type: &str) -> Detail {
    if d.breakdown.is_none() {
        let s = semantics(provider, executor_type);
        if d.cache_read == 0
            && d.cached > 0
            && d.input == 0
            && d.output == 0
            && d.reasoning == 0
            && d.cache_creation == 0
            && d.total == 0
            && matches!(s, Semantics::Subset | Semantics::SeparateReasoning)
        {
            d.cache_read = d.cached;
        }
        d.breakdown = Some(breakdown_for(&d, s));
    }
    if d.total == 0 {
        d.total = d.breakdown.map_or(0, |b| b.total);
    }
    d
}

/// Go `hasNonZeroTokenUsage`.
/// Whether `usage` holds token counts the provider reported (zeros included), not just a
/// service tier or nothing: a record without them says `usage_observed: false` (U1, #99).
fn usage_reported(usage: Option<&Detail>) -> bool {
    usage.is_some_and(|d| d.breakdown.is_some())
}

fn has_tokens(d: &Detail) -> bool {
    d.input != 0
        || d.output != 0
        || d.reasoning != 0
        || d.cached != 0
        || d.cache_read != 0
        || d.cache_creation != 0
        || d.total != 0
        || d.breakdown.is_some_and(|b| b.total != 0)
}

// ---- parsers -------------------------------------------------------------------

/// Go `jsonPayload`: the JSON object of an SSE `data:` line or a bare JSON line.
fn json_payload(line: &[u8]) -> Option<&[u8]> {
    let mut trimmed = line.trim_ascii();
    if trimmed.is_empty() || trimmed == b"[DONE]" || trimmed.starts_with(b"event:") {
        return None;
    }
    if let Some(rest) = trimmed.strip_prefix(b"data:") {
        trimmed = rest.trim_ascii();
    }
    (trimmed.first() == Some(&b'{')).then_some(trimmed)
}

fn first<'a>(root: &Res<'a>, paths: &[&str]) -> Res<'a> {
    paths.iter().map(|p| root.get(*p)).find(Res::exists).unwrap_or_default()
}

/// Go `extractResponseServiceTierFromValidJSON`.
fn service_tier_valid(payload: &[u8]) -> String {
    ["response.service_tier", "service_tier", "interaction.service_tier"]
        .iter()
        .map(|p| gj::get(payload, *p).str().trim().to_owned())
        .find(|t| !t.is_empty())
        .unwrap_or_default()
}

/// Go `extractResponseServiceTier`.
fn service_tier(payload: &[u8]) -> String {
    if payload.is_empty() || !gj::valid(payload) {
        return String::new();
    }
    service_tier_valid(payload)
}

fn has_openai_bucket_fields(node: &Res<'_>) -> bool {
    [
        "prompt_tokens",
        "input_tokens",
        "completion_tokens",
        "output_tokens",
        "prompt_tokens_details.cached_tokens",
        "input_tokens_details.cached_tokens",
        "prompt_tokens_details.cache_write_tokens",
        "prompt_tokens_details.cache_creation_tokens",
        "input_tokens_details.cache_write_tokens",
        "input_tokens_details.cache_creation_tokens",
        "completion_tokens_details.reasoning_tokens",
        "output_tokens_details.reasoning_tokens",
    ]
    .iter()
    .any(|p| node.get(*p).exists())
}

/// Go `hasOpenAIStyleUsageTokenFields`.
fn has_openai_token_fields(node: &Res<'_>) -> bool {
    node.exists() && node.is_object() && (node.get("total_tokens").exists() || has_openai_bucket_fields(node))
}

/// Go `parseOpenAIStyleUsageNode`.
fn parse_openai_node(node: &Res<'_>) -> Detail {
    let input = first(node, &["prompt_tokens", "input_tokens"]);
    let output = first(node, &["completion_tokens", "output_tokens"]);
    let mut d = Detail {
        input: input.int(),
        output: output.int(),
        total: node.get("total_tokens").int(),
        ..Detail::default()
    };
    let cached = first(
        node,
        &[
            "prompt_tokens_details.cached_tokens",
            "input_tokens_details.cached_tokens",
        ],
    );
    if cached.exists() {
        d.cached = cached.int();
        d.cache_read = cached.int();
    }
    let creation = first(
        node,
        &[
            "input_tokens_details.cache_creation_tokens",
            "input_tokens_details.cache_write_tokens",
            "prompt_tokens_details.cache_creation_tokens",
            "prompt_tokens_details.cache_write_tokens",
        ],
    );
    if creation.exists() {
        d.cache_creation = creation.int();
    }
    let reasoning = first(
        node,
        &[
            "completion_tokens_details.reasoning_tokens",
            "output_tokens_details.reasoning_tokens",
        ],
    );
    if reasoning.exists() {
        d.reasoning = reasoning.int();
    }
    d.breakdown = Some(if has_openai_bucket_fields(node) {
        if input.exists() && output.exists() {
            subset(d.input, d.cache_read, d.cache_creation, d.output, d.reasoning, d.total)
        } else {
            let (cache_read, cache_creation) = if input.exists() {
                (d.cache_read, d.cache_creation)
            } else {
                (0, 0)
            };
            let reasoning = if output.exists() { d.reasoning } else { 0 };
            partial_subset(d.input, cache_read, cache_creation, d.output, reasoning, d.total)
        }
    } else {
        unclassified(d.total)
    });
    if d.total == 0 {
        d.total = d.breakdown.map_or(0, |b| b.total);
    }
    // xAI's provider-reported cost and server-side tool counts (#178), when sent.
    let number = |n: Res<'_>| (n.kind == Kind::Number).then(|| n.int());
    d.cost_in_usd_ticks = number(node.get("cost_in_usd_ticks"));
    let tools = node.get("server_side_tool_usage_details");
    if tools.is_object() {
        d.server_side_tools = XAI_TOOL_COUNTERS
            .iter()
            .filter_map(|name| number(tools.get(name)).map(|n| (*name, n)))
            .collect();
    }
    d
}

/// Go `ParseOpenAIUsage`.
pub fn parse_openai(body: &[u8]) -> Detail {
    let tier = service_tier(body);
    let node = gj::parse(body).get("usage");
    if !has_openai_token_fields(&node) {
        return Detail {
            response_service_tier: tier,
            ..Detail::default()
        };
    }
    let mut d = parse_openai_node(&node);
    d.response_service_tier = tier;
    d
}

/// Go `ParseCodexUsage`.
pub fn parse_codex(data: &[u8]) -> Option<Detail> {
    let tier = service_tier(data);
    let node = gj::parse(data).get("response.usage");
    if !has_openai_token_fields(&node) {
        return (!tier.is_empty()).then(|| Detail {
            response_service_tier: tier,
            ..Detail::default()
        });
    }
    let mut d = parse_openai_node(&node);
    d.response_service_tier = tier;
    Some(d)
}

/// Go `parseClaudeUsageNode`: cache buckets are separate from `input_tokens`;
/// thinking is part of `output_tokens`.
fn parse_claude_node(node: &Res<'_>) -> Detail {
    let cache_read = node.get("cache_read_input_tokens").int();
    let cache_creation = node.get("cache_creation_input_tokens").int();
    let output = node.get("output_tokens").int();
    let reasoning = first(
        node,
        &[
            "output_tokens_details.thinking_tokens",
            "output_tokens_details.reasoning_tokens",
            "thinking_tokens",
        ],
    )
    .int()
    .max(0);
    let non_reasoning = if reasoning > 0 && reasoning <= output {
        output - reasoning
    } else if reasoning > output {
        0
    } else {
        output
    };
    let mut d = Detail {
        input: node.get("input_tokens").int(),
        output,
        reasoning,
        cached: cache_read,
        cache_read,
        cache_creation,
        cache_creation_5m: node
            .get("cache_creation.ephemeral_5m_input_tokens")
            .exists()
            .then(|| node.get("cache_creation.ephemeral_5m_input_tokens").int()),
        cache_creation_1h: node
            .get("cache_creation.ephemeral_1h_input_tokens")
            .exists()
            .then(|| node.get("cache_creation.ephemeral_1h_input_tokens").int()),
        ..Detail::default()
    };
    if d.cached == 0 {
        d.cached = d.cache_creation;
    }
    // Claude's documented `usage.service_tier` and `usage.server_tool_use` (U1, #99).
    d.response_service_tier = node.get("service_tier").str().trim().to_owned();
    let count = |path: &str| {
        let n = node.get(path);
        (n.kind == Kind::Number).then(|| n.int())
    };
    d.web_search_requests = count("server_tool_use.web_search_requests");
    d.web_fetch_requests = count("server_tool_use.web_fetch_requests");
    d.total = d
        .input
        .wrapping_add(output)
        .wrapping_add(d.cache_read)
        .wrapping_add(d.cache_creation);
    d.breakdown = Some(independent(
        d.input,
        d.cache_read,
        d.cache_creation,
        non_reasoning,
        d.reasoning,
        d.total,
    ));
    d
}

/// Go `ParseClaudeUsage`.
pub fn parse_claude(body: &[u8]) -> Detail {
    let node = gj::parse(body).get("usage");
    if !node.exists() {
        return Detail::default();
    }
    parse_claude_node(&node)
}

/// Go `ParseClaudeStreamUsage`.
fn parse_claude_stream(line: &[u8]) -> Option<Detail> {
    let payload = json_payload(line).filter(|p| gj::valid(p))?;
    let node = first(&gj::parse(payload), &["usage", "message.usage"]);
    node.exists().then(|| parse_claude_node(&node))
}

/// The buffered-response parser for a format (Go's executor for that format).
pub fn parse_body(format: Format, body: &[u8]) -> Detail {
    match format {
        Format::Claude => parse_claude(body),
        // A buffered Responses body is the completed response object: usage and
        // service_tier are top-level, as `ParseOpenAIUsage` reads them.
        Format::OpenAI | Format::OpenAIResponse | Format::Codex => parse_openai(body),
    }
}

/// Go `StreamUsageBuffer`, fed line by line in one format the way that format's Go
/// executor feeds it.
#[derive(Debug, Default)]
pub struct StreamUsage {
    detail: Detail,
    ok: bool,
}

impl StreamUsage {
    /// Go `StreamUsageBuffer.Observe`: the latest usage wins; a tier-only update keeps
    /// the usage and replaces the tier.
    ///
    /// Reported zero usage with a tier is usage, not a tier-only update, while nothing
    /// was reported before (U1, #99); it never replaces earlier usage.
    fn observe(&mut self, detail: Detail) {
        let tier = detail.response_service_tier.trim().to_owned();
        let first_usage = detail.breakdown.is_some() && self.detail.breakdown.is_none();
        if tier.is_empty() || has_tokens(&detail) || first_usage {
            let preserved = std::mem::take(&mut self.detail.response_service_tier);
            self.detail = detail;
            if self.detail.response_service_tier.is_empty() {
                self.detail.response_service_tier = preserved;
            }
        } else {
            self.detail.response_service_tier = tier;
        }
        self.ok = true;
    }

    /// Go `ObserveMergedStreamUsage` with `MergeStreamUsageDetail`.
    fn observe_merged(&mut self, update: Detail) {
        if !self.ok {
            self.observe(update);
            return;
        }
        let existing = &self.detail;
        let mut m = update;
        for (field, old) in [
            (&mut m.input, existing.input),
            (&mut m.cached, existing.cached),
            (&mut m.cache_read, existing.cache_read),
            (&mut m.cache_creation, existing.cache_creation),
            (&mut m.output, existing.output),
            (&mut m.reasoning, existing.reasoning),
        ] {
            if *field == 0 && old > 0 {
                *field = old;
            }
        }
        if m.response_service_tier.is_empty() {
            m.response_service_tier = existing.response_service_tier.clone();
        }
        m.cache_creation_5m = m.cache_creation_5m.or(existing.cache_creation_5m);
        m.cache_creation_1h = m.cache_creation_1h.or(existing.cache_creation_1h);
        m.web_search_requests = m.web_search_requests.or(existing.web_search_requests);
        m.web_fetch_requests = m.web_fetch_requests.or(existing.web_fetch_requests);
        let mut cached = m.cache_read + m.cache_creation;
        if cached == 0 {
            cached = m.cached;
        }
        let calculated = m.input + m.output + cached;
        if m.total == 0 || m.total < calculated {
            m.total = calculated;
        }
        let non_reasoning = (m.output - m.reasoning).max(0);
        m.breakdown = Some(independent(
            m.input,
            m.cache_read,
            m.cache_creation,
            non_reasoning,
            m.reasoning,
            m.total,
        ));
        self.observe(m);
    }

    /// Go `ObserveOpenAIStream`.
    fn observe_openai(&mut self, line: &[u8]) {
        let Some(payload) = json_payload(line) else { return };
        let contains = |needle: &[u8]| payload.windows(needle.len()).any(|w| w == needle);
        let usage_candidate = contains(b"\"usage\"");
        let need_tier = self.detail.response_service_tier.is_empty() || usage_candidate;
        let tier_candidate = need_tier && contains(b"\"service_tier\"");
        if (!usage_candidate && !tier_candidate) || !gj::valid(payload) {
            return;
        }
        let mut detail = Detail::default();
        let mut usage_ok = false;
        if usage_candidate {
            let node = gj::get(payload, "usage");
            if has_openai_token_fields(&node) {
                detail = parse_openai_node(&node);
                usage_ok = true;
            }
        }
        if tier_candidate {
            detail.response_service_tier = service_tier_valid(payload);
        }
        if usage_ok || !detail.response_service_tier.is_empty() {
            self.observe(detail);
        }
    }

    /// One client-visible stream line of `format`.
    pub fn line(&mut self, format: Format, line: &[u8]) {
        match format {
            Format::Claude => {
                if let Some(d) = parse_claude_stream(line) {
                    self.observe_merged(d);
                }
            }
            Format::OpenAI => self.observe_openai(line),
            // The first terminal event's usage. A `response.failed` carries the usage of
            // the work done before the failure (U1, #99), which its failure record keeps.
            Format::OpenAIResponse | Format::Codex => {
                let Some(payload) = json_payload(line) else { return };
                let kind = gj::get(payload, "type");
                if !matches!(
                    kind.str().as_ref(),
                    "response.completed" | "response.incomplete" | "response.done" | "response.failed"
                ) || self.ok
                {
                    return;
                }
                if let Some(d) = parse_codex(payload) {
                    self.observe(d);
                }
            }
        }
    }

    /// Go `StreamUsageBuffer.Detail`.
    pub fn detail(&self) -> Option<&Detail> {
        self.ok.then_some(&self.detail)
    }
}

// ---- response model ------------------------------------------------------------

const MAX_RESPONSE_MODEL: usize = 128;

fn string_at(data: &[u8], path: &str) -> Option<String> {
    let r = gj::get(data, path);
    (r.kind == Kind::String).then(|| r.str().trim().to_owned())
}

fn bounded(model: String) -> Option<String> {
    (model.len() <= MAX_RESPONSE_MODEL).then_some(model)
}

fn interactions_terminal(event: &str, status: &str) -> bool {
    matches!(
        event,
        "interaction.completed" | "interaction.done" | "interaction.failed" | "interaction.cancelled"
    ) || matches!(status, "completed" | "incomplete" | "cancelled" | "failed")
}

fn event_type(data: &[u8]) -> String {
    let event = gj::get(data, "event_type").str().into_owned();
    if event.is_empty() {
        gj::get(data, "type").str().into_owned()
    } else {
        event
    }
}

/// Go `extractResponseModelEvent`: the model an upstream frame reports serving and
/// whether the frame ends the response.
fn response_model_event(line: &[u8], provider: &str) -> (String, bool) {
    let Some(data) = json_payload(line) else {
        return (String::new(), false);
    };
    match provider.trim().go_lower().as_str() {
        "codex" => {
            let (carries, terminal) = match gj::get(data, "type").str().trim() {
                "response.created" | "response.in_progress" => (true, false),
                "response.completed" | "response.incomplete" | "response.done" => (true, true),
                _ => (false, false),
            };
            if !carries || !gj::valid(data) {
                return (String::new(), false);
            }
            let model = string_at(data, "response.model").and_then(bounded).unwrap_or_default();
            (model, terminal)
        }
        "claude" => match gj::get(data, "type").str().as_ref() {
            "message_start" => (
                string_at(data, "message.model").and_then(bounded).unwrap_or_default(),
                false,
            ),
            // #71 (Go v8.0.67 fa11a573): a server-side fallback hop names the model
            // that serves from there on, and a `fallback_message` iteration the model
            // that served the response.
            "content_block_start" => {
                let model = (gj::get(data, "content_block.type").str() == "fallback")
                    .then(|| string_at(data, "content_block.to.model"))
                    .flatten()
                    .and_then(bounded)
                    .unwrap_or_default();
                (model, false)
            }
            "message_delta" => (fallback_iteration_model(data).unwrap_or_default(), false),
            "message_stop" => (String::new(), true),
            "message" => (
                fallback_iteration_model(data)
                    .or_else(|| string_at(data, "model").and_then(bounded))
                    .unwrap_or_default(),
                true,
            ),
            _ => {
                if !gj::valid(data) {
                    return (String::new(), false);
                }
                let model = match string_at(data, "message.model") {
                    Some(m) => bounded(m),
                    None => string_at(data, "model").and_then(bounded),
                };
                (model.unwrap_or_default(), false)
            }
        },
        "gemini" | "gemini-interactions" | "vertex" | "aistudio" | "antigravity" => {
            if !gj::valid(data) {
                return (String::new(), false);
            }
            let model = ["response.modelVersion", "modelVersion", "interaction.model", "model"]
                .iter()
                .find_map(|p| string_at(data, p))
                .and_then(bounded)
                .unwrap_or_default();
            let finish = first(
                &gj::parse(data),
                &["candidates.0.finishReason", "response.candidates.0.finishReason"],
            );
            // d9451cb6: Vertex's endpoints/openapi route answers in Chat Completions.
            let terminal = (finish.exists() && !finish.str().is_empty())
                || gj::get(data, "object").str() == "chat.completion"
                || !gj::get(data, "choices.0.finish_reason").str().is_empty()
                || interactions_terminal(&event_type(data), &gj::get(data, "interaction.status").str());
            (model, terminal)
        }
        _ => generic_response_model(data),
    }
}

/// Go `extractClaudeFallbackIterationModel`: the model of the last `fallback_message`
/// entry in `usage.iterations`, which Anthropic documents as the serving model.
fn fallback_iteration_model(data: &[u8]) -> Option<String> {
    let iterations = gj::get(data, "usage.iterations");
    if !iterations.is_array() {
        return None;
    }
    let mut served = None;
    iterations.each(|_, it| {
        if it.get("type").str() == "fallback_message"
            && it.get("model").kind == Kind::String
            && let Some(model) = bounded(it.get("model").str().trim().to_owned()).filter(|m| !m.is_empty())
        {
            served = Some(model);
        }
        true
    });
    served
}

/// Go `extractGenericResponseModelEvent`.
fn generic_response_model(data: &[u8]) -> (String, bool) {
    if !gj::valid(data) {
        return (String::new(), false);
    }
    if let Some(m) = string_at(data, "response.model").and_then(bounded) {
        let kind = gj::get(data, "type").str().into_owned();
        let terminal = matches!(
            kind.as_str(),
            "response.completed" | "response.done" | "response.incomplete"
        );
        return (m, terminal);
    }
    if let Some(m) = string_at(data, "interaction.model").and_then(bounded) {
        let status = gj::get(data, "interaction.status").str().into_owned();
        return (m, interactions_terminal(&event_type(data), &status));
    }
    for (path, finish) in [
        ("modelVersion", "candidates.0.finishReason"),
        ("response.modelVersion", "response.candidates.0.finishReason"),
    ] {
        if let Some(m) = string_at(data, path).and_then(bounded) {
            let f = gj::get(data, finish);
            return (m, f.exists() && !f.str().is_empty());
        }
    }
    if let Some(m) = string_at(data, "message.model").and_then(bounded) {
        return (m, false);
    }
    if let Some(m) = string_at(data, "model").and_then(bounded) {
        let object = gj::get(data, "object").str().into_owned();
        let finish = gj::get(data, "choices.0.finish_reason").str().into_owned();
        let status = gj::get(data, "status").str().into_owned();
        let terminal =
            object == "chat.completion" || !finish.is_empty() || status == "completed" || status == "incomplete";
        return (m, terminal);
    }
    let kind = event_type(data);
    let status = gj::get(data, "interaction.status").str().into_owned();
    (
        String::new(),
        interactions_terminal(&kind, &status) || kind == "message_stop",
    )
}

/// Go `UsageReporter.ObserveResponseModel` state.
#[derive(Debug, Default)]
pub struct ResponseModel {
    model: String,
    final_: bool,
}

impl ResponseModel {
    pub fn observe(&mut self, line: &[u8], provider: &str) {
        if self.final_ {
            return;
        }
        let (served, terminal) = response_model_event(line, provider);
        if !served.is_empty() {
            self.model = served;
        }
        if terminal {
            self.final_ = true;
        }
    }

    /// Go `SetResponseModel`: trimmed and bounded, unless a terminal event fixed it.
    pub fn set(&mut self, model: &str) {
        let model = model.trim();
        if self.final_ || model.is_empty() || model.len() > MAX_RESPONSE_MODEL {
            return;
        }
        self.model = model.to_owned();
    }

    pub fn get(&self) -> &str {
        &self.model
    }
}

/// Go `normalizeModelName`: lower case without the thinking suffix.
fn normalize_model_name(model: &str) -> String {
    let lower = model.trim().go_lower();
    crate::protocol::thinking::parse_suffix(&lower)
        .model_name
        .trim()
        .to_owned()
}

/// Go `isDatedModelAlias`: `dated` is `base` plus a date or a three-digit version.
fn is_dated_model_alias(base: &str, dated: &str) -> bool {
    let Some(suffix) = dated.strip_prefix(base).and_then(|s| s.strip_prefix('-')) else {
        return false;
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    match suffix.len() {
        10 => {
            let b = suffix.as_bytes();
            b[4] == b'-' && b[7] == b'-' && digits(&suffix[..4]) && digits(&suffix[5..7]) && digits(&suffix[8..])
        }
        8 | 3 => digits(suffix),
        _ => false,
    }
}

/// Go `IsModelSubstituted`: whether the upstream served another model than the
/// requested one; dated aliases, provider prefixes and `-latest` are the same model.
pub fn is_model_substituted(requested: &str, served: &str) -> bool {
    let served = normalize_model_name(served);
    let requested = normalize_model_name(requested);
    if served.is_empty() || requested.is_empty() || requested == served {
        return false;
    }
    let dated = |a: &str, b: &str| is_dated_model_alias(a, b) || is_dated_model_alias(b, a);
    if dated(&requested, &served) {
        return false;
    }
    // Go `stripModelProviderPrefix`: after the last `/`, unless it ends the name.
    let strip = |m: &str| match m.rfind('/') {
        Some(i) if i < m.len() - 1 => m[i + 1..].to_owned(),
        _ => m.to_owned(),
    };
    let (req, srv) = (strip(&requested), strip(&served));
    if req == srv || dated(&req, &srv) {
        return false;
    }
    let (req, srv) = (
        req.strip_suffix("-latest").unwrap_or(&req),
        srv.strip_suffix("-latest").unwrap_or(&srv),
    );
    !(req == srv || dated(req, srv))
}

/// Provider, credential ID, normalized requested and served model.
type SubstitutionKey = (String, String, String, String);

/// Go `codexModelSubstitutionWarns`: one warning per provider, credential and model
/// pair per ten minutes, at most 1024 pairs remembered.
fn substitution_warning_allowed(key: SubstitutionKey) -> bool {
    use std::collections::HashMap;
    use std::time::{Duration, Instant};
    const WINDOW: Duration = Duration::from_secs(600);
    const MAX: usize = 1024;
    static LAST: std::sync::Mutex<Option<HashMap<SubstitutionKey, Instant>>> = std::sync::Mutex::new(None);
    let mut guard = LAST.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let last = guard.get_or_insert_with(HashMap::new);
    let now = Instant::now();
    if last.get(&key).is_some_and(|at| now.duration_since(*at) < WINDOW) {
        return false;
    }
    if last.len() >= MAX {
        last.retain(|_, at| now.duration_since(*at) < WINDOW);
        if last.len() >= MAX {
            last.clear();
        }
    }
    last.insert(key, now);
    true
}

/// Go `warnModelSubstitution`, after an attempt's record is published.
fn warn_model_substitution(record: &Record, upstream_model: &str, auth_id: &str) {
    let served = record.response_model.as_str();
    let expected = if upstream_model.is_empty() {
        record.model.as_str()
    } else {
        upstream_model
    };
    if served.is_empty() || !is_model_substituted(expected, served) {
        return;
    }
    if !record.model.is_empty() && !is_model_substituted(&record.model, served) {
        return;
    }
    let provider = if record.provider.is_empty() {
        "codex"
    } else {
        record.provider.as_str()
    };
    let key = (
        provider.to_owned(),
        auth_id.to_owned(),
        normalize_model_name(expected),
        normalize_model_name(served),
    );
    if !substitution_warning_allowed(key) {
        return;
    }
    let index = if record.auth_index.trim().is_empty() {
        "nil"
    } else {
        record.auth_index.trim()
    };
    tracing::warn!(
        "{provider} executor: upstream served model {} for requested model {} (auth_index={index})",
        crate::text::quote(served),
        crate::text::quote(&record.model),
    );
}

// ---- the queued record ---------------------------------------------------------

/// Downstream request facts (Go `ClientRequestMetadata`, request ID, endpoint).
#[derive(Debug, Clone, Default)]
pub struct Client {
    pub client_ip: String,
    pub resolved_client_ip: String,
    pub x_forwarded_for: String,
    pub user_agent: String,
    /// The bound canonical session (Go `syncMetadataSessionToContext`).
    pub session_id: String,
    pub parent_session_id: String,
    pub is_fork: bool,
    /// Go `NodeKind` and `IsCompaction`: set only by an LCP pick (`fork`, `compaction`).
    pub node_kind: String,
    pub is_compaction: bool,
    /// Where `session_id` came from (`crate::gateway::session::Session::source`), `lcp` for an
    /// LCP pick's session or `home` for Home's. Not in Go.
    pub session_source: &'static str,
    pub request_id: String,
    /// `METHOD /route/template`.
    pub endpoint: String,
    /// The matched client key (Go gin `userApiKey`).
    pub api_key: String,
    /// The access provider that accepted it (Go gin `accessProvider`, 37fe2608).
    pub access_provider: String,
}

/// One attempt's record before Go's queue normalization (Go `usage.Record` as the
/// executor's reporter builds it).
#[derive(Debug, Clone, Default)]
pub struct Record {
    /// RFC 3339 with nanoseconds, Go `time.Time` JSON.
    pub timestamp: String,
    pub latency_ms: i64,
    pub ttft_ms: i64,
    pub execution_id: String,
    pub provider: String,
    pub executor_type: String,
    pub model: String,
    pub alias: String,
    pub source: String,
    pub auth_index: String,
    pub access_token_sha256: String,
    pub auth_type: String,
    pub reasoning_effort: String,
    pub service_tier: String,
    pub generate: bool,
    pub stream: bool,
    pub failed: bool,
    pub fail_status: i64,
    pub fail_body: String,
    /// Go v8.0.67 `usage.Failure.Code` (18883a57): how a request-scoped error rule
    /// classified the failure, `request_scoped` (`stop`, `continue`) or `force_cooldown`
    /// (`*-and-cooldown`); empty otherwise. A local fact, never upstream text.
    pub fail_code: &'static str,
    pub detail: Detail,
    pub response_model: String,
    /// Upstream response headers, names as received.
    pub response_headers: Vec<(String, Vec<String>)>,
    /// [`prompt_prefix`] of the client request; empty when it has none. Not in Go.
    pub prompt_prefix_hash: String,
    pub claude_cache: Option<crate::execution::ClaudeCacheTelemetry>,
    /// Present only for additional telemetry-only records that Go omits.
    pub usage_observed: Option<bool>,
    pub timings: Timings,
}

impl Record {
    /// Every text the upstream's answer put in the record: what [`finish`] scrubs of the
    /// authentication values the upstream request or socket carried (#25). The patterns
    /// name every field, so a new field does not compile until it is placed here: with
    /// the upstream's text, or with the local, request, credential and numeric facts,
    /// which no upstream answer writes.
    fn upstream_text(&mut self) -> impl Iterator<Item = &mut String> {
        let Record {
            fail_body,
            response_model,
            response_headers,
            detail,
            timestamp: _,
            latency_ms: _,
            ttft_ms: _,
            execution_id: _,
            provider: _,
            executor_type: _,
            model: _,
            alias: _,
            source: _,
            auth_index: _,
            access_token_sha256: _,
            auth_type: _,
            reasoning_effort: _,
            service_tier: _,
            generate: _,
            stream: _,
            failed: _,
            fail_status: _,
            fail_code: _,
            prompt_prefix_hash: _,
            claude_cache: _,
            usage_observed: _,
            timings: _,
        } = self;
        let Detail {
            response_service_tier,
            input: _,
            output: _,
            reasoning: _,
            cached: _,
            cache_read: _,
            cache_creation: _,
            cache_creation_5m: _,
            cache_creation_1h: _,
            total: _,
            breakdown: _,
            web_search_requests: _,
            web_fetch_requests: _,
            cost_in_usd_ticks: _,
            server_side_tools: _,
        } = detail;
        let headers = response_headers
            .iter_mut()
            .flat_map(|(name, values)| std::iter::once(name).chain(values.iter_mut()));
        [fail_body, response_model, response_service_tier]
            .into_iter()
            .chain(headers)
    }
}

/// Where an attempt's time went, in milliseconds from when the server accepted the
/// client request, and the bytes it moved. Not in Go's record: cliproxy-rs adds these
/// fields (docs/DIFFERENCES-FROM-GO.md), and leaves out each one it did not see. The
/// upstream stages come from the shared HTTP client ([`crate::execution::UpstreamStage`]),
/// which the Claude executor times; other executors leave them out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Timings {
    /// The client request body was read to its end.
    pub client_body_received_ms: Option<i64>,
    /// The upstream request was about to be sent.
    pub upstream_started_ms: Option<i64>,
    /// The upstream client had a connection and started writing the request.
    pub upstream_connected_ms: Option<i64>,
    /// `true`: the upstream request went out on a pooled connection. Left out when a
    /// connection was dialled while it waited: the client may still have used a pooled
    /// one that came free first, and does not say which.
    pub upstream_connection_reused: Option<bool>,
    /// The upstream client took the last of the request body into its write buffer.
    /// Handed over, not acknowledged: the client's buffer (it takes another piece below
    /// 408 KiB, so under 472 KiB) and the kernel's socket buffer may still hold the end
    /// of the body.
    pub upstream_body_sent_ms: Option<i64>,
    /// The upstream response head arrived.
    pub upstream_headers_ms: Option<i64>,
    /// The first upstream response body bytes arrived.
    pub upstream_first_byte_ms: Option<i64>,
    /// The request's first response bytes, a keep-alive included, were handed to the
    /// client's connection before the record was published: a stream's first event, or
    /// a keep-alive sent while a buffered response waited.
    pub client_first_byte_ms: Option<i64>,
    /// The client request body, as received.
    pub client_request_bytes: Option<i64>,
    /// The upstream request body of the last hop: its size once handed over, 0 for a
    /// hop without one (a redirect followed as a GET).
    pub upstream_request_bytes: Option<i64>,
    /// The upstream response body, as received (before any content decoding).
    pub upstream_response_bytes: Option<i64>,
}

impl Timings {
    /// The fields after Go's: a `timings` object and the byte counts.
    fn write(&self, w: &mut Writer) {
        let stages = [
            ("client_body_received_ms", self.client_body_received_ms),
            ("upstream_started_ms", self.upstream_started_ms),
            ("upstream_connected_ms", self.upstream_connected_ms),
            ("upstream_body_sent_ms", self.upstream_body_sent_ms),
            ("upstream_headers_ms", self.upstream_headers_ms),
            ("upstream_first_byte_ms", self.upstream_first_byte_ms),
            ("client_first_byte_ms", self.client_first_byte_ms),
        ];
        if stages.iter().any(|(_, v)| v.is_some()) || self.upstream_connection_reused.is_some() {
            w.object("timings", |t| {
                for (key, value) in stages {
                    if let Some(value) = value {
                        t.int(key, value);
                    }
                }
                if let Some(reused) = self.upstream_connection_reused {
                    t.bool("upstream_connection_reused", reused);
                }
            });
        }
        for (key, value) in [
            ("client_request_bytes", self.client_request_bytes),
            ("upstream_request_bytes", self.upstream_request_bytes),
            ("upstream_response_bytes", self.upstream_response_bytes),
        ] {
            if let Some(value) = value {
                w.int(key, value);
            }
        }
    }
}

struct Writer(Vec<u8>, bool);

impl Writer {
    fn key(&mut self, k: &str) {
        if self.1 {
            self.0.push(b',');
        }
        self.1 = true;
        crate::protocol::json::marshal_str(&mut self.0, k.as_bytes(), true);
        self.0.push(b':');
    }
    fn str(&mut self, k: &str, v: &str) {
        self.key(k);
        crate::protocol::json::marshal_str(&mut self.0, v.as_bytes(), true);
    }
    fn opt_str(&mut self, k: &str, v: &str) {
        if !v.is_empty() {
            self.str(k, v);
        }
    }
    fn int(&mut self, k: &str, v: i64) {
        use std::io::Write;
        self.key(k);
        // Straight into the record: no string per number.
        let _ = write!(self.0, "{v}");
    }
    fn bool(&mut self, k: &str, v: bool) {
        self.key(k);
        self.0.extend_from_slice(if v { b"true" } else { b"false" });
    }
    fn object(&mut self, k: &str, f: impl FnOnce(&mut Writer)) {
        self.key(k);
        let mut inner = Writer(std::mem::take(&mut self.0), false);
        inner.0.push(b'{');
        f(&mut inner);
        inner.0.push(b'}');
        self.0 = inner.0;
    }
}

/// Go `isHierarchyParent` is irrelevant here: `HandleUsage` restores the client
/// metadata's parent whenever the reporter dropped it.
fn sessions(client: &Client) -> (String, String) {
    let session = crate::protocol::session::normalize_to_canonical_uuid(&client.session_id);
    let mut parent = crate::protocol::session::normalize_to_canonical_uuid(&client.parent_session_id);
    if session.is_empty() || session == parent {
        parent.clear();
    }
    (session, parent)
}

/// Go `usageQueuePlugin.HandleUsage`: the queued `queuedUsageDetail` JSON.
/// Go `usage.APIKeyFromContext` for a Home 401 record from a handler whose context
/// derives from the request's (Alpha Search, the live handlers): the key the access
/// middleware set wins; otherwise the key this request's Home pick authenticated
/// (`setHomeUserAPIKeyOnGinContext`).
pub(crate) fn home_unauthorized_key(principal: &str, home_key: &str) -> String {
    let principal = principal.trim();
    if principal.is_empty() {
        home_key.trim()
    } else {
        principal
    }
    .to_owned()
}

/// Go `usage.IsNativeAccessProvider`: CPA's own client-key providers.
pub(crate) fn is_native_access_provider(provider: &str) -> bool {
    use crate::text::GoStr;
    let provider = provider.trim();
    provider.go_eq_fold("config-inline") || provider.go_eq_fold("config-api-key")
}

pub fn queued(record: &Record, client: &Client) -> Vec<u8> {
    let or = |v: &str, d: &str| -> String {
        let v = v.trim();
        if v.is_empty() { d.to_owned() } else { v.to_owned() }
    };
    let model = or(&record.model, "unknown");
    let alias = or(&record.alias, &model);
    let provider = or(&record.provider, "unknown");
    let executor_type = or(&record.executor_type, "unknown");
    let auth_type = or(&record.auth_type, "unknown");
    let request_id = client.request_id.trim();
    let execution_id = or(&record.execution_id, "");
    let execution_id = if execution_id.is_empty() {
        uuid::Uuid::new_v4().to_string()
    } else {
        execution_id
    };
    let service_tier = or(&record.service_tier, "default");
    let (session, parent) = sessions(client);
    let detail = ensure_breakdown(record.detail.clone(), &record.provider, &record.executor_type);
    let b = detail.breakdown.unwrap_or_else(|| unclassified(0));
    let (fail_status, fail_body) = if record.failed {
        let status = if record.fail_status > 0 {
            record.fail_status
        } else {
            500
        };
        (status, record.fail_body.trim().to_owned())
    } else {
        (200, String::new())
    };
    let mut w = Writer(Vec::with_capacity(1024), false);
    w.0.push(b'{');
    w.str("timestamp", &record.timestamp);
    w.int("latency_ms", record.latency_ms);
    w.int("ttft_ms", record.ttft_ms);
    w.str("source", &record.source);
    w.str("auth_index", &record.auth_index);
    w.opt_str("access_token_sha256", &record.access_token_sha256);
    w.str("client_ip", &client.client_ip);
    w.str("resolved_client_ip", &client.resolved_client_ip);
    w.str("x_forwarded_for", &client.x_forwarded_for);
    w.str("user_agent", &client.user_agent);
    w.object("tokens", |t| {
        t.int("input_tokens", detail.input);
        t.int("output_tokens", detail.output);
        t.int("reasoning_tokens", detail.reasoning);
        t.int("cached_tokens", detail.cached);
        t.int("cache_read_tokens", detail.cache_read);
        t.bool("cache_read_tokens_present", true);
        t.int("cache_creation_tokens", detail.cache_creation);
        if let Some(n) = detail.cache_creation_5m {
            t.int("cache_creation_5m_tokens", n);
        }
        if let Some(n) = detail.cache_creation_1h {
            t.int("cache_creation_1h_tokens", n);
        }
        t.int("total_tokens", detail.total);
    });
    if detail.web_search_requests.is_some() || detail.web_fetch_requests.is_some() {
        // Not in Go v8.0.35's record: provider-reported server tool counts (U1, #99).
        w.object("server_tool_use", |s| {
            if let Some(n) = detail.web_search_requests {
                s.int("web_search_requests", n);
            }
            if let Some(n) = detail.web_fetch_requests {
                s.int("web_fetch_requests", n);
            }
        });
    }
    // Not in Go v8.0.35's record: xAI's reported cost and server-side tool counts (#178).
    if let Some(ticks) = detail.cost_in_usd_ticks {
        w.int("cost_in_usd_ticks", ticks);
    }
    if !detail.server_side_tools.is_empty() {
        w.object("server_side_tool_usage_details", |s| {
            for (name, n) in &detail.server_side_tools {
                s.int(name, *n);
            }
        });
    }
    w.bool("failed", record.failed);
    if let Some(observed) = record.usage_observed {
        w.bool("usage_observed", observed);
    }
    w.bool("generate", record.generate);
    w.bool("stream", record.stream);
    w.object("fail", |f| {
        f.int("status_code", fail_status);
        f.str("body", &fail_body);
        // Go `failDetail.Code`, `json:"code,omitempty"`.
        if record.failed && !record.fail_code.is_empty() {
            f.str("code", record.fail_code);
        }
    });
    if !record.response_headers.is_empty() {
        let mut headers = record.response_headers.clone();
        headers.sort_by(|a, b| a.0.cmp(&b.0));
        w.object("response_headers", |h| {
            for (name, values) in &headers {
                h.key(name);
                h.0.push(b'[');
                for (i, v) in values.iter().enumerate() {
                    if i > 0 {
                        h.0.push(b',');
                    }
                    crate::protocol::json::marshal_str(&mut h.0, v.as_bytes(), true);
                }
                h.0.push(b']');
            }
        });
    }
    w.int("accounting_version", 2);
    w.object("token_breakdown", |t| {
        t.int("schema_version", 2);
        t.str(
            "quality",
            match b.quality {
                Quality::Complete => "complete",
                Quality::Inconsistent => "inconsistent",
                Quality::Unclassified => "unclassified",
            },
        );
        t.int("total_tokens", b.total);
        t.object("input", |i| {
            i.int("total_tokens", b.input[0]);
            i.int("uncached_tokens", b.input[1]);
            i.int("cache_read_tokens", b.input[2]);
            i.int("cache_write_tokens", b.input[3]);
        });
        t.object("output", |o| {
            o.int("total_tokens", b.output[0]);
            o.int("non_reasoning_tokens", b.output[1]);
            o.int("reasoning_tokens", b.output[2]);
        });
        t.int("unclassified_tokens", b.unclassified);
    });
    w.str("provider", &provider);
    w.str("executor_type", &executor_type);
    w.str("model", &model);
    w.str("alias", &alias);
    w.str("endpoint", client.endpoint.trim());
    w.str("auth_type", &auth_type);
    w.str("api_key", client.api_key.trim());
    w.bool("is_native_key", is_native_access_provider(&client.access_provider));
    w.opt_str("access_provider", client.access_provider.trim());
    w.str("request_id", request_id);
    w.opt_str("execution_id", &execution_id);
    w.opt_str("trace_id", request_id);
    w.opt_str("session_id", &session);
    w.opt_str("parent_session_id", &parent);
    w.opt_str("node_kind", client.node_kind.trim());
    if client.is_fork {
        w.bool("is_fork", true);
    }
    if client.is_compaction {
        w.bool("is_compaction", true);
    }
    w.str("reasoning_effort", record.reasoning_effort.trim());
    w.str("service_tier", &service_tier);
    w.opt_str("response_service_tier", detail.response_service_tier.trim());
    w.opt_str("response_model", record.response_model.trim());
    // Not in Go's record (docs/DIFFERENCES-FROM-GO.md).
    w.opt_str("session_source", client.session_source);
    w.opt_str("prompt_prefix_hash", &record.prompt_prefix_hash);
    if let Some(cache) = &record.claude_cache {
        w.key("claude_cache");
        serde_json::to_writer(&mut w.0, cache).expect("cache telemetry contains only JSON primitives");
    }
    record.timings.write(&mut w);
    w.0.push(b'}');
    w.0
}

// ---- per-attempt tracking ------------------------------------------------------

/// Request-level facts shared by every attempt of one request (Go's handler context:
/// client metadata, requested alias, reasoning effort, service tier, generate, stream).
#[derive(Debug, Clone)]
pub struct Facts {
    pub client: Client,
    /// Client response format, for parsing the client-visible response.
    pub format: Format,
    pub alias: String,
    pub reasoning_effort: String,
    pub service_tier: String,
    pub generate: bool,
    pub stream: bool,
    /// When the request arrived: the origin of every attempt's [`Timings`].
    pub clock: std::sync::Arc<crate::gateway::observability::Clock>,
    /// [`prompt_prefix`] of the client body.
    pub prompt_prefix_hash: String,
    /// Shared by failover/refresh attempts, independent of credential changes.
    pub attempts: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl Facts {
    /// Go `setReasoningEffortMetadata`, `setServiceTierMetadata` and
    /// `setGenerateMetadata` over the client body; the alias is the client's model. The
    /// request's clock and prompt prefix come from its `trace`.
    pub fn new(
        client: Client,
        entry: Format,
        response: Format,
        model: &str,
        body: &[u8],
        stream: bool,
        trace: &crate::gateway::dispatch::Trace,
    ) -> Self {
        let generate = gj::get(body, "generate");
        Self {
            client,
            format: response,
            alias: model.trim().to_owned(),
            reasoning_effort: crate::protocol::thinking::extract_reasoning_effort(body, entry.as_str(), model),
            service_tier: requested_tier(body),
            generate: !(generate.is_bool() && !generate.bool()),
            stream,
            clock: trace.clock(),
            prompt_prefix_hash: trace.prompt_prefix(entry, body),
            attempts: Default::default(),
        }
    }

    /// The facts of a later request on the same connection, from its frame (in the
    /// connection's format, `format`): its model (the connection's alias when it names
    /// none), reasoning effort, service tier, `generate` and prompt prefix, as
    /// [`Facts::new`] reads them. The client, clock, stream mode and attempt counter stay
    /// the connection's.
    fn successor(&self, body: &[u8]) -> Self {
        let model = gj::get(body, "model");
        let model = model.str();
        let alias = match model.trim() {
            "" => self.alias.clone(),
            model => model.to_owned(),
        };
        let generate = gj::get(body, "generate");
        Self {
            client: self.client.clone(),
            format: self.format,
            reasoning_effort: crate::protocol::thinking::extract_reasoning_effort(body, self.format.as_str(), &alias),
            alias,
            service_tier: requested_tier(body),
            generate: !(generate.is_bool() && !generate.bool()),
            stream: self.stream,
            clock: self.clock.clone(),
            prompt_prefix_hash: prompt_prefix(self.format, body),
            attempts: self.attempts.clone(),
        }
    }
}

/// Go `setServiceTierMetadata`: the request's `service_tier`, `auto` when it names none.
fn requested_tier(body: &[u8]) -> String {
    let tier = gj::get(body, "service_tier");
    match tier.str().trim() {
        "" => "auto".into(),
        tier => tier.to_owned(),
    }
}

/// A fingerprint of the client's system prompt and tool definitions as it sent them
/// (before translation or cloaking, `cache_control` markers included): the first 64
/// bits of a SHA-256 over the raw `tools` and system values, in hex. Requests with the
/// same value share their cacheable prefix up to the messages; a different mode or a
/// subagent shows as another value. Nothing of the text itself is kept. Empty when the
/// body has neither value, or for a format whose system prompt sits among its messages.
/// Cost: one walk over the body's top-level members (large values are skipped by
/// bracket matching; nothing is decoded or copied), SHA-256 over the two values, and
/// one 16-byte string.
// ponytail: Chat Completions bodies get none (their system
// prompt is a message or sits in an envelope); add a lookup when a client of theirs
// needs one.
pub(crate) fn prompt_prefix(format: Format, body: &[u8]) -> String {
    use sha2::Digest;
    let system: &[&[u8]] = match format {
        Format::Claude => &[b"system"],
        Format::OpenAIResponse | Format::Codex => &[b"instructions"],
        _ => return String::new(),
    };
    let (mut tools, mut instructions) = (None, None);
    gj::object_members(body, |key, escaped, value| {
        if !escaped && key == b"tools" {
            tools.get_or_insert(value);
        } else if !escaped && system.contains(&key) {
            instructions.get_or_insert(value);
        }
        tools.is_none() || instructions.is_none()
    });
    if tools.is_none() && instructions.is_none() {
        return String::new();
    }
    let mut hash = sha2::Sha256::new();
    hash.update(b"cpa:prompt-prefix:v1\0");
    for value in [tools, instructions] {
        // Length first, so the boundary between the two values is unambiguous.
        let value = value.unwrap_or_default();
        hash.update((value.len() as u64).to_le_bytes());
        hash.update(value);
    }
    let digest = hash.finalize();
    format!("{:016x}", u64::from_be_bytes(digest[..8].try_into().expect("8 bytes")))
}

/// Go executor identity for a credential's provider: `Identifier()` and the
/// executor's type name (both reach the record and pick the token semantics).
// ponytail: Codex attempts over the upstream WebSocket report `CodexExecutor`; Go
// names them `CodexWebsocketsExecutor`.
fn executor_identity(c: &crate::credential::Credential) -> (String, &'static str) {
    let provider = c.provider.trim().go_lower();
    let executor = match provider.as_str() {
        "claude" => "ClaudeExecutor",
        "codex" => "CodexExecutor",
        "gemini" | "gemini-interactions" => "GeminiExecutor",
        "vertex" => "GeminiVertexExecutor",
        "aistudio" => "AIStudioExecutor",
        "antigravity" => "AntigravityExecutor",
        "kimi" => "KimiExecutor",
        "meta" => "MetaExecutor",
        "xai" => "XAIExecutor",
        "devin" => "DevinExecutor",
        _ => {
            return (crate::catalog::dynamic::provider_key(c), "OpenAICompatExecutor");
        }
    };
    (provider, executor)
}

/// Go's TTFT marks (`StartResponseTTFT`, `MarkFirstResponseByte`,
/// `ObserveTokenEvent`), taken only when the executor reports them.
#[derive(Default)]
struct Ttft {
    /// The executor reports marks; the server's own approximation is not used.
    tracked: bool,
    start: Option<std::time::Instant>,
    ttft: Option<std::time::Duration>,
    first_packet: Option<std::time::Duration>,
}

impl Ttft {
    fn start(&mut self) {
        self.tracked = true;
        if self.ttft.is_none() && self.start.is_none() {
            self.start = Some(std::time::Instant::now());
        }
    }

    fn first_byte(&mut self) {
        self.tracked = true;
        if self.ttft.is_none()
            && let Some(start) = self.start.take()
        {
            self.ttft = Some(start.elapsed());
        }
    }

    fn token(&mut self, is_token: bool) {
        self.tracked = true;
        let Some(start) = self.start.filter(|_| self.ttft.is_none()) else {
            return;
        };
        if !is_token && self.first_packet.is_some() {
            return;
        }
        let elapsed = start.elapsed();
        self.first_packet.get_or_insert(elapsed);
        if is_token {
            self.ttft = Some(elapsed);
            self.start = None;
        }
    }

    /// Go `ttftDuration`: TTFT, else the first packet, else zero.
    fn get(&self) -> std::time::Duration {
        self.ttft.or(self.first_packet).unwrap_or_default()
    }
}

/// An attempt outcome the executor published itself (Go `Publish`/`PublishFailure`),
/// with what Go's `buildRecord` reads at that moment.
struct Outcome {
    failed: bool,
    /// `detail` is usage the provider reported, not an absence of usage.
    observed: bool,
    status: i64,
    body: String,
    detail: Detail,
    model: String,
    latency: std::time::Duration,
    ttft: std::time::Duration,
}

/// The upstream exchange's stages as the shared HTTP client reported them. Apart from
/// its start, they describe the last hop sent: a new hop starts over, and what an
/// earlier hop reports late is dropped.
#[derive(Default)]
struct Exchange {
    started: Option<std::time::Instant>,
    hop: u32,
    connected: Option<std::time::Instant>,
    body_sent: Option<std::time::Instant>,
    headers: Option<std::time::Instant>,
    first_byte: Option<std::time::Instant>,
    /// The request went out on a pooled connection; unknown after a dial.
    reused: Option<bool>,
    request_bytes: Option<u64>,
    response_bytes: Option<u64>,
}

impl Exchange {
    fn mark(&mut self, stage: crate::execution::UpstreamStage, now: std::time::Instant) {
        use crate::execution::UpstreamStage;
        match stage {
            UpstreamStage::Started => {
                self.started.get_or_insert(now);
            }
            UpstreamStage::Hop { hop, body } => {
                *self = Self {
                    started: self.started,
                    hop,
                    // A hop without a body sends none; one with a body says how much
                    // once it is handed over.
                    request_bytes: (!body).then_some(0),
                    ..Self::default()
                };
            }
            UpstreamStage::Connected { hop } if hop == self.hop => self.connected = Some(now),
            UpstreamStage::BodySent { hop, bytes } if hop == self.hop => {
                self.body_sent = Some(now);
                self.request_bytes = Some(bytes);
            }
            // An earlier hop's body, taken after its redirect arrived.
            UpstreamStage::Connected { .. } | UpstreamStage::BodySent { .. } => {}
            UpstreamStage::Headers { dialled } => {
                self.headers = Some(now);
                self.reused = (!dialled).then_some(true);
            }
            UpstreamStage::FirstByte => self.first_byte = Some(now),
            UpstreamStage::BodyRead { bytes } => self.response_bytes = Some(bytes),
        }
    }
}

/// What an executor reported through `ExecRequest::usage`.
#[derive(Default)]
struct Reported {
    seen: bool,
    body: Option<Detail>,
    stream: StreamUsage,
    model: ResponseModel,
    effort: Option<String>,
    /// The service tier the request carried upstream ([`UsageObserver::request_sent`]).
    ///
    /// [`UsageObserver::request_sent`]: crate::execution::UsageObserver::request_sent
    sent_tier: Option<String>,
    upstream_model: String,
    ttft: Ttft,
    usage_required: bool,
    outcome: Option<Outcome>,
    /// Go created no reporter for this attempt: publish nothing. Also set once the
    /// attempt's report was taken for its one record, or handed to a detached record.
    discarded: bool,
    exchange: Exchange,
    claude_cache: Option<crate::execution::ClaudeCacheTelemetry>,
    /// The executor type and model of the built-in executor the attempt was handed to.
    delegated: Option<(String, String)>,
    /// For a record of its own ([`Own`]): when its first upstream payload arrived, from
    /// the record's start.
    first: Option<std::time::Duration>,
}

impl Reported {
    /// The usage reported so far: a body, else the stream's last usage.
    fn usage(&self) -> Option<Detail> {
        self.body.clone().or_else(|| self.stream.detail().cloned())
    }

    /// What is left once a report was taken: a later publish or detach finds nothing.
    fn taken() -> Self {
        Self {
            discarded: true,
            ..Self::default()
        }
    }
}

/// What every record of one attempt shares: where records go, the request, and the
/// credential and model the attempt runs on, with what it learned while it ran (the
/// upstream's response headers, the session an LCP pick bound). The attempt's
/// [`Tracker`] publishes its one record from it. Once the executor detached the attempt
/// ([`crate::execution::UsageObserver::detach`]), the Tracker publishes nothing and each
/// upstream response's record ([`Own`]) publishes from it instead.
struct Origin {
    queue: std::sync::Arc<crate::runtime::Runtime>,
    facts: std::sync::Arc<Facts>,
    auth_id: String,
    /// The record before any outcome ([`Tracker::start`], [`Tracker::arrived`]).
    template: Record,
    /// The request's client facts under this attempt's session, when it has its own:
    /// Home's canonical session for a dispatched credential, else its LCP pick's.
    client: Option<Client>,
    /// Go `requestedAt`.
    requested_at: chrono::DateTime<chrono::FixedOffset>,
    attempt: usize,
    /// The authentication values the upstream request or socket serving the attempt
    /// carried ([`crate::execution::UsageObserver::sent_credentials`]). Every record of the
    /// attempt keeps nothing of the upstream's answer that carries them ([`finish`]): its
    /// own, a detached one and each successor on the same connection, published before or
    /// after the others. The binding belongs to the connection, not to a report, so taking
    /// a report for its record never takes it.
    sent: crate::execution::SentCredentials,
}

/// A record that publishes itself, for one upstream response on a connection that
/// carries several: once, when the executor settles it, or when dropped unsettled.
struct Own {
    /// The facts of the request that asked for this response.
    facts: std::sync::Arc<Facts>,
    /// When the record started.
    requested_at: chrono::DateTime<chrono::FixedOffset>,
    /// The attempt's own response, detached: it keeps the attempt's execution ID and
    /// timings. A successor gets its own execution ID and no timings, which measure the
    /// attempt's request.
    detached: bool,
    published: std::sync::atomic::AtomicBool,
}

/// The executor-facing observer; `provider` picks the response-model extractor.
struct Observer {
    provider: String,
    started: std::time::Instant,
    cache_telemetry: bool,
    state: std::sync::Mutex<Reported>,
    origin: std::sync::Arc<std::sync::Mutex<Origin>>,
    /// `None` for an attempt, which its [`Tracker`] publishes.
    own: Option<Own>,
}

/// The clock a record measures from and the attempt it belongs to.
struct Stamp<'a> {
    started: std::time::Instant,
    requested_at: chrono::DateTime<chrono::FixedOffset>,
    attempt: usize,
    client: &'a Client,
    auth_id: &'a str,
    /// [`Origin::sent`].
    sent: &'a crate::execution::SentCredentials,
}

impl Observer {
    fn lock(&self) -> std::sync::MutexGuard<'_, Reported> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Lock order: the origin before the report.
    fn origin(&self) -> std::sync::MutexGuard<'_, Origin> {
        self.origin.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Everything reported so far, for the one record that publishes it.
    fn take(&self) -> Reported {
        std::mem::replace(&mut *self.lock(), Reported::taken())
    }

    /// A record of its own notes its first upstream payload.
    fn first_payload(&self, r: &mut Reported) {
        if self.own.is_some() && r.first.is_none() {
            r.first = Some(self.started.elapsed());
        }
    }

    /// `keep_usage`: a failure that keeps the usage reported so far.
    fn settle(&self, failed: bool, keep_usage: bool, status: i64, body: &str) {
        {
            let mut r = self.lock();
            if r.outcome.is_some() {
                return;
            }
            let usage = if failed && !keep_usage { None } else { r.usage() };
            r.outcome = Some(Outcome {
                failed,
                observed: usage_reported(usage.as_ref()),
                status,
                body: body.trim().to_owned(),
                detail: usage.unwrap_or_default(),
                model: r.model.get().to_owned(),
                latency: self.started.elapsed(),
                ttft: if self.own.is_some() && !r.ttft.tracked {
                    r.first.unwrap_or_default()
                } else {
                    r.ttft.get()
                },
            });
        }
        self.publish_own();
    }

    /// A record of its own: publishes, once, its settled outcome, or, settled by nobody,
    /// what it observed (an attempt's Tracker does the same with `Ending::Done`).
    fn publish_own(&self) {
        let Some(own) = &self.own else { return };
        if own.published.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return;
        }
        let origin = self.origin();
        let queue = origin.queue.usage_queue();
        if !queue.accepts() {
            return;
        }
        let mut reported = self.take();
        if reported.discarded {
            return;
        }
        let facts = &own.facts;
        let mut record = origin.template.clone();
        if !own.detached {
            record.execution_id = uuid::Uuid::new_v4().to_string();
        }
        record.timestamp = go_timestamp(&own.requested_at);
        record.alias.clone_from(&facts.alias);
        record.reasoning_effort.clone_from(&facts.reasoning_effort);
        record.service_tier.clone_from(&facts.service_tier);
        record.generate = facts.generate;
        record.stream = facts.stream;
        if let Some((executor_type, model)) = reported.delegated.clone() {
            (record.executor_type, record.model) = (executor_type, model);
        }
        let (latency, ttft);
        if let Some(outcome) = reported.outcome.take() {
            record.failed = outcome.failed;
            record.fail_status = outcome.status;
            record.fail_body = outcome.body;
            record.detail = outcome.detail;
            if !outcome.observed {
                record.usage_observed = Some(false);
            }
            record.response_model = outcome.model;
            (latency, ttft) = (outcome.latency, outcome.ttft);
        } else {
            let usage = reported.usage();
            record.detail = if reported.usage_required && usage.is_none() {
                if reported.claude_cache.is_none() {
                    return;
                }
                record.usage_observed = Some(false);
                Detail::default()
            } else {
                if !usage_reported(usage.as_ref()) {
                    record.usage_observed = Some(false);
                }
                usage.unwrap_or_default()
            };
            record.response_model = reported.model.get().to_owned();
            latency = self.started.elapsed();
            ttft = if reported.ttft.tracked {
                reported.ttft.get()
            } else {
                reported.first.unwrap_or_default()
            };
        }
        record.timings = if own.detached {
            timings(&origin.facts, &reported.exchange)
        } else {
            Timings::default()
        };
        record.prompt_prefix_hash.clone_from(&facts.prompt_prefix_hash);
        let stamp = Stamp {
            started: self.started,
            requested_at: own.requested_at,
            attempt: origin.attempt,
            client: origin.client.as_ref().unwrap_or(&facts.client),
            auth_id: &origin.auth_id,
            sent: &origin.sent,
        };
        finish(queue, record, reported, latency, ttft, &stamp);
    }

    /// A record of its own on this observer's origin.
    fn record(&self, facts: std::sync::Arc<Facts>, reported: Reported, detached: bool) -> Self {
        let (started, requested_at) = if detached {
            (self.started, self.origin().requested_at)
        } else {
            (std::time::Instant::now(), chrono::Local::now().fixed_offset())
        };
        Self {
            provider: self.provider.clone(),
            started,
            cache_telemetry: self.cache_telemetry,
            state: std::sync::Mutex::new(reported),
            origin: self.origin.clone(),
            own: Some(Own {
                facts,
                requested_at,
                detached,
                published: Default::default(),
            }),
        }
    }
}

impl Drop for Observer {
    fn drop(&mut self) {
        self.publish_own();
    }
}

impl crate::execution::UsageObserver for Observer {
    fn wants_claude_cache(&self) -> bool {
        self.cache_telemetry
    }

    fn claude_cache(&self, telemetry: crate::execution::ClaudeCacheTelemetry) {
        self.lock().claude_cache = Some(telemetry);
    }

    fn response_body(&self, format: Format, body: &[u8]) {
        let mut r = self.lock();
        self.first_payload(&mut r);
        r.seen = true;
        r.body = Some(parse_body(format, body));
        r.model.observe(body, &self.provider);
    }

    fn response_line(&self, format: Format, line: &[u8]) {
        let mut r = self.lock();
        self.first_payload(&mut r);
        r.seen = true;
        r.stream.line(format, line);
        r.model.observe(line, &self.provider);
    }

    fn response_usage_line(&self, format: Format, line: &[u8]) {
        let mut r = self.lock();
        self.first_payload(&mut r);
        r.seen = true;
        r.stream.line(format, line);
    }

    fn delegated_executor(&self, executor_type: &str, model: &str) {
        self.lock().delegated = Some((executor_type.to_owned(), model.to_owned()));
    }

    fn request(&self, format: Format, payload: &[u8]) {
        self.request_for(format.as_str(), payload);
    }

    fn request_for(&self, identifier: &str, payload: &[u8]) {
        self.lock().effort = Some(crate::protocol::thinking::extract_translated_reasoning_effort(
            payload, identifier,
        ));
    }

    fn request_sent(&self, payload: &[u8]) {
        self.lock().sent_tier = Some(requested_tier(payload));
    }

    fn upstream_model(&self, model: &str) {
        self.lock().upstream_model = model.trim().to_owned();
    }

    fn response_model(&self, model: &str) {
        self.lock().model.set(model);
    }

    fn response_model_line(&self, line: &[u8]) {
        self.lock().model.observe(line, &self.provider);
    }

    fn round_trip_started(&self) {
        self.lock().ttft.start();
    }

    fn first_byte(&self) {
        self.lock().ttft.first_byte();
    }

    fn token_event(&self, is_token: bool) {
        self.lock().ttft.token(is_token);
    }

    fn publish(&self) {
        self.settle(false, false, 0, "");
    }

    fn publish_failure(&self, status: u16, body: &str) {
        self.settle(true, false, i64::from(status), body);
    }

    fn publish_failure_with_usage(&self, status: u16, body: &str) {
        self.settle(true, true, i64::from(status), body);
    }

    fn usage_required(&self) {
        self.lock().usage_required = true;
    }

    fn discard(&self) {
        self.lock().discarded = true;
    }

    fn upstream_stage(&self, stage: crate::execution::UpstreamStage) {
        let now = std::time::Instant::now();
        self.lock().exchange.mark(stage, now);
    }

    /// The attempt's report so far moves to the record, which keeps the attempt's
    /// start, execution ID and timings; the attempt is then discarded, so its Tracker
    /// publishes nothing. An attempt already published or discarded detaches nothing.
    fn detach(&self) -> Option<std::sync::Arc<dyn crate::execution::UsageObserver>> {
        if self.own.is_some() {
            return None;
        }
        let reported = {
            let mut r = self.lock();
            if r.discarded {
                return None;
            }
            std::mem::replace(&mut *r, Reported::taken())
        };
        let facts = self.origin().facts.clone();
        Some(std::sync::Arc::new(self.record(facts, reported, true)))
    }

    fn successor(&self, request: Option<&[u8]>) -> Option<std::sync::Arc<dyn crate::execution::UsageObserver>> {
        let own = self.own.as_ref()?;
        let facts = match request {
            Some(body) => std::sync::Arc::new(own.facts.successor(body)),
            None => own.facts.clone(),
        };
        Some(std::sync::Arc::new(self.record(facts, Reported::default(), false)))
    }

    fn sent_credentials(&self, sent: &crate::execution::SentCredentials) {
        self.origin().sent = sent.clone();
    }
}

/// The record's [`Timings`]: each mark as an offset from the request's arrival.
fn timings(facts: &Facts, exchange: &Exchange) -> Timings {
    let clock = &facts.clock;
    let accepted = clock.accepted();
    let ms = |at: Option<std::time::Instant>| {
        at.map(|at| i64::try_from(at.saturating_duration_since(accepted).as_millis()).unwrap_or(i64::MAX))
    };
    let bytes = |n: Option<u64>| n.map(|n| i64::try_from(n).unwrap_or(i64::MAX));
    let body = clock.body_received();
    Timings {
        client_body_received_ms: ms(body.map(|(at, _)| at)),
        upstream_started_ms: ms(exchange.started),
        upstream_connected_ms: ms(exchange.connected),
        upstream_connection_reused: exchange.reused,
        upstream_body_sent_ms: ms(exchange.body_sent),
        upstream_headers_ms: ms(exchange.headers),
        upstream_first_byte_ms: ms(exchange.first_byte),
        client_first_byte_ms: ms(clock.response_started()),
        client_request_bytes: bytes(body.map(|(_, n)| n)),
        upstream_request_bytes: bytes(exchange.request_bytes),
        upstream_response_bytes: bytes(exchange.response_bytes),
    }
}

/// What every record takes from its report once its outcome, detail, timings and prompt
/// prefix are set: latency, TTFT, returned cache buckets, Claude cache telemetry, the
/// reported reasoning effort and the service tier sent. Then it is queued.
fn finish(
    queue: &crate::gateway::usage::UsageQueue,
    mut record: Record,
    reported: Reported,
    latency: std::time::Duration,
    ttft: std::time::Duration,
    stamp: &Stamp<'_>,
) {
    // #25: nothing of the upstream's answer keeps a value its request or socket carried.
    // Status, scope and every other field stay as classified.
    for text in record.upstream_text() {
        if let std::borrow::Cow::Owned(scrubbed) = stamp.sent.scrub_str(text) {
            *text = scrubbed;
        }
    }
    // Go can publish a tokenless failure after observing a Claude response.
    // Keep the returned TTL buckets as observations, not inferred accounting.
    let returned_cache = reported.usage().map(|d| (d.cache_creation_5m, d.cache_creation_1h));
    record.latency_ms = latency.as_millis() as i64;
    record.ttft_ms = ttft.as_millis() as i64;
    if let Some((five, hour)) = returned_cache {
        record.detail.cache_creation_5m = five;
        record.detail.cache_creation_1h = hour;
    }
    if let Some(mut cache) = reported.claude_cache {
        let at = |instant: Option<std::time::Instant>| {
            instant.map(|instant| {
                let offset = instant.saturating_duration_since(stamp.started).as_millis() as i64;
                stamp.requested_at.timestamp_millis().saturating_add(offset)
            })
        };
        cache.request_start_unix_ms = at(reported.exchange.started);
        cache.first_response_byte_unix_ms = at(reported.exchange.first_byte);
        // Completion of this upstream attempt, not the downstream socket flush.
        cache.end_unix_ms = Some(stamp.requested_at.timestamp_millis().saturating_add(record.latency_ms));
        cache.attempt = stamp.attempt;
        if stamp.client.is_compaction {
            cache.base_request_type = "compaction".into();
            if cache.request_type != "retry" {
                cache.request_type = "compaction".into();
            }
        }
        if stamp.attempt > 0 {
            cache.request_type = "retry".into();
        }
        record.claude_cache = Some(cache);
    }
    if let Some(effort) = reported.effort {
        record.reasoning_effort = effort;
    }
    if let Some(tier) = reported.sent_tier {
        record.service_tier = tier;
    }
    queue.enqueue(queued(&record, stamp.client));
    warn_model_substitution(&record, &reported.upstream_model, stamp.auth_id);
}

/// How the attempt ended for the server.
enum Ending<'a> {
    /// Success, or a stream the client dropped (Go's deferred publish).
    Done,
    /// The error and its request-scoped rule classification (`Record::fail_code`).
    Failed(&'a crate::execution::ExecError, &'static str),
}

/// One upstream attempt's record in progress. It publishes at most once: what the
/// executor published itself, else on success, on failure, or when dropped
/// mid-stream (Go's deferred publish). An attempt the executor detached publishes
/// nothing here: its responses publish their own records.
pub struct Tracker {
    facts: std::sync::Arc<Facts>,
    started: std::time::Instant,
    observer: std::sync::Arc<Observer>,
    stream: StreamUsage,
    body: Option<Detail>,
    model: ResponseModel,
    first: Option<std::time::Duration>,
    published: bool,
}

impl Tracker {
    /// The attempt's session is its LCP pick's (Go `syncMetadataSessionToContext`), not
    /// the request's.
    pub fn lcp_session(&mut self, m: &crate::gateway::lcp::Match) {
        let bound = crate::protocol::session::bound_session_identity;
        let mut client = self.facts.client.clone();
        client.session_id = bound(&m.session);
        client.parent_session_id = if m.parent.is_empty() {
            String::new()
        } else {
            bound(&m.parent)
        };
        let (node_kind, fork, compaction) = m.node();
        client.node_kind = node_kind.into();
        client.is_fork = fork;
        client.is_compaction = compaction;
        client.session_source = "lcp";
        self.observer.origin().client = Some(client);
    }

    /// Starts the record for one attempt of `credential` on `upstream_model`.
    pub fn start(
        rt: &std::sync::Arc<crate::runtime::Runtime>,
        facts: &std::sync::Arc<Facts>,
        credential: &crate::credential::Credential,
        upstream_model: &str,
    ) -> Self {
        let (provider, executor_type) = executor_identity(credential);
        let now = chrono::Local::now();
        let record = Record {
            timestamp: go_timestamp(&now),
            execution_id: uuid::Uuid::new_v4().to_string(),
            executor_type: executor_type.to_owned(),
            model: crate::protocol::thinking::parse_suffix(upstream_model).model_name,
            alias: facts.alias.clone(),
            source: source(credential, &facts.client.api_key),
            auth_index: crate::config::credentials::auth_index(credential),
            access_token_sha256: access_token_sha256(credential),
            auth_type: crate::catalog::dynamic::auth_kind(credential)
                .unwrap_or_default()
                .to_owned(),
            reasoning_effort: facts.reasoning_effort.clone(),
            service_tier: facts.service_tier.clone(),
            generate: facts.generate,
            stream: facts.stream,
            provider: provider.clone(),
            ..Record::default()
        };
        let started = std::time::Instant::now();
        let home_client = match home_session(&facts.client, credential) {
            std::borrow::Cow::Owned(client) => Some(client),
            std::borrow::Cow::Borrowed(_) => None,
        };
        let origin = Origin {
            queue: rt.clone(),
            facts: facts.clone(),
            auth_id: credential.id.clone(),
            template: record,
            client: home_client,
            requested_at: now.fixed_offset(),
            attempt: facts.attempts.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            sent: crate::execution::SentCredentials::default(),
        };
        Self {
            facts: facts.clone(),
            started,
            observer: std::sync::Arc::new(Observer {
                provider,
                started,
                cache_telemetry: rt.usage_queue().wants_claude_cache(),
                state: std::sync::Mutex::default(),
                origin: std::sync::Arc::new(std::sync::Mutex::new(origin)),
                own: None,
            }),
            stream: StreamUsage::default(),
            body: None,
            model: ResponseModel::default(),
            first: None,
            published: false,
        }
    }

    /// The sink to hand the executor.
    pub fn sink(&self) -> crate::execution::UsageSink {
        crate::execution::UsageSink::new(self.observer.clone())
    }

    /// The upstream answered; its headers are the record's `response_headers`.
    // ponytail: without the executor's TTFT marks (`UsageSink::round_trip_started` and
    // `first_byte`), TTFT is measured to the executor's response (buffered) or first
    // event (streams); Go measures from the upstream request to its first body byte.
    pub fn arrived(&mut self, headers: &axum::http::HeaderMap) {
        self.first.get_or_insert_with(|| self.started.elapsed());
        self.observer.origin().template.response_headers = go_headers(headers);
    }

    /// The buffered client-format body (or the joined events of a stream the client
    /// did not ask to stream).
    pub fn body(&mut self, body: &[u8]) {
        if body.trim_ascii_start().first() == Some(&b'{') {
            self.body = Some(parse_body(self.facts.format, body));
            self.model.observe(body, &self.observer.provider);
        } else {
            for line in body.split(|b| *b == b'\n') {
                self.line(line);
            }
        }
    }

    /// One client-format stream event.
    pub fn event(&mut self, event: &[u8]) {
        self.first.get_or_insert_with(|| self.started.elapsed());
        for line in event.split(|b| *b == b'\n') {
            self.line(line);
        }
    }

    fn line(&mut self, line: &[u8]) {
        if line.trim_ascii().is_empty() {
            return;
        }
        self.stream.line(self.facts.format, line);
        self.model.observe(line, &self.observer.provider);
    }

    pub fn succeed(mut self) {
        self.publish(Ending::Done);
    }

    /// The error's status and body, with the usage the provider reported before the
    /// failure (U1, #99; Go v8.0.35 kept it only for Claude).
    pub fn fail(self, error: &crate::execution::ExecError) {
        self.fail_classified(error, "");
    }

    /// [`Self::fail`] with the failure's request-scoped rule classification
    /// ([`Record::fail_code`]).
    pub fn fail_classified(mut self, error: &crate::execution::ExecError, code: &'static str) {
        self.publish(Ending::Failed(error, code));
    }

    fn publish(&mut self, ending: Ending<'_>) {
        if std::mem::replace(&mut self.published, true) {
            return;
        }
        let mut origin = self.observer.origin();
        let rt = origin.queue.clone();
        let queue = rt.usage_queue();
        if !queue.accepts() {
            return;
        }
        let mut reported = self.observer.take();
        if reported.discarded {
            return;
        }
        // Only an attempt that was never detached gets here, so no other record reads
        // the template.
        let mut record = std::mem::take(&mut origin.template);
        if let Some((executor_type, model)) = reported.delegated.clone() {
            (record.executor_type, record.model) = (executor_type, model);
        }
        if let Ending::Failed(_, code) = ending {
            record.fail_code = code;
        }
        let (latency, ttft);
        if let Some(outcome) = reported.outcome.take() {
            record.failed = outcome.failed;
            record.fail_status = outcome.status;
            record.fail_body = outcome.body;
            record.detail = outcome.detail;
            if !outcome.observed {
                record.usage_observed = Some(false);
            }
            record.response_model = outcome.model;
            (latency, ttft) = (outcome.latency, outcome.ttft);
        } else {
            let usage = if reported.seen {
                reported.usage()
            } else {
                self.body.take().or_else(|| self.stream.detail().cloned())
            };
            // Missing usage stays distinct from zero usage (U1, #99).
            if !usage_reported(usage.as_ref()) {
                record.usage_observed = Some(false);
            }
            let detail = match ending {
                Ending::Done if reported.usage_required && usage.is_none() => {
                    if reported.claude_cache.is_none() {
                        return;
                    }
                    // Keep activity and timestamps even when a Claude stream never
                    // returns usage.
                    record.usage_observed = Some(false);
                    Detail::default()
                }
                Ending::Done => usage.unwrap_or_default(),
                Ending::Failed(error, _) => {
                    record.failed = true;
                    record.fail_status = i64::from(crate::runtime::classify::go_status(error));
                    record.fail_body = crate::runtime::classify::error_text(error);
                    if record.response_headers.is_empty() {
                        record.response_headers = go_headers(&error.headers);
                    }
                    usage.unwrap_or_default()
                }
            };
            record.detail = detail;
            record.response_model = if reported.seen || !reported.model.get().is_empty() {
                reported.model.get().to_owned()
            } else {
                self.model.get().to_owned()
            };
            latency = self.started.elapsed();
            ttft = if reported.ttft.tracked {
                reported.ttft.get()
            } else {
                self.first.unwrap_or_default()
            };
        }
        record.timings = timings(&self.facts, &reported.exchange);
        record.prompt_prefix_hash = self.facts.prompt_prefix_hash.clone();
        let stamp = Stamp {
            started: self.started,
            requested_at: origin.requested_at,
            attempt: origin.attempt,
            client: origin.client.as_ref().unwrap_or(&self.facts.client),
            auth_id: &origin.auth_id,
            sent: &origin.sent,
        };
        finish(queue, record, reported, latency, ttft, &stamp);
    }
}

impl Drop for Tracker {
    /// Go's deferred publish: a stream the client dropped keeps what was observed. An
    /// attempt dropped before the upstream answered it ([`Tracker::arrived`]) served
    /// nothing, so it is never a success: it records the canceled failure an executor
    /// records for a request the client abandons (`crate::providers::cancel`: 499, `context
    /// canceled`), unless the executor settled its outcome first.
    fn drop(&mut self) {
        if self.first.is_some() {
            self.publish(Ending::Done);
        } else {
            let canceled = crate::execution::ExecError::local(
                CANCELED_STATUS,
                crate::execution::FailureScope::Request,
                CANCELED_BODY,
            );
            self.publish(Ending::Failed(&canceled, ""));
        }
    }
}

/// The record of an attempt abandoned before the upstream answered it, as
/// `crate::providers::cancel` records one: internal accounting only.
const CANCELED_STATUS: u16 = 499;
const CANCELED_BODY: &str = "context canceled";

/// Go's `http.Header` view of upstream headers: canonical names, values in order.
fn go_headers(headers: &axum::http::HeaderMap) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for name in headers.keys() {
        let values = headers
            .get_all(name)
            .iter()
            .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
            .collect();
        out.push((canonical_header(name.as_str()), values));
    }
    out
}

/// Go `textproto.CanonicalMIMEHeaderKey` for a valid token: upper-case the first
/// letter and each letter after `-`, lower-case the rest.
fn canonical_header(name: &str) -> String {
    let mut upper = true;
    name.chars()
        .map(|c| {
            let out = if upper {
                c.to_ascii_uppercase()
            } else {
                c.to_ascii_lowercase()
            };
            upper = c == '-';
            out
        })
        .collect()
}

/// Go `resolveUsageSource`: a Vertex project, the account (OAuth email or API key),
/// the metadata email or `api_key` attribute, else the client key.
pub fn source(c: &crate::credential::Credential, client_key: &str) -> String {
    let meta = |k: &str| c.str(k).map(str::trim).filter(|v| !v.is_empty());
    let attr = |k: &str| c.attributes.get(k).map(|v| v.trim()).filter(|v| !v.is_empty());
    if c.provider.trim().go_eq_fold("vertex")
        && let Some(project) = meta("project_id").or_else(|| meta("project"))
    {
        return project.to_owned();
    }
    let (_, account) = crate::catalog::dynamic::account_info(c);
    Some(account.as_str())
        .filter(|a| !a.is_empty())
        .or_else(|| meta("email"))
        .or_else(|| attr("api_key"))
        .unwrap_or(client_key.trim())
        .to_owned()
}

/// Go `AccessTokenSHA256`.
pub fn access_token_sha256(c: &crate::credential::Credential) -> String {
    crate::config::credentials::access_token_sha256(c)
}

/// Go `Auth.AuthSourceKind` over a dispatched auth's attributes. Its last fallback,
/// `FileName`, never reaches a Home auth: Go's auth JSON omits it.
fn auth_source_kind(c: &crate::credential::Credential) -> &'static str {
    let attr = |k: &str| c.attributes.get(k).map_or("", |v| v.trim());
    let normalize = |s: &str| match s.trim().go_lower().as_str() {
        "config" => Some("config"),
        "file" | "filesystem" => Some("file"),
        "git" => Some("git"),
        "memory" | "runtime" | "runtime_only" => Some("memory"),
        "objectstore" | "object-store" => Some("objectstore"),
        "postgres" | "postgresql" | "database" | "db" => Some("postgres"),
        _ => None,
    };
    if attr("runtime_only").eq_ignore_ascii_case("true") {
        return "memory";
    }
    if let Some(kind) = normalize(attr("source_backend")) {
        return kind;
    }
    let source = attr("source");
    if !source.is_empty() {
        if source.go_lower().starts_with("config:") {
            return "config";
        }
        return normalize(source).unwrap_or("file");
    }
    if attr("path").is_empty() { "" } else { "file" }
}

/// The request facts with the session Home dispatched under (Go
/// `syncMetadataSessionToContext` with `CanonicalSessionIDMetadataKey`): a dispatched
/// credential's canonical session and its parent replace the request's own.
fn home_session<'a>(client: &'a Client, credential: &crate::credential::Credential) -> std::borrow::Cow<'a, Client> {
    let attr = |k: &str| credential.attributes.get(k).map_or("", |v| v.trim());
    let session = attr(crate::gateway::remote::SESSION);
    if session.is_empty() {
        return std::borrow::Cow::Borrowed(client);
    }
    let parent = attr(crate::gateway::remote::PARENT_SESSION);
    let mut client = client.clone();
    client.session_id = session.to_owned();
    client.session_source = "home";
    client.parent_session_id = if parent == session {
        String::new()
    } else {
        parent.to_owned()
    };
    std::borrow::Cow::Owned(client)
}

/// Go `Manager.reportHomeUnauthorized`: a result-only, zero-token record for an
/// upstream 401 in Home mode that no executor reporter records (count-tokens attempts,
/// Codex Alpha Search). It needs an auth index and an access-token fingerprint, so API
/// keys publish nothing. `facts` carries what the request context holds: the alias,
/// reasoning effort and service tier (Go's context defaults when it holds none).
pub fn publish_home_unauthorized(
    rt: &crate::runtime::Runtime,
    facts: &Facts,
    credential: &crate::credential::Credential,
    provider: &str,
    model: &str,
    body: &str,
) {
    let queue = rt.usage_queue();
    if !queue.accepts() {
        return;
    }
    let auth_index = crate::config::credentials::auth_index(credential).trim().to_owned();
    let access_token_sha256 = access_token_sha256(credential);
    if auth_index.is_empty() || access_token_sha256.is_empty() {
        return;
    }
    let provider = match provider.trim() {
        "" => credential
            .attributes
            .get(crate::gateway::remote::PROVIDER)
            .map_or(credential.provider.as_str(), String::as_str)
            .trim(),
        provider => provider,
    };
    let model = model.trim();
    let alias = match facts.alias.trim() {
        "" => model,
        alias => alias,
    };
    let record = Record {
        timestamp: go_timestamp(&chrono::Local::now()),
        provider: provider.to_owned(),
        executor_type: "home-result".into(),
        model: model.to_owned(),
        alias: alias.to_owned(),
        source: auth_source_kind(credential).to_owned(),
        auth_index,
        access_token_sha256,
        auth_type: crate::catalog::dynamic::auth_kind(credential)
            .unwrap_or_default()
            .to_owned(),
        reasoning_effort: facts.reasoning_effort.trim().to_owned(),
        service_tier: facts.service_tier.trim().to_owned(),
        failed: true,
        fail_status: 401,
        fail_body: if body.is_empty() {
            "upstream unauthorized".into()
        } else {
            body.to_owned()
        },
        // No provider usage (U1, #99).
        usage_observed: Some(false),
        ..Record::default()
    };
    // Go 37fe2608: the record carries the request's client key and access provider.
    queue.enqueue(queued(&record, &home_session(&facts.client, credential)));
}

/// Go `time.Time.MarshalJSON` (RFC 3339 with trailing-zero-trimmed nanoseconds).
pub fn go_timestamp<Tz: chrono::TimeZone>(t: &chrono::DateTime<Tz>) -> String
where
    Tz::Offset: std::fmt::Display,
{
    use chrono::{Offset, Timelike};
    let mut out = t.format("%Y-%m-%dT%H:%M:%S").to_string();
    let nanos = t.nanosecond() % 1_000_000_000;
    if nanos != 0 {
        let frac = format!("{nanos:09}");
        out.push('.');
        out.push_str(frac.trim_end_matches('0'));
    }
    let offset = t.offset().fix().local_minus_utc();
    if offset == 0 {
        out.push('Z');
    } else {
        let sign = if offset < 0 { '-' } else { '+' };
        let abs = offset.abs();
        out.push_str(&format!("{sign}{:02}:{:02}", abs / 3600, (abs % 3600) / 60));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// U1 (#99): reported zero usage is usage, also when the terminal event names a
    /// service tier; it never replaces usage an earlier event reported.
    #[test]
    fn zero_usage_with_a_tier_is_reported_usage() {
        let zero = r#"data: {"type":"response.completed","response":{"service_tier":"default","usage":{"input_tokens":0,"output_tokens":0,"total_tokens":0}}}"#;
        let mut stream = StreamUsage::default();
        stream.line(Format::Codex, zero.as_bytes());
        assert!(usage_reported(stream.detail()), "{:?}", stream.detail());
        assert_eq!(stream.detail().unwrap().response_service_tier, "default");

        let mut chat = StreamUsage::default();
        for line in [
            r#"data: {"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}"#,
            r#"data: {"service_tier":"flex","usage":{"prompt_tokens":0,"completion_tokens":0,"total_tokens":0}}"#,
        ] {
            chat.line(Format::OpenAI, line.as_bytes());
        }
        let detail = chat.detail().unwrap();
        assert_eq!((detail.total, detail.response_service_tier.as_str()), (5, "flex"));
    }

    /// #178: xAI's documented `usage.cost_in_usd_ticks` and
    /// `usage.server_side_tool_usage_details` counts (docs.x.ai API reference, Responses
    /// `usage`, read 2026-11-14) reach the record, buffered and streamed. A member the
    /// provider did not send, or sent as `null`, stays absent; a reported zero stays zero;
    /// undocumented members are not carried.
    #[test]
    fn xai_cost_and_server_side_tool_counts_are_recorded() {
        let record = |detail: Detail| -> Value {
            let record = Record {
                detail,
                ..Record::default()
            };
            serde_json::from_slice(&queued(&record, &Client::default())).unwrap()
        };
        let completed = r#"{"type":"response.completed","response":{"usage":{"input_tokens":10,"output_tokens":4,"total_tokens":14,"cost_in_usd_ticks":123456,"cost_in_nano_usd":12345,"num_server_side_tools_used":2,"server_side_tool_usage_details":{"web_search_calls":1,"x_search_calls":1,"x_posts_fetched":0,"future_calls":9}}}}"#;
        let buffered = parse_codex(completed.as_bytes()).unwrap();
        let mut streamed = StreamUsage::default();
        streamed.line(Format::Codex, format!("data: {completed}").as_bytes());
        for (case, detail) in [
            ("buffered", buffered),
            ("streamed", streamed.detail().cloned().unwrap()),
        ] {
            let got = record(detail);
            assert_eq!(got["cost_in_usd_ticks"], 123456, "{case}: {got}");
            assert_eq!(
                got["server_side_tool_usage_details"],
                serde_json::json!({"web_search_calls": 1, "x_search_calls": 1, "x_posts_fetched": 0}),
                "{case}: {got}"
            );
            assert!(got.get("cost_in_nano_usd").is_none(), "{case}: {got}");
        }
        let null_cost = r#"{"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2,"cost_in_usd_ticks":null}}}"#;
        for plain in [
            parse_codex(null_cost.as_bytes()).unwrap(),
            parse_openai(br#"{"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#),
        ] {
            let got = record(plain);
            assert!(got.get("cost_in_usd_ticks").is_none(), "{got}");
            assert!(got.get("server_side_tool_usage_details").is_none(), "{got}");
        }
    }

    /// U1 (#99): Claude's documented `usage.service_tier` and `usage.server_tool_use`
    /// counts (platform.claude.com service-tiers, web-search and web-fetch pages, read
    /// 2026-11-14) reach the record, buffered and streamed. A count the provider did not
    /// send stays absent; a reported zero stays zero.
    #[test]
    fn claude_service_tier_and_server_tool_counts_are_recorded() {
        let record = |detail: Detail| -> Value {
            let record = Record {
                detail,
                ..Record::default()
            };
            serde_json::from_slice(&queued(&record, &Client::default())).unwrap()
        };
        let buffered = parse_claude(
            br#"{"usage":{"input_tokens":10,"output_tokens":4,"service_tier":"priority","server_tool_use":{"web_search_requests":2,"web_fetch_requests":0}}}"#,
        );
        let mut streamed = StreamUsage::default();
        for line in [
            r#"data: {"type":"message_start","message":{"usage":{"input_tokens":10,"output_tokens":1,"service_tier":"priority"}}}"#,
            r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":4,"server_tool_use":{"web_search_requests":2,"web_fetch_requests":0}}}"#,
        ] {
            streamed.line(Format::Claude, line.as_bytes());
        }
        for (case, detail) in [
            ("buffered", buffered),
            ("streamed", streamed.detail().cloned().unwrap()),
        ] {
            let got = record(detail);
            assert_eq!(got["response_service_tier"], "priority", "{case}: {got}");
            assert_eq!(
                got["server_tool_use"],
                serde_json::json!({"web_search_requests": 2, "web_fetch_requests": 0}),
                "{case}: {got}"
            );
        }
        let plain = record(parse_claude(br#"{"usage":{"input_tokens":1,"output_tokens":1}}"#));
        assert!(plain.get("server_tool_use").is_none(), "{plain}");
        assert!(plain.get("response_service_tier").is_none(), "{plain}");
    }

    /// #71: usage names the model a server-side fallback served with, from the
    /// documented signals (a `fallback` block's `to.model`, a `fallback_message`
    /// iteration), streamed or buffered; without them the reported model stands.
    #[test]
    fn claude_usage_reports_the_fallback_serving_model() {
        let observe = |lines: &[&str]| {
            let mut model = ResponseModel::default();
            for line in lines {
                model.observe(line.as_bytes(), "claude");
            }
            model.get().to_owned()
        };
        let start = r#"data: {"type":"message_start","message":{"id":"m","model":"claude-opus-4-7","usage":{"input_tokens":1}}}"#;
        let block = r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"fallback","from":{"model":"claude-opus-4-7"},"to":{"model":"claude-sonnet-4-5"},"trigger":{"type":"refusal","category":null}}}"#;
        let text = r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#;
        let delta = r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":8}}"#;
        let delta_iterations = r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":8,"iterations":[{"type":"message","model":"claude-opus-4-7"},{"type":"fallback_message","model":"claude-opus-4-8"}]}}"#;
        let stop = r#"data: {"type":"message_stop"}"#;
        assert_eq!(observe(&[start, block, text, delta, stop]), "claude-sonnet-4-5");
        assert_eq!(observe(&[start, text, delta_iterations, stop]), "claude-opus-4-8");
        assert_eq!(observe(&[start, text, delta, stop]), "claude-opus-4-7");
        let buffered = r#"{"type":"message","model":"claude-opus-4-7","content":[],"usage":{"iterations":[{"type":"fallback_message","model":"claude-opus-4-8"}]}}"#;
        assert_eq!(observe(&[buffered]), "claude-opus-4-8");
        let plain = r#"{"type":"message","model":"claude-opus-4-7","content":[],"usage":{"iterations":[{"type":"message","model":"claude-opus-4-7"}]}}"#;
        assert_eq!(observe(&[plain]), "claude-opus-4-7");
        // A non-fallback block or a malformed iteration names nothing.
        let tool =
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","to":{"model":"x"}}}"#;
        let bad = r#"data: {"type":"message_delta","delta":{},"usage":{"iterations":[{"type":"fallback_message","model":7}]}}"#;
        assert_eq!(observe(&[start, tool, bad, stop]), "claude-opus-4-7");
    }

    /// Go `resolveUsageSource` after d9451cb6: without a project, a Vertex service account
    /// is named by `AccountInfo`, its trimmed `client_email`, not by the client key.
    #[test]
    fn a_service_account_without_a_project_is_its_own_usage_source() {
        let metadata = serde_json::json!({
            "type": "vertex",
            "service_account": {"client_email": " sa@p.iam.gserviceaccount.com "},
        });
        let credential = crate::credential::Credential::from_file(
            std::path::Path::new("/auth"),
            std::path::Path::new("/auth/vertex-sa.json"),
            metadata.as_object().unwrap().clone(),
        )
        .unwrap();
        assert_eq!(source(&credential, "client-key"), "sa@p.iam.gserviceaccount.com");
    }

    #[test]
    fn usage_timestamps_round_trip_fraction_and_offset() {
        use chrono::TimeZone;
        let base = chrono::Utc.with_ymd_and_hms(2026, 10, 3, 8, 0, 0).unwrap();
        for nanos in [0, 1, 123_456_000, 999_999_999] {
            let instant = base + chrono::Duration::nanoseconds(nanos);
            for offset in [0, 5 * 3600 + 1800, -8 * 3600] {
                let local = instant.with_timezone(&chrono::FixedOffset::east_opt(offset).unwrap());
                let encoded = go_timestamp(&local);
                let parsed = chrono::DateTime::parse_from_rfc3339(&encoded).unwrap();
                assert_eq!(parsed, local);
                assert_eq!(parsed.offset(), local.offset());
            }
        }
    }

    /// Per-response records (#25): a detached attempt publishes nothing itself, each
    /// record publishes once when settled (not when the attempt's stream ends), a
    /// record dropped unsettled publishes what it observed, and an attempt that already
    /// published cannot be detached into a second record of the same response.
    #[test]
    fn detached_records_publish_once_each_and_the_attempt_none() {
        let rt = usage_runtime();
        let facts = std::sync::Arc::new(Facts::new(
            Client::default(),
            Format::OpenAIResponse,
            Format::OpenAIResponse,
            "gpt-fixture",
            br#"{"model":"gpt-fixture","input":[]}"#,
            true,
            &crate::gateway::dispatch::Trace::default(),
        ));
        let line = |id: &str, input: i64| {
            format!(
                r#"{{"type":"response.completed","response":{{"id":"{id}","usage":{{"input_tokens":{input},"output_tokens":1,"total_tokens":{}}}}}}}"#,
                input + 1
            )
        };
        let tracker = Tracker::start(&rt, &facts, &credential_for("codex"), "gpt-fixture");
        let first = tracker.sink().detach().expect("an attempt detaches once");
        assert!(tracker.sink().detach().is_none(), "the attempt is detached already");
        first.response_line(Format::Codex, line("r1", 5).as_bytes());
        first.publish();
        // A later failure of the same record changes nothing.
        first.publish_failure(502, "late");
        let queue = rt.usage_queue();
        let r1: Vec<Value> = queue
            .pop_oldest(10)
            .iter()
            .map(|q| serde_json::from_slice(q).unwrap())
            .collect();
        assert_eq!(r1.len(), 1, "{r1:?}");
        assert_eq!(
            (r1[0]["failed"].clone(), r1[0]["tokens"]["input_tokens"].clone()),
            (false.into(), 5.into())
        );
        let second = first.successor(Some(br#"{"type":"response.create","service_tier":"flex","input":[]}"#));
        second.response_line(Format::Codex, line("r2", 11).as_bytes());
        drop(second);
        // The attempt's own end publishes nothing.
        tracker.fail(&crate::execution::ExecError::local(
            502,
            crate::execution::FailureScope::Request,
            "socket closed",
        ));
        let rest: Vec<Value> = queue
            .pop_oldest(10)
            .iter()
            .map(|q| serde_json::from_slice(q).unwrap())
            .collect();
        assert_eq!(rest.len(), 1, "{rest:?}");
        assert_eq!(rest[0]["tokens"]["input_tokens"], 11);
        assert_eq!(rest[0]["service_tier"], "flex");
        assert_eq!(
            rest[0]["alias"], "gpt-fixture",
            "the connection's model when the frame names none"
        );
        assert_ne!(rest[0]["execution_id"], r1[0]["execution_id"]);

        // The attempt published first (its stream ended): no record to detach.
        let tracker = Tracker::start(&rt, &facts, &credential_for("codex"), "gpt-fixture");
        let sink = tracker.sink();
        tracker.succeed();
        assert_eq!(queue.pop_oldest(10).len(), 1);
        assert!(sink.detach().is_none());
    }

    /// #25: the values an attempt's socket carried bind every record of its connection,
    /// whichever publishes first. Publishing a record takes its report, never the
    /// binding: a detached record, an explicit successor made after it published and an
    /// automatic successor of that one each keep the model, the service tier and the
    /// failure text the upstream reported scrubbed of the values, and every other byte.
    #[test]
    fn every_record_of_a_connection_keeps_its_binding() {
        let rt = usage_runtime();
        let facts = std::sync::Arc::new(Facts::new(
            Client::default(),
            Format::OpenAIResponse,
            Format::OpenAIResponse,
            "gpt-fixture",
            br#"{"model":"gpt-fixture","input":[]}"#,
            true,
            &crate::gateway::dispatch::Trace::default(),
        ));
        let line = |id: &str| {
            format!(
                r#"{{"type":"response.completed","response":{{"id":"{id}","model":"m at-FAKE","service_tier":"echo at-FAKE","usage":{{"input_tokens":4,"output_tokens":1,"total_tokens":5}}}}}}"#
            )
        };
        let tracker = Tracker::start(&rt, &facts, &credential_for("codex"), "gpt-fixture");
        tracker
            .sink()
            .sent_credentials(&crate::execution::SentCredentials::new(["at-FAKE"]));
        let first = tracker.sink().detach().expect("an attempt detaches once");
        first.response_line(Format::Codex, line("r1").as_bytes());
        first.publish();
        let explicit = first.successor(Some(br#"{"type":"response.create","input":[]}"#));
        explicit.response_line(Format::Codex, line("r2").as_bytes());
        explicit.publish();
        let automatic = explicit.successor(None);
        automatic.publish_failure(400, "bad at-FAKE, kept at-OTHER");
        drop((first, explicit, automatic));
        let records: Vec<Value> = rt
            .usage_queue()
            .pop_oldest(10)
            .iter()
            .map(|q| serde_json::from_slice(q).unwrap())
            .collect();
        assert_eq!(records.len(), 3, "{records:?}");
        for record in &records {
            assert!(!record.to_string().contains("at-FAKE"), "{record}");
        }
        for record in &records[..2] {
            assert_eq!(record["response_model"], "m [REDACTED]", "{record}");
            assert_eq!(record["response_service_tier"], "echo [REDACTED]", "{record}");
            assert_eq!(record["tokens"]["total_tokens"], 5, "{record}");
        }
        assert_eq!(records[2]["fail"]["status_code"], 400);
        assert_eq!(records[2]["fail"]["body"], "bad [REDACTED], kept at-OTHER");
    }

    /// An attempt dropped before the upstream answered it ([`Tracker::arrived`]) served
    /// nothing: its record is the canceled failure (499, no tokens), never a success. One
    /// dropped after the upstream answered keeps Go's deferred publish, a success with
    /// what was observed, and an outcome the executor settled first stays.
    #[test]
    fn an_attempt_dropped_before_the_upstream_answered_is_canceled() {
        let rt = usage_runtime();
        let facts = std::sync::Arc::new(Facts::new(
            Client::default(),
            Format::OpenAIResponse,
            Format::OpenAIResponse,
            "gpt-fixture",
            br#"{"model":"gpt-fixture","input":[]}"#,
            true,
            &crate::gateway::dispatch::Trace::default(),
        ));
        let start = || Tracker::start(&rt, &facts, &credential_for("codex"), "gpt-fixture");
        let published = || -> Value {
            let records = rt.usage_queue().pop_oldest(10);
            assert_eq!(records.len(), 1);
            serde_json::from_slice(&records[0]).unwrap()
        };
        drop(start());
        let abandoned = published();
        assert_eq!(abandoned["failed"], true, "{abandoned}");
        assert_eq!(abandoned["fail"]["status_code"], 499);
        assert_eq!(abandoned["fail"]["body"], "context canceled");
        assert_eq!(abandoned["tokens"]["total_tokens"], 0);
        let mut answered = start();
        answered.arrived(&axum::http::HeaderMap::new());
        drop(answered);
        assert_eq!(published()["failed"], false);
        let settled = start();
        settled.sink().publish_failure(502, "upstream failed");
        drop(settled);
        assert_eq!(published()["fail"]["status_code"], 502);
    }

    #[test]
    fn owned_outcomes_preserve_payload_ttft_and_explicit_marks() {
        use crate::execution::UsageObserver;
        use std::time::Duration;

        let rt = usage_runtime();
        let facts = std::sync::Arc::new(Facts::new(
            Client::default(),
            Format::OpenAIResponse,
            Format::OpenAIResponse,
            "gpt-fixture",
            b"{}",
            true,
            &crate::gateway::dispatch::Trace::default(),
        ));
        // The saved first payload must survive success, failure, and drop. An
        // executor's explicit marks retain precedence, including a marked zero.
        for detached in [false, true] {
            for marked in [None, Some(0), Some(7)] {
                for ending in ["success", "failure", "failure_with_usage", "drop"] {
                    let tracker = Tracker::start(&rt, &facts, &credential_for("codex"), "gpt-fixture");
                    let reported = Reported {
                        first: Some(Duration::from_millis(43)),
                        ttft: Ttft {
                            tracked: marked.is_some(),
                            ttft: marked.map(Duration::from_millis),
                            ..Default::default()
                        },
                        ..Default::default()
                    };
                    let own = tracker.observer.record(facts.clone(), reported, detached);
                    match ending {
                        "success" => own.publish(),
                        "failure" => own.publish_failure(502, "failed"),
                        "failure_with_usage" => own.publish_failure_with_usage(502, "failed"),
                        "drop" => {}
                        _ => unreachable!(),
                    }
                    drop(own);
                    let got = rt.usage_queue().pop_oldest(10);
                    assert_eq!(got.len(), 1, "{ending}, detached={detached}, marked={marked:?}");
                    let record: Value = serde_json::from_slice(&got[0]).unwrap();
                    assert_eq!(
                        record["ttft_ms"],
                        marked.unwrap_or(43),
                        "{ending}, detached={detached}, marked={marked:?}: {record}"
                    );
                    tracker.sink().discard();
                    drop(tracker);
                }
            }
        }
    }

    /// Claude's executor-published outcomes through the queue: an apply_patch
    /// rejection is Go's `reporter.PublishFailure(err)` after usage was observed (no
    /// tokens, response model kept, and the later failure the server sees changes
    /// nothing); `discard` is Go returning before it created a reporter (no record).
    #[test]
    fn claude_executor_outcomes_reach_the_queue() {
        use crate::execution::{ExecError, FailureScope};
        let rt = usage_runtime();
        let facts = std::sync::Arc::new(Facts::new(
            Client::default(),
            Format::OpenAIResponse,
            Format::OpenAIResponse,
            "claude-sonnet-4-6",
            b"{}",
            true,
            &crate::gateway::dispatch::Trace::default(),
        ));
        let error = ExecError::local(502, FailureScope::Credential, "Invalid apply_patch tool arguments");
        let tracker = Tracker::start(&rt, &facts, &credential_for("claude"), "claude-sonnet-4-6");
        let sink = tracker.sink();
        sink.claude_cache(crate::execution::ClaudeCacheTelemetry {
            request_type: "compaction".into(),
            base_request_type: "compaction".into(),
            ..Default::default()
        });
        sink.usage_required();
        sink.response_line(
            Format::Claude,
            br#"data: {"type":"message_start","message":{"model":"claude-upstream","usage":{"input_tokens":9,"output_tokens":1,"cache_creation_input_tokens":12,"cache_creation":{"ephemeral_5m_input_tokens":7,"ephemeral_1h_input_tokens":5}}}}"#,
        );
        sink.publish_failure(502, "Invalid apply_patch tool arguments");
        tracker.fail(&error);
        let raw = rt.usage_queue().pop_oldest(10);
        let observation: Value = serde_json::from_slice(&raw[0]).unwrap();
        assert_eq!(
            observation["tokens"]["cache_creation_tokens"], 0,
            "Go failure accounting unchanged"
        );
        assert_eq!(observation["tokens"]["cache_creation_5m_tokens"], 7);
        assert_eq!(observation["tokens"]["cache_creation_1h_tokens"], 5);
        assert_eq!(observation["claude_cache"]["request_type"], "compaction");
        assert_eq!(observation["claude_cache"]["attempt"], 0);
        let got: Vec<Value> = raw.iter().map(|q| summary(q)).collect();
        assert_eq!(
            got,
            [serde_json::json!({
                "failed": true, "fail_status": 502, "fail_body": "Invalid apply_patch tool arguments",
                "input": 0, "output": 0, "total": 0, "response_model": "claude-upstream",
                "reasoning_effort": "", "model": "claude-sonnet-4-6",
            })]
        );
        // Without the executor's report the server keeps Claude's buffered usage
        // (StreamUsageBuffer.PublishFailure), so the report above is what drops it.
        let tracker = Tracker::start(&rt, &facts, &credential_for("claude"), "claude-sonnet-4-6");
        tracker.sink().claude_cache(crate::execution::ClaudeCacheTelemetry {
            request_type: "compaction".into(),
            base_request_type: "compaction".into(),
            ..Default::default()
        });
        tracker.sink().response_line(
            Format::Claude,
            br#"data: {"type":"message_start","message":{"model":"claude-upstream","usage":{"input_tokens":9,"output_tokens":1}}}"#,
        );
        tracker.fail(&error);
        let got = rt.usage_queue().pop_oldest(10);
        assert_eq!(summary(&got[0])["input"], 9);
        let observation: Value = serde_json::from_slice(&got[0]).unwrap();
        assert_eq!(observation["claude_cache"]["request_type"], "retry");
        assert_eq!(observation["claude_cache"]["base_request_type"], "compaction");
        assert_eq!(observation["claude_cache"]["attempt"], 1);
        assert!(observation["tokens"].get("cache_creation_5m_tokens").is_none());
        let tracker = Tracker::start(&rt, &facts, &credential_for("claude"), "claude-sonnet-4-6");
        tracker.sink().usage_required();
        tracker.sink().claude_cache(crate::execution::ClaudeCacheTelemetry {
            request_type: "main".into(),
            base_request_type: "main".into(),
            ..Default::default()
        });
        tracker.succeed();
        let got = rt.usage_queue().pop_oldest(10);
        assert_eq!(got.len(), 1, "tokenless Claude activity is still observed");
        let observation: Value = serde_json::from_slice(&got[0]).unwrap();
        assert_eq!(observation["tokens"]["total_tokens"], 0);
        assert!(observation["claude_cache"].is_object());
        assert_eq!(observation["usage_observed"], false);
        assert_eq!(
            observation["failed"], false,
            "absence of usage is not a successful completion signal"
        );
        let mut tracker = Tracker::start(&rt, &facts, &credential_for("claude"), "claude-sonnet-4-6");
        tracker.sink().usage_required();
        tracker
            .sink()
            .claude_cache(crate::execution::ClaudeCacheTelemetry::default());
        // Same path as a client dropping its stream before usage: the upstream answered.
        tracker.arrived(&axum::http::HeaderMap::new());
        drop(tracker);
        let got = rt.usage_queue().pop_oldest(10);
        let observation: Value = serde_json::from_slice(&got[0]).unwrap();
        assert_eq!(observation["usage_observed"], false);
        for fail in [true, false] {
            let tracker = Tracker::start(&rt, &facts, &credential_for("claude"), "claude-sonnet-4-6");
            tracker.sink().discard();
            if fail {
                tracker.fail(&ExecError::local(
                    501,
                    FailureScope::Request,
                    "/responses/compact not supported",
                ));
            } else {
                tracker.succeed();
            }
            assert!(
                rt.usage_queue().pop_oldest(10).is_empty(),
                "discarded attempt (fail: {fail})"
            );
        }
    }

    /// Archived request/event inputs exercise the collector schema. Expectations below
    /// come from the provider token fields and the Rust accounting contract, rather
    /// than recorded proxy output; serialization order is deliberately irrelevant.
    #[test]
    fn queued_records_preserve_provider_usage_and_request_context() {
        let fixture: Value =
            serde_json::from_str(include_str!("../../tests/gateway/fixtures/usage_inputs.json")).unwrap();
        let cases = fixture.as_array().unwrap();
        assert_eq!(cases.len(), 9);
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let s = |k: &str| case[k].as_str().unwrap_or_default().to_owned();
            // Gemini, Vertex, Interactions and OpenAI-compatible credentials were removed
            // (issue #49).
            let Some(format) = Format::parse(&s("format")) else {
                continue;
            };
            if name.contains("compat") || name.starts_with("vertex ") {
                continue;
            }
            let lines: Vec<&str> = case["lines"]
                .as_array()
                .unwrap()
                .iter()
                .map(|l| l.as_str().unwrap())
                .collect();
            let detail = if case["stream"].as_bool().unwrap() {
                let mut stream = StreamUsage::default();
                for line in &lines {
                    stream.line(format, line.as_bytes());
                }
                stream.detail().cloned().unwrap_or_default()
            } else {
                parse_body(format, lines[0].as_bytes())
            };
            let mut response_model = ResponseModel::default();
            for line in &lines {
                response_model.observe(line.as_bytes(), &s("provider"));
            }
            let mut metadata = case["metadata"].as_object().cloned().unwrap_or_default();
            metadata.insert("type".into(), Value::from(s("auth_provider")));
            let path = format!("/auth/{}", s("auth_id"));
            let mut credential = crate::credential::Credential::from_file(
                std::path::Path::new("/auth"),
                std::path::Path::new(&path),
                metadata,
            )
            .unwrap();
            // Go's fixture auth has no `type` in metadata unless it set one.
            if case["metadata"].get("type").is_none() {
                credential.metadata.remove("type");
            }
            credential.attributes = case["attributes"]
                .as_object()
                .map(|a| {
                    a.iter()
                        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
                        .collect()
                })
                .unwrap_or_default();
            let reasoning = if s("translated_payload").is_empty() {
                s("reasoning_effort")
            } else {
                crate::protocol::thinking::extract_translated_reasoning_effort(
                    s("translated_payload").as_bytes(),
                    &s("translated_format"),
                )
            };
            let record = Record {
                timestamp: s("requested_at"),
                latency_ms: case["latency_ms"].as_i64().unwrap(),
                ttft_ms: case["ttft_ms"].as_i64().unwrap(),
                execution_id: s("execution_id"),
                provider: s("provider"),
                executor_type: s("executor_type"),
                model: s("model"),
                alias: s("alias"),
                source: source(&credential, &s("client_key")),
                auth_index: crate::config::credentials::auth_index(&credential),
                access_token_sha256: access_token_sha256(&credential),
                auth_type: crate::catalog::dynamic::auth_kind(&credential)
                    .unwrap_or_default()
                    .to_owned(),
                reasoning_effort: reasoning,
                service_tier: s("service_tier"),
                generate: case["generate"].as_bool().unwrap(),
                stream: case["stream"].as_bool().unwrap(),
                failed: case["failed"].as_bool().unwrap(),
                fail_status: case["fail_status"].as_i64().unwrap(),
                fail_body: s("fail_body"),
                // Go v8.0.35's records, which predate `fail.code`.
                fail_code: "",
                detail,
                response_model: response_model.get().to_owned(),
                response_headers: case["response_headers"]
                    .as_object()
                    .map(|h| {
                        h.iter()
                            .map(|(k, v)| {
                                let values = v.as_array().unwrap().iter().map(|x| x.as_str().unwrap().to_owned());
                                (k.clone(), values.collect())
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                // Go's record has neither: none are written.
                prompt_prefix_hash: String::new(),
                claude_cache: None,
                usage_observed: None,
                timings: Timings::default(),
            };
            let client = Client {
                client_ip: s("client_ip"),
                resolved_client_ip: s("resolved_client_ip"),
                x_forwarded_for: s("x_forwarded_for"),
                user_agent: s("user_agent"),
                session_id: s("session_id"),
                parent_session_id: s("parent_session_id"),
                is_fork: false,
                node_kind: String::new(),
                is_compaction: false,
                session_source: "",
                request_id: s("request_id"),
                endpoint: s("endpoint"),
                api_key: s("client_key"),
                access_provider: String::new(),
            };
            let got = queued(&record, &client);
            let actual: Value = serde_json::from_slice(&got).unwrap();
            let (input, output, total) = match name {
                "claude buffered api key" => (10, 20, 38),
                "claude stream oauth" => (12, 40, 152),
                "openai stream last usage" => (7, 3, 10),
                "openai partial usage" => (5, 0, 5),
                "responses buffered codex oauth" => (200, 80, 280),
                "responses stream terminal" => (9, 4, 13),
                "failure without usage" => (0, 0, 0),
                "failure after stream usage" => (11, 2, 13),
                "client key source fallback" => (1, 1, 2),
                other => panic!("unqualified usage scenario {other}"),
            };
            assert_eq!(actual["tokens"]["input_tokens"], input, "{name}");
            assert_eq!(actual["tokens"]["output_tokens"], output, "{name}");
            assert_eq!(actual["tokens"]["total_tokens"], total, "{name}");
            for field in [
                "timestamp",
                "execution_id",
                "provider",
                "executor_type",
                "model",
                "alias",
                "endpoint",
                "request_id",
                "client_ip",
                "resolved_client_ip",
                "x_forwarded_for",
                "user_agent",
                "session_id",
                "parent_session_id",
                "reasoning_effort",
                "service_tier",
            ] {
                let expected = match field {
                    "timestamp" => &case["requested_at"],
                    "reasoning_effort" => continue, // translated payloads have a separate parser contract
                    _ => &case[field],
                };
                if let Some(expected) = expected.as_str().filter(|v| !v.is_empty())
                    && !matches!(field, "session_id" | "parent_session_id")
                {
                    assert_eq!(actual[field], expected, "{name}: {field}");
                }
            }
            assert_eq!(actual["failed"], case["failed"], "{name}");
            assert_eq!(actual["stream"], case["stream"], "{name}");
            assert_eq!(actual["generate"], case["generate"], "{name}");
            assert_eq!(actual["api_key"], s("client_key").trim(), "{name}");
            assert_eq!(actual["is_native_key"], false, "{name}");
            assert_eq!(actual["accounting_version"], 2);
            assert_eq!(actual["token_breakdown"]["total_tokens"], total);
        }
    }

    #[test]
    fn queued_payload_distinguishes_native_key() {
        for (provider, native) in [
            ("config-inline", true),
            ("  CONFIG-API-KEY ", true),
            ("custom-auth-provider", false),
            ("realtime-client-secret", false),
            ("", false),
        ] {
            let client = Client {
                api_key: "fake-client-key".into(),
                access_provider: provider.into(),
                ..Default::default()
            };
            let got: Value = serde_json::from_slice(&queued(&Record::default(), &client)).unwrap();
            assert_eq!(got["api_key"], "fake-client-key");
            assert_eq!(got["is_native_key"], native);
            if provider.trim().is_empty() {
                assert!(got.get("access_provider").is_none());
            } else {
                assert_eq!(got["access_provider"], provider.trim());
            }
        }
    }

    /// A runtime whose usage queue keeps records.
    fn usage_runtime() -> std::sync::Arc<crate::runtime::Runtime> {
        let cfg =
            crate::config::Config::parse("observability:\n  usage:\n    usage-statistics-enabled: true\n").unwrap();
        let executors = crate::providers::Executors {
            claude: crate::providers::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
            codex: Default::default(),
            meta: Default::default(),
            xai: Default::default(),
        };
        let rt = std::sync::Arc::new(crate::gateway::testing::runtime(cfg.clone(), Vec::new(), executors));
        rt.usage_queue().configure(true, &cfg);
        rt
    }

    fn credential_for(provider: &str) -> crate::credential::Credential {
        let mut metadata = serde_json::Map::new();
        metadata.insert("type".into(), provider.into());
        let mut c = crate::credential::Credential::from_file(
            std::path::Path::new("/auth"),
            std::path::Path::new("/auth/a.json"),
            metadata,
        )
        .unwrap();
        c.attributes.insert("auth_kind".into(), "oauth".into());
        c
    }

    /// The fields Go's reporter goldens compare, from one queued record.
    fn summary(queued: &[u8]) -> Value {
        let v: Value = serde_json::from_slice(queued).unwrap();
        let tokens = &v["tokens"];
        serde_json::json!({
            "failed": v["failed"],
            "fail_status": v["fail"]["status_code"].as_i64().unwrap_or(0),
            "fail_body": v["fail"]["body"].as_str().unwrap_or(""),
            "input": tokens["input_tokens"],
            "output": tokens["output_tokens"],
            "total": tokens["total_tokens"],
            "response_model": v["response_model"].as_str().unwrap_or(""),
            "reasoning_effort": v["reasoning_effort"].as_str().unwrap_or(""),
            "model": v["model"],
        })
    }

    /// Publication is exactly once; a terminal failure cannot rewrite a record the
    /// executor already published, and a failure preserves any observed usage.
    #[test]
    fn reporter_publication_is_exactly_once_and_keeps_observed_usage() {
        use crate::execution::{ExecError, FailureScope};
        for (publish_first, fail_first, usage_required, has_usage) in
            (0..16).map(|bits| (bits & 1 != 0, bits & 2 != 0, bits & 4 != 0, bits & 8 != 0))
        {
            let rt = usage_runtime();
            let facts = std::sync::Arc::new(Facts::new(
                Client::default(),
                Format::OpenAI,
                Format::OpenAI,
                "grok-test",
                b"{}",
                true,
                &crate::gateway::dispatch::Trace::default(),
            ));
            let tracker = Tracker::start(&rt, &facts, &credential_for("xai"), "grok-test");
            let sink = tracker.sink();
            if usage_required {
                sink.usage_required();
            }
            if has_usage {
                sink.response_body(
                    Format::OpenAI,
                    br#"{"usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}"#,
                );
            }
            if fail_first {
                sink.publish_failure(429, "slow down");
            }
            if publish_first {
                sink.publish();
            }
            tracker.succeed();
            let records = rt.usage_queue().pop_oldest(10);
            let should_publish = publish_first || fail_first || !usage_required || has_usage;
            assert_eq!(records.len(), usize::from(should_publish));
            if let Some(record) = records.first() {
                let got = summary(record);
                assert_eq!(got["failed"], fail_first);
                assert_eq!(got["fail_status"], if fail_first { 429 } else { 200 });
                assert_eq!(got["input"], if has_usage && !fail_first { 5 } else { 0 });
                assert_eq!(got["output"], if has_usage && !fail_first { 2 } else { 0 });
                assert_eq!(got["total"], if has_usage && !fail_first { 7 } else { 0 });
            }
        }
        // A stream error after receiving usage carries those counts into failure.
        let rt = usage_runtime();
        let facts = std::sync::Arc::new(Facts::new(
            Client::default(),
            Format::Claude,
            Format::Claude,
            "claude-test",
            b"{}",
            true,
            &crate::gateway::dispatch::Trace::default(),
        ));
        let tracker = Tracker::start(&rt, &facts, &credential_for("claude"), "claude-test");
        tracker.sink().response_line(
            Format::Claude,
            br#"data: {"type":"message_start","message":{"usage":{"input_tokens":9,"output_tokens":1}}}"#,
        );
        tracker.sink().response_line(
            Format::Claude,
            br#"data: {"type":"message_delta","usage":{"output_tokens":6}}"#,
        );
        tracker.fail(&ExecError::local(502, FailureScope::Request, "stream broke"));
        let records = rt.usage_queue().pop_oldest(10);
        assert_eq!(records.len(), 1);
        let got = summary(&records[0]);
        assert_eq!(got["failed"], true);
        assert_eq!(got["fail_status"], 502);
        assert_eq!(got["fail_body"], "stream broke");
        assert_eq!(
            (got["input"].as_i64(), got["output"].as_i64(), got["total"].as_i64()),
            (Some(9), Some(6), Some(15))
        );
    }

    /// Accounting treats an optional provider prefix, effort suffix and recognized
    /// dated/latest deployment suffixes as the same requested model family.
    #[test]
    fn model_substitution_distinguishes_family_changes_from_deployments() {
        for base in ["gpt-5.5", "claude-sonnet-4-6", "grok-4"] {
            for equivalent in [
                base.to_owned(),
                base.to_ascii_uppercase(),
                format!("provider/{base}"),
                format!("{base}-2026-01-01"),
                format!("{base}-20260101"),
                format!("{base}-001"),
                format!("{base}-latest"),
                format!("{base}(high)"),
                format!(" {base} "),
            ] {
                assert!(!is_model_substituted(base, &equivalent), "{base} -> {equivalent}");
                assert!(!is_model_substituted(&equivalent, base), "{equivalent} -> {base}");
            }
            for different in [
                format!("{base}-other"),
                format!("{base}-0001"),
                format!("{base}-2026-1-01"),
            ] {
                assert!(is_model_substituted(base, &different), "{base} -> {different}");
            }
            assert!(!is_model_substituted(base, ""));
            assert!(!is_model_substituted("", base));
        }
        assert!(is_model_substituted("gpt-5.5", "gpt-5.4"));
    }

    /// Home records identify unauthorized OAuth attempts without fabricating provider
    /// usage. The input archive contains no recorded proxy answers.
    #[test]
    fn home_unauthorized_records_keep_identity_and_never_fabricate_usage() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../tests/gateway/fixtures/home_unauthorized_inputs.json"
        ))
        .unwrap();
        let rt = usage_runtime();
        let s = |v: &Value| v.as_str().unwrap_or_default().to_owned();
        for case in fixture.as_array().unwrap() {
            let name = s(&case["name"]);
            let mut attributes: std::collections::BTreeMap<String, String> = case["attributes"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(k, v)| (k.clone(), s(v)))
                .collect();
            attributes.insert(crate::config::credentials::HOME_AUTH_INDEX.into(), s(&case["index"]));
            attributes.insert(crate::gateway::remote::PROVIDER.into(), s(&case["auth_provider"]));
            let c = &case["client"];
            let mut client = Client {
                client_ip: s(&c["client_ip"]),
                resolved_client_ip: s(&c["resolved_client_ip"]),
                x_forwarded_for: s(&c["x_forwarded_for"]),
                user_agent: s(&c["user_agent"]),
                session_id: s(&c["session_id"]),
                parent_session_id: s(&c["parent_session_id"]),
                is_fork: c["is_fork"].as_bool().unwrap_or(false),
                request_id: s(&c["request_id"]),
                endpoint: s(&c["endpoint"]),
                api_key: "fake-client-key".into(),
                access_provider: "config-inline".into(),
                ..Default::default()
            };
            if name == "count_tokens_oauth_runtime" {
                attributes.insert(crate::gateway::remote::SESSION.into(), client.session_id.clone());
                attributes.insert(
                    crate::gateway::remote::PARENT_SESSION.into(),
                    client.parent_session_id.clone(),
                );
                client.session_id = "the-request-own-session".into();
                client.parent_session_id.clear();
            }
            let credential = crate::credential::Credential {
                id: s(&case["id"]),
                provider: s(&case["auth_provider"]).go_lower(),
                source: crate::credential::Source::File(s(&case["id"]).into()),
                disabled: false,
                label: String::new(),
                attributes,
                metadata: case["metadata"].as_object().cloned().unwrap_or_default(),
                revision: 0,
            };
            let request = &case["request"];
            let with_options = request["with_options"].as_bool().unwrap_or(false);
            let tier = s(&request["service_tier"]);
            let facts = Facts {
                client,
                format: Format::Claude,
                alias: if with_options {
                    s(&request["alias"])
                } else {
                    String::new()
                },
                reasoning_effort: if with_options {
                    s(&request["reasoning_effort"])
                } else {
                    String::new()
                },
                service_tier: if with_options && !tier.is_empty() {
                    tier
                } else {
                    "default".into()
                },
                generate: true,
                stream: true,
                clock: Default::default(),
                prompt_prefix_hash: String::new(),
                attempts: Default::default(),
            };
            rt.usage_queue().pop_oldest(100);
            let body = case["body"].as_str().unwrap_or_default();
            publish_home_unauthorized(
                &rt,
                &facts,
                &credential,
                &s(&case["provider"]),
                &s(&case["model"]),
                body,
            );
            let mut got: Vec<Value> = rt
                .usage_queue()
                .pop_oldest(10)
                .iter()
                .map(|r| serde_json::from_slice(r).unwrap())
                .collect();
            if name == "api_key_has_no_fingerprint" {
                assert!(got.is_empty(), "API keys have no OAuth fingerprint: {got:?}");
                continue;
            }
            assert_eq!(got.len(), 1, "{name}");
            let got = got.remove(0);
            assert_eq!(got["failed"], true);
            assert_eq!(got["generate"], false);
            assert_eq!(got["stream"], false);
            assert_eq!(got["usage_observed"], false);
            assert_eq!(got["fail"]["status_code"], 401);
            assert_eq!(
                got["fail"]["body"],
                if body.is_empty() { "upstream unauthorized" } else { body }
            );
            assert_eq!(got["executor_type"], "home-result");
            assert_eq!(got["auth_index"], case["index"]);
            assert_eq!(got["model"], s(&case["model"]).trim());
            assert_eq!(
                got["alias"],
                if facts.alias.is_empty() {
                    s(&case["model"]).trim().to_owned()
                } else {
                    facts.alias.clone()
                }
            );
            assert_eq!(got["reasoning_effort"], facts.reasoning_effort);
            assert_eq!(got["service_tier"], facts.service_tier);
            assert_eq!(got["api_key"], "fake-client-key");
            assert_eq!(got["access_provider"], "config-inline");
            assert_eq!(got["is_native_key"], true);
            assert_eq!(got["tokens"]["total_tokens"], 0);
            assert_eq!(got["token_breakdown"]["total_tokens"], 0);
            assert!(chrono::DateTime::parse_from_rfc3339(got["timestamp"].as_str().unwrap()).is_ok());
            assert!(uuid::Uuid::parse_str(got["execution_id"].as_str().unwrap()).is_ok());
            let fingerprint = got["access_token_sha256"].as_str().unwrap();
            assert_eq!(fingerprint.len(), 64);
            assert!(fingerprint.bytes().all(|b| b.is_ascii_hexdigit()));
            // Fingerprints are safe identifiers; raw upstream credentials do not occur.
            let encoded = serde_json::to_string(&got).unwrap();
            for token in ["tok-claude-a", "tok-codex-b", "tok-kimi", "tok-gem", "tok-x", "tok-f"] {
                assert!(!encoded.contains(token));
            }
            let expected_source = match name.as_str() {
                "count_tokens_oauth_runtime" => "memory",
                "alpha_search_no_body" => "config",
                "blank_provider_falls_back_and_source_kinds" => "objectstore",
                "path_source" | "unknown_source_is_file" => "file",
                "no_source_attributes" => "",
                _ => unreachable!(),
            };
            assert_eq!(got["source"], expected_source, "{name}");
            assert_eq!(
                got["session_source"].as_str(),
                (name == "count_tokens_oauth_runtime").then_some("home")
            );
            if name == "blank_provider_falls_back_and_source_kinds" {
                assert_eq!(got["provider"], "xai");
            }
        }
    }

    /// Go `syncMetadataSessionToContext` in Home mode: a dispatched credential's
    /// attempt reports the session Home was sent (its parent cleared when equal), not
    /// the request's own; other credentials keep the request's.
    #[test]
    fn home_attempts_report_the_dispatched_session() {
        let rt = usage_runtime();
        let client = Client {
            session_id: "request-session".into(),
            parent_session_id: "request-parent".into(),
            ..Client::default()
        };
        let trace = crate::gateway::dispatch::Trace::default();
        let facts = std::sync::Arc::new(Facts::new(
            client,
            Format::Claude,
            Format::Claude,
            "m",
            b"{}",
            false,
            &trace,
        ));
        let record = |attrs: &[(&str, &str)]| {
            let mut credential = credential_for("claude");
            for (k, v) in attrs {
                credential.attributes.insert((*k).into(), (*v).into());
            }
            rt.usage_queue().pop_oldest(100);
            Tracker::start(&rt, &facts, &credential, "m").succeed();
            let queued = rt.usage_queue().pop_oldest(10);
            assert_eq!(queued.len(), 1);
            let v: Value = serde_json::from_slice(&queued[0]).unwrap();
            (v["session_id"].clone(), v["parent_session_id"].clone())
        };
        let uuid = |s: &str| Value::from(crate::protocol::session::normalize_to_canonical_uuid(s));
        assert_eq!(record(&[]), (uuid("request-session"), uuid("request-parent")));
        assert_eq!(
            record(&[
                (crate::gateway::remote::SESSION, "home-session"),
                (crate::gateway::remote::PARENT_SESSION, "")
            ]),
            (uuid("home-session"), Value::Null)
        );
        assert_eq!(
            record(&[
                (crate::gateway::remote::SESSION, "home-session"),
                (crate::gateway::remote::PARENT_SESSION, "home-parent")
            ]),
            (uuid("home-session"), uuid("home-parent"))
        );
        assert_eq!(
            record(&[
                (crate::gateway::remote::SESSION, "home-session"),
                (crate::gateway::remote::PARENT_SESSION, "home-session")
            ]),
            (uuid("home-session"), Value::Null)
        );
    }

    /// A loopback upstream for redirected exchanges, on raw sockets with a small receive
    /// window so an unread body stays with the sender. `/see-other` reads the body and
    /// answers 303 to `/landing`, which answers `hello`; `/temporary` reads the body and
    /// answers 307 to `/cut`, which reads the head and 64 KiB of the body, then closes.
    async fn redirecting_upstream() -> String {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.set_recv_buffer_size(4096).unwrap();
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let listener = socket.listen(16).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            while let Ok((conn, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut conn = tokio::io::BufReader::new(conn);
                    loop {
                        let (mut path, mut length) = (String::new(), 0);
                        loop {
                            let mut line = String::new();
                            if conn.read_line(&mut line).await.unwrap_or(0) == 0 {
                                return;
                            }
                            let line = line.trim_end().to_ascii_lowercase();
                            if line.is_empty() {
                                break;
                            }
                            if path.is_empty() {
                                path = line.split(' ').nth(1).unwrap_or_default().to_owned();
                            } else if let Some(value) = line.strip_prefix("content-length:") {
                                length = value.trim().parse().unwrap();
                            }
                        }
                        let mut body = vec![0; if path == "/cut" { 64 << 10 } else { length }];
                        if conn.read_exact(&mut body).await.is_err() {
                            return;
                        }
                        let reply: &[u8] = match path.as_str() {
                            "/see-other" => {
                                b"HTTP/1.1 303 See Other\r\nLocation: /landing\r\nContent-Length: 0\r\n\r\n"
                            }
                            "/temporary" => {
                                b"HTTP/1.1 307 Temporary Redirect\r\nLocation: /cut\r\nContent-Length: 0\r\n\r\n"
                            }
                            "/landing" => b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello",
                            // Closed with the rest of the upload unread.
                            _ => return,
                        };
                        conn.get_mut().write_all(reply).await.unwrap();
                    }
                });
            }
        });
        url
    }

    /// The published record of one timed POST of `body` to `url` through the shared
    /// HTTP client, as the Claude executor sends it, within `timeout` if any.
    async fn timed_exchange(
        rt: &std::sync::Arc<crate::runtime::Runtime>,
        url: &str,
        body: bytes::Bytes,
        timeout: Option<std::time::Duration>,
    ) -> Value {
        use futures_util::StreamExt;
        let facts = std::sync::Arc::new(Facts::new(
            Client::default(),
            Format::Claude,
            Format::Claude,
            "m",
            b"{}",
            true,
            &crate::gateway::dispatch::Trace::default(),
        ));
        let tracker = Tracker::start(rt, &facts, &credential_for("claude"), "m");
        let sink = tracker.sink();
        let clients = crate::providers::proxy::GoClients::new(Default::default()).timed();
        let sent = crate::providers::proxy::send_routed(
            &|_| {
                Ok(crate::providers::proxy::Route {
                    client: clients.get(&crate::providers::proxy::Proxy::Direct),
                    order: None,
                })
            },
            url,
            crate::providers::proxy::GoHeaders::new(),
            body,
            timeout,
            Some(&sink),
        )
        .await;
        match sent {
            Ok(upstream) => {
                let chunks: Vec<_> = upstream.body.collect().await;
                assert!(chunks.iter().all(Result::is_ok));
                tracker.succeed();
            }
            Err(error) => tracker.fail(&error),
        }
        let queued = rt.usage_queue().pop_oldest(10);
        assert_eq!(queued.len(), 1);
        serde_json::from_slice(&queued[0]).unwrap()
    }

    /// A redirect starts the upload measurements over. A POST answered with 303 and
    /// followed as a GET reports no upload and no request body, not the POST's. A 307
    /// whose second upload is cut short reports that hop's connection and no upload,
    /// not the first hop's finished one. A request the deadline stops before it is sent
    /// reports no request body.
    #[tokio::test]
    async fn redirects_start_the_upload_measurements_over() {
        let rt = usage_runtime();
        let upstream = redirecting_upstream().await;
        let record = timed_exchange(
            &rt,
            &format!("{upstream}/see-other"),
            bytes::Bytes::from_static(b"{}"),
            None,
        )
        .await;
        let timings = &record["timings"];
        assert_eq!(record["failed"], false, "{record}");
        assert_eq!(record["upstream_request_bytes"], 0, "{record}");
        assert_eq!(record["upstream_response_bytes"], 5, "{record}");
        assert!(timings["upstream_headers_ms"].is_i64(), "{record}");
        for stale in ["upstream_connected_ms", "upstream_body_sent_ms"] {
            assert!(timings.get(stale).is_none(), "{stale}: {record}");
        }

        // More than the client and the sockets hold, so the second upload cannot finish.
        let body = bytes::Bytes::from(vec![b'x'; 6 << 20]);
        let record = timed_exchange(&rt, &format!("{upstream}/temporary"), body, None).await;
        let timings = &record["timings"];
        assert_eq!(record["failed"], true, "{record}");
        assert!(timings["upstream_connected_ms"].is_i64(), "{record}");
        assert!(timings.get("upstream_body_sent_ms").is_none(), "{record}");
        assert!(record.get("upstream_request_bytes").is_none(), "{record}");

        let expired = Some(std::time::Duration::ZERO);
        let record = timed_exchange(&rt, &format!("{upstream}/landing"), bytes::Bytes::new(), expired).await;
        assert_eq!(record["failed"], true, "{record}");
        assert!(record.get("upstream_request_bytes").is_none(), "{record}");
    }

    /// A late report from an earlier hop (its body, taken after its redirect arrived)
    /// does not fill in the hop being sent.
    #[test]
    fn a_late_hop_report_is_dropped() {
        use crate::execution::UpstreamStage;
        let start = std::time::Instant::now();
        let at = |ms| start + std::time::Duration::from_millis(ms);
        let mut exchange = Exchange::default();
        for (ms, stage) in [
            (0, UpstreamStage::Started),
            (0, UpstreamStage::Hop { hop: 0, body: true }),
            (1, UpstreamStage::Connected { hop: 0 }),
            (2, UpstreamStage::Hop { hop: 1, body: true }),
            (3, UpstreamStage::Connected { hop: 1 }),
            (4, UpstreamStage::Connected { hop: 0 }),
            (5, UpstreamStage::BodySent { hop: 0, bytes: 9 }),
        ] {
            exchange.mark(stage, at(ms));
        }
        assert_eq!((exchange.started, exchange.connected), (Some(at(0)), Some(at(3))));
        assert_eq!((exchange.body_sent, exchange.request_bytes), (None, None));
    }

    /// TTFT chooses the first token mark, uses a first-packet fallback, and stays
    /// unchanged once settled. Controlled instants avoid sleep and speed bounds.
    #[test]
    fn ttft_marks_settle_once_with_a_packet_fallback() {
        let mut absent = Ttft::default();
        absent.first_byte();
        assert!(absent.tracked);
        assert_eq!(absent.get(), std::time::Duration::ZERO);
        let start = std::time::Instant::now() - std::time::Duration::from_secs(1);
        let packet = std::time::Duration::from_millis(10);
        let mut t = Ttft {
            tracked: true,
            start: Some(start),
            first_packet: Some(packet),
            ttft: None,
        };
        t.token(false);
        assert_eq!(t.get(), packet, "later metadata does not replace the first packet");
        t.token(true);
        let settled = t.get();
        assert!(t.start.is_none());
        assert_eq!(t.ttft, Some(settled));
        t.first_byte();
        t.start();
        t.token(true);
        assert_eq!(t.get(), settled);
        let mut byte = Ttft {
            tracked: true,
            start: Some(start),
            ..Default::default()
        };
        byte.first_byte();
        assert!(byte.ttft.is_some());
        assert!(byte.start.is_none());
    }
}
