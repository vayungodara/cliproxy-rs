//! Usage records for the usage queue, one per upstream attempt (Go
//! sdk/cliproxy/usage/accounting.go, internal/runtime/executor/helps/usage_helpers.go
//! and response_model.go, internal/redisqueue/plugin.go `HandleUsage`).
//!
//! Go's executors parse usage from the upstream response in its own format. Executors
//! that forward those payloads through `ExecRequest::usage` get the same parse here;
//! otherwise the dispatch loop parses the client-format response, which matches Go
//! whenever the upstream speaks the client's format.

use cpa_common::gostr::GoStr;
use cpa_common::json::{self as gj, Kind, Res};
use cpa_core::format::Format;

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
    pub total: i64,
    pub breakdown: Option<Breakdown>,
    pub response_service_tier: String,
}

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

/// Go `invalidUsageTokenBreakdown`.
fn invalid(total: i64) -> Breakdown {
    let total = total.max(0);
    Breakdown {
        quality: Quality::Inconsistent,
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
        ..Detail::default()
    };
    if d.cached == 0 {
        d.cached = d.cache_creation;
    }
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

/// Go `safeUsageTokenSum`.
fn safe_sum(values: &[i64]) -> Option<i64> {
    sum(values)
}

/// Go `parseGeminiFamilyUsageDetail`.
fn parse_gemini_node(node: &Res<'_>) -> Detail {
    let cached = node.get("cachedContentTokenCount").int();
    let tool_use = first(node, &["toolUsePromptTokenCount", "tool_use_prompt_token_count"]).int();
    let input = safe_sum(&[node.get("promptTokenCount").int(), tool_use]);
    let mut d = Detail {
        input: input.unwrap_or(0),
        output: node.get("candidatesTokenCount").int(),
        reasoning: node.get("thoughtsTokenCount").int(),
        total: node.get("totalTokenCount").int(),
        cached,
        cache_read: cached,
        ..Detail::default()
    };
    if input.is_none() {
        d.breakdown = Some(invalid(d.total));
        return d;
    }
    if d.total == 0 {
        match safe_sum(&[d.input, d.output, d.reasoning]) {
            Some(total) => d.total = total,
            None => {
                d.total = 0;
                d.breakdown = Some(invalid(0));
                return d;
            }
        }
    }
    d.breakdown = Some(separate_reasoning(
        d.input,
        d.cache_read,
        d.cache_creation,
        d.output,
        d.reasoning,
        d.total,
    ));
    d
}

/// Go `ParseGeminiUsage`.
pub fn parse_gemini(body: &[u8]) -> Detail {
    let node = first(&gj::parse(body), &["usageMetadata", "usage_metadata"]);
    if !node.exists() {
        return Detail::default();
    }
    parse_gemini_node(&node)
}

/// Go `ParseGeminiStreamUsage`. The Gemini-family executors report the line Go parses
/// (the Gemini executor's `FilterSSEUsageMetadata` payload, Vertex's raw line), so a
/// non-terminal chunk's usage counts when it is reported.
fn parse_gemini_stream(line: &[u8]) -> Option<Detail> {
    let payload = json_payload(line).filter(|p| gj::valid(p))?;
    let node = first(&gj::parse(payload), &["usageMetadata", "usage_metadata"]);
    if !node.exists() {
        return None;
    }
    let d = parse_gemini_node(&node);
    has_tokens(&d).then_some(d)
}

/// Go `ParseAntigravityStreamUsage`: the wrapped `response.usageMetadata` first, and
/// zero usage still counts.
fn parse_antigravity_stream(line: &[u8]) -> Option<Detail> {
    let payload = json_payload(line).filter(|p| gj::valid(p))?;
    let node = first(
        &gj::parse(payload),
        &["response.usageMetadata", "usageMetadata", "usage_metadata"],
    );
    node.exists().then(|| parse_gemini_node(&node))
}

/// Go `parseInteractionsUsageDetail`.
fn parse_interactions_node(node: &Res<'_>) -> Detail {
    let cache_read = first(node, &["cache_read_tokens", "cacheReadTokens"]);
    let tool_use = first(
        node,
        &[
            "tool_use_tokens",
            "total_tool_use_tokens",
            "toolUseTokens",
            "totalToolUseTokens",
        ],
    )
    .int();
    let input = safe_sum(&[
        first(node, &["input_tokens", "prompt_tokens", "total_input_tokens"]).int(),
        tool_use,
    ]);
    let mut d = Detail {
        input: input.unwrap_or(0),
        output: first(node, &["output_tokens", "completion_tokens", "total_output_tokens"]).int(),
        reasoning: first(
            node,
            &["reasoning_tokens", "thoughtsTokenCount", "total_thought_tokens"],
        )
        .int(),
        total: first(node, &["total_tokens", "totalTokenCount"]).int(),
        cached: first(
            node,
            &["cached_tokens", "cachedContentTokenCount", "total_cached_tokens"],
        )
        .int(),
        cache_read: cache_read.int(),
        cache_creation: first(
            node,
            &[
                "cache_creation_tokens",
                "cacheCreationTokens",
                "cache_write_tokens",
                "cacheWriteTokens",
            ],
        )
        .int(),
        ..Detail::default()
    };
    if input.is_none() {
        d.breakdown = Some(invalid(d.total));
        return d;
    }
    if !cache_read.exists() && d.cached > 0 {
        d.cache_read = d.cached;
    }
    if d.total == 0 {
        match safe_sum(&[d.input, d.output, d.reasoning]) {
            Some(total) => d.total = total,
            None => {
                d.total = 0;
                d.breakdown = Some(invalid(0));
                return d;
            }
        }
    }
    d.breakdown = Some(separate_reasoning(
        d.input,
        d.cache_read,
        d.cache_creation,
        d.output,
        d.reasoning,
        d.total,
    ));
    d
}

/// Go `ParseInteractionsUsage`.
pub fn parse_interactions(body: &[u8]) -> Detail {
    let root = gj::parse(body);
    let node = first(
        &root,
        &[
            "usage",
            "total_usage",
            "metadata.total_usage",
            "metadata.usage",
            "usageMetadata",
            "usage_metadata",
            "interaction.usage",
            "interaction.total_usage",
            "interaction.metadata.total_usage",
        ],
    );
    if !node.exists() {
        return Detail::default();
    }
    let mut d = if node.get("promptTokenCount").exists() || node.get("candidatesTokenCount").exists() {
        parse_gemini_node(&node)
    } else {
        parse_interactions_node(&node)
    };
    d.response_service_tier = service_tier(body);
    d
}

/// Go `ParseInteractionsStreamUsage`.
fn parse_interactions_stream(line: &[u8]) -> Option<Detail> {
    let payload = json_payload(line).unwrap_or(line);
    if payload.is_empty() || !gj::valid(payload) {
        return None;
    }
    let d = parse_interactions(payload);
    has_tokens(&d).then_some(d)
}

/// The buffered-response parser for a format (Go's executor for that format).
pub fn parse_body(format: Format, body: &[u8]) -> Detail {
    match format {
        Format::Claude => parse_claude(body),
        // A buffered Responses body is the completed response object: usage and
        // service_tier are top-level, as `ParseOpenAIUsage` reads them.
        Format::OpenAI | Format::OpenAIResponse | Format::Codex => parse_openai(body),
        Format::Gemini => parse_gemini(body),
        Format::Antigravity => {
            let node = first(
                &gj::parse(body),
                &["response.usageMetadata", "usageMetadata", "usage_metadata"],
            );
            if node.exists() {
                parse_gemini_node(&node)
            } else {
                Detail::default()
            }
        }
        Format::Interactions => parse_interactions(body),
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
    fn observe(&mut self, detail: Detail) {
        let tier = detail.response_service_tier.trim().to_owned();
        if tier.is_empty() || has_tokens(&detail) {
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
            // Go's Codex executor publishes the first terminal event's usage.
            Format::OpenAIResponse | Format::Codex => {
                let Some(payload) = json_payload(line) else { return };
                let kind = gj::get(payload, "type");
                if !matches!(
                    kind.str().as_ref(),
                    "response.completed" | "response.incomplete" | "response.done"
                ) || self.ok
                {
                    return;
                }
                if let Some(d) = parse_codex(payload) {
                    self.observe(d);
                }
            }
            Format::Gemini => {
                if let Some(d) = parse_gemini_stream(line) {
                    self.observe(d);
                }
            }
            Format::Antigravity => {
                if let Some(d) = parse_antigravity_stream(line) {
                    self.observe(d);
                }
            }
            Format::Interactions => {
                if let Some(d) = parse_interactions_stream(line) {
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
            "message_stop" => (String::new(), true),
            "message" => (string_at(data, "model").and_then(bounded).unwrap_or_default(), true),
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
            let terminal = (finish.exists() && !finish.str().is_empty())
                || interactions_terminal(&event_type(data), &gj::get(data, "interaction.status").str());
            (model, terminal)
        }
        _ => generic_response_model(data),
    }
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
    let lower = model.trim().to_lowercase();
    cpa_common::thinking::parse_suffix(&lower).model_name.trim().to_owned()
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
        cpa_common::gostr::quote(served),
        cpa_common::gostr::quote(&record.model),
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
    pub request_id: String,
    /// `METHOD /route/template`.
    pub endpoint: String,
    /// The matched client key (Go gin `userApiKey`).
    pub api_key: String,
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
    pub detail: Detail,
    pub response_model: String,
    /// Upstream response headers, names as received.
    pub response_headers: Vec<(String, Vec<String>)>,
}

struct Writer(Vec<u8>, bool);

impl Writer {
    fn key(&mut self, k: &str) {
        if self.1 {
            self.0.push(b',');
        }
        self.1 = true;
        cpa_common::json::marshal_str(&mut self.0, k.as_bytes(), true);
        self.0.push(b':');
    }
    fn str(&mut self, k: &str, v: &str) {
        self.key(k);
        cpa_common::json::marshal_str(&mut self.0, v.as_bytes(), true);
    }
    fn opt_str(&mut self, k: &str, v: &str) {
        if !v.is_empty() {
            self.str(k, v);
        }
    }
    fn int(&mut self, k: &str, v: i64) {
        self.key(k);
        self.0.extend_from_slice(v.to_string().as_bytes());
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
    let session = cpa_common::session::normalize_to_canonical_uuid(&client.session_id);
    let mut parent = cpa_common::session::normalize_to_canonical_uuid(&client.parent_session_id);
    if session.is_empty() || session == parent {
        parent.clear();
    }
    (session, parent)
}

/// Go `usageQueuePlugin.HandleUsage`: the queued `queuedUsageDetail` JSON.
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
        t.int("total_tokens", detail.total);
    });
    w.bool("failed", record.failed);
    w.bool("generate", record.generate);
    w.bool("stream", record.stream);
    w.object("fail", |f| {
        f.int("status_code", fail_status);
        f.str("body", &fail_body);
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
                    cpa_common::json::marshal_str(&mut h.0, v.as_bytes(), true);
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
}

impl Facts {
    /// Go `setReasoningEffortMetadata`, `setServiceTierMetadata` and
    /// `setGenerateMetadata` over the client body; the alias is the client's model.
    pub fn new(client: Client, entry: Format, response: Format, model: &str, body: &[u8], stream: bool) -> Self {
        let tier = gj::get(body, "service_tier");
        let tier = tier.str().trim().to_owned();
        let generate = gj::get(body, "generate");
        Self {
            client,
            format: response,
            alias: model.trim().to_owned(),
            reasoning_effort: cpa_common::thinking::extract_reasoning_effort(body, entry.as_str(), model),
            service_tier: if tier.is_empty() { "auto".into() } else { tier },
            generate: !(generate.is_bool() && !generate.bool()),
            stream,
        }
    }
}

/// Go executor identity for a credential's provider: `Identifier()` and the
/// executor's type name (both reach the record and pick the token semantics).
// ponytail: Codex attempts over the upstream WebSocket report `CodexExecutor`; Go
// names them `CodexWebsocketsExecutor`.
fn executor_identity(c: &cpa_core::credential::Credential) -> (String, &'static str) {
    let provider = c.provider.trim().to_lowercase();
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
            return (cpa_core::registry::dynamic::provider_key(c), "OpenAICompatExecutor");
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
    status: i64,
    body: String,
    detail: Detail,
    model: String,
    latency: std::time::Duration,
    ttft: std::time::Duration,
}

/// What an executor reported through `ExecRequest::usage`.
#[derive(Default)]
struct Reported {
    seen: bool,
    body: Option<Detail>,
    stream: StreamUsage,
    model: ResponseModel,
    effort: Option<String>,
    upstream_model: String,
    ttft: Ttft,
    usage_required: bool,
    outcome: Option<Outcome>,
    /// Go created no reporter for this attempt: publish nothing.
    discarded: bool,
}

impl Reported {
    /// The usage reported so far: a body, else the stream's last usage.
    fn usage(&self) -> Option<Detail> {
        self.body.clone().or_else(|| self.stream.detail().cloned())
    }
}

/// The executor-facing observer; `provider` picks the response-model extractor.
struct Observer {
    provider: String,
    started: std::time::Instant,
    state: std::sync::Mutex<Reported>,
}

impl Observer {
    fn lock(&self) -> std::sync::MutexGuard<'_, Reported> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn settle(&self, failed: bool, status: i64, body: &str) {
        let mut r = self.lock();
        if r.outcome.is_some() {
            return;
        }
        let detail = if failed {
            Detail::default()
        } else {
            r.usage().unwrap_or_default()
        };
        r.outcome = Some(Outcome {
            failed,
            status,
            body: body.trim().to_owned(),
            detail,
            model: r.model.get().to_owned(),
            latency: self.started.elapsed(),
            ttft: r.ttft.get(),
        });
    }
}

impl cpa_core::exec::UsageObserver for Observer {
    fn response_body(&self, format: Format, body: &[u8]) {
        let mut r = self.lock();
        r.seen = true;
        r.body = Some(parse_body(format, body));
        r.model.observe(body, &self.provider);
    }

    fn response_line(&self, format: Format, line: &[u8]) {
        let mut r = self.lock();
        r.seen = true;
        r.stream.line(format, line);
        r.model.observe(line, &self.provider);
    }

    fn request(&self, format: Format, payload: &[u8]) {
        self.request_for(format.as_str(), payload);
    }

    fn request_for(&self, identifier: &str, payload: &[u8]) {
        self.lock().effort = Some(cpa_common::thinking::extract_translated_reasoning_effort(
            payload, identifier,
        ));
    }

    fn upstream_model(&self, model: &str) {
        self.lock().upstream_model = model.trim().to_owned();
    }

    fn response_model(&self, model: &str) {
        self.lock().model.set(model);
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
        self.settle(false, 0, "");
    }

    fn publish_failure(&self, status: u16, body: &str) {
        self.settle(true, i64::from(status), body);
    }

    fn usage_required(&self) {
        self.lock().usage_required = true;
    }

    fn discard(&self) {
        self.lock().discarded = true;
    }
}

/// How the attempt ended for the server.
enum Ending<'a> {
    /// Success, or a stream the client dropped (Go's deferred publish).
    Done,
    Failed(&'a cpa_core::exec::ExecError),
}

/// One upstream attempt's record in progress. It publishes at most once: what the
/// executor published itself, else on success, on failure, or when dropped
/// mid-stream (Go's deferred publish).
pub struct Tracker {
    queue: std::sync::Arc<crate::Runtime>,
    facts: std::sync::Arc<Facts>,
    record: Record,
    auth_id: String,
    started: std::time::Instant,
    observer: std::sync::Arc<Observer>,
    stream: StreamUsage,
    body: Option<Detail>,
    model: ResponseModel,
    first: Option<std::time::Duration>,
    published: bool,
    /// The request's client facts under this attempt's session, when it has its own:
    /// Home's canonical session for a dispatched credential, else its LCP pick's.
    client: Option<Client>,
    /// Go `requestedAt`, for usage plugins.
    requested_at: chrono::DateTime<chrono::FixedOffset>,
    /// Go `baseURL`: the credential's `base_url` attribute, else its metadata's.
    base_url: String,
}

impl Tracker {
    /// The attempt's session is its LCP pick's (Go `syncMetadataSessionToContext`), not
    /// the request's.
    pub fn lcp_session(&mut self, m: &crate::lcp::Match) {
        let bound = cpa_common::session::bound_session_identity;
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
        self.client = Some(client);
    }

    /// Starts the record for one attempt of `credential` on `upstream_model`.
    pub fn start(
        rt: &std::sync::Arc<crate::Runtime>,
        facts: &std::sync::Arc<Facts>,
        credential: &cpa_core::credential::Credential,
        upstream_model: &str,
    ) -> Self {
        let (provider, executor_type) = executor_identity(credential);
        let now = chrono::Local::now();
        let base_url = credential
            .attributes
            .get("base_url")
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
            .or_else(|| credential.str("base_url").map(str::trim))
            .unwrap_or_default()
            .to_owned();
        let record = Record {
            timestamp: go_timestamp(&now),
            execution_id: uuid::Uuid::new_v4().to_string(),
            executor_type: executor_type.to_owned(),
            model: cpa_common::thinking::parse_suffix(upstream_model).model_name,
            alias: facts.alias.clone(),
            source: source(credential, &facts.client.api_key),
            auth_index: cpa_core::config::credentials::auth_index(credential),
            access_token_sha256: access_token_sha256(credential),
            auth_type: cpa_core::registry::dynamic::auth_kind(credential)
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
        Self {
            queue: rt.clone(),
            facts: facts.clone(),
            record,
            auth_id: credential.id.clone(),
            started,
            observer: std::sync::Arc::new(Observer {
                provider,
                started,
                state: std::sync::Mutex::default(),
            }),
            stream: StreamUsage::default(),
            body: None,
            model: ResponseModel::default(),
            first: None,
            published: false,
            client: home_client,
            requested_at: now.fixed_offset(),
            base_url,
        }
    }

    /// The sink to hand the executor.
    pub fn sink(&self) -> cpa_core::exec::UsageSink {
        cpa_core::exec::UsageSink::new(self.observer.clone())
    }

    /// The upstream answered; its headers are the record's `response_headers`.
    // ponytail: without the executor's TTFT marks (`UsageSink::round_trip_started` and
    // `first_byte`), TTFT is measured to the executor's response (buffered) or first
    // event (streams); Go measures from the upstream request to its first body byte.
    pub fn arrived(&mut self, headers: &axum::http::HeaderMap) {
        self.first.get_or_insert_with(|| self.started.elapsed());
        self.record.response_headers = go_headers(headers);
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

    /// Go `PublishFailure`: the error's status and body without usage. Claude's
    /// executor fails through its stream buffer (`StreamUsageBuffer.PublishFailure`),
    /// which keeps the usage seen so far.
    pub fn fail(mut self, error: &cpa_core::exec::ExecError) {
        self.publish(Ending::Failed(error));
    }

    fn publish(&mut self, ending: Ending<'_>) {
        if std::mem::replace(&mut self.published, true) {
            return;
        }
        let queue = self.queue.usage_queue();
        // Go publishes every record to all usage plugins; the queue is one of them.
        let plugins = self.queue.plugins().has_usage_plugins();
        if !queue.accepts() && !plugins {
            return;
        }
        let reported = std::mem::take(&mut *self.observer.lock());
        if reported.discarded {
            return;
        }
        let mut record = std::mem::take(&mut self.record);
        let (latency, ttft);
        if let Some(outcome) = reported.outcome {
            record.failed = outcome.failed;
            record.fail_status = outcome.status;
            record.fail_body = outcome.body;
            record.detail = outcome.detail;
            record.response_model = outcome.model;
            (latency, ttft) = (outcome.latency, outcome.ttft);
        } else {
            let usage = if reported.seen {
                reported.usage()
            } else {
                self.body.take().or_else(|| self.stream.detail().cloned())
            };
            let detail = match ending {
                Ending::Done if reported.usage_required && usage.is_none() => return,
                Ending::Done => usage.unwrap_or_default(),
                Ending::Failed(error) => {
                    record.failed = true;
                    record.fail_status = i64::from(crate::classify::go_status(error));
                    record.fail_body = crate::classify::error_text(error);
                    if record.response_headers.is_empty() {
                        record.response_headers = go_headers(&error.headers);
                    }
                    if self.observer.provider == "claude" {
                        usage.unwrap_or_default()
                    } else {
                        Detail::default()
                    }
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
        record.latency_ms = latency.as_millis() as i64;
        record.ttft_ms = ttft.as_millis() as i64;
        if let Some(effort) = reported.effort {
            record.reasoning_effort = effort;
        }
        if plugins {
            self.queue
                .plugins()
                .publish_usage(self.plugin_record(&record, latency, ttft));
        }
        if queue.accepts() {
            queue.enqueue(queued(&record, self.client.as_ref().unwrap_or(&self.facts.client)));
        }
        warn_model_substitution(&record, &reported.upstream_model, &self.auth_id);
    }
}

impl Tracker {
    /// Go `usageAdapter.HandleUsage`'s record.
    fn plugin_record(
        &self,
        record: &Record,
        latency: std::time::Duration,
        ttft: std::time::Duration,
    ) -> cpa_plugin::api::UsageRecord {
        let client = self.client.as_ref().unwrap_or(&self.facts.client);
        // Go `NewUsageReporter`: the client request's session; a parent that is not a
        // hierarchy parent is dropped. The adapter then applies its own rule.
        let parent = if client.session_id.is_empty() || client.session_id == client.parent_session_id {
            String::new()
        } else {
            client.parent_session_id.clone()
        };
        let (session_id, parent_session_id) = cpa_plugin::transform::usage_session_ids(
            &client.session_id,
            &parent,
            &client.session_id,
            &client.parent_session_id,
        );
        let nanos = |d: std::time::Duration| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX);
        cpa_plugin::api::UsageRecord {
            request_id: record.execution_id.clone(),
            trace_id: client.request_id.trim().to_owned(),
            provider: record.provider.clone(),
            base_url: self.base_url.clone(),
            executor_type: record.executor_type.clone(),
            model: record.model.clone(),
            alias: record.alias.clone(),
            api_key: client.api_key.clone(),
            session_id,
            parent_session_id,
            auth_id: self.auth_id.clone(),
            auth_index: record.auth_index.clone(),
            auth_type: record.auth_type.clone(),
            source: record.source.clone(),
            reasoning_effort: record.reasoning_effort.clone(),
            service_tier: record.service_tier.clone(),
            response_service_tier: record.detail.response_service_tier.clone(),
            response_model: record.response_model.clone(),
            generate: record.generate,
            stream: record.stream,
            requested_at: cpa_plugin::gojson::GoTime(Some(self.requested_at)),
            latency_ns: nanos(latency),
            ttft_ns: nanos(ttft),
            failed: record.failed,
            failure: cpa_plugin::api::UsageFailure {
                status_code: record.fail_status,
                body: record.fail_body.clone(),
            },
            detail: cpa_plugin::api::UsageDetail {
                input_tokens: record.detail.input,
                output_tokens: record.detail.output,
                reasoning_tokens: record.detail.reasoning,
                cached_tokens: record.detail.cached,
                cache_read_tokens: record.detail.cache_read,
                cache_creation_tokens: record.detail.cache_creation,
                total_tokens: record.detail.total,
            },
            response_headers: record.response_headers.iter().cloned().collect(),
        }
    }
}

impl Drop for Tracker {
    fn drop(&mut self) {
        self.publish(Ending::Done);
    }
}

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
pub fn source(c: &cpa_core::credential::Credential, client_key: &str) -> String {
    let meta = |k: &str| c.str(k).map(str::trim).filter(|v| !v.is_empty());
    let attr = |k: &str| c.attributes.get(k).map(|v| v.trim()).filter(|v| !v.is_empty());
    if c.provider.trim().go_eq_fold("vertex")
        && let Some(project) = meta("project_id").or_else(|| meta("project"))
    {
        return project.to_owned();
    }
    let account = match cpa_core::registry::dynamic::auth_kind(c) {
        Some("oauth") => meta("email"),
        Some("apikey") => attr("api_key"),
        _ => None,
    };
    account
        .or_else(|| meta("email"))
        .or_else(|| attr("api_key"))
        .unwrap_or(client_key.trim())
        .to_owned()
}

/// Go `AccessTokenSHA256`.
pub fn access_token_sha256(c: &cpa_core::credential::Credential) -> String {
    cpa_core::config::credentials::access_token_sha256(c)
}

/// Go `Auth.AuthSourceKind` over a dispatched auth's attributes. Its last fallback,
/// `FileName`, never reaches a Home auth: Go's auth JSON omits it.
fn auth_source_kind(c: &cpa_core::credential::Credential) -> &'static str {
    let attr = |k: &str| c.attributes.get(k).map_or("", |v| v.trim());
    let normalize = |s: &str| match s.trim().to_lowercase().as_str() {
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
        if source.to_lowercase().starts_with("config:") {
            return "config";
        }
        return normalize(source).unwrap_or("file");
    }
    if attr("path").is_empty() { "" } else { "file" }
}

/// The request facts with the session Home dispatched under (Go
/// `syncMetadataSessionToContext` with `CanonicalSessionIDMetadataKey`): a dispatched
/// credential's canonical session and its parent replace the request's own.
fn home_session<'a>(client: &'a Client, credential: &cpa_core::credential::Credential) -> std::borrow::Cow<'a, Client> {
    let attr = |k: &str| credential.attributes.get(k).map_or("", |v| v.trim());
    let session = attr(crate::remote::SESSION);
    if session.is_empty() {
        return std::borrow::Cow::Borrowed(client);
    }
    let parent = attr(crate::remote::PARENT_SESSION);
    let mut client = client.clone();
    client.session_id = session.to_owned();
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
    rt: &crate::Runtime,
    facts: &Facts,
    credential: &cpa_core::credential::Credential,
    provider: &str,
    model: &str,
    body: &str,
) {
    let queue = rt.usage_queue();
    if !queue.accepts() {
        return;
    }
    let auth_index = cpa_core::config::credentials::auth_index(credential).trim().to_owned();
    let access_token_sha256 = access_token_sha256(credential);
    if auth_index.is_empty() || access_token_sha256.is_empty() {
        return;
    }
    let provider = match provider.trim() {
        "" => credential
            .attributes
            .get(crate::remote::PROVIDER)
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
        auth_type: cpa_core::registry::dynamic::auth_kind(credential)
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
        ..Record::default()
    };
    // The record carries no client key (Go leaves `Record.APIKey` empty).
    let mut client = home_session(&facts.client, credential).into_owned();
    client.api_key.clear();
    queue.enqueue(queued(&record, &client));
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

    const FIXTURE: &str = include_str!("../tests/fixtures/server_go.json");

    #[test]
    fn go_timestamps() {
        use chrono::TimeZone;
        let t = chrono::Utc.with_ymd_and_hms(2026, 10, 3, 8, 0, 0).unwrap();
        assert_eq!(go_timestamp(&t), "2026-10-03T08:00:00Z");
        let t = t + chrono::Duration::nanoseconds(123_456_000);
        assert_eq!(go_timestamp(&t), "2026-10-03T08:00:00.123456Z");
        let east = chrono::FixedOffset::east_opt(5 * 3600 + 1800).unwrap();
        assert_eq!(
            go_timestamp(&t.with_timezone(&east)),
            "2026-10-03T13:30:00.123456+05:30"
        );
    }

    /// Claude's executor-published outcomes through the queue: an apply_patch
    /// rejection is Go's `reporter.PublishFailure(err)` after usage was observed (no
    /// tokens, response model kept, and the later failure the server sees changes
    /// nothing); `discard` is Go returning before it created a reporter (no record).
    #[test]
    fn claude_executor_outcomes_reach_the_queue() {
        use cpa_core::exec::{ExecError, FailureScope};
        let rt = usage_runtime();
        let facts = std::sync::Arc::new(Facts::new(
            Client::default(),
            Format::OpenAIResponse,
            Format::OpenAIResponse,
            "claude-sonnet-4-6",
            b"{}",
            true,
        ));
        let error = ExecError::local(502, FailureScope::Credential, "Invalid apply_patch tool arguments");
        let tracker = Tracker::start(&rt, &facts, &credential_for("claude"), "claude-sonnet-4-6");
        let sink = tracker.sink();
        sink.usage_required();
        sink.response_line(
            Format::Claude,
            br#"data: {"type":"message_start","message":{"model":"claude-upstream","usage":{"input_tokens":9,"output_tokens":1}}}"#,
        );
        sink.publish_failure(502, "Invalid apply_patch tool arguments");
        tracker.fail(&error);
        let got: Vec<Value> = rt.usage_queue().pop_oldest(10).iter().map(|q| summary(q)).collect();
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
        tracker.sink().response_line(
            Format::Claude,
            br#"data: {"type":"message_start","message":{"model":"claude-upstream","usage":{"input_tokens":9,"output_tokens":1}}}"#,
        );
        tracker.fail(&error);
        let got = rt.usage_queue().pop_oldest(10);
        assert_eq!(summary(&got[0])["input"], 9);
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

    /// Records from Go's real parsers, `UsageReporter` and `usageQueuePlugin`
    /// (tests/reference/server/main.go `usageRecords`), byte for byte.
    #[test]
    fn queued_records_match_go() {
        let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
        let cases = fixture["usage"].as_array().unwrap();
        assert_eq!(cases.len(), 14);
        for (index, case) in cases.iter().enumerate() {
            let name = case["name"].as_str().unwrap();
            let s = |k: &str| case[k].as_str().unwrap_or_default().to_owned();
            let format = Format::parse(&s("format")).unwrap();
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
            let mut credential = cpa_core::credential::Credential::from_file(
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
                cpa_common::thinking::extract_translated_reasoning_effort(
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
                auth_index: cpa_core::config::credentials::auth_index(&credential),
                access_token_sha256: access_token_sha256(&credential),
                auth_type: cpa_core::registry::dynamic::auth_kind(&credential)
                    .unwrap_or_default()
                    .to_owned(),
                reasoning_effort: reasoning,
                service_tier: s("service_tier"),
                generate: case["generate"].as_bool().unwrap(),
                stream: case["stream"].as_bool().unwrap(),
                failed: case["failed"].as_bool().unwrap(),
                fail_status: case["fail_status"].as_i64().unwrap(),
                fail_body: s("fail_body"),
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
                request_id: s("request_id"),
                endpoint: s("endpoint"),
                api_key: s("client_key"),
            };
            let got = queued(&record, &client);
            // Go's exact bytes: the fixture re-indents the queued payload; compacting
            // restores it.
            let raw = gj::get(FIXTURE.as_bytes(), &format!("usage.{index}.queued"));
            let want = gj::compact(raw.raw(), false);
            assert_eq!(String::from_utf8_lossy(&got), String::from_utf8_lossy(&want), "{name}");
        }
    }

    /// A runtime whose usage queue keeps records.
    fn usage_runtime() -> std::sync::Arc<crate::Runtime> {
        let cfg =
            cpa_core::config::Config::parse("observability:\n  usage:\n    usage-statistics-enabled: true\n").unwrap();
        let executors = cpa_exec::Executors {
            claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
            codex: Default::default(),
            devices: Default::default(),
            openai: Default::default(),
            google: Default::default(),
        };
        let rt = std::sync::Arc::new(crate::testing::runtime(cfg.clone(), Vec::new(), executors));
        rt.usage_queue().configure(true, &cfg);
        rt
    }

    fn credential_for(provider: &str) -> cpa_core::credential::Credential {
        let mut metadata = serde_json::Map::new();
        metadata.insert("type".into(), provider.into());
        let mut c = cpa_core::credential::Credential::from_file(
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

    /// Go `UsageReporter` call sequences as Go's executors make them
    /// (tests/reference/server/main.go `reporterSequences`), replayed through the
    /// tracker and the executor sink: what Go publishes, and when it publishes nothing.
    #[test]
    fn reporter_sequences_match_go() {
        use cpa_core::exec::{ExecError, FailureScope};
        let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
        let cases = fixture["reporter"].as_array().unwrap();
        let gemini = [
            r#"data: {"candidates":[{"content":{"parts":[{"text":"a"}]}}],"usageMetadata":{"promptTokenCount":4,"candidatesTokenCount":1,"totalTokenCount":5}}"#,
            r#"data: {"candidates":[{"content":{"parts":[{"text":"b"}]}}],"usageMetadata":{"promptTokenCount":4,"candidatesTokenCount":3,"totalTokenCount":7}}"#,
        ];
        let claude = [
            r#"data: {"type":"message_start","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":9,"output_tokens":1}}}"#,
            r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":6}}"#,
        ];
        let kimi_chunk = r#"data: {"id":"c","object":"chat.completion.chunk","model":"kimi-k2","choices":[{"index":0,"delta":{"content":"x"}}]}"#;
        let kimi_usage = r#"data: {"id":"c","object":"chat.completion.chunk","model":"kimi-k2","choices":[],"usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}"#;
        let gemini_body = br#"{"usageMetadata":{"promptTokenCount":2,"candidatesTokenCount":2,"totalTokenCount":4}}"#;
        let error = |status: u16, text: &str| ExecError::local(status, FailureScope::Request, text);
        let rt = usage_runtime();
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let (provider, model) = match name.split(' ').next().unwrap() {
                "gemini" | "publish" | "failure" => ("gemini", "gemini-2.5-pro"),
                "claude" => ("claude", "claude-sonnet-4-6"),
                "kimi" => ("kimi", "kimi-k2"),
                "devin" => ("devin", "devin-chat"),
                _ => ("codex", "gpt-5.5"),
            };
            let facts = std::sync::Arc::new(Facts::new(
                Client::default(),
                Format::OpenAI,
                Format::OpenAI,
                model,
                b"{}",
                true,
            ));
            let tracker = Tracker::start(&rt, &facts, &credential_for(provider), model);
            let sink = tracker.sink();
            let lines = |format: Format, lines: &[&str]| {
                for line in lines {
                    sink.response_line(format, line.as_bytes());
                }
            };
            match name {
                "gemini stream usage then scan error" => {
                    lines(Format::Gemini, &gemini);
                    tracker.fail(&error(0, "read: connection reset"));
                }
                "gemini stream usage" => {
                    lines(Format::Gemini, &gemini);
                    tracker.succeed();
                }
                "claude stream usage then scan error" => {
                    lines(Format::Claude, &claude);
                    tracker.fail(&error(502, "stream broke"));
                }
                "kimi stream without usage" => {
                    sink.usage_required();
                    lines(Format::OpenAI, &[kimi_chunk, "data: [DONE]"]);
                    tracker.succeed();
                }
                "kimi stream with usage" => {
                    sink.usage_required();
                    lines(Format::OpenAI, &[kimi_chunk, kimi_usage, "data: [DONE]"]);
                    tracker.succeed();
                }
                // The executor reports no body because Go's condition did not publish.
                "kimi native nonstream without usage" => {
                    sink.usage_required();
                    tracker.succeed();
                }
                _ if name.starts_with("kimi translated effort ") => {
                    let payload = name.trim_start_matches("kimi translated effort ");
                    sink.request_for("kimi", payload.as_bytes());
                    sink.response_body(
                        Format::OpenAI,
                        br#"{"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
                    );
                    sink.publish();
                    tracker.succeed();
                }
                "devin set response model" => {
                    sink.response_model("  devin-model-x ");
                    tracker.succeed();
                }
                // Go's deferred EnsurePublished wins over the stream error the client sees.
                "devin truncated before EOS" => {
                    sink.publish();
                    tracker.fail(&error(0, "devin stream terminated prematurely before EOS trailer"));
                }
                "devin trailer error" => {
                    sink.publish_failure(0, "devin trailer: permission_denied");
                    tracker.fail(&error(403, "devin trailer: permission_denied"));
                }
                "publish then failure" => {
                    sink.response_body(Format::Gemini, gemini_body);
                    sink.publish();
                    tracker.fail(&error(500, "late"));
                }
                "failure then publish" => {
                    sink.publish_failure(429, "slow down");
                    sink.response_body(Format::Gemini, gemini_body);
                    sink.publish();
                    tracker.succeed();
                }
                "terminal response model then set" => {
                    sink.response_line(
                        Format::OpenAIResponse,
                        br#"data: {"type":"response.completed","response":{"model":"gpt-5.5-2026-01-01"}}"#,
                    );
                    sink.response_model("other-model");
                    sink.publish();
                    tracker.succeed();
                }
                other => panic!("unscripted Go case {other}"),
            }
            let got: Vec<Value> = rt.usage_queue().pop_oldest(10).iter().map(|q| summary(q)).collect();
            let want: Vec<Value> = case["records"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| {
                    // The golden holds Go's record before the queue; Go's queue plugin
                    // (redisqueue/plugin.go `failDetail`) reports 200 for a success and
                    // 500 for a failure without status, as `queued` does.
                    let status = match (r["failed"].as_bool().unwrap(), r["fail_status"].as_i64().unwrap()) {
                        (false, _) => 200,
                        (true, s) if s <= 0 => 500,
                        (true, s) => s,
                    };
                    serde_json::json!({
                        "failed": r["failed"], "fail_status": status, "fail_body": r["fail_body"],
                        "input": r["input"], "output": r["output"], "total": r["total"],
                        "response_model": r["response_model"], "reasoning_effort": r["reasoning_effort"],
                        "model": r["model"],
                    })
                })
                .collect();
            assert_eq!(got, want, "{name}");
        }
    }

    /// Go `IsModelSubstituted` (tests/reference/server/main.go `substitutions`).
    #[test]
    fn model_substitution_matches_go() {
        let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
        for case in fixture["substitution"].as_array().unwrap() {
            let (requested, served) = (case["requested"].as_str().unwrap(), case["served"].as_str().unwrap());
            assert_eq!(
                is_model_substituted(requested, served),
                case["substituted"].as_bool().unwrap(),
                "{requested:?} -> {served:?}"
            );
        }
    }

    /// Go `Manager.ReportHomeUnauthorized` through Go's usage queue plugin
    /// (zz_rustgolden_unauthorized_test.go in the reference): the same queued record,
    /// or none without an access token. The first case reaches Go's session through
    /// Home's canonical session on the credential, not the request's own.
    #[test]
    fn home_unauthorized_records_match_go() {
        let fixture: Value = serde_json::from_str(include_str!("../tests/fixtures/home_unauthorized_go.json")).unwrap();
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
            attributes.insert(cpa_core::config::credentials::HOME_AUTH_INDEX.into(), s(&case["index"]));
            attributes.insert(crate::remote::PROVIDER.into(), s(&case["auth_provider"]));
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
                api_key: "client-key-must-not-appear".into(),
                ..Default::default()
            };
            if name == "count_tokens_oauth_runtime" {
                attributes.insert(crate::remote::SESSION.into(), client.session_id.clone());
                attributes.insert(crate::remote::PARENT_SESSION.into(), client.parent_session_id.clone());
                client.session_id = "the-request-own-session".into();
                client.parent_session_id.clear();
            }
            let credential = cpa_core::credential::Credential {
                id: s(&case["id"]),
                provider: s(&case["auth_provider"]).to_lowercase(),
                source: cpa_core::credential::Source::File(s(&case["id"]).into()),
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
            if case["record"].is_null() {
                assert!(got.is_empty(), "{name}: {got:?}");
                continue;
            }
            assert_eq!(got.len(), 1, "{name}");
            let mut want = case["record"].clone();
            for record in [&mut want, &mut got[0]] {
                let record = record.as_object_mut().unwrap();
                assert!(record.remove("timestamp").is_some(), "{name}");
                assert_eq!(
                    record.remove("execution_id").map(|v| v.as_str().unwrap().len()),
                    Some(36)
                );
            }
            assert_eq!(got[0], want, "{name}");
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
        let facts = std::sync::Arc::new(Facts::new(client, Format::Claude, Format::Claude, "m", b"{}", false));
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
        let uuid = |s: &str| Value::from(cpa_common::session::normalize_to_canonical_uuid(s));
        assert_eq!(record(&[]), (uuid("request-session"), uuid("request-parent")));
        assert_eq!(
            record(&[
                (crate::remote::SESSION, "home-session"),
                (crate::remote::PARENT_SESSION, "")
            ]),
            (uuid("home-session"), Value::Null)
        );
        assert_eq!(
            record(&[
                (crate::remote::SESSION, "home-session"),
                (crate::remote::PARENT_SESSION, "home-parent")
            ]),
            (uuid("home-session"), uuid("home-parent"))
        );
        assert_eq!(
            record(&[
                (crate::remote::SESSION, "home-session"),
                (crate::remote::PARENT_SESSION, "home-session")
            ]),
            (uuid("home-session"), Value::Null)
        );
    }

    /// Go's TTFT: from the upstream request to the first body byte; the first packet
    /// stands in until a token event; nothing without a start.
    #[test]
    fn ttft_marks_follow_go() {
        let mut t = Ttft::default();
        t.first_byte();
        assert!(t.tracked);
        assert_eq!(t.get(), std::time::Duration::ZERO);

        let mut t = Ttft::default();
        t.start();
        std::thread::sleep(std::time::Duration::from_millis(5));
        t.token(false);
        let packet = t.get();
        assert!(packet >= std::time::Duration::from_millis(5));
        std::thread::sleep(std::time::Duration::from_millis(5));
        t.token(false);
        assert_eq!(t.get(), packet, "a second non-token frame changes nothing");
        t.token(true);
        let ttft = t.get();
        assert!(ttft > packet);
        t.first_byte();
        t.start();
        assert_eq!(t.get(), ttft, "the first TTFT wins");
    }
}
