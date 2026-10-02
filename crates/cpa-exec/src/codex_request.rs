//! Codex request shaping: body rules and upstream headers, applied in Go's order so
//! appended keys land where Go puts them (codex_executor_execute.go, codex_executor_stream.go,
//! codex_executor_request.go, codex_websockets_request.go, helps/codex_input_ids.go,
//! helps/codex_tool_schema.go, openai_responses_signature.go).

use std::time::Duration;

use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_core::exec::{ExecError, ExecRequest, FailureScope};
use cpa_core::format::Format;
use gjson::Kind;
use http::{HeaderMap, HeaderName, HeaderValue};
use sha2::{Digest, Sha256};

use crate::codex_json::{delete, set_bool_if_different, set_raw, set_str, set_str_if_different};

pub const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
pub(crate) const USER_AGENT: &str = "codex-tui/0.154.0 (Mac OS 26.5.2; arm64) iTerm.app/3.6.11 (codex-tui; 0.154.0)";
pub(crate) const ORIGINATOR: &str = "codex-tui";
pub(crate) const WS_BETA: &str = "responses_websockets=2026-02-06";
const ROUTING_HINT: &str = "x-codex-routing-hint";
const LITE_HEADER: &str = "x-openai-internal-codex-responses-lite";

/// Which upstream call a body is shaped for; each has its own Go rule list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Call {
    Stream,
    NonStream,
    Compact,
    Websocket,
}

/// Codex settings read from the config snapshot (v8 paths; legacy keys are already moved).
// ponytail: Go scopes `oauth.providers.codex.*` written in v8 form to OAuth credentials
// only (Config.OAuthOnlyFields). The shared Config does not record that presence yet, so
// these apply to API keys too, as legacy-layout settings do in Go.
#[derive(Debug, Clone, Default)]
pub(crate) struct Settings {
    pub disable_cloaking: bool,
    pub bootstrap_buffering: bool,
    pub bootstrap_timeout: Option<Duration>,
    pub model_level_cooling: bool,
    pub default_user_agent: String,
    pub default_beta_features: String,
    pub image_generation: bool,
}

fn setting<'a>(cfg: &'a Config, path: &[&str]) -> Option<&'a serde_yaml_ng::Value> {
    path.iter().try_fold(&cfg.document, |v, k| v.get(*k))
}

impl Settings {
    pub fn from(cfg: &Config) -> Self {
        let codex = |k: &str| setting(cfg, &["oauth", "providers", "codex", k]);
        let flag = |k: &str| codex(k).and_then(|v| v.as_bool()).unwrap_or(false);
        let text = |path: &[&str]| {
            setting(cfg, path)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .trim()
                .to_owned()
        };
        let image_off = match setting(cfg, &["multimedia", "disable-image-generation"]) {
            None => true,
            Some(v) => v.as_bool() == Some(false) || v.as_str().is_some_and(|s| matches!(s.trim(), "" | "false")),
        };
        Self {
            disable_cloaking: flag("disable-codex-cloaking"),
            bootstrap_buffering: flag("stream-bootstrap-buffering"),
            bootstrap_timeout: codex("stream-bootstrap-timeout")
                .and_then(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .or_else(|| v.as_i64().map(|n| n.to_string()))
                })
                .and_then(|s| go_duration(&s))
                .filter(|d| !d.is_zero()),
            model_level_cooling: flag("model-level-cooling"),
            default_user_agent: text(&["oauth", "providers", "codex", "header-defaults", "user-agent"]),
            default_beta_features: text(&["oauth", "providers", "codex", "header-defaults", "beta-features"]),
            image_generation: image_off,
        }
    }
}

/// `StreamBootstrapTimeoutDuration`: Go durations, bare seconds, or off words.
fn go_duration(raw: &str) -> Option<Duration> {
    let raw = raw.trim();
    if raw.is_empty()
        || raw == "0"
        || ["none", "unlimited", "disabled", "off", "never"]
            .iter()
            .any(|w| raw.eq_ignore_ascii_case(w))
    {
        return None;
    }
    if let Ok(secs) = raw.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let mut total = 0f64;
    let mut rest = raw;
    while !rest.is_empty() {
        let num_len = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        let value: f64 = rest[..num_len].parse().ok()?;
        rest = &rest[num_len..];
        let unit_len = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        let scale = match &rest[..unit_len] {
            "ns" => 1e-9,
            "us" | "µs" => 1e-6,
            "ms" => 1e-3,
            "s" => 1.0,
            "m" => 60.0,
            "h" => 3600.0,
            _ => return None,
        };
        total += value * scale;
        rest = &rest[unit_len..];
    }
    Duration::try_from_secs_f64(total).ok()
}

/// The credential fields the Codex executor reads (`codexCreds`, `codexAuthUsesAPIKey`).
pub(crate) struct View<'a> {
    pub credential: &'a Credential,
    pub token: &'a str,
    pub api_key: bool,
    pub base_url: &'a str,
}

impl<'a> View<'a> {
    pub fn new(credential: &'a Credential) -> Self {
        let attr = |k: &str| credential.attributes.get(k).map(String::as_str).unwrap_or_default();
        let api_key = attr("api_key");
        let token = if api_key.is_empty() {
            credential.str("access_token").unwrap_or_default()
        } else {
            api_key
        };
        let kind = attr("auth_kind").trim().to_ascii_lowercase().replace(['_', '-'], "");
        let is_api_key = kind == "apikey" || (kind.is_empty() && !api_key.trim().is_empty());
        let base_url = match attr("base_url") {
            "" => DEFAULT_BASE_URL,
            url => url,
        };
        Self {
            credential,
            token,
            api_key: is_api_key,
            base_url: base_url.trim_end_matches('/'),
        }
    }

    pub fn attr(&self, key: &str) -> &str {
        self.credential
            .attributes
            .get(key)
            .map(String::as_str)
            .unwrap_or_default()
    }

    /// Plan used for the free-plan image tool rule: refreshed metadata wins over the
    /// attribute computed at load time.
    fn plan_type(&self) -> &str {
        self.credential
            .str("plan_type")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(self.attr("plan_type"))
    }

    /// `isCodexCloakingDisabled`: attribute, then the global setting.
    // ponytail: Go also consults the matching `codex-api-key` config entry; the
    // synthesizer already copies its value into `codex_disable_cloaking`.
    pub fn cloaking_disabled(&self, settings: &Settings) -> bool {
        go_bool(self.attr("codex_disable_cloaking")).unwrap_or(settings.disable_cloaking)
    }

    /// `codexWebsocketsEnabled`: attribute first, then metadata.
    pub fn websockets(&self) -> bool {
        match self.attr("websockets").trim() {
            "" => match self.credential.metadata.get("websockets") {
                Some(serde_json::Value::Bool(b)) => *b,
                Some(serde_json::Value::String(s)) => go_bool(s).unwrap_or(false),
                _ => false,
            },
            raw => go_bool(raw).unwrap_or(false),
        }
    }
}

/// Go `strconv.ParseBool`.
fn go_bool(s: &str) -> Option<bool> {
    match s.trim() {
        "1" | "t" | "T" | "true" | "TRUE" | "True" => Some(true),
        "0" | "f" | "F" | "false" | "FALSE" | "False" => Some(false),
        _ => None,
    }
}

/// `thinking.ParseSuffix(model).ModelName`.
pub(crate) fn base_model(model: &str) -> String {
    cpa_common::thinking::parse_suffix(model).model_name
}

pub(crate) fn header<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or_default()
}

/// `util.IsCodexResponsesLiteRequest`.
pub(crate) fn is_lite(body: &str, headers: &HeaderMap) -> bool {
    if header(headers, LITE_HEADER).trim().eq_ignore_ascii_case("true") {
        return true;
    }
    let v = gjson::get(
        body,
        "client_metadata.ws_request_header_x_openai_internal_codex_responses_lite",
    );
    v.kind() == Kind::True || (v.kind() == Kind::String && v.str().trim().eq_ignore_ascii_case("true"))
}

/// `helps.IsNativeCodexRequest`: Codex/Responses formats on both sides plus the lite marker.
pub(crate) fn is_native(req: &ExecRequest) -> bool {
    let codexish = |f: Format| matches!(f, Format::Codex | Format::OpenAIResponse);
    codexish(req.source_format)
        && codexish(req.response_format)
        && is_lite(&String::from_utf8_lossy(&req.body), &req.headers)
}

/// `translateCodexRequestPairWithUpdateIntent` + `helps.ApplyRequestThinking`: the
/// registered pair (Go's model-rewrite fallback for same-format bodies), then the
/// thinking pipeline for `codex` (`openai-response` for compact).
// ponytail: Go translates `opts.OriginalRequest` separately for payload rules and passes
// the translator's configuration-update intent; payload rules are not applied yet, so
// only the request payload is translated and `updates_changed` stays false. Resolved
// API-key model capabilities (`ResolvedModelInfo`) are not bound to the attempt either.
fn translate_request(req: &ExecRequest, call: Call) -> Result<String, ExecError> {
    let target = if call == Call::Compact {
        Format::OpenAIResponse
    } else {
        Format::Codex
    };
    let registered = cpa_translate::pair(req.source_format, target).is_some();
    if !registered && !matches!(req.source_format, Format::Codex | Format::OpenAIResponse) {
        // ponytail: Go forwards an untranslatable body as-is; refusing locally is clearer.
        return Err(ExecError::local(
            501,
            FailureScope::Request,
            format!(
                "{} -> {} request translation is not registered",
                req.source_format.as_str(),
                target.as_str()
            ),
        ));
    }
    let model = base_model(&req.model);
    let ctx = cpa_translate::RequestCtx {
        model: &model,
        stream: matches!(call, Call::Stream | Call::Websocket),
    };
    let body = cpa_translate::translate_request(req.source_format, target, &ctx, &req.body)
        .map_err(|e| ExecError::local(400, FailureScope::Request, e.to_string()))?;
    let body = String::from_utf8(body)
        .map_err(|_| ExecError::local(400, FailureScope::Request, "request body is not valid UTF-8 JSON"))?;
    let payload = String::from_utf8_lossy(&req.body);
    let original = String::from_utf8_lossy(&req.original_body);
    cpa_common::thinking::apply_request_thinking(&cpa_common::thinking::RequestThinking {
        body: &body,
        payload: &payload,
        original: &original,
        model: &req.model,
        from: req.source_format.as_str(),
        to: target.as_str(),
        provider: "codex",
        resolved: None,
        has_request_transformer: registered,
        updates_changed: false,
    })
    .map_err(|e| ExecError::local(e.status(), FailureScope::Request, e.message))
}

/// Applies the Codex body rules for `call`.
// ponytail: payload rules (M4-0031), multi-agent v2 optimisation
// (client.codex.optimize-multi-agent-v2, default off) and the Claude-source reasoning
// replay cache are not applied here yet.
pub(crate) fn shape(req: &ExecRequest, view: &View<'_>, settings: &Settings, call: Call) -> Result<String, ExecError> {
    let model = base_model(&req.model);
    let model = model.as_str();
    let mut body = translate_request(req, call)?;
    match call {
        Call::NonStream => {
            body = set_str_if_different(body, "model", model);
            body = set_bool_if_different(body, "stream", true);
            for key in [
                "previous_response_id",
                "generate",
                "prompt_cache_retention",
                "safety_identifier",
                "stream_options",
            ] {
                body = delete(&body, key);
            }
        }
        Call::Stream => {
            for key in [
                "previous_response_id",
                "generate",
                "prompt_cache_retention",
                "safety_identifier",
            ] {
                body = delete(&body, key);
            }
            let delivery = gjson::get(&body, "stream_options.reasoning_summary_delivery");
            let delivery = delivery
                .exists()
                .then(|| (delivery.kind(), delivery.str().to_owned(), delivery.json().to_owned()));
            body = delete(&body, "stream_options");
            if let Some((kind, text, raw)) = delivery {
                // sjson.SetBytes with the decoded value: strings re-quote, scalars stay raw.
                body = if kind == Kind::String {
                    set_str(&body, "stream_options.reasoning_summary_delivery", &text)
                } else {
                    set_raw(&body, "stream_options.reasoning_summary_delivery", &raw)
                };
            }
            body = set_str_if_different(body, "model", model);
        }
        Call::Websocket => {
            body = set_str_if_different(body, "model", model);
            body = set_bool_if_different(body, "stream", true);
            body = delete(&body, "prompt_cache_retention");
            body = delete(&body, "safety_identifier");
        }
        Call::Compact => {
            body = set_str_if_different(body, "model", model);
            body = delete(&body, "stream");
        }
    }
    if !is_native(req) {
        let instructions = gjson::get(&body, "instructions");
        if !instructions.exists() || instructions.kind() == Kind::Null {
            body = set_str(&body, "instructions", "");
        }
    }
    if call != Call::Compact && settings.image_generation {
        body = ensure_image_tool(body, model, view, &req.headers);
    }
    body = sanitize_reasoning(body);
    body = if call == Call::Websocket {
        if is_lite(&body, &req.headers) {
            set_bool_if_different(body, "parallel_tool_calls", false)
        } else {
            body
        }
    } else {
        normalize_parallel_tool_calls(body, &req.headers)
    };
    Ok(normalize_tool_schemas(body))
}

fn ensure_image_tool(body: String, model: &str, view: &View<'_>, headers: &HeaderMap) -> String {
    const TOOL: &str = r#"{"type":"image_generation","output_format":"png"}"#;
    if is_lite(&body, headers)
        || model.ends_with("spark")
        || (view.credential.provider.eq_ignore_ascii_case("codex") && view.plan_type().eq_ignore_ascii_case("free"))
    {
        return body;
    }
    let tools = gjson::get(&body, "tools");
    if !tools.exists() || tools.kind() != Kind::Array {
        return set_raw(&body, "tools", &format!("[{TOOL}]"));
    }
    let present = tools.array().iter().any(|t| {
        let kind = t.get("type");
        let name = t.get("name");
        match kind.str() {
            "image_generation" => true,
            "function" => name.str() == "image_gen.imagegen",
            "namespace" => {
                name.str() == "image_gen"
                    && t.get("tools")
                        .array()
                        .iter()
                        .any(|n| n.get("type").str() == "function" && n.get("name").str() == "imagegen")
            }
            _ => false,
        }
    });
    if present {
        body
    } else {
        set_raw(&body, "tools.-1", TOOL)
    }
}

fn normalize_parallel_tool_calls(body: String, headers: &HeaderMap) -> String {
    if is_lite(&body, headers) {
        return set_bool_if_different(body, "parallel_tool_calls", false);
    }
    if !gjson::get(&body, "parallel_tool_calls").exists() {
        return body;
    }
    let tools = gjson::get(&body, "tools");
    if tools.kind() == Kind::Array && !tools.array().is_empty() {
        return body;
    }
    delete(&body, "parallel_tool_calls")
}

/// `sanitizeOpenAIResponsesReasoningEncryptedContentWithCompat(..., isCompat=false)`.
// ponytail: per-model `is-compat` (third-party Responses models) is not wired; Codex
// models are never compat.
fn sanitize_reasoning(body: String) -> String {
    let input = gjson::get(&body, "input");
    if input.kind() != Kind::Array {
        return body;
    }
    let strip_orphan_ids = !gjson::get(&body, "store").bool();
    let items = input.array();
    let mut rebuilt: Option<Vec<String>> = None;
    for (index, item) in items.iter().enumerate() {
        let mut next = item.json().to_owned();
        let mut changed = false;
        if item.get("type").str().trim() == "reasoning" {
            let content = item.get("content");
            if content.kind() == Kind::Array && !content.array().is_empty() {
                let summary = item.get("summary");
                let empty_summary = !summary.exists()
                    || summary.kind() == Kind::Null
                    || (summary.kind() == Kind::Array && summary.array().is_empty());
                if empty_summary {
                    let parts: Vec<String> = content
                        .array()
                        .iter()
                        .filter(|p| p.get("type").str().trim() == "reasoning_text" && !p.get("text").str().is_empty())
                        .map(|p| set_str(r#"{"type":"summary_text"}"#, "text", p.get("text").str()))
                        .collect();
                    if !parts.is_empty() {
                        next = set_raw(&next, "summary", &format!("[{}]", parts.join(",")));
                    }
                }
                next = set_raw(&next, "content", "[]");
                changed = true;
            }
            let encrypted = item.get("encrypted_content");
            let has_id = item.get("id").exists();
            if !encrypted.exists() {
                if strip_orphan_ids && has_id {
                    next = delete(&next, "id");
                    changed = true;
                }
            } else {
                let valid = encrypted.kind() == Kind::String
                    && encrypted.str() == encrypted.str().trim()
                    && cpa_common::signature::is_valid_gpt_reasoning_signature(encrypted.str());
                if !valid {
                    next = delete(&next, "encrypted_content");
                    changed = true;
                    if strip_orphan_ids && has_id {
                        next = delete(&next, "id");
                    }
                }
            }
        }
        if changed && rebuilt.is_none() {
            rebuilt = Some(items[..index].iter().map(|i| i.json().to_owned()).collect());
        }
        if let Some(rebuilt) = rebuilt.as_mut() {
            rebuilt.push(if changed { next } else { item.json().to_owned() });
        }
    }
    match rebuilt {
        Some(items) => set_raw(&body, "input", &format!("[{}]", items.join(","))),
        None => body,
    }
}

/// `helps.NormalizeCodexToolSchemas`: collapse large pure-`const` unions into `enum`.
// ponytail: adapter for cpa-common::payload (server thread owns codex_tool_schema.go);
// replace this and its helpers with the shared module. The companion stripIncompatiblePatterns pass (drops `\p{..}` regex patterns by
// re-encoding the schema) is not ported; such schemas pass through unchanged.
fn normalize_tool_schemas(body: String) -> String {
    let tools = gjson::get(&body, "tools");
    match normalize_tool_list(&tools) {
        Some(updated) => set_raw(&body, "tools", &updated),
        None => body,
    }
}

fn normalize_tool_list(tools: &gjson::Value<'_>) -> Option<String> {
    if tools.kind() != Kind::Array {
        return None;
    }
    let raw = tools.json();
    let mut out = String::new();
    let mut offset = 0;
    let mut changed = false;
    tools.each(|_, tool| {
        if let Some(updated) = normalize_tool(&tool) {
            let start = tool.json().as_ptr() as usize - raw.as_ptr() as usize;
            out.push_str(&raw[offset..start]);
            out.push_str(&updated);
            offset = start + tool.json().len();
            changed = true;
        }
        true
    });
    changed.then(|| {
        out.push_str(&raw[offset..]);
        out
    })
}

fn normalize_tool(tool: &gjson::Value<'_>) -> Option<String> {
    let kind = tool.get("type");
    match kind.str() {
        "namespace" => {
            let inner = normalize_tool_list(&tool.get("tools"))?;
            Some(set_raw(tool.json(), "tools", &inner))
        }
        "function" | "custom" => {
            let params = tool.get("parameters");
            if params.kind() != Kind::Object {
                return None;
            }
            let mut raw = params.json().to_owned();
            let mut changed = false;
            let mut updates = Vec::new();
            let properties = params.get("properties");
            if properties.kind() == Kind::Object {
                properties.each(|name, prop| {
                    if let Some(updated) = normalize_property(&prop) {
                        updates.push((name.str().to_owned(), updated));
                    }
                    true
                });
            }
            for (name, updated) in updates {
                let key = name.replace('\\', "\\\\").replace('.', "\\.").replace(':', "\\:");
                raw = set_raw(&raw, &format!("properties.{key}"), &updated);
                changed = true;
            }
            changed.then(|| set_raw(tool.json(), "parameters", &raw))
        }
        _ => None,
    }
}

fn normalize_property(prop: &gjson::Value<'_>) -> Option<String> {
    if prop.kind() != Kind::Object {
        return None;
    }
    let name = match (prop.get("oneOf").exists(), prop.get("anyOf").exists()) {
        (true, false) => "oneOf",
        (false, true) => "anyOf",
        _ => return None,
    };
    let union = prop.get(name);
    let branches = union.array();
    if union.kind() != Kind::Array || branches.len() < 8 {
        return None;
    }
    let mut keys = Vec::new();
    let mut raws = Vec::new();
    for branch in &branches {
        if branch.kind() != Kind::Object {
            return None;
        }
        let mut only_const = true;
        branch.each(|k, _| {
            only_const &= matches!(k.str(), "const" | "description" | "title");
            true
        });
        let value = branch.get("const");
        if !only_const || !value.exists() {
            return None;
        }
        let key = canonical_key(&value)?;
        if keys.contains(&key) {
            return None;
        }
        keys.push(key);
        raws.push(value.json().to_owned());
    }
    let existing = prop.get("enum");
    if existing.exists() && existing.kind() == Kind::Array {
        let mut existing_keys = Vec::new();
        for v in existing.array() {
            existing_keys.push(canonical_key(&v)?);
        }
        let mut unique = existing_keys.clone();
        unique.sort();
        unique.dedup();
        let mut wanted = keys.clone();
        wanted.sort();
        let same = existing_keys.len() == keys.len() && unique.len() == existing_keys.len() && unique == wanted;
        return same.then(|| delete(prop.json(), name));
    }
    let with_enum = set_raw(prop.json(), "enum", &format!("[{}]", raws.join(",")));
    Some(delete(&with_enum, name))
}

/// `canonicalJSONValueKey`: numbers compare as exact rationals (`1` == `1.0` == `1e0`).
fn canonical_key(value: &gjson::Value<'_>) -> Option<String> {
    Some(match value.kind() {
        Kind::String => format!("s:{}", value.str()),
        Kind::Number => format!("n:{}", rational(value.json().trim())),
        Kind::True => "b:true".into(),
        Kind::False => "b:false".into(),
        Kind::Null => "null".into(),
        _ => return None,
    })
}

/// Exact decimal normal form of a JSON number: sign, significant digits, exponent.
fn rational(raw: &str) -> String {
    let (mantissa, exp) = match raw.find(['e', 'E']) {
        Some(i) => (&raw[..i], raw[i + 1..].parse::<i64>().unwrap_or(0)),
        None => (raw, 0),
    };
    let negative = mantissa.starts_with('-');
    let mantissa = mantissa.trim_start_matches(['-', '+']);
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits = format!("{int}{frac}");
    let exp = exp - frac.len() as i64;
    let digits = digits.trim_start_matches('0');
    if digits.is_empty() {
        return "0".into();
    }
    let trimmed = digits.trim_end_matches('0');
    let exp = exp + (digits.len() - trimmed.len()) as i64;
    format!("{}{trimmed}e{exp}", if negative { "-" } else { "" })
}

/// `helps.SanitizeCodexInputItemIDs`: type prefixes, the 64-char limit, collision suffixes
/// and dropping over-long encrypted reasoning items.
pub(crate) fn sanitize_input_ids(body: String) -> String {
    const LIMIT: usize = 64;
    const OCCUPIED: u8 = 1;
    const PRESERVED: u8 = 2;
    let input = gjson::get(&body, "input");
    if input.kind() != Kind::Array {
        return body;
    }
    let items = input.array();
    let normalize = |item: &gjson::Value<'_>, id: &str| -> String {
        let prefix = match item.get("type").str() {
            "message" => "msg",
            "reasoning" => "rs",
            "function_call" => "fc",
            "custom_tool_call" => "ctc",
            "custom_tool_call_output" => "ctco",
            _ => return id.to_owned(),
        };
        if id.is_empty() || id.starts_with(prefix) {
            id.to_owned()
        } else {
            format!("{prefix}_{id}")
        }
    };
    let dropped = |item: &gjson::Value<'_>| {
        let id = item.get("id");
        let enc = item.get("encrypted_content");
        item.get("type").str() == "reasoning"
            && id.kind() == Kind::String
            && id.str().chars().count() > LIMIT
            && enc.kind() == Kind::String
            && !enc.str().is_empty()
    };
    let mut states: std::collections::HashMap<String, u8> = Default::default();
    for item in &items {
        let id = item.get("id");
        if dropped(item) || id.kind() != Kind::String {
            continue;
        }
        let normalized = normalize(item, id.str());
        let mut state = states.get(&normalized).copied().unwrap_or(0);
        if normalized == id.str() {
            state |= PRESERVED;
        }
        if normalized.chars().count() <= LIMIT {
            state |= OCCUPIED;
        }
        if state != 0 {
            states.insert(normalized, state);
        }
    }
    let hashed = |id: &str, attempt: usize| {
        let mut input = id.to_owned();
        if attempt > 0 {
            input.push('\0');
            input.push_str(&attempt.to_string());
        }
        let digest = Sha256::digest(input.as_bytes());
        let suffix = format!(
            "_{}",
            digest[..8].iter().map(|b| format!("{b:02x}")).collect::<String>()
        );
        let keep = (LIMIT - suffix.len()).min(id.chars().count());
        format!("{}{suffix}", id.chars().take(keep).collect::<String>())
    };
    let mut shortened: std::collections::HashMap<String, String> = Default::default();
    let mut collisions: std::collections::HashMap<String, String> = Default::default();
    let mut rebuilt = Vec::with_capacity(items.len());
    let mut changed = false;
    for item in &items {
        if dropped(item) {
            changed = true;
            continue;
        }
        let mut raw = item.json().to_owned();
        let id = item.get("id");
        if id.kind() == Kind::String {
            let original = id.str();
            let mut next = normalize(item, original);
            if next != original && states.get(&next).is_some_and(|s| s & PRESERVED != 0) {
                next = match collisions.get(&next) {
                    Some(c) => c.clone(),
                    None => {
                        let mut attempt = 0;
                        let candidate = loop {
                            let candidate = hashed(&next, attempt);
                            if states.get(&candidate).is_some_and(|s| s & OCCUPIED != 0) {
                                attempt += 1;
                                continue;
                            }
                            break candidate;
                        };
                        collisions.insert(next.clone(), candidate.clone());
                        *states.entry(candidate.clone()).or_default() |= OCCUPIED;
                        candidate
                    }
                };
            }
            if next.chars().count() > LIMIT {
                next = match shortened.get(&next) {
                    Some(s) => s.clone(),
                    None => {
                        let mut candidate = hashed(&next, 0);
                        let mut attempt = 1;
                        while states.get(&candidate).is_some_and(|s| s & OCCUPIED != 0) {
                            candidate = hashed(&next, attempt);
                            attempt += 1;
                        }
                        shortened.insert(next.clone(), candidate.clone());
                        *states.entry(candidate.clone()).or_default() |= OCCUPIED;
                        candidate
                    }
                };
            }
            if next != original {
                raw = set_str(&raw, "id", &next);
                changed = true;
            }
        }
        rebuilt.push(raw);
    }
    if changed {
        set_raw(&body, "input", &format!("[{}]", rebuilt.join(",")))
    } else {
        body
    }
}

/// `uuid.NewString()`: a random v4 UUID.
pub(crate) fn random_uuid() -> String {
    let mut bytes = [0u8; 16];
    let _ = getrandom::fill(&mut bytes);
    uuid::Builder::from_random_bytes(bytes).into_uuid().to_string()
}

/// UUIDv5(OID, "cli-proxy-api\0codex\0<kind>\0<id>") (`stableProviderSessionUUID`).
pub(crate) fn provider_session_uuid(kind: &str, id: &str) -> Option<String> {
    let id = id.trim();
    (!id.is_empty()).then(|| {
        uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_OID,
            format!("cli-proxy-api\0codex\0{kind}\0{id}").as_bytes(),
        )
        .to_string()
    })
}

/// `ProviderSessionUUID`: the downstream WebSocket connection when there is one, else
/// the derived session identity.
fn session_uuid(req: &ExecRequest, ws_session: Option<&str>) -> Option<String> {
    match ws_session {
        Some(id) => provider_session_uuid("execution-session", id),
        None => req
            .session
            .as_deref()
            .and_then(|s| provider_session_uuid("derived-session", s)),
    }
}

/// `cacheHelper` / `applyCodexPromptCacheHeadersWithContext`: the prompt cache key from
/// the client body (Responses/Chat) or the session identity, written into the body.
// ponytail: Claude-source prompt caching (helps.ClaudeCodePromptCache) arrives with the
// Claude -> Codex translator; it falls back to the session UUID meanwhile.
pub(crate) fn prompt_cache(
    req: &ExecRequest,
    body: String,
    ws_session: Option<&str>,
    websocket: bool,
) -> (String, Option<String>) {
    let original = String::from_utf8_lossy(&req.body);
    let client_key = gjson::get(&original, "prompt_cache_key");
    let mut id = match req.source_format {
        Format::OpenAIResponse if client_key.exists() => client_key.str().to_owned(),
        Format::OpenAI if client_key.exists() && !websocket => client_key.str().trim().to_owned(),
        _ => String::new(),
    };
    if id.is_empty() {
        id = session_uuid(req, ws_session).unwrap_or_default();
    }
    if id.is_empty() && !websocket && req.source_format == Format::OpenAI && !req.caller.principal.trim().is_empty() {
        id = uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_OID,
            format!("cli-proxy-api:codex:prompt-cache:{}", req.caller.principal.trim()).as_bytes(),
        )
        .to_string();
    }
    if id.is_empty() {
        return (body, None);
    }
    (set_str_if_different(body, "prompt_cache_key", &id), Some(id))
}

fn set(headers: &mut HeaderMap, name: &str, value: &str) {
    if let (Ok(name), Ok(value)) = (HeaderName::try_from(name), HeaderValue::from_str(value)) {
        headers.insert(name, value);
    }
}

/// `misc.EnsureHeader`: the client's value wins, then what is already set.
fn ensure_from_client(headers: &mut HeaderMap, client: &HeaderMap, name: &str) {
    let value = header(client, name).trim();
    if !value.is_empty() {
        set(headers, name, value);
    }
}

/// `ensureHeaderWithConfigPrecedence`: existing, config, client, fallback.
fn ensure_config_first(headers: &mut HeaderMap, client: &HeaderMap, name: &str, config: &str, fallback: &str) {
    if !header(headers, name).trim().is_empty() {
        return;
    }
    for value in [config, header(client, name), fallback] {
        if !value.trim().is_empty() {
            set(headers, name, value.trim());
            return;
        }
    }
}

/// `util.ApplyCustomHeadersFromAttrs`: `header:<Name>` attributes, `$Client-Header`
/// references and `$CPA-SESSION-ID`. Unresolvable references are omitted.
// ponytail: adapter for cpa-common::headers (server thread owns header_helpers.go).
pub(crate) fn custom_headers(view: &View<'_>, client: &HeaderMap, session: Option<&str>) -> Vec<(String, String)> {
    const SESSION_VAR: &str = "$CPA-SESSION-ID";
    let mut out = Vec::new();
    for (key, value) in &view.credential.attributes {
        let Some(name) = key.strip_prefix("header:").map(str::trim).filter(|n| !n.is_empty()) else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        let resolved = if value.to_ascii_uppercase().contains(SESSION_VAR) {
            let Some(session) = session.filter(|s| !s.is_empty()) else {
                continue;
            };
            let mut replaced = String::new();
            let mut rest = value;
            while let Some(i) = rest.to_ascii_uppercase().find(SESSION_VAR) {
                replaced.push_str(&rest[..i]);
                replaced.push_str(session);
                rest = &rest[i + SESSION_VAR.len()..];
            }
            replaced.push_str(rest);
            replaced
        } else if let Some(var) = value.strip_prefix('$') {
            let found = header(client, var.trim());
            if var.trim().is_empty() || found.is_empty() {
                continue;
            }
            found.to_owned()
        } else {
            value.to_owned()
        };
        out.push((name.to_owned(), resolved));
    }
    out
}

/// `applyModelHeaderOverrides`: models.json `config.override_header` for the model.
fn model_overrides(model: &str) -> Vec<(String, String)> {
    let Some(info) = cpa_core::registry::pinned().lookup(model) else {
        return Vec::new();
    };
    info.raw
        .get("config")
        .and_then(|c| c.get("override_header"))
        .and_then(serde_json::Value::as_object)
        .map(|m| {
            m.iter()
                .filter(|(k, _)| !k.trim().is_empty())
                .filter_map(|(k, v)| Some((k.trim().to_owned(), v.as_str()?.to_owned())))
                .collect()
        })
        .unwrap_or_default()
}

/// HTTP upstream headers (`applyCodexHeadersFromSources`, routing hint, model overrides).
pub(crate) fn http_headers(
    view: &View<'_>,
    settings: &Settings,
    client: &HeaderMap,
    body: &str,
    model: &str,
    cache_id: Option<&str>,
    stream: bool,
) -> HeaderMap {
    let mut h = HeaderMap::new();
    if let Some(id) = cache_id {
        set(&mut h, "session-id", id);
    }
    set(&mut h, "content-type", "application/json");
    if !view.token.trim().is_empty() {
        set(&mut h, "authorization", &format!("Bearer {}", view.token));
    }
    for name in [
        "x-codex-beta-features",
        "version",
        "x-codex-turn-metadata",
        "x-codex-turn-state",
        "x-client-request-id",
        "x-codex-window-id",
        "thread-id",
        "session-id",
        LITE_HEADER,
    ] {
        ensure_from_client(&mut h, client, name);
    }
    let default_ua = if view.api_key { "" } else { &settings.default_user_agent };
    ensure_config_first(&mut h, client, "user-agent", default_ua, USER_AGENT);
    set(
        &mut h,
        "accept",
        if stream {
            "text/event-stream"
        } else {
            "application/json"
        },
    );
    set(&mut h, "connection", "Keep-Alive");
    apply_identity(&mut h, view, settings, client, cache_id);
    routing_hint(&mut h, view, client, body, model, cache_id);
    model_header_overrides(&mut h, model);
    // Go's transport adds this and decodes the body transparently.
    set(&mut h, "accept-encoding", "gzip");
    h
}

/// Originator, account header, operator headers and cloaking, shared by HTTP and WebSocket.
fn apply_identity(h: &mut HeaderMap, view: &View<'_>, settings: &Settings, client: &HeaderMap, session: Option<&str>) {
    let originator = header(client, "originator").trim();
    if !originator.is_empty() {
        set(h, "originator", originator);
    } else if !view.api_key {
        set(h, "originator", ORIGINATOR);
    }
    if !view.api_key
        && let Some(account) = view.credential.str("account_id")
    {
        set(h, "chatgpt-account-id", account.trim());
    }
    for (name, value) in custom_headers(view, client, session) {
        set(h, &name, &value);
    }
    if !view.cloaking_disabled(settings) {
        set(h, "user-agent", USER_AGENT);
        set(h, "originator", ORIGINATOR);
    }
}

/// `applyCodexRoutingHint`: `model=<slug>[;tier=<service_tier>]` for OAuth credentials.
fn routing_hint(
    h: &mut HeaderMap,
    view: &View<'_>,
    client: &HeaderMap,
    body: &str,
    model: &str,
    session: Option<&str>,
) {
    if view.api_key {
        return;
    }
    h.remove(ROUTING_HINT);
    let operator = custom_headers(view, client, session)
        .into_iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(ROUTING_HINT))
        .map(|(_, v)| v);
    if let Some(value) = operator.filter(|v| !v.trim().is_empty()) {
        set(h, ROUTING_HINT, value.trim());
        return;
    }
    let model = model.trim();
    if model.is_empty() {
        return;
    }
    let mut hint = format!("model={model}");
    let tier = gjson::get(body, "service_tier");
    if tier.kind() == Kind::String && !tier.str().trim().is_empty() {
        hint.push_str(";tier=");
        hint.push_str(tier.str().trim());
    }
    set(h, ROUTING_HINT, &hint);
}

fn model_header_overrides(h: &mut HeaderMap, model: &str) {
    let overrides = model_overrides(model);
    if overrides.is_empty() {
        return;
    }
    for (k, v) in overrides {
        set(h, &k, &v);
    }
    let has_session = ["session-id", "session_id"]
        .iter()
        .any(|n| !header(h, n).trim().is_empty());
    if header(h, "user-agent").contains("Mac OS") && !has_session {
        set(h, "session_id", &random_uuid());
    }
}

/// WebSocket handshake headers (`applyCodexPromptCacheHeadersWithContext`,
/// `applyCodexWebsocketHeaders`, routing hint, model overrides).
pub(crate) fn ws_headers(
    view: &View<'_>,
    settings: &Settings,
    client: &HeaderMap,
    body: &str,
    model: &str,
    cache_id: Option<&str>,
    native: bool,
) -> HeaderMap {
    let mut h = HeaderMap::new();
    if let Some(id) = cache_id {
        set(&mut h, "session_id", id);
        set(&mut h, "conversation_id", id);
    }
    if !view.token.trim().is_empty() {
        set(&mut h, "authorization", &format!("Bearer {}", view.token));
    }
    let (default_ua, default_beta) = if view.api_key {
        ("", "")
    } else {
        (
            settings.default_user_agent.as_str(),
            settings.default_beta_features.as_str(),
        )
    };
    // ensureHeaderWithPriority: existing, client, config.
    for value in [header(client, "x-codex-beta-features"), default_beta] {
        if !value.trim().is_empty() {
            set(&mut h, "x-codex-beta-features", value.trim());
            break;
        }
    }
    for name in [
        "x-codex-turn-state",
        "x-codex-turn-metadata",
        "x-client-request-id",
        "x-responsesapi-include-timing-metrics",
        "version",
    ] {
        ensure_from_client(&mut h, client, name);
    }
    if native {
        ensure_from_client(&mut h, client, LITE_HEADER);
    }
    if view.api_key {
        let ua = header(client, "user-agent").trim();
        if !ua.is_empty() {
            set(&mut h, "user-agent", ua);
        }
    } else {
        ensure_config_first(&mut h, client, "user-agent", default_ua, USER_AGENT);
    }
    let mut beta = header(client, "openai-beta").trim().to_owned();
    if !beta.contains("responses_websockets=") {
        beta = WS_BETA.to_owned();
    }
    set(&mut h, "openai-beta", &beta);
    // ensureCodexWebsocketSessionHeader: existing, client, then a fresh id for Mac OS UAs.
    let mut session = header(&h, "session_id").trim().to_owned();
    if session.is_empty() {
        session = ["session-id", "session_id"]
            .iter()
            .map(|n| header(client, n).trim())
            .find(|v| !v.is_empty())
            .unwrap_or_default()
            .to_owned();
    }
    if session.is_empty() && header(&h, "user-agent").contains("Mac OS") {
        session = random_uuid();
    }
    if !session.is_empty() {
        set(&mut h, "session_id", &session);
    }
    h.remove("session-id");
    if native && view.cloaking_disabled(settings) {
        h.remove("session_id");
        h.remove("conversation_id");
        for name in [
            "session-id",
            "session_id",
            "conversation_id",
            "thread-id",
            ROUTING_HINT,
            "x-codex-window-id",
        ] {
            for value in client.get_all(name) {
                h.append(HeaderName::from_static(name), value.clone());
            }
        }
    }
    apply_identity(&mut h, view, settings, client, cache_id);
    routing_hint(&mut h, view, client, body, model, cache_id);
    model_header_overrides(&mut h, model);
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_follow_go_parse_duration() {
        assert_eq!(go_duration("1m30s"), Some(Duration::from_secs(90)));
        assert_eq!(go_duration("1.5s"), Some(Duration::from_millis(1500)));
        assert_eq!(go_duration("45"), Some(Duration::from_secs(45)));
        assert_eq!(go_duration("off"), None);
        assert_eq!(go_duration("soon"), None);
    }

    #[test]
    fn rationals_compare_like_big_rat() {
        assert_eq!(rational("8"), rational("8.0"));
        assert_eq!(rational("8"), rational("0.8e1"));
        assert_eq!(rational("-0.50"), rational("-5e-1"));
        assert_ne!(rational("8"), rational("80"));
        assert_eq!(rational("0.000"), "0");
    }
}
