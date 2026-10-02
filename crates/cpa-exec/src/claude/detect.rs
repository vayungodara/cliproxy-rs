//! Native Claude Code detection (helps/claude_client_detection.go).
//!
//! A request is confirmed native only with all strong signals (x-app=cli, a native
//! User-Agent on the measured release line, the claude-code beta and a valid
//! metadata.user_id; count_tokens omits the last) from a verified entrypoint, or as
//! one of the exactly measured Haiku helper shapes. Copying the User-Agent alone is
//! not enough.

use http::HeaderMap;

use super::profile::{self, Profile};
use super::settings::Settings;
use crate::rawjson;

/// Entrypoints whose wire behaviour was captured and reviewed for passthrough.
pub(crate) const NATIVE_ENTRYPOINTS: &[&str] = &["cli", "sdk-cli", "claude-vscode"];
pub(crate) const HELPER_MODEL: &str = "claude-haiku-4-5-20251001";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Detection {
    pub confirmed: bool,
    pub helper_profile: bool,
    pub entrypoint: String,
}

/// First non-empty value of `name`, trimmed (`headerValueCaseInsensitive`).
pub(crate) fn header<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .map(str::trim)
        .find(|v| !v.is_empty())
        .unwrap_or_default()
}

/// Go `headerValue`: the first value, untrimmed.
fn raw_header<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or_default()
}

/// All non-empty trimmed values of `name` (`HeaderValuesCaseInsensitive`).
pub(crate) fn header_values<'a>(headers: &'a HeaderMap, name: &str) -> Vec<&'a str> {
    headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .collect()
}

/// `(?i)^claude-cli/\S+\s+\(external,\s*([^,)]+)(?:,\s*agent-sdk/([^,)]+))?`
pub(crate) fn user_agent_details(user_agent: &str) -> (String, String) {
    let ua = user_agent.trim();
    let Some(rest) = strip_prefix_ci(ua, "claude-cli/") else {
        return Default::default();
    };
    let Some(space) = rest.find(char::is_whitespace) else {
        return Default::default();
    };
    if space == 0 {
        return Default::default();
    }
    let rest = rest[space..].trim_start();
    let Some(rest) = strip_prefix_ci(rest, "(external,") else {
        return Default::default();
    };
    let rest = rest.trim_start();
    let end = rest.find([',', ')']).unwrap_or(rest.len());
    if end == 0 {
        return Default::default();
    }
    let entrypoint = rest[..end].trim().to_ascii_lowercase();
    let mut sdk = String::new();
    if let Some(after) = rest[end..].strip_prefix(',')
        && let Some(v) = strip_prefix_ci(after.trim_start(), "agent-sdk/")
    {
        let end = v.find([',', ')']).unwrap_or(v.len());
        if end > 0 {
            sdk = v[..end].trim().to_owned();
        }
    }
    (entrypoint, sdk)
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    (s.len() >= prefix.len() && s.is_char_boundary(prefix.len()) && s[..prefix.len()].eq_ignore_ascii_case(prefix))
        .then(|| &s[prefix.len()..])
}

/// `(?i)^claude-cli/N.N.N\s+\(external,\s*[^,)]+(?:,\s*agent-sdk/N.N.N)?\)$`
pub(crate) fn native_user_agent(user_agent: &str) -> bool {
    let Some(rest) = strip_prefix_ci(user_agent, "claude-cli/") else {
        return false;
    };
    let semver = |s: &str| {
        let p: Vec<_> = s.split('.').collect();
        p.len() == 3 && p.iter().all(|x| !x.is_empty() && x.bytes().all(|b| b.is_ascii_digit()))
    };
    let Some(ws) = rest.find(char::is_whitespace) else {
        return false;
    };
    if !semver(&rest[..ws]) {
        return false;
    }
    let Some(rest) = strip_prefix_ci(rest[ws..].trim_start(), "(external,") else {
        return false;
    };
    let Some(inner) = rest.strip_suffix(')') else {
        return false;
    };
    let inner = inner.trim_start();
    let (entry, sdk) = match inner.find([',', ')']) {
        Some(i) if inner.as_bytes()[i] == b',' => (&inner[..i], Some(inner[i + 1..].trim_start())),
        Some(_) => return false,
        None => (inner, None),
    };
    if entry.is_empty() {
        return false;
    }
    match sdk {
        None => true,
        Some(sdk) => strip_prefix_ci(sdk, "agent-sdk/").is_some_and(semver),
    }
}

fn has_claude_code_beta(headers: &HeaderMap) -> bool {
    headers
        .get_all("anthropic-beta")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|b| b.trim() == "claude-code-20250219")
}

/// `isValidUserID`: Claude Code's JSON metadata.user_id.
pub(crate) fn valid_user_id(user_id: &str) -> bool {
    let Ok(serde_json::Value::Object(v)) = serde_json::from_str::<serde_json::Value>(user_id) else {
        return false;
    };
    let field = |k: &str| match v.get(k) {
        None | Some(serde_json::Value::Null) => Some(""),
        Some(serde_json::Value::String(s)) => Some(s.as_str()),
        _ => None,
    };
    let (Some(device), Some(session), Some(account)) = (field("device_id"), field("session_id"), field("account_uuid"))
    else {
        return false;
    };
    device.len() == 64
        && device.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && uuid::Uuid::parse_str(session).is_ok()
        && (account.is_empty() || uuid::Uuid::parse_str(account).is_ok())
}

pub(crate) fn detect(headers: &HeaderMap, payload: &str, count_tokens: bool, settings: &Settings) -> Detection {
    let user_agent = raw_header(headers, "user-agent");
    let (entrypoint, _) = user_agent_details(user_agent);
    let x_app = raw_header(headers, "x-app") == "cli";
    let ua = profile::plausible_user_agent(user_agent, settings);
    let betas = has_claude_code_beta(headers);
    let user_id = rawjson::get(payload, "metadata.user_id");
    let metadata_user_id = user_id.kind() == gjson::Kind::String && valid_user_id(user_id.str());
    let native = NATIVE_ENTRYPOINTS.contains(&entrypoint.as_str());
    let standard = x_app && ua && betas && (count_tokens || metadata_user_id);
    let helper = native
        && !count_tokens
        && entrypoint == "cli"
        && !betas
        && x_app
        && ua
        && metadata_user_id
        && helper_profile(headers, payload, settings);
    Detection {
        confirmed: (standard || helper) && native,
        helper_profile: helper,
        entrypoint,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    Minimal,
    Structured,
    Title280,
}

fn helper_betas(redact: bool, trailing: &[&str]) -> String {
    let mut b = vec!["oauth-2025-04-20", "interleaved-thinking-2025-05-14"];
    if redact {
        b.push("redact-thinking-2026-02-12");
    }
    b.extend([
        "thinking-token-count-2026-05-13",
        "context-management-2025-06-27",
        "prompt-caching-scope-2026-01-05",
    ]);
    b.extend(trailing);
    b.join(",")
}

fn measured_beta_shape(betas: &str) -> Option<Shape> {
    let so = "structured-outputs-2025-12-15";
    let profiles = [
        (helper_betas(true, &[]), Shape::Minimal),
        (helper_betas(false, &[]), Shape::Minimal),
        (
            helper_betas(true, &["advisor-tool-2026-03-01", so, "cache-diagnosis-2026-04-07"]),
            Shape::Structured,
        ),
        (
            helper_betas(true, &[so, "fallback-credit-2026-06-01"]),
            Shape::Structured,
        ),
        (helper_betas(true, &[so]), Shape::Structured),
        (helper_betas(false, &[so]), Shape::Structured),
        (
            helper_betas(
                true,
                &[
                    so,
                    "server-side-fallback-2026-06-01",
                    "fallback-credit-2026-06-01",
                    "cache-diagnosis-2026-04-07",
                ],
            ),
            Shape::Title280,
        ),
    ];
    profiles.into_iter().find(|(p, _)| p == betas).map(|(_, s)| s)
}

fn joined_betas(headers: &HeaderMap) -> String {
    headers
        .get_all("anthropic-beta")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .collect::<Vec<_>>()
        .join(",")
}

fn helper_profile(headers: &HeaderMap, payload: &str, settings: &Settings) -> bool {
    let Some(shape) = measured_beta_shape(&joined_betas(headers)) else {
        return false;
    };
    body_shape(payload) == Some(shape) && helper_headers(headers, settings) && helper_session(headers, payload)
}

fn helper_headers(headers: &HeaderMap, settings: &Settings) -> bool {
    let expected = [
        ("accept", "application/json"),
        ("content-type", "application/json"),
        ("x-stainless-lang", "js"),
        ("x-stainless-runtime", "node"),
        ("x-stainless-retry-count", "0"),
        ("x-stainless-timeout", "600"),
        ("anthropic-version", "2023-06-01"),
        ("anthropic-dangerous-direct-browser-access", "true"),
    ];
    if expected.iter().any(|(n, v)| raw_header(headers, n) != *v) {
        return false;
    }
    if [
        "x-stainless-package-version",
        "x-stainless-runtime-version",
        "x-stainless-os",
        "x-stainless-arch",
    ]
    .iter()
    .any(|n| raw_header(headers, n).is_empty())
    {
        return false;
    }
    let baseline = Profile::default_for(settings);
    let candidate = (
        profile::version(raw_header(headers, "user-agent")),
        raw_header(headers, "x-stainless-package-version"),
        raw_header(headers, "x-stainless-runtime-version"),
    );
    if candidate.0.is_none()
        || candidate.0 != profile::version(&baseline.user_agent)
        || candidate.1 != baseline.package_version
        || candidate.2 != baseline.runtime_version
    {
        return false;
    }
    if !raw_header(headers, "x-stainless-async").is_empty()
        || raw_header(headers, "accept-encoding") != "gzip, deflate, br, zstd"
    {
        return false;
    }
    let request_id = raw_header(headers, "x-client-request-id");
    request_id.is_empty() || uuid::Uuid::parse_str(request_id).is_ok()
}

fn helper_session(headers: &HeaderMap, payload: &str) -> bool {
    let metadata = rawjson::get(payload, "metadata");
    if metadata.kind() != gjson::Kind::Object || !keys_exactly(metadata.json(), &["user_id"]) {
        return false;
    }
    let user_id = metadata.get("user_id");
    if user_id.kind() != gjson::Kind::String || !valid_user_id(user_id.str()) {
        return false;
    }
    let identity = user_id.str();
    if !keys_exactly(identity, &["device_id", "account_uuid", "session_id"])
        && !keys_exactly(
            identity,
            &["device_id", "account_uuid", "session_id", "parent_session_id"],
        )
    {
        return false;
    }
    raw_header(headers, "x-claude-code-session-id") == gjson::get(identity, "session_id").str()
}

/// `claudeJSONObjectHasKeys`: valid JSON object whose keys are exactly `want`, in order.
pub(crate) fn keys_exactly(raw: &str, want: &[&str]) -> bool {
    if !gjson::valid(raw) || gjson::parse(raw).kind() != gjson::Kind::Object {
        return false;
    }
    let mut keys = Vec::new();
    gjson::parse(raw).each(|k, _| {
        keys.push(k.str().to_owned());
        true
    });
    keys.len() == want.len() && keys.iter().zip(want).all(|(a, b)| a == b)
}

fn body_shape(payload: &str) -> Option<Shape> {
    let minimal = ["model", "max_tokens", "messages", "metadata"];
    let structured = [
        "model",
        "messages",
        "system",
        "tools",
        "metadata",
        "max_tokens",
        "thinking",
        "temperature",
        "output_config",
        "stream",
    ];
    let title = ["model", "max_tokens", "messages", "metadata", "output_config"];
    let shape = if keys_exactly(payload, &minimal) {
        Shape::Minimal
    } else if keys_exactly(payload, &structured) {
        Shape::Structured
    } else if keys_exactly(payload, &title) {
        Shape::Title280
    } else {
        return None;
    };
    let get = |p: &'static str| rawjson::get(payload, p);
    let max_tokens = get("max_tokens");
    if get("model").str() != HELPER_MODEL || max_tokens.kind() != gjson::Kind::Number {
        return None;
    }
    let messages = get("messages");
    if messages.kind() != gjson::Kind::Array || messages.array().len() != 1 {
        return None;
    }
    let message = messages.get("0");
    if !keys_exactly(message.json(), &["role", "content"]) || message.get("role").str() != "user" {
        return None;
    }
    let content = message.get("content");
    let schema_object = |oc: &gjson::Value| {
        let format = oc.get("format");
        keys_exactly(oc.json(), &["format"])
            && keys_exactly(format.json(), &["type", "schema"])
            && format.get("type").str() == "json_schema"
    };
    match shape {
        Shape::Minimal => (max_tokens.json() == "1" && content.kind() == gjson::Kind::String).then_some(shape),
        Shape::Title280 => {
            let oc = get("output_config");
            let schema = oc.get("format.schema");
            (max_tokens.json() == "80"
                && content.kind() == gjson::Kind::String
                && schema_object(&oc)
                && keys_exactly(schema.json(), &["type"])
                && schema.get("type").str() == "object")
                .then_some(shape)
        }
        Shape::Structured => {
            let blocks = content.array();
            if content.kind() != gjson::Kind::Array || blocks.len() != 1 {
                return None;
            }
            if !keys_exactly(blocks[0].json(), &["type", "text"]) || blocks[0].get("type").str() != "text" {
                return None;
            }
            let system = get("system");
            let sys = system.array();
            if system.kind() != gjson::Kind::Array
                || sys.len() != 3
                || sys
                    .iter()
                    .any(|b| !keys_exactly(b.json(), &["type", "text"]) || b.get("type").str() != "text")
            {
                return None;
            }
            let billing = sys[0].get("text");
            if !billing.str().starts_with("x-anthropic-billing-header:")
                || !billing_has_cch(billing.str())
                || !sys[1].get("text").str().starts_with("You are Claude Code")
            {
                return None;
            }
            let tools = get("tools");
            if tools.kind() != gjson::Kind::Array || !tools.array().is_empty() {
                return None;
            }
            let thinking = get("thinking");
            if !keys_exactly(thinking.json(), &["type"]) || thinking.get("type").str() != "disabled" {
                return None;
            }
            let oc = get("output_config");
            let schema = oc.get("format.schema");
            let props = schema.get("properties");
            let title = props.get("title");
            let required = schema.get("required");
            let ok = schema_object(&oc)
                && keys_exactly(
                    schema.json(),
                    &["type", "properties", "required", "additionalProperties"],
                )
                && schema.get("type").str() == "object"
                && keys_exactly(props.json(), &["title"])
                && keys_exactly(title.json(), &["type"])
                && title.get("type").str() == "string"
                && required.kind() == gjson::Kind::Array
                && required.array().len() == 1
                && required.get("0").str() == "title"
                && schema.get("additionalProperties").kind() == gjson::Kind::False
                && max_tokens.json() == "32000"
                && get("temperature").json() == "1"
                && get("stream").kind() == gjson::Kind::True;
            ok.then_some(shape)
        }
    }
}

/// Five lowercase hex digits followed by `;` after ` cch=`.
pub(crate) fn billing_has_cch(billing: &str) -> bool {
    let Some(marker) = billing.find(" cch=") else {
        return false;
    };
    let start = marker + 5;
    let end = start + 5;
    end < billing.len()
        && billing.as_bytes()[end] == b';'
        && billing.as_bytes()[start..end]
            .iter()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
}
