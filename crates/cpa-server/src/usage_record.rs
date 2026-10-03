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

    pub fn get(&self) -> &str {
        &self.model
    }
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
    if client.is_fork {
        w.bool("is_fork", true);
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

/// What an executor reported through `ExecRequest::usage`.
#[derive(Default)]
struct Reported {
    seen: bool,
    body: Option<Detail>,
    stream: StreamUsage,
    model: ResponseModel,
    effort: Option<String>,
}

/// The executor-facing observer; `provider` picks the response-model extractor.
struct Observer {
    provider: String,
    state: std::sync::Mutex<Reported>,
}

impl Observer {
    fn lock(&self) -> std::sync::MutexGuard<'_, Reported> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
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
        self.lock().effort = Some(cpa_common::thinking::extract_translated_reasoning_effort(
            payload,
            format.as_str(),
        ));
    }

    fn failed(&self) {
        let mut r = self.lock();
        r.seen = true;
        r.body = Some(Detail::default());
    }
}

/// One upstream attempt's record in progress. It publishes exactly once: on
/// success, on failure, or when dropped mid-stream (Go's deferred publish).
pub struct Tracker {
    queue: std::sync::Arc<crate::Runtime>,
    facts: std::sync::Arc<Facts>,
    record: Record,
    started: std::time::Instant,
    observer: std::sync::Arc<Observer>,
    stream: StreamUsage,
    body: Option<Detail>,
    model: ResponseModel,
    first: Option<std::time::Duration>,
    published: bool,
}

impl Tracker {
    /// Starts the record for one attempt of `credential` on `upstream_model`.
    pub fn start(
        rt: &std::sync::Arc<crate::Runtime>,
        facts: &std::sync::Arc<Facts>,
        credential: &cpa_core::credential::Credential,
        upstream_model: &str,
    ) -> Self {
        let (provider, executor_type) = executor_identity(credential);
        let record = Record {
            timestamp: go_timestamp(&chrono::Local::now()),
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
        Self {
            queue: rt.clone(),
            facts: facts.clone(),
            record,
            started: std::time::Instant::now(),
            observer: std::sync::Arc::new(Observer {
                provider,
                state: std::sync::Mutex::default(),
            }),
            stream: StreamUsage::default(),
            body: None,
            model: ResponseModel::default(),
            first: None,
            published: false,
        }
    }

    /// The sink to hand the executor.
    pub fn sink(&self) -> cpa_core::exec::UsageSink {
        cpa_core::exec::UsageSink::new(self.observer.clone())
    }

    /// The upstream answered; its headers are the record's `response_headers`.
    // ponytail: TTFT is measured to the executor's response (buffered) or first event
    // (streams); Go measures to the first upstream body byte.
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
        self.publish();
    }

    /// Go `PublishFailure`: the error's status and body, and any usage seen so far.
    pub fn fail(mut self, error: &cpa_core::exec::ExecError) {
        self.record.failed = true;
        self.record.fail_status = i64::from(crate::classify::go_status(error));
        self.record.fail_body = crate::classify::error_text(error);
        if self.record.response_headers.is_empty() {
            self.record.response_headers = go_headers(&error.headers);
        }
        self.publish();
    }

    fn publish(&mut self) {
        if std::mem::replace(&mut self.published, true) {
            return;
        }
        let queue = self.queue.usage_queue();
        if !queue.accepts() {
            return;
        }
        let reported = std::mem::take(&mut *self.observer.lock());
        let (detail, model) = if reported.seen {
            let detail = reported.body.or_else(|| reported.stream.detail().cloned());
            (detail, reported.model.get().to_owned())
        } else {
            let detail = self.body.take().or_else(|| self.stream.detail().cloned());
            (detail, self.model.get().to_owned())
        };
        let mut record = std::mem::take(&mut self.record);
        record.detail = detail.unwrap_or_default();
        record.response_model = model;
        if let Some(effort) = reported.effort {
            record.reasoning_effort = effort;
        }
        record.latency_ms = self.started.elapsed().as_millis() as i64;
        record.ttft_ms = self.first.map_or(0, |d| d.as_millis() as i64);
        queue.enqueue(queued(&record, &self.facts.client));
    }
}

impl Drop for Tracker {
    fn drop(&mut self) {
        self.publish();
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

/// Go `AccessTokenSHA256`: hex SHA-256 of the metadata access token (top level or
/// under `token`), empty without one.
pub fn access_token_sha256(c: &cpa_core::credential::Credential) -> String {
    use sha2::{Digest, Sha256};
    let pick = |m: &serde_json::Map<String, serde_json::Value>| {
        ["access_token", "accessToken"]
            .iter()
            .find_map(|k| {
                m.get(*k)
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
            })
            .map(str::to_owned)
    };
    let token = pick(&c.metadata).or_else(|| {
        ["token", "Token"]
            .iter()
            .find_map(|k| c.metadata.get(*k).and_then(serde_json::Value::as_object).and_then(pick))
    });
    token.map_or_else(String::new, |t| {
        Sha256::digest(t.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    })
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

    /// Go's `PublishFailure(err)` after usage was observed (an apply_patch rejection):
    /// an empty detail wins over the reported body and lines, the response model stays.
    #[test]
    fn executor_failure_report_drops_tokens_keeps_model() {
        use cpa_core::exec::UsageObserver;
        let observer = Observer {
            provider: "claude".into(),
            state: std::sync::Mutex::default(),
        };
        observer.response_line(
            Format::Claude,
            br#"data: {"type":"message_start","message":{"model":"claude-upstream","usage":{"input_tokens":9,"cache_creation_input_tokens":4}}}"#,
        );
        observer.response_body(
            Format::Claude,
            br#"{"model":"claude-upstream","usage":{"input_tokens":9,"output_tokens":2}}"#,
        );
        assert_ne!(observer.lock().body, Some(Detail::default()), "the body parsed tokens");
        observer.failed();
        let reported = std::mem::take(&mut *observer.lock());
        assert!(reported.seen);
        assert_eq!(reported.body, Some(Detail::default()));
        assert_eq!(reported.model.get(), "claude-upstream");
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
}
