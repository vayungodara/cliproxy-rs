//! Pure request and response rewrites of the OpenAI-compatible executor, each a port of
//! the Go helper named in its doc comment.

use std::time::{Duration, SystemTime};

use cpa_common::gostr::GoStr;
use cpa_common::json::{self as gj, Kind, Res};
use cpa_common::thinking::parse_suffix;
use cpa_core::config::Config;
use cpa_core::credential::{Credential, Source};
use http::HeaderMap;

use crate::openai_compat_go as go;

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

/// One `credential_options.models` entry Home sent (JSON, Go's field names).
fn home_compat_model(model: &serde_json::Value) -> Option<CompatModel> {
    if !cpa_home::dispatch::compat_model_shape(model) {
        return None;
    }
    let text = |key: &str| {
        model
            .get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    Some(CompatModel {
        name: text("name"),
        alias: text("alias"),
        use_max_completion_tokens: model.get("use-max-completion-tokens") == Some(&serde_json::Value::Bool(true)),
        input_modalities: model
            .get("input-modalities")
            .and_then(serde_json::Value::as_array)
            .map(|list| list.iter().map(|m| m.as_str().unwrap_or_default().to_owned()).collect())
            .unwrap_or_default(),
    })
}

/// `resolveCompatConfig` in Home mode: Home sends no provider entries, so the selected
/// credential's non-secret options make the entry. Its `credential_options` count when
/// they decode; after a type error Go keeps the well-typed fields but counts them only
/// when another source is present (here a mistyped model entry is dropped whole). The
/// `support_prompt_cache_key` attribute overrides the flag, and the options entry Home
/// mode selected for this attempt's model replaces the list. `None` outside Home mode
/// or without any of the three.
fn home_compat(credential: &Credential) -> Option<Compat> {
    use cpa_core::config::credentials::{HOME_MODEL_OPTIONS, HOME_PROVIDER};
    use serde_json::Value;
    if !credential.attributes.contains_key(HOME_PROVIDER) {
        return None;
    }
    let mut compat = Compat {
        name: credential.attributes.get("compat_name").cloned().unwrap_or_default(),
        ..Compat::default()
    };
    let mut present = false;
    if let Some(options) = credential.metadata.get("credential_options") {
        let mut decoded = options.is_object() || options.is_null();
        if let Some(options) = options.as_object() {
            match options.get("support-prompt-cache-key") {
                None | Some(Value::Null) => {}
                Some(Value::Bool(flag)) => compat.support_prompt_cache_key = *flag,
                Some(_) => decoded = false,
            }
            match options.get("models") {
                None | Some(Value::Null) => {}
                Some(Value::Array(models)) => {
                    for model in models {
                        match home_compat_model(model) {
                            Some(model) => compat.models.push(model),
                            None => decoded = false,
                        }
                    }
                }
                Some(_) => decoded = false,
            }
        }
        present = decoded;
    }
    if let Some(flag) = credential
        .attributes
        .get("support_prompt_cache_key")
        .and_then(|raw| crate::xai::parse_bool(raw))
    {
        compat.support_prompt_cache_key = flag;
        present = true;
    }
    if let Some(selected) = credential
        .attributes
        .get(HOME_MODEL_OPTIONS)
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .and_then(|model| home_compat_model(&model))
    {
        compat.models = vec![selected];
        present = true;
    }
    present.then_some(compat)
}

/// `resolveCompatConfig`: in Home mode the selected credential's options
/// ([`home_compat`]); else the config entry by `config_index` for config-sourced
/// credentials, else the first enabled entry whose name matches `compat_name`,
/// `provider_key` or the provider.
pub(crate) fn resolve_compat(credential: &Credential, cfg: &Config) -> Option<Compat> {
    if let Some(compat) = home_compat(credential) {
        return Some(compat);
    }
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
fn model_name(model: &str) -> String {
    parse_suffix(model.trim()).model_name.trim().to_owned()
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
        .find(|m| model.go_eq_fold(&model_name(&m.name)))
        .or_else(|| compat.models.iter().find(|m| model.go_eq_fold(&model_name(&m.alias))))
}

fn attribute<'a>(credential: &'a Credential, key: &str) -> &'a str {
    credential.attributes.get(key).map_or("", |v| v.trim())
}

/// `normalizeAuthKind`.
fn normalize_auth_kind(kind: &str) -> &'static str {
    match kind.trim().go_lower().as_str() {
        "apikey" | "api_key" | "api-key" => "apikey",
        "oauth" | "oauth2" => "oauth",
        _ => "",
    }
}

/// `Auth.AuthKind`: explicit kind, then the field-shape fallbacks.
pub(crate) fn auth_kind(credential: &Credential) -> &'static str {
    let explicit = normalize_auth_kind(attribute(credential, "auth_kind"));
    if !explicit.is_empty() {
        return explicit;
    }
    let explicit = normalize_auth_kind(credential.str("auth_kind").unwrap_or_default());
    if !explicit.is_empty() {
        return explicit;
    }
    if !attribute(credential, "api_key").is_empty() {
        return "apikey";
    }
    let oauth_keys = [
        "access_token",
        "refresh_token",
        "id_token",
        "email",
        "token_type",
        "expires_at",
        "expired",
    ];
    let has_oauth = oauth_keys
        .iter()
        .any(|k| credential.str(k).is_some_and(|v| !v.trim().is_empty()))
        || credential
            .metadata
            .get("token")
            .and_then(serde_json::Value::as_object)
            .is_some_and(|t| !t.is_empty());
    if has_oauth { "oauth" } else { "" }
}

/// `ShouldUseMaxCompletionTokensForModel`.
pub(crate) fn uses_max_completion_tokens(compat: Option<&Compat>, upstream: &str, requested: &str) -> bool {
    let Some(compat) = compat else { return false };
    find_model(compat, upstream)
        .or_else(|| find_model(compat, requested))
        .is_some_and(|m| m.use_max_completion_tokens)
}

/// `NormalizeOpenAIMaxTokens`.
pub(crate) fn normalize_max_tokens(mut payload: Vec<u8>, use_max_completion_tokens: bool) -> Vec<u8> {
    let (from, to) = if use_max_completion_tokens {
        ("max_tokens", "max_completion_tokens")
    } else {
        ("max_completion_tokens", "max_tokens")
    };
    let source = gj::get(&payload, from);
    let (has_from, raw) = (source.exists(), source.raw().to_vec());
    let has_to = gj::get(&payload, to).exists();
    if has_from && !has_to {
        gj::set_raw(&mut payload, to, &raw);
    }
    if has_from {
        gj::delete(&mut payload, from);
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
            match m.trim().go_lower().as_str() {
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
        if let Some(m) = compat.models.iter().find(|m| model.go_eq_fold(&model_name(&m.name))) {
            return Some(text_only(&m.input_modalities));
        }
        let aliases: Vec<&CompatModel> = compat
            .models
            .iter()
            .filter(|m| model.go_eq_fold(&model_name(&m.alias)))
            .collect();
        (!aliases.is_empty()).then(|| aliases.iter().all(|m| text_only(&m.input_modalities)))
    }
    let Some(compat) = compat else { return false };
    lookup(compat, upstream)
        .or_else(|| lookup(compat, requested))
        .unwrap_or(false)
}

fn is_image_part(item: &Res<'_>) -> bool {
    if !item.is_object() {
        return false;
    }
    matches!(
        item.get("type").str().trim().go_lower().as_str(),
        "image" | "image_url" | "input_image"
    ) || item.get("image_url").exists()
        || item.get("input_image").exists()
}

/// `openAIToolResultPartText`.
fn part_text(item: &Res<'_>) -> Option<Vec<u8>> {
    if item.kind == Kind::String {
        return Some(item.bytes().into_owned());
    }
    if item.is_object() {
        if is_image_part(item) {
            return Some(IMAGE_OMITTED.into());
        }
        let text = item.get("text");
        if text.kind == Kind::String {
            return Some(text.bytes().into_owned());
        }
    }
    (!item.raw().is_empty()).then(|| item.raw().to_vec())
}

/// `flattenOpenAIToolResultContent`.
fn flatten_tool_content(content: &Res<'_>) -> Vec<u8> {
    if content.kind == Kind::String {
        return content.bytes().into_owned();
    }
    if content.is_array() {
        let parts: Vec<Vec<u8>> = content.array().iter().filter_map(part_text).collect();
        return parts.join(&b"\n\n"[..]);
    }
    if content.is_object() {
        if is_image_part(content) {
            return IMAGE_OMITTED.into();
        }
        let text = content.get("text");
        if text.kind == Kind::String {
            return text.bytes().into_owned();
        }
    }
    content.raw().to_vec()
}

/// `NormalizeOpenAIToolResultsTextOnly`.
pub(crate) fn normalize_tool_results_text_only(mut payload: Vec<u8>) -> Vec<u8> {
    let messages = gj::get(&payload, "messages");
    if !messages.is_array() {
        return payload;
    }
    let list = messages.array();
    if list.is_empty() {
        return payload;
    }
    let mut out: Vec<Vec<u8>> = Vec::with_capacity(list.len());
    let mut replaced = false;
    for msg in &list {
        let mut raw = msg.raw().to_vec();
        match &*msg.get("role").bytes() {
            b"tool" => {
                let content = msg.get("content");
                if content.exists() && content.kind != Kind::String {
                    gj::set_str(&mut raw, "content", flatten_tool_content(&content));
                } else if content.kind == Kind::String
                    && *content.bytes() == *RELAY_PLACEHOLDER.as_bytes()
                    && gj::set_str(&mut raw, "content", IMAGE_OMITTED)
                {
                    replaced = true;
                }
                out.push(raw);
            }
            b"user" => {
                let content = msg.get("content");
                if content.is_array() {
                    let (mut notice, mut images) = (false, false);
                    let mut remaining: Vec<Vec<u8>> = Vec::new();
                    for part in content.array() {
                        if part.is_object() {
                            if *part.get("type").bytes() == *b"text"
                                && *part.get("text").bytes() == *RELAY_NOTICE.as_bytes()
                            {
                                notice = true;
                                continue;
                            }
                            if is_image_part(&part) {
                                images = true;
                                continue;
                            }
                        }
                        remaining.push(part.raw().to_vec());
                    }
                    if notice && images {
                        // Go walks back from the last message but stops at the first one.
                        if !replaced
                            && let Some(last) = out.last_mut()
                            && *gj::get(last, "role").bytes() == *b"tool"
                        {
                            let previous = gj::get(last, "content").bytes().into_owned();
                            if !contains(&previous, IMAGE_OMITTED.as_bytes()) {
                                let next = if previous.is_empty() {
                                    IMAGE_OMITTED.as_bytes().to_vec()
                                } else {
                                    [&previous[..], b"\n\n", IMAGE_OMITTED.as_bytes()].concat()
                                };
                                gj::set_str(last, "content", next);
                            }
                        }
                        replaced = false;
                        if remaining.is_empty() {
                            continue;
                        }
                        gj::set_raw(&mut raw, "content", gj::join(&remaining));
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
    gj::set_raw(&mut payload, "messages", gj::join(&out));
    payload
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty() || haystack.windows(needle.len()).any(|w| w == needle)
}

/// `helps.SetStringIfDifferent`.
pub(crate) fn set_str_if_different(body: &mut Vec<u8>, path: &str, value: &str) {
    let current = gj::get(body, path);
    if current.kind == Kind::String && *current.bytes() == *value.as_bytes() {
        return;
    }
    gj::set_str(body, path, value);
}

/// `helps.SetBoolIfDifferent`.
pub(crate) fn set_bool_if_different(body: &mut Vec<u8>, path: &str, value: bool) {
    let current = gj::get(body, path).kind;
    if current == if value { Kind::True } else { Kind::False } {
        return;
    }
    gj::set_bool(body, path, value);
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
pub(crate) fn claude_code_prompt_cache(model: &str, payload: &[u8], headers: &HeaderMap) -> Option<String> {
    let model = model.trim();
    let session = header_value(headers, "x-claude-code-session-id").or_else(|| {
        let user_id = gj::get(payload, "metadata.user_id").str().into_owned();
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
            .then(|| gj::get(user_id.as_bytes(), "session_id").str().trim().to_owned())
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
pub fn ensure_responses_usage_details(payload: &[u8]) -> Vec<u8> {
    let trimmed = go::trim_space(payload);
    if trimmed.is_empty() {
        return payload.to_vec();
    }
    let patch = |body: &[u8]| -> Option<Vec<u8>> {
        if *gj::get(body, "object").bytes() == *b"response.compaction" {
            return None;
        }
        let updated = usage_details_at(usage_details_at(body.to_vec(), "response.usage"), "usage");
        (updated != body).then_some(updated)
    };
    if trimmed[0] == b'{' {
        return patch(trimmed).unwrap_or_else(|| payload.to_vec());
    }
    if !contains(payload, b"data:") {
        return payload.to_vec();
    }
    let mut modified = false;
    let lines: Vec<Vec<u8>> = payload
        .split(|&b| b == b'\n')
        .map(|line| {
            if !go::trim_space(line).starts_with(b"data:") {
                return line.to_vec();
            }
            // Go keeps the 5-byte prefix for an indented `data:` line, which then never
            // yields a `{` payload.
            let prefix = if line.starts_with(b"data: ") { 6 } else { 5 };
            let data = go::trim_space(&line[prefix..]);
            if data.first() != Some(&b'{') {
                return line.to_vec();
            }
            match patch(data) {
                Some(updated) => {
                    modified = true;
                    [&line[..prefix], &updated[..]].concat()
                }
                None => line.to_vec(),
            }
        })
        .collect();
    if modified { lines.join(&b'\n') } else { payload.to_vec() }
}

/// `ensureUsageDetailsAt`.
fn usage_details_at(mut body: Vec<u8>, path: &str) -> Vec<u8> {
    if !gj::get(&body, path).is_object() {
        return body;
    }
    for (field, leaf, empty) in [
        ("output_tokens_details", "reasoning_tokens", r#"{"reasoning_tokens":0}"#),
        ("input_tokens_details", "cached_tokens", r#"{"cached_tokens":0}"#),
    ] {
        let details_path = format!("{path}.{field}");
        let leaf_path = format!("{details_path}.{leaf}");
        let (missing, wrong_type, leaf_missing) = {
            let details = gj::get(&body, &details_path);
            let value = details.get(leaf);
            (
                !details.exists(),
                !details.is_object(),
                !value.exists() || value.kind == Kind::Null,
            )
        };
        // Go assigns sjson's result even on error (a nil body); these paths never fail
        // on a document whose usage node is an object.
        if missing {
            gj::set_int(&mut body, &leaf_path, 0);
        } else if wrong_type {
            gj::set_raw(&mut body, &details_path, empty);
        } else if leaf_missing {
            gj::set_int(&mut body, &leaf_path, 0);
        }
    }
    body
}

/// `sanitizeOpenAIResponsesReasoningEncryptedContent` (isCompat false): reasoning
/// `content` is promoted into an empty summary and cleared, ids without usable
/// `encrypted_content` are dropped unless `store` is true, invalid signatures are removed.
pub(crate) fn sanitize_reasoning_encrypted_content(body: Vec<u8>) -> Vec<u8> {
    let input = gj::get(&body, "input");
    if !input.is_array() {
        return body;
    }
    let strip_ids = !gj::get(&body, "store").bool();
    let items = input.array();
    let mut rebuilt: Option<Vec<Vec<u8>>> = None;
    for (index, item) in items.iter().enumerate() {
        let keep = |rebuilt: &mut Option<Vec<Vec<u8>>>, raw: &[u8]| {
            if let Some(list) = rebuilt {
                list.push(raw.to_vec());
            }
        };
        let edit = |rebuilt: &mut Option<Vec<Vec<u8>>>, raw: Vec<u8>| {
            let list = rebuilt.get_or_insert_with(|| items[..index].iter().map(|i| i.raw().to_vec()).collect());
            list.push(raw);
        };
        if item.get("type").str().trim() != "reasoning" {
            keep(&mut rebuilt, item.raw());
            continue;
        }
        let mut next = item.raw().to_vec();
        let mut changed = false;
        let content = item.get("content");
        if content.is_array() && !content.array().is_empty() {
            let summary = item.get("summary");
            let summary_empty =
                !summary.exists() || summary.kind == Kind::Null || (summary.is_array() && summary.array().is_empty());
            if summary_empty {
                let parts: Vec<Vec<u8>> = content
                    .array()
                    .iter()
                    .filter(|p| p.get("type").str().trim() == "reasoning_text")
                    .map(|p| p.get("text").bytes().into_owned())
                    .filter(|text| !text.is_empty())
                    .map(|text| {
                        let mut part = br#"{"type":"summary_text"}"#.to_vec();
                        gj::set_str(&mut part, "text", text);
                        part
                    })
                    .collect();
                if !parts.is_empty() {
                    gj::set_raw(&mut next, "summary", gj::join(&parts));
                }
            }
            if gj::set_raw(&mut next, "content", "[]") {
                changed = true;
            }
        }
        let encrypted = item.get("encrypted_content");
        if !encrypted.exists() {
            if strip_ids && item.get("id").exists() && gj::delete(&mut next, "id") {
                changed = true;
            }
            if changed {
                edit(&mut rebuilt, next);
            } else {
                keep(&mut rebuilt, item.raw());
            }
            continue;
        }
        let invalid = match encrypted.kind {
            Kind::String => {
                let raw = encrypted.str();
                raw != raw.trim() || cpa_common::signature::inspect_gpt_reasoning_signature(raw.as_bytes()).is_err()
            }
            _ => true,
        };
        if !invalid || !gj::delete(&mut next, "encrypted_content") {
            if changed {
                edit(&mut rebuilt, next);
            } else {
                keep(&mut rebuilt, item.raw());
            }
            continue;
        }
        if strip_ids && item.get("id").exists() {
            gj::delete(&mut next, "id");
        }
        edit(&mut rebuilt, next);
    }
    match rebuilt {
        Some(list) => {
            let mut body = body;
            gj::set_raw(&mut body, "input", gj::join(&list));
            body
        }
        None => body,
    }
}

/// Go `TokenizerForModel` + `CountOpenAIChatTokens`.
pub(crate) fn count_chat_tokens(model: &str, payload: &[u8]) -> Result<i64, String> {
    use crate::tokenizer::Encoding;
    let m = model.trim().to_ascii_lowercase();
    let cl100k = !m.is_empty()
        && !m.starts_with("gpt-5")
        && !m.starts_with("gpt-4.1")
        && !m.starts_with("gpt-4o")
        && (m.starts_with("gpt-4") || m.starts_with("gpt-3"))
        || m.is_empty();
    let encoder = crate::tokenizer::encoder(if cl100k {
        Encoding::Cl100kBase
    } else {
        Encoding::O200kBase
    })?;
    if payload.is_empty() {
        return Ok(0);
    }
    let mut segments = Vec::new();
    let root = gj::parse(payload);
    let messages = root.get("messages");
    for message in messages.is_array().then(|| messages.array()).into_iter().flatten() {
        add(&mut segments, &s(&message.get("role")));
        add(&mut segments, &s(&message.get("name")));
        collect_content(&message.get("content"), &mut segments);
        let calls = message.get("tool_calls");
        for call in calls.is_array().then(|| calls.array()).into_iter().flatten() {
            add(&mut segments, &s(&call.get("id")));
            add(&mut segments, &s(&call.get("type")));
            let function = call.get("function");
            if function.exists() {
                function_fields(&function, &mut segments, true);
            }
        }
        let call = message.get("function_call");
        if call.exists() {
            add(&mut segments, &s(&call.get("name")));
            add(&mut segments, &s(&call.get("arguments")));
        }
    }
    let tools = root.get("tools");
    if tools.is_array() {
        for tool in tools.array() {
            tool_payload(&tool, &mut segments);
        }
    } else if tools.exists() {
        tool_payload(&tools, &mut segments);
    }
    let functions = root.get("functions");
    for function in functions.is_array().then(|| functions.array()).into_iter().flatten() {
        function_fields(&function, &mut segments, false);
    }
    let choice = root.get("tool_choice");
    if choice.kind == Kind::String {
        add(&mut segments, &s(&choice));
    } else if choice.exists() {
        add(&mut segments, &raw(&choice));
    }
    let format = root.get("response_format");
    if format.exists() {
        add(&mut segments, &s(&format.get("type")));
        add(&mut segments, &s(&format.get("name")));
        for key in ["json_schema", "schema"] {
            let schema = format.get(key);
            if schema.exists() {
                add(&mut segments, &raw(&schema));
            }
        }
    }
    add(&mut segments, &s(&root.get("input")));
    add(&mut segments, &s(&root.get("prompt")));
    let joined = segments.join("\n");
    let joined = joined.trim();
    if joined.is_empty() {
        return Ok(0);
    }
    Ok(encoder.encode_ordinary(joined).len() as i64)
}

/// gjson `Result.String()`, decoded for the tokenizer.
fn s(value: &Res<'_>) -> String {
    value.str().into_owned()
}

/// gjson `Result.Raw`.
fn raw(value: &Res<'_>) -> String {
    String::from_utf8_lossy(value.raw()).into_owned()
}

fn add(segments: &mut Vec<String>, value: &str) {
    let value = value.trim();
    if !value.is_empty() {
        segments.push(value.to_owned());
    }
}

fn function_fields(function: &Res<'_>, segments: &mut Vec<String>, arguments: bool) {
    add(segments, &s(&function.get("name")));
    add(segments, &s(&function.get("description")));
    if arguments {
        add(segments, &s(&function.get("arguments")));
    }
    let params = function.get("parameters");
    if params.exists() {
        add(segments, &raw(&params));
    }
}

fn tool_payload(tool: &Res<'_>, segments: &mut Vec<String>) {
    add(segments, &s(&tool.get("type")));
    add(segments, &s(&tool.get("name")));
    add(segments, &s(&tool.get("description")));
    let function = tool.get("function");
    if function.exists() {
        function_fields(&function, segments, false);
    }
}

fn collect_content(content: &Res<'_>, segments: &mut Vec<String>) {
    match content.kind {
        Kind::String => add(segments, &s(content)),
        Kind::Json if content.is_array() => {
            for part in content.array() {
                match s(&part.get("type")).as_str() {
                    "text" | "input_text" | "output_text" => add(segments, &s(&part.get("text"))),
                    "image_url" => add(segments, &s(&part.get("image_url.url"))),
                    "input_audio" | "output_audio" | "audio" => add(segments, &s(&part.get("id"))),
                    "tool_result" => {
                        add(segments, &s(&part.get("name")));
                        collect_content(&part.get("content"), segments);
                    }
                    _ if part.is_array() => collect_content(&part, segments),
                    _ if part.is_object() => add(segments, &raw(&part)),
                    _ => add(segments, &s(&part)),
                }
            }
        }
        Kind::Json if content.is_object() => add(segments, &raw(content)),
        _ => {}
    }
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
        if let Some(deadline) = crate::openai_compat_go::parse_http_time(raw) {
            return Some(deadline.duration_since(now).unwrap_or_default());
        }
    }
    let code = gj::get(body, "error.code").str().trim().go_lower();
    let message = gj::get(body, "error.message").str().trim().go_lower();
    (code.contains("tpmratelimitexceeded")
        || (message.contains("tokens per minute") && message.contains("limit") && message.contains("exceeded")))
    .then_some(Duration::from_secs(60))
}

/// `openAICompatErrorEvent`.
pub(crate) fn error_event(name: &str) -> bool {
    ["error", "response.error", "response.failed"]
        .iter()
        .any(|e| name.go_eq_fold(e))
}

/// `openAICompatStreamDataError`: the status to report when a data frame is an error.
pub(crate) fn stream_data_error(payload: &[u8], event: &str) -> Option<u16> {
    if payload.is_empty() || !go::json_valid(payload) {
        return None;
    }
    // gjson finds no object keys in an array or scalar; skip parsing hostile nesting.
    if payload[0] != b'{' {
        return error_event(event).then_some(502);
    }
    let kind = gj::get(payload, "type").str().into_owned();
    let has_error = ["error", "response.error"].iter().any(|p| {
        let node = gj::get(payload, p);
        node.exists() && node.raw() != b"null"
    });
    let top_level = gj::get(payload, "code").exists() && gj::get(payload, "message").exists();
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
        status = gj::get(payload, path).int();
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

    /// A Home-dispatched OpenAI-compatible credential.
    fn home_credential(metadata: serde_json::Value, attributes: &[(&str, &str)]) -> Credential {
        let mut c = Credential {
            id: "c1".into(),
            provider: "openai-compatibility".into(),
            source: Source::Config {
                section: "home".into(),
                index: 0,
            },
            disabled: false,
            label: "home".into(),
            attributes: Default::default(),
            metadata: metadata.as_object().cloned().unwrap_or_default(),
            revision: 0,
        };
        c.attributes
            .insert(cpa_core::config::credentials::HOME_PROVIDER.into(), "contract".into());
        c.attributes.insert("compat_name".into(), "contract".into());
        for (k, v) in attributes {
            c.attributes.insert((*k).into(), (*v).into());
        }
        c
    }

    /// Go `resolveCompatConfig` in Home mode: the selected credential's options make the
    /// entry (the config's entries never count); the attribute overrides the cache-key
    /// flag; the entry Home mode selected replaces the model list; mistyped options
    /// count only alongside another source; nothing at all falls back to the config.
    #[test]
    fn home_credentials_take_their_compat_entry_from_their_options() {
        let cfg = Config::parse(
            "openai-compatibility:\n  - name: contract\n    base-url: http://127.0.0.1:9\n    support-prompt-cache-key: true\n    models:\n      - name: from-config\n",
        )
        .unwrap();
        let options = serde_json::json!({"credential_options": {"support-prompt-cache-key": true,
            "models": [{"name": "a", "use-max-completion-tokens": true}, {"name": "b"}]}});
        let compat = resolve_compat(&home_credential(options.clone(), &[]), &cfg).unwrap();
        assert!(compat.support_prompt_cache_key);
        let names: Vec<&str> = compat.models.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["a", "b"]);
        let off = home_credential(options.clone(), &[("support_prompt_cache_key", "0")]);
        assert!(!resolve_compat(&off, &cfg).unwrap().support_prompt_cache_key);
        let selected = home_credential(
            options,
            &[(
                cpa_core::config::credentials::HOME_MODEL_OPTIONS,
                r#"{"name":"b","input-modalities":["text"]}"#,
            )],
        );
        let compat = resolve_compat(&selected, &cfg).unwrap();
        assert_eq!(compat.models.len(), 1);
        assert_eq!(
            (compat.models[0].name.as_str(), compat.models[0].input_modalities.len()),
            ("b", 1)
        );
        // Mistyped options alone are not an entry: the config one serves.
        let mistyped = home_credential(serde_json::json!({"credential_options": {"models": "invalid"}}), &[]);
        assert_eq!(resolve_compat(&mistyped, &cfg).unwrap().models[0].name, "from-config");
        let with_flag = home_credential(
            serde_json::json!({"credential_options": {"models": "invalid"}}),
            &[("support_prompt_cache_key", "true")],
        );
        let compat = resolve_compat(&with_flag, &cfg).unwrap();
        assert!(compat.support_prompt_cache_key && compat.models.is_empty());
        let bare = home_credential(serde_json::json!({}), &[]);
        assert_eq!(resolve_compat(&bare, &cfg).unwrap().models[0].name, "from-config");
        // Outside Home mode the options are ignored.
        let mut local = home_credential(
            serde_json::json!({"credential_options": {"models": [{"name": "a"}]}}),
            &[],
        );
        local.attributes.remove(cpa_core::config::credentials::HOME_PROVIDER);
        assert_eq!(resolve_compat(&local, &cfg).unwrap().models[0].name, "from-config");
    }

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
            normalize_max_tokens(br#"{"max_tokens":1.50,"x":1}"#.to_vec(), true),
            br#"{"x":1,"max_completion_tokens":1.50}"#
        );
        assert_eq!(normalize_max_tokens(br#"{"x":1}"#.to_vec(), true), br#"{"x":1}"#);
        assert_eq!(
            normalize_max_tokens(br#"{"max_tokens":2,"max_completion_tokens":3}"#.to_vec(), false),
            br#"{"max_tokens":2}"#
        );
    }

    #[test]
    fn claude_code_session_from_payload_user_id() {
        let headers = HeaderMap::new();
        let payload = br#"{"metadata":{"user_id":"user_abc_account__session_0f-9a"}}"#;
        let a = claude_code_prompt_cache("m", payload, &headers).unwrap();
        let mut h = HeaderMap::new();
        h.insert("x-claude-code-session-id", "0f-9a".parse().unwrap());
        assert_eq!(claude_code_prompt_cache("m", b"{}", &h).unwrap(), a);
        assert!(claude_code_prompt_cache("m", br#"{"metadata":{"user_id":"x_session_ABC"}}"#, &headers).is_none());
        assert_eq!(
            claude_code_prompt_cache(
                "m",
                br#"{"metadata":{"user_id":"{\"session_id\":\"0f-9a\"}"}}"#,
                &headers
            )
            .unwrap(),
            a
        );
    }
}
