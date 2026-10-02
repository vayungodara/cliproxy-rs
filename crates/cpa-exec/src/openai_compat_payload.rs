//! Pure request and response rewrites of the OpenAI-compatible executor, each a port of
//! the Go helper named in its doc comment.

use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use cpa_core::config::Config;
use cpa_core::credential::{Credential, Source};
use http::HeaderMap;

use crate::openai_compat_http::json;

/// One `api-keys.openai-compatibility[]` entry, the fields the executor reads.
#[derive(Debug, Clone, Default)]
pub(crate) struct Compat {
    pub name: String,
    pub disabled: bool,
    pub support_prompt_cache_key: bool,
    pub models: Vec<CompatModel>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct CompatModel {
    pub name: String,
    pub alias: String,
    pub use_max_completion_tokens: bool,
    pub input_modalities: Vec<String>,
}

fn yaml_str(v: Option<&serde_yaml_ng::Value>) -> String {
    match v {
        Some(serde_yaml_ng::Value::String(s)) => s.clone(),
        Some(serde_yaml_ng::Value::Number(n)) => n.to_string(),
        Some(serde_yaml_ng::Value::Bool(b)) => b.to_string(),
        _ => String::new(),
    }
}

fn yaml_bool(v: Option<&serde_yaml_ng::Value>) -> bool {
    v.and_then(serde_yaml_ng::Value::as_bool).unwrap_or(false)
}

/// `cfg.OpenAICompatibility` after `SanitizeOpenAICompatibility`: entries without a
/// base-url are dropped, so indexes match the synthesized `config_index` attribute.
pub(crate) fn compat_entries(cfg: &Config) -> Vec<Compat> {
    let groups = cfg
        .document
        .get("api-keys")
        .and_then(|k| k.get("openai-compatibility"))
        .and_then(serde_yaml_ng::Value::as_sequence);
    let mut out = Vec::new();
    for group in groups.into_iter().flatten() {
        if yaml_str(group.get("base-url")).trim().is_empty() {
            continue;
        }
        let models = group
            .get("models")
            .and_then(serde_yaml_ng::Value::as_sequence)
            .into_iter()
            .flatten()
            .map(|m| CompatModel {
                name: yaml_str(m.get("name")),
                alias: yaml_str(m.get("alias")),
                use_max_completion_tokens: yaml_bool(m.get("use-max-completion-tokens")),
                input_modalities: m
                    .get("input-modalities")
                    .and_then(serde_yaml_ng::Value::as_sequence)
                    .map(|s| s.iter().map(|v| yaml_str(Some(v))).collect())
                    .unwrap_or_default(),
            })
            .collect();
        out.push(Compat {
            name: yaml_str(group.get("name")).trim().to_owned(),
            disabled: yaml_bool(group.get("disabled")),
            support_prompt_cache_key: yaml_bool(group.get("support-prompt-cache-key")),
            models,
        });
    }
    out
}

/// `resolveCompatConfig`: the config entry by `config_index` for config-sourced
/// credentials, else the first enabled entry whose name matches `compat_name`,
/// `provider_key` or the provider.
// ponytail: Home mode's credential_options path (M6) is not ported.
pub(crate) fn resolve_compat(credential: &Credential, cfg: &Config) -> Option<Compat> {
    let entries = compat_entries(cfg);
    let attr = |k: &str| credential.attributes.get(k).map(|v| v.trim()).unwrap_or_default();
    if matches!(credential.source, Source::Config { .. })
        && let Ok(index) = attr("config_index").parse::<usize>()
        && let Some(entry) = entries.get(index)
        && !entry.disabled
    {
        return Some(entry.clone());
    }
    let candidates = [attr("compat_name"), attr("provider_key"), credential.provider.trim()];
    entries.into_iter().find(|entry| {
        !entry.disabled
            && candidates
                .iter()
                .any(|c| !c.is_empty() && c.eq_ignore_ascii_case(&entry.name))
    })
}

/// `normalizeOpenAICompatibilityModelName`.
fn model_name(model: &str) -> &str {
    crate::openai_compat_http::parse_suffix(model.trim()).0.trim()
}

/// The configured model matching `model` by name, then by alias.
pub(crate) fn find_model<'a>(compat: &'a Compat, model: &str) -> Option<&'a CompatModel> {
    let model = model_name(model);
    if model.is_empty() {
        return None;
    }
    compat
        .models
        .iter()
        .find(|m| model.eq_ignore_ascii_case(model_name(&m.name)))
        .or_else(|| {
            compat
                .models
                .iter()
                .find(|m| model.eq_ignore_ascii_case(model_name(&m.alias)))
        })
}

/// `ShouldUseMaxCompletionTokensForModel`.
pub(crate) fn uses_max_completion_tokens(compat: Option<&Compat>, upstream: &str, requested: &str) -> bool {
    let Some(compat) = compat else { return false };
    find_model(compat, upstream)
        .or_else(|| find_model(compat, requested))
        .is_some_and(|m| m.use_max_completion_tokens)
}

/// `NormalizeOpenAIMaxTokens`.
pub(crate) fn normalize_max_tokens(payload: String, use_max_completion_tokens: bool) -> String {
    let max_tokens = gjson::get(&payload, "max_tokens");
    let max_completion = gjson::get(&payload, "max_completion_tokens");
    let (from, to) = if use_max_completion_tokens {
        ("max_tokens", "max_completion_tokens")
    } else {
        ("max_completion_tokens", "max_tokens")
    };
    let (has_from, has_to, raw) = if use_max_completion_tokens {
        (
            max_tokens.exists(),
            max_completion.exists(),
            max_tokens.json().to_owned(),
        )
    } else {
        (
            max_completion.exists(),
            max_tokens.exists(),
            max_completion.json().to_owned(),
        )
    };
    if !has_from && !has_to {
        return payload;
    }
    let mut payload = payload;
    if has_from && !has_to {
        payload = json::set_raw(&payload, to, &raw);
    }
    if has_from {
        payload = json::delete(&payload, from);
    }
    payload
}

const IMAGE_OMITTED: &str = "[image omitted: unsupported by upstream]";
const RELAY_NOTICE: &str = "Images returned by the preceding tool call(s):";
const RELAY_PLACEHOLDER: &str = "[Tool returned image content; the images follow in the next user message.]";

/// `ShouldNormalizeOpenAIToolResultsForModel`: the model declares input modalities that
/// include text but not image. Every alias match must exclude images.
pub(crate) fn excludes_images(compat: Option<&Compat>, upstream: &str, requested: &str) -> bool {
    fn text_only(modalities: &[String]) -> bool {
        if modalities.is_empty() {
            return false;
        }
        let mut text = false;
        for m in modalities {
            match m.trim().to_ascii_lowercase().as_str() {
                "image" => return false,
                "text" => text = true,
                _ => {}
            }
        }
        text
    }
    fn lookup(compat: &Compat, model: &str) -> Option<bool> {
        let model = model_name(model);
        if model.is_empty() {
            return None;
        }
        if let Some(m) = compat
            .models
            .iter()
            .find(|m| model.eq_ignore_ascii_case(model_name(&m.name)))
        {
            return Some(text_only(&m.input_modalities));
        }
        let aliases: Vec<&CompatModel> = compat
            .models
            .iter()
            .filter(|m| model.eq_ignore_ascii_case(model_name(&m.alias)))
            .collect();
        (!aliases.is_empty()).then(|| aliases.iter().all(|m| text_only(&m.input_modalities)))
    }
    let Some(compat) = compat else { return false };
    lookup(compat, upstream)
        .or_else(|| lookup(compat, requested))
        .unwrap_or(false)
}

fn is_image_part(item: &gjson::Value<'_>) -> bool {
    if item.kind() != gjson::Kind::Object {
        return false;
    }
    matches!(
        item.get("type").str().trim().to_ascii_lowercase().as_str(),
        "image" | "image_url" | "input_image"
    ) || item.get("image_url").exists()
        || item.get("input_image").exists()
}

fn part_text(item: &gjson::Value<'_>) -> Option<String> {
    if item.kind() == gjson::Kind::String {
        return Some(item.str().to_owned());
    }
    if item.kind() == gjson::Kind::Object {
        if is_image_part(item) {
            return Some(IMAGE_OMITTED.to_owned());
        }
        let text = item.get("text");
        if text.kind() == gjson::Kind::String {
            return Some(text.str().to_owned());
        }
    }
    (!item.json().is_empty()).then(|| item.json().to_owned())
}

fn flatten_tool_content(content: &gjson::Value<'_>) -> String {
    match content.kind() {
        gjson::Kind::String => content.str().to_owned(),
        gjson::Kind::Array => content
            .array()
            .iter()
            .filter_map(part_text)
            .collect::<Vec<_>>()
            .join("\n\n"),
        gjson::Kind::Object if is_image_part(content) => IMAGE_OMITTED.to_owned(),
        gjson::Kind::Object if content.get("text").kind() == gjson::Kind::String => {
            content.get("text").str().to_owned()
        }
        _ => content.json().to_owned(),
    }
}

/// `NormalizeOpenAIToolResultsTextOnly`.
pub(crate) fn normalize_tool_results_text_only(payload: String) -> String {
    let messages = gjson::get(&payload, "messages");
    if messages.kind() != gjson::Kind::Array {
        return payload;
    }
    let list = messages.array();
    if list.is_empty() {
        return payload;
    }
    let mut out: Vec<String> = Vec::with_capacity(list.len());
    let mut replaced = false;
    for msg in &list {
        let mut raw = msg.json().to_owned();
        match msg.get("role").str() {
            "tool" => {
                let content = msg.get("content");
                if content.exists() && content.kind() != gjson::Kind::String {
                    raw = json::set_str(&raw, "content", &flatten_tool_content(&content));
                } else if content.kind() == gjson::Kind::String && content.str() == RELAY_PLACEHOLDER {
                    raw = json::set_str(&raw, "content", IMAGE_OMITTED);
                    replaced = true;
                }
                out.push(raw);
            }
            "user" => {
                let content = msg.get("content");
                if content.kind() == gjson::Kind::Array {
                    let (mut notice, mut images) = (false, false);
                    let mut remaining = Vec::new();
                    for part in content.array() {
                        if part.kind() == gjson::Kind::Object {
                            if part.get("type").str() == "text" && part.get("text").str() == RELAY_NOTICE {
                                notice = true;
                                continue;
                            }
                            if is_image_part(&part) {
                                images = true;
                                continue;
                            }
                        }
                        remaining.push(part.json().to_owned());
                    }
                    if notice && images {
                        if !replaced
                            && let Some(last) = out.last_mut()
                            && gjson::get(last, "role").str() == "tool"
                        {
                            let previous = json::string(last, "content");
                            if !previous.contains(IMAGE_OMITTED) {
                                let next = if previous.is_empty() {
                                    IMAGE_OMITTED.to_owned()
                                } else {
                                    format!("{previous}\n\n{IMAGE_OMITTED}")
                                };
                                *last = json::set_str(last, "content", &next);
                            }
                        }
                        replaced = false;
                        if remaining.is_empty() {
                            continue;
                        }
                        raw = json::set_raw(&raw, "content", &format!("[{}]", remaining.join(",")));
                    }
                }
                out.push(raw);
            }
            _ => {
                replaced = false;
                out.push(raw);
            }
        }
    }
    json::set_raw(&payload, "messages", &format!("[{}]", out.join(",")))
}

/// `uuid.NewSHA1(uuid.NameSpaceOID, identity)`.
fn oid_uuid(identity: &str) -> String {
    uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, identity.as_bytes()).to_string()
}

/// `headerValueCaseInsensitive`: first non-empty trimmed value.
fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .map(str::trim)
        .find(|v| !v.is_empty())
        .map(str::to_owned)
}

/// `ClaudeCodePromptCache`: a stable key per Claude Code session, agent and model.
pub(crate) fn claude_code_prompt_cache(model: &str, payload: &str, headers: &HeaderMap) -> Option<String> {
    let model = model.trim();
    let session = header_value(headers, "x-claude-code-session-id").or_else(|| {
        let user_id = json::string(payload, "metadata.user_id");
        if let Some(pos) = user_id.rfind("_session_") {
            let id = &user_id[pos + "_session_".len()..];
            if !id.is_empty()
                && id
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase() || b == b'-')
            {
                return Some(id.to_owned());
            }
        }
        user_id
            .starts_with('{')
            .then(|| json::string(&user_id, "session_id").trim().to_owned())
            .filter(|s| !s.is_empty())
    })?;
    if model.is_empty() {
        return None;
    }
    let agent = header_value(headers, "x-claude-code-agent-id").unwrap_or_else(|| "main".into());
    Some(oid_uuid(&format!(
        "cli-proxy-api:codex:claude-code\0{model}\0claude:{session}:agent:{agent}"
    )))
}

/// `EnsureResponsesUsageDetails`.
pub(crate) fn ensure_responses_usage_details(payload: &[u8]) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(payload) else {
        return payload.to_vec();
    };
    let trimmed = text.trim_matches(|c: char| c.is_ascii_whitespace());
    if trimmed.is_empty() {
        return payload.to_vec();
    }
    let patch = |body: &str| -> Option<String> {
        if json::string(body, "object") == "response.compaction" {
            return None;
        }
        let updated = usage_details_at(&usage_details_at(body, "response.usage"), "usage");
        (updated != body).then_some(updated)
    };
    if trimmed.starts_with('{') {
        return patch(trimmed).map_or_else(|| payload.to_vec(), String::into_bytes);
    }
    if !text.contains("data:") {
        return payload.to_vec();
    }
    let mut modified = false;
    let lines: Vec<String> = text
        .split('\n')
        .map(|line| {
            if !line.trim().starts_with("data:") {
                return line.to_owned();
            }
            let prefix = if line.starts_with("data: ") {
                6
            } else if line.starts_with("data:") {
                5
            } else {
                return line.to_owned();
            };
            let data = line[prefix..].trim();
            if !data.starts_with('{') {
                return line.to_owned();
            }
            match patch(data) {
                Some(updated) => {
                    modified = true;
                    format!("{}{updated}", &line[..prefix])
                }
                None => line.to_owned(),
            }
        })
        .collect();
    if modified {
        lines.join("\n").into_bytes()
    } else {
        payload.to_vec()
    }
}

fn usage_details_at(body: &str, path: &str) -> String {
    let usage = gjson::get(body, path);
    if usage.kind() != gjson::Kind::Object {
        return body.to_owned();
    }
    let mut body = body.to_owned();
    for (field, leaf, empty) in [
        ("output_tokens_details", "reasoning_tokens", r#"{"reasoning_tokens":0}"#),
        ("input_tokens_details", "cached_tokens", r#"{"cached_tokens":0}"#),
    ] {
        let details_path = format!("{path}.{field}");
        let leaf_path = format!("{details_path}.{leaf}");
        let (missing, wrong_type, leaf_missing) = {
            let details = gjson::get(&body, &details_path);
            let value = details.get(leaf);
            (
                !details.exists(),
                details.kind() != gjson::Kind::Object,
                !value.exists() || value.kind() == gjson::Kind::Null,
            )
        };
        if missing {
            body = json::set_raw(&body, &leaf_path, "0");
        } else if wrong_type {
            body = json::set_raw(&body, &details_path, empty);
        } else if leaf_missing {
            body = json::set_raw(&body, &leaf_path, "0");
        }
    }
    body
}

/// `sanitizeOpenAIResponsesReasoningEncryptedContent` (isCompat false): reasoning
/// `content` is promoted into an empty summary and cleared, ids without usable
/// `encrypted_content` are dropped unless `store` is true, invalid signatures are removed.
pub(crate) fn sanitize_reasoning_encrypted_content(body: String) -> String {
    let input = gjson::get(&body, "input");
    if input.kind() != gjson::Kind::Array {
        return body;
    }
    let strip_ids = !json::boolean(&gjson::get(&body, "store"));
    let items = input.array();
    let mut rebuilt: Option<Vec<String>> = None;
    for (index, item) in items.iter().enumerate() {
        let keep = |rebuilt: &mut Option<Vec<String>>, raw: &str| {
            if let Some(list) = rebuilt {
                list.push(raw.to_owned());
            }
        };
        let edit = |rebuilt: &mut Option<Vec<String>>, raw: String| {
            let list = rebuilt.get_or_insert_with(|| items[..index].iter().map(|i| i.json().to_owned()).collect());
            list.push(raw);
        };
        if item.get("type").str().trim() != "reasoning" {
            keep(&mut rebuilt, item.json());
            continue;
        }
        let mut next = item.json().to_owned();
        let mut changed = false;
        let content = item.get("content");
        if content.kind() == gjson::Kind::Array && !content.array().is_empty() {
            let summary = item.get("summary");
            let summary_empty = !summary.exists()
                || summary.kind() == gjson::Kind::Null
                || (summary.kind() == gjson::Kind::Array && summary.array().is_empty());
            if summary_empty {
                let parts: Vec<String> = content
                    .array()
                    .iter()
                    .filter(|p| p.get("type").str().trim() == "reasoning_text" && !p.get("text").str().is_empty())
                    .map(|p| json::set_str(r#"{"type":"summary_text"}"#, "text", p.get("text").str()))
                    .collect();
                if !parts.is_empty() {
                    next = json::set_raw(&next, "summary", &format!("[{}]", parts.join(",")));
                }
            }
            next = json::set_raw(&next, "content", "[]");
            changed = true;
        }
        let encrypted = item.get("encrypted_content");
        if !encrypted.exists() {
            if strip_ids && item.get("id").exists() {
                next = json::delete(&next, "id");
                changed = true;
            }
            if changed {
                edit(&mut rebuilt, next);
            } else {
                keep(&mut rebuilt, item.json());
            }
            continue;
        }
        let invalid = match encrypted.kind() {
            gjson::Kind::String => {
                let raw = encrypted.str();
                raw != raw.trim() || crate::openai_compat_http::inspect_gpt_reasoning_signature(raw).is_err()
            }
            _ => true,
        };
        if !invalid {
            if changed {
                edit(&mut rebuilt, next);
            } else {
                keep(&mut rebuilt, item.json());
            }
            continue;
        }
        next = json::delete(&next, "encrypted_content");
        if strip_ids && item.get("id").exists() {
            next = json::delete(&next, "id");
        }
        edit(&mut rebuilt, next);
    }
    match rebuilt {
        Some(list) => json::set_raw(&body, "input", &format!("[{}]", list.join(","))),
        None => body,
    }
}

/// Go `TokenizerForModel` + `CountOpenAIChatTokens`.
pub(crate) fn count_chat_tokens(model: &str, payload: &str) -> Result<i64, String> {
    static O200K: OnceLock<Result<tiktoken_rs::CoreBPE, String>> = OnceLock::new();
    static CL100K: OnceLock<Result<tiktoken_rs::CoreBPE, String>> = OnceLock::new();
    let m = model.trim().to_ascii_lowercase();
    let cl100k = !m.is_empty()
        && !m.starts_with("gpt-5")
        && !m.starts_with("gpt-4.1")
        && !m.starts_with("gpt-4o")
        && (m.starts_with("gpt-4") || m.starts_with("gpt-3"))
        || m.is_empty();
    let encoder = if cl100k {
        CL100K.get_or_init(|| tiktoken_rs::cl100k_base().map_err(|e| e.to_string()))
    } else {
        O200K.get_or_init(|| tiktoken_rs::o200k_base().map_err(|e| e.to_string()))
    };
    let encoder = encoder.as_ref().map_err(Clone::clone)?;
    if payload.is_empty() {
        return Ok(0);
    }
    let mut segments = Vec::new();
    let root = gjson::parse(payload);
    for message in root.get("messages").array() {
        add(&mut segments, message.get("role").str());
        add(&mut segments, message.get("name").str());
        collect_content(&message.get("content"), &mut segments);
        for call in message.get("tool_calls").array() {
            add(&mut segments, call.get("id").str());
            add(&mut segments, call.get("type").str());
            let function = call.get("function");
            if function.exists() {
                function_fields(&function, &mut segments, true);
            }
        }
        let call = message.get("function_call");
        if call.exists() {
            add(&mut segments, call.get("name").str());
            add(&mut segments, call.get("arguments").str());
        }
    }
    let tools = root.get("tools");
    if tools.kind() == gjson::Kind::Array {
        for tool in tools.array() {
            tool_payload(&tool, &mut segments);
        }
    } else if tools.exists() {
        tool_payload(&tools, &mut segments);
    }
    for function in root.get("functions").array() {
        function_fields(&function, &mut segments, false);
    }
    let choice = root.get("tool_choice");
    if choice.kind() == gjson::Kind::String {
        add(&mut segments, choice.str());
    } else if choice.exists() {
        add(&mut segments, choice.json());
    }
    let format = root.get("response_format");
    if format.exists() {
        add(&mut segments, format.get("type").str());
        add(&mut segments, format.get("name").str());
        for key in ["json_schema", "schema"] {
            let schema = format.get(key);
            if schema.exists() {
                add(&mut segments, schema.json());
            }
        }
    }
    add(&mut segments, &json::string(payload, "input"));
    add(&mut segments, &json::string(payload, "prompt"));
    let joined = segments.join("\n");
    let joined = joined.trim();
    if joined.is_empty() {
        return Ok(0);
    }
    Ok(encoder.encode_ordinary(joined).len() as i64)
}

fn add(segments: &mut Vec<String>, value: &str) {
    let value = value.trim();
    if !value.is_empty() {
        segments.push(value.to_owned());
    }
}

fn function_fields(function: &gjson::Value<'_>, segments: &mut Vec<String>, arguments: bool) {
    add(segments, function.get("name").str());
    add(segments, function.get("description").str());
    if arguments {
        add(segments, function.get("arguments").str());
    }
    let params = function.get("parameters");
    if params.exists() {
        add(segments, params.json());
    }
}

fn tool_payload(tool: &gjson::Value<'_>, segments: &mut Vec<String>) {
    add(segments, tool.get("type").str());
    add(segments, tool.get("name").str());
    add(segments, tool.get("description").str());
    let function = tool.get("function");
    if function.exists() {
        function_fields(&function, segments, false);
    }
}

fn collect_content(content: &gjson::Value<'_>, segments: &mut Vec<String>) {
    match content.kind() {
        gjson::Kind::String => add(segments, content.str()),
        gjson::Kind::Array => {
            for part in content.array() {
                match part.get("type").str() {
                    "text" | "input_text" | "output_text" => add(segments, part.get("text").str()),
                    "image_url" => add(segments, part.get("image_url.url").str()),
                    "input_audio" | "output_audio" | "audio" => add(segments, part.get("id").str()),
                    "tool_result" => {
                        add(segments, part.get("name").str());
                        collect_content(&part.get("content"), segments);
                    }
                    _ if part.kind() == gjson::Kind::Array => collect_content(&part, segments),
                    _ if part.kind() == gjson::Kind::Object => add(segments, part.json()),
                    _ => add(segments, &kimi_string(&part)),
                }
            }
        }
        gjson::Kind::Object => add(segments, content.json()),
        _ => {}
    }
}

fn kimi_string(value: &gjson::Value<'_>) -> String {
    crate::kimi_json::gstr(value)
}

/// `openAICompatRetryAfter`: only 429s carry a hint; integer or HTTP-date `Retry-After`,
/// else one minute for explicit tokens-per-minute limits.
pub(crate) fn retry_after(status: u16, headers: &HeaderMap, body: &[u8], now: SystemTime) -> Option<Duration> {
    if status != 429 {
        return None;
    }
    let raw = headers
        .get(http::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .trim();
    if !raw.is_empty() {
        if let Ok(seconds) = raw.parse::<i64>()
            && seconds >= 0
        {
            return Some(Duration::from_secs(seconds as u64));
        }
        if let Ok(deadline) = httpdate::parse_http_date(raw) {
            return Some(deadline.duration_since(now).unwrap_or_default());
        }
    }
    let body = String::from_utf8_lossy(body);
    let code = json::string(&body, "error.code").trim().to_lowercase();
    let message = json::string(&body, "error.message").trim().to_lowercase();
    (code.contains("tpmratelimitexceeded")
        || (message.contains("tokens per minute") && message.contains("limit") && message.contains("exceeded")))
    .then_some(Duration::from_secs(60))
}

/// `openAICompatErrorEvent`.
pub(crate) fn error_event(name: &str) -> bool {
    ["error", "response.error", "response.failed"]
        .iter()
        .any(|e| name.eq_ignore_ascii_case(e))
}

/// `openAICompatStreamDataError`: the status to report when a data frame is an error.
pub(crate) fn stream_data_error(payload: &str, event: &str) -> Option<u16> {
    if payload.is_empty() || !json::valid(payload) {
        return None;
    }
    let kind = json::string(payload, "type");
    let has_error = ["error", "response.error"].iter().any(|p| {
        let node = gjson::get(payload, p);
        node.exists() && node.json() != "null"
    });
    let top_level = gjson::get(payload, "code").exists() && gjson::get(payload, "message").exists();
    if !has_error && !error_event(&kind) && !error_event(event) && !top_level {
        return None;
    }
    let mut status = 0;
    for path in [
        "status",
        "status_code",
        "error.status",
        "error.status_code",
        "response.error.status",
        "response.error.status_code",
    ] {
        status = gjson::get(payload, path).i64();
        if (400..=599).contains(&status) {
            break;
        }
    }
    Some(if (400..=599).contains(&status) {
        status as u16
    } else {
        502
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_rules_follow_go() {
        let now = httpdate::parse_http_date("Fri, 02 Oct 2026 12:00:00 GMT").unwrap();
        let mut h = HeaderMap::new();
        assert_eq!(retry_after(503, &h, b"", now), None);
        h.insert("retry-after", "Fri, 02 Oct 2026 12:00:30 GMT".parse().unwrap());
        assert_eq!(retry_after(429, &h, b"", now), Some(Duration::from_secs(30)));
        h.insert("retry-after", "-1".parse().unwrap());
        assert_eq!(retry_after(429, &h, br#"{"error":{"code":"x"}}"#, now), None);
        assert_eq!(
            retry_after(
                429,
                &HeaderMap::new(),
                br#"{"error":{"message":"tokens per minute limit exceeded"}}"#,
                now
            ),
            Some(Duration::from_secs(60))
        );
    }

    #[test]
    fn max_tokens_rewrites_raw_values() {
        assert_eq!(
            normalize_max_tokens(r#"{"max_tokens":1.50,"x":1}"#.into(), true),
            r#"{"x":1,"max_completion_tokens":1.50}"#
        );
        assert_eq!(normalize_max_tokens(r#"{"x":1}"#.into(), true), r#"{"x":1}"#);
        assert_eq!(
            normalize_max_tokens(r#"{"max_tokens":2,"max_completion_tokens":3}"#.into(), false),
            r#"{"max_tokens":2}"#
        );
    }

    #[test]
    fn claude_code_session_from_payload_user_id() {
        let headers = HeaderMap::new();
        let payload = r#"{"metadata":{"user_id":"user_abc_account__session_0f-9a"}}"#;
        let a = claude_code_prompt_cache("m", payload, &headers).unwrap();
        let mut h = HeaderMap::new();
        h.insert("x-claude-code-session-id", "0f-9a".parse().unwrap());
        assert_eq!(claude_code_prompt_cache("m", "{}", &h).unwrap(), a);
        assert!(claude_code_prompt_cache("m", r#"{"metadata":{"user_id":"x_session_ABC"}}"#, &headers).is_none());
        assert_eq!(
            claude_code_prompt_cache(
                "m",
                r#"{"metadata":{"user_id":"{\"session_id\":\"0f-9a\"}"}}"#,
                &headers
            )
            .unwrap(),
            a
        );
    }
}
