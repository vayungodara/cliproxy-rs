//! Session identity for the Claude wire: the agent-conversation UUID written to
//! metadata.user_id and X-Claude-Code-Session-Id, per-key cached session IDs, and the
//! billing/diagnostics continuity store.
//!
//! Sources: sdk/cliproxy/session (ExtractSessionInfo, DeriveID, Enrich),
//! sdk/cliproxy/auth/selector.go (ExtractSessionID, message hash),
//! helps/claude_credential_identity.go, helps/session_id_cache.go and
//! helps/claude_diagnostics.go.
//!
//! ponytail: only the session *ID* of ExtractSessionInfo is ported (parent/agent
//! names feed scheduler trees, not this wire). DeriveID covers Messages-shaped
//! bodies (Claude and OpenAI Chat); Responses/Gemini/Interactions roots and LCP
//! identities fall through to the message hash. Home KV mode is
//! not ported; all caches are process-local like Go's non-Home mode.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use cpa_core::format::Format;
use http::HeaderMap;
use serde_json::Value as Json;
use sha2::{Digest, Sha256};

use crate::rawjson;

/// `NormalizeExplicitID`.
pub(crate) fn normalize(raw: &str) -> String {
    if raw.chars().any(char::is_control) {
        return String::new();
    }
    let raw = raw.trim();
    if raw.is_empty() || raw.len() > 256 {
        return String::new();
    }
    raw.to_owned()
}

/// `BoundSessionIdentity`.
fn bound(id: String) -> String {
    if id.len() <= 256 {
        return id;
    }
    let hash = hex(&Sha256::digest(id.as_bytes()));
    let mut end = (255 - 1 - hash.len()).min(id.len());
    while !id.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}#{hash}", &id[..end])
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `sessionHeaderValue`: first value that normalizes to a non-empty ID.
fn header(headers: &HeaderMap, name: &str) -> String {
    headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .map(normalize)
        .find(|v| !v.is_empty())
        .unwrap_or_default()
}

/// A JSON body with Go's `request` envelope view (`hasNestedReq`).
struct Body<'a> {
    json: &'a str,
    nested: Option<String>,
}

impl<'a> Body<'a> {
    fn new(json: &'a str) -> Self {
        let req = rawjson::get(json, "request");
        let nested = (req.exists() && !rawjson::get(json, "contents").exists()).then(|| req.json().to_owned());
        Self { json, nested }
    }
    /// First normalized value of `path` at the root, then in `request`.
    fn id(&self, path: &str) -> String {
        let v = normalize(gjson::get(self.json, path).str());
        if !v.is_empty() {
            return v;
        }
        self.nested
            .as_deref()
            .map(|n| normalize(gjson::get(n, path).str()))
            .unwrap_or_default()
    }
    fn first(&self, paths: &[&str]) -> String {
        paths
            .iter()
            .map(|p| self.id(p))
            .find(|v| !v.is_empty())
            .unwrap_or_default()
    }
}

/// `ClaudeMetadataIdentities`: (session, agent) from Claude Code's metadata.user_id.
fn claude_metadata_identity(json: &str) -> (String, String) {
    let body = Body::new(json);
    let mut user_id = gjson::get(json, "metadata.user_id").str().trim().to_owned();
    if user_id.is_empty()
        && let Some(n) = &body.nested
    {
        user_id = gjson::get(n, "metadata.user_id").str().trim().to_owned();
    }
    if user_id.is_empty() {
        return Default::default();
    }
    if user_id.starts_with('{') {
        let p = |k: &str| normalize(gjson::get(&user_id, k).str());
        let agent = Some(p("agent_id"))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| p("subagent_id"));
        return (p("session_id"), agent);
    }
    // Legacy `..._session_<hex/dashes>` suffix.
    if let Some(i) = user_id.rfind("_session_") {
        let sid = &user_id[i + 9..];
        if !sid.is_empty()
            && sid
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase() || b == b'-')
        {
            let agent = body.first(&["metadata.agent_id", "metadata.subagent_id"]);
            return (normalize(sid), agent);
        }
    }
    Default::default()
}

/// `ExtractSessionInfo(...).SessionID`, bounded.
fn explicit_session(headers: &HeaderMap, payload: &str) -> String {
    let body = (!payload.is_empty()).then(|| Body::new(payload));
    let body_agent = || {
        body.as_ref()
            .map(|b| b.first(&["metadata.agent_id", "metadata.subagent_id"]))
            .unwrap_or_default()
    };
    let claude = |sid: &str, agent: String| {
        if !agent.is_empty() && agent != "main" {
            format!("claude:{sid}:agent:{agent}")
        } else {
            format!("claude:{sid}")
        }
    };
    let sid = header(headers, "x-claude-code-session-id");
    if !sid.is_empty() {
        let mut agent = header(headers, "x-claude-code-agent-id");
        if agent.is_empty() {
            agent = body_agent();
        }
        if agent.is_empty() {
            agent = claude_metadata_identity(payload).1;
        }
        return bound(claude(&sid, agent));
    }
    if !payload.is_empty() {
        let (sid, mut agent) = claude_metadata_identity(payload);
        if !sid.is_empty() {
            if agent.is_empty() {
                agent = header(headers, "x-claude-code-agent-id");
            }
            if agent.is_empty() {
                agent = body_agent();
            }
            return bound(claude(&sid, agent));
        }
    }
    if let Some(id) = codex_session(headers, body.as_ref()) {
        return bound(id);
    }
    for (name, prefix) in [
        ("x-http-session-id", "agy:"),
        ("x-session-id", "header:"),
        ("x-session-affinity", "affinity:"),
        ("x-slot-session-id", "slot:"),
        ("x-task-id", "task:"),
        ("x-task_id", "task:"),
        ("x-conversation-id", "conv:"),
        ("x-thread-id", "thread:"),
        ("x-client-request-id", "clientreq:"),
    ] {
        let v = header(headers, name);
        if !v.is_empty() {
            return bound(format!("{prefix}{v}"));
        }
    }
    let Some(body) = body else {
        return String::new();
    };
    let found = body.first(&["cachedContent", "cached_content"]);
    if !found.is_empty() {
        return bound(format!("geminicache:{found}"));
    }
    let found = body.first(&["thread_id", "threadId", "metadata.thread_id"]);
    if !found.is_empty() {
        return bound(format!("thread:{found}"));
    }
    let sid = body.first(&[
        "session_id",
        "sessionId",
        "sessionID",
        "child_session_id",
        "childSessionId",
        "metadata.session_id",
        "metadata.sessionId",
        "metadata.sessionID",
        "metadata.child_session_id",
        "extra_body.session_id",
        "extra_body.sessionId",
        "extra_body.sessionID",
    ]);
    if !sid.is_empty() {
        let mut agent = body.first(&["metadata.agent_id", "metadata.subagent_id"]);
        if agent.is_empty() {
            agent = header(headers, "x-claude-code-agent-id");
        }
        if agent.is_empty() {
            agent = header(headers, "x-agent-id");
        }
        return bound(if !agent.is_empty() && agent != "main" {
            format!("session:{sid}:agent:{agent}")
        } else {
            format!("session:{sid}")
        });
    }
    let task = body.first(&[
        "task_id",
        "taskId",
        "taskID",
        "action_id",
        "actionId",
        "actionID",
        "metadata.task_id",
        "metadata.taskId",
        "metadata.taskID",
        "metadata.action_id",
        "metadata.actionId",
        "metadata.actionID",
        "extra_body.task_id",
        "extra_body.taskId",
        "extra_body.taskID",
    ]);
    if !task.is_empty() {
        return bound(format!("task:{task}"));
    }
    let pck = body.first(&["prompt_cache_key", "promptCacheKey"]);
    if !pck.is_empty() {
        return bound(format!("pck:{pck}"));
    }
    let conversation = conversation_id(&body);
    if !conversation.is_empty() {
        return bound(conversation);
    }
    let user = body.id("metadata.user_id");
    if !user.is_empty() {
        return bound(format!("user:{user}"));
    }
    let cid = body.first(&[
        "conversation_id",
        "conversationId",
        "chat_id",
        "chatId",
        "metadata.conversation_id",
        "extra_body.conversation_id",
    ]);
    if !cid.is_empty() {
        return bound(format!("conv:{cid}"));
    }
    String::new()
}

fn conversation_id(body: &Body<'_>) -> String {
    let mut conversation = gjson::get(body.json, "conversation").json().to_owned();
    if conversation.is_empty()
        && let Some(n) = &body.nested
    {
        conversation = gjson::get(n, "conversation").json().to_owned();
    }
    let parsed = gjson::parse(&conversation);
    let id = normalize(parsed.get("id").str());
    if !id.is_empty() {
        return format!("conv:{id}");
    }
    if parsed.kind() == gjson::Kind::String {
        let id = normalize(parsed.str());
        if !id.is_empty() {
            return format!("conv:{id}");
        }
    }
    String::new()
}

fn codex_session(headers: &HeaderMap, body: Option<&Body<'_>>) -> Option<String> {
    let first = |names: &[&str]| {
        names
            .iter()
            .map(|n| header(headers, n))
            .find(|v| !v.is_empty())
            .unwrap_or_default()
    };
    let mut sid = first(&["session-id", "session_id"]);
    let mut tid = first(&["thread-id", "thread_id"]);
    let meta = headers
        .get("x-codex-turn-metadata")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .unwrap_or_default();
    let meta_get = |k: &str| {
        if meta.is_empty() {
            String::new()
        } else {
            normalize(gjson::get(meta, k).str())
        }
    };
    if sid.is_empty() {
        sid = meta_get("session_id");
    }
    if tid.is_empty() {
        tid = meta_get("thread_id");
    }
    if tid.is_empty()
        && !sid.is_empty()
        && let Some(b) = body
    {
        tid = b.first(&["thread_id", "threadId", "metadata.thread_id"]);
    }
    if sid.is_empty() && tid.is_empty() {
        return None;
    }
    let mut parent = first(&["x-codex-parent-thread-id"]);
    if parent.is_empty() {
        parent = meta_get("parent_thread_id");
    }
    let mut forked = meta_get("forked_from_thread_id");
    if forked.is_empty() {
        forked = meta_get("forked_from_id");
    }
    if forked.is_empty()
        && let Some(b) = body
    {
        forked = b.first(&[
            "forked_from_thread_id",
            "forked_from_id",
            "metadata.forked_from_thread_id",
            "metadata.forked_from_id",
            "extra_body.forked_from_thread_id",
            "extra_body.forked_from_id",
        ]);
    }
    let clean = {
        let name = if meta.is_empty() {
            String::new()
        } else {
            gjson::get(meta, "agent_name").str().to_owned()
        };
        let raw = name.as_str();
        let raw = raw.strip_prefix("/root/").unwrap_or(raw);
        let raw = normalize(raw.strip_prefix('/').unwrap_or(raw).trim());
        if raw == "root" || raw == "main" {
            String::new()
        } else {
            raw
        }
    };
    let sub = header(headers, "x-openai-subagent");
    let subagent = (!sub.is_empty() && !sub.eq_ignore_ascii_case("false") && sub != "0")
        || (!meta.is_empty() && gjson::get(meta, "subagent_kind").str() == "thread_spawn");
    let tid_or_sid = if tid.is_empty() { sid.clone() } else { tid.clone() };
    if !forked.is_empty() {
        let mut fork = tid_or_sid;
        if fork == forked && !sid.is_empty() && sid != forked {
            fork = sid;
        }
        return Some(format!("codex:{fork}"));
    }
    if subagent
        || (!tid.is_empty() && !sid.is_empty() && tid != sid)
        || (!parent.is_empty() && parent != tid && parent != sid)
    {
        if !clean.is_empty() && !sid.is_empty() {
            return Some(format!("codex:{sid}:agent:{clean}"));
        }
        return Some(format!("codex:{tid_or_sid}"));
    }
    Some(format!("codex:{}", if sid.is_empty() { &tid } else { &sid }))
}

/// `hasExplicitSession` (session.Enrich): any explicit identity suppresses derivation.
pub(crate) fn has_explicit_session(headers: &HeaderMap, payload: &str) -> bool {
    const HEADERS: &[&str] = &[
        "x-claude-code-session-id",
        "x-claude-code-agent-id",
        "x-claude-code-parent-agent-id",
        "session-id",
        "session_id",
        "x-codex-parent-thread-id",
        "x-codex-turn-metadata",
        "x-openai-subagent",
        "x-http-session-id",
        "x-session-id",
        "x-session-affinity",
        "x-parent-session-id",
        "x-parent-session-affinity",
        "x-parent-id",
        "x-slot-session-id",
        "x-parent-slot-session-id",
        "x-task-id",
        "x-parent-task-id",
        "x-conversation-id",
        "x-parent-conversation-id",
        "x-thread-id",
        "x-parent-thread-id",
        "thread-id",
        "x-client-request-id",
    ];
    if HEADERS.iter().any(|h| !header(headers, h).is_empty()) {
        return true;
    }
    if payload.is_empty() {
        return false;
    }
    let body = Body::new(payload);
    const PATHS: &[&str] = &[
        "session_id",
        "sessionId",
        "sessionID",
        "child_session_id",
        "childSessionId",
        "task_id",
        "taskId",
        "taskID",
        "action_id",
        "actionId",
        "cachedContent",
        "cached_content",
        "thread_id",
        "threadId",
        "conversation_id",
        "conversationId",
        "chat_id",
        "chatId",
        "prompt_cache_key",
        "promptCacheKey",
        "parent_session_id",
        "parentSessionId",
        "parent_thread_id",
        "parentThreadId",
        "parent_id",
        "parentId",
        "parentID",
        "parent_task_id",
        "parentTaskId",
        "parent_action_id",
        "parentActionId",
        "parent_session",
        "parentSession",
        "parent_subagent_id",
        "forkSource.sessionId",
        "previousSessionId",
        "forked_from_thread_id",
        "forked_from_id",
        "metadata.session_id",
        "metadata.sessionId",
        "metadata.task_id",
        "metadata.taskId",
        "metadata.thread_id",
        "metadata.conversation_id",
        "metadata.parent_id",
        "metadata.parent_task_id",
        "metadata.parent_agent_id",
        "extra_body.session_id",
        "extra_body.task_id",
        "extra_body.parent_id",
        "extra_body.parent_task_id",
    ];
    if PATHS.iter().any(|p| !body.id(p).is_empty()) {
        return true;
    }
    if !claude_metadata_identity(payload).0.is_empty() || !body.id("metadata.user_id").is_empty() {
        return true;
    }
    !conversation_id(&body).is_empty()
}

/// `session.CallerScope`.
pub(crate) fn caller_scope(client_key: &str) -> String {
    let key = client_key.trim();
    if key.is_empty() {
        return String::new();
    }
    hex(&Sha256::digest(
        format!("cli-proxy-api:caller-scope:v1\0{key}").as_bytes(),
    ))
}

/// `session.DeriveID` for Messages-shaped bodies: `ctx:v1:` + sha256 of the canonical
/// root (instructions and the first user turn), stable across later turns.
pub(crate) fn derive_id(format: Format, payload: &str, caller_scope: &str) -> String {
    if !matches!(format, Format::Claude | Format::OpenAI) {
        return String::new();
    }
    let Ok(Json::Object(body)) = serde_json::from_str::<Json>(payload) else {
        return String::new();
    };
    let mut instructions = Vec::new();
    if format == Format::Claude
        && let Some(system) = body.get("system")
    {
        instruction(&mut instructions, system);
    }
    let mut user = Vec::new();
    for message in body.get("messages").and_then(Json::as_array).into_iter().flatten() {
        let Some(message) = message.as_object() else { continue };
        let role = message
            .get("role")
            .and_then(Json::as_str)
            .unwrap_or_default()
            .trim()
            .to_lowercase();
        let content = message.get("content").unwrap_or(&Json::Null);
        match role.as_str() {
            "system" | "developer" => instruction(&mut instructions, content),
            "user" => {
                parts(&mut user, content);
                if !user.is_empty() {
                    break;
                }
            }
            _ => {}
        }
    }
    if user.is_empty() {
        return String::new();
    }
    let mut root = format!(
        r#"{{"version":"cpa-session-root-v1","format":{},"caller_scope":{}"#,
        rawjson::go_string(format.as_str()),
        rawjson::go_string(caller_scope.trim())
    );
    if !instructions.is_empty() {
        let items: Vec<_> = instructions.iter().map(|s| rawjson::go_string(s)).collect();
        root.push_str(&format!(r#","instructions":[{}]"#, items.join(",")));
    }
    let items: Vec<_> = user
        .iter()
        .map(|(kind, mime, value)| {
            let mime = if mime.is_empty() {
                String::new()
            } else {
                format!(r#","mime":{}"#, rawjson::go_string(mime))
            };
            format!(
                r#"{{"kind":{}{mime},"value":{}}}"#,
                rawjson::go_string(kind),
                rawjson::go_string(value)
            )
        })
        .collect();
    root.push_str(&format!(r#","user":[{}]}}"#, items.join(",")));
    format!("ctx:v1:{}", hex(&Sha256::digest(root.as_bytes())))
}

type Part = (String, String, String);

fn instruction(out: &mut Vec<String>, value: &Json) {
    let mut collected = Vec::new();
    parts(&mut collected, value);
    let text: Vec<_> = collected
        .into_iter()
        .filter(|(k, _, v)| k == "text" && !v.is_empty())
        .map(|(_, _, v)| v)
        .collect();
    if !text.is_empty() {
        out.push(text.join("\n").chars().take(50).collect());
    }
}

fn parts(out: &mut Vec<Part>, value: &Json) {
    match value {
        Json::Null => {}
        Json::String(s) => {
            if !s.is_empty() {
                out.push(("text".into(), String::new(), s.clone()));
            }
        }
        Json::Array(items) => items.iter().for_each(|v| parts(out, v)),
        Json::Object(o) => {
            if let Some(Json::String(text)) = o.get("text") {
                return parts(out, &Json::String(text.clone()));
            }
            for key in ["content", "parts"] {
                if let Some(nested) = o.get(key) {
                    return parts(out, nested);
                }
            }
            let media = |out: &mut Vec<Part>, kind: &str, v: &Json, fallback: &str| {
                let kind = if kind.trim().is_empty() { "media" } else { kind.trim() };
                match v {
                    Json::String(s) if !s.is_empty() => out.push((kind.into(), fallback.into(), s.clone())),
                    Json::String(_) => {}
                    Json::Object(m) => {
                        let field = |keys: &[&str]| {
                            keys.iter()
                                .find_map(|k| m.get(*k))
                                .and_then(Json::as_str)
                                .unwrap_or_default()
                                .trim()
                                .to_owned()
                        };
                        let mut mime = field(&["mimeType", "mime_type", "media_type"]);
                        if mime.is_empty() {
                            mime = fallback.into();
                        }
                        let value = field(&["url", "uri", "fileUri", "file_uri", "data"]);
                        if !value.is_empty() {
                            out.push((kind.into(), mime, value));
                        }
                    }
                    other => parts(out, other),
                }
            };
            if let Some(v) = o.get("image_url") {
                return media(out, "image", v, "");
            }
            if let Some(v) = o.get("inlineData").or_else(|| o.get("inline_data")) {
                return media(out, "inline_data", v, "");
            }
            if let Some(v) = o.get("fileData").or_else(|| o.get("file_data")) {
                return media(out, "file", v, "");
            }
            if let Some(v) = o.get("source") {
                let lower = |k: &str| {
                    o.get(k)
                        .and_then(Json::as_str)
                        .unwrap_or_default()
                        .trim()
                        .to_lowercase()
                };
                return media(out, &lower("type"), v, &lower("media_type"));
            }
            out.push(("json".into(), String::new(), go_marshal(value, true)));
        }
        other => out.push(("json".into(), String::new(), go_marshal(other, false))),
    }
}

/// Go `json.Marshal` of decoded `any`: sorted keys, HTML-escaped strings, float64
/// numbers; `cache_control` dropped when `normalize` (session normalizeJSONValue).
fn go_marshal(value: &Json, strip_cache: bool) -> String {
    match value {
        Json::Null => "null".into(),
        Json::Bool(b) => b.to_string(),
        Json::Number(n) => go_float(n.as_f64().unwrap_or(0.0)),
        Json::String(s) => rawjson::go_string(s),
        Json::Array(a) => format!(
            "[{}]",
            a.iter()
                .map(|v| go_marshal(v, strip_cache))
                .collect::<Vec<_>>()
                .join(",")
        ),
        Json::Object(o) => {
            let mut keys: Vec<_> = o
                .keys()
                .filter(|k| !(strip_cache && k.trim().eq_ignore_ascii_case("cache_control")))
                .collect();
            keys.sort();
            let members: Vec<_> = keys
                .iter()
                .map(|k| format!("{}:{}", rawjson::go_string(k), go_marshal(&o[*k], strip_cache)))
                .collect();
            format!("{{{}}}", members.join(","))
        }
    }
}

/// Go's float64 JSON encoding (`strconv.AppendFloat` 'f' or 'e' with exponent cleanup).
fn go_float(f: f64) -> String {
    let abs = f.abs();
    if abs != 0.0 && !(1e-6..1e21).contains(&abs) {
        let s = format!("{f:e}");
        // Rust: 1e21, 1.5e-7; Go: 1e+21, 1.5e-07.
        let (mantissa, exp) = s.split_once('e').unwrap_or((&s, "0"));
        let (sign, digits) = exp.strip_prefix('-').map_or(("+", exp), |d| ("-", d));
        let digits = if digits.len() < 2 {
            format!("0{digits}")
        } else {
            digits.to_owned()
        };
        return format!("{mantissa}e{sign}{digits}");
    }
    let s = format!("{f}");
    s.strip_suffix(".0").map(str::to_owned).unwrap_or(s)
}

/// Message-hash fallback (`extractMessageHashIDs`), primary ID only.
fn message_hash(payload: &str) -> String {
    let truncate = |s: &str| -> String {
        if s.len() > 100 {
            // Go slices bytes; keep that, including a possibly split rune.
            String::from_utf8_lossy(&s.as_bytes()[..100]).into_owned()
        } else {
            s.to_owned()
        }
    };
    let content_text = |content: &gjson::Value| -> String {
        if content.kind() == gjson::Kind::String {
            return content.str().to_owned();
        }
        let mut texts = Vec::new();
        if content.kind() == gjson::Kind::Array {
            for part in content.array() {
                if part.get("type").str() == "text" && !part.get("text").str().is_empty() {
                    texts.push(part.get("text").str().to_owned());
                }
            }
        }
        texts.join(" ")
    };
    let (mut system, mut user, mut assistant) = (String::new(), String::new(), String::new());
    let messages = rawjson::get(payload, "messages");
    if messages.kind() == gjson::Kind::Array {
        for msg in messages.array() {
            let content = content_text(&msg.get("content"));
            if content.is_empty() {
                continue;
            }
            let slot = match msg.get("role").str() {
                "system" => &mut system,
                "user" => &mut user,
                "assistant" => &mut assistant,
                _ => continue,
            };
            if slot.is_empty() {
                *slot = truncate(&content);
            }
            if !system.is_empty() && !user.is_empty() && !assistant.is_empty() {
                break;
            }
        }
    }
    if system.is_empty() {
        let top = rawjson::get(payload, "system");
        if top.kind() == gjson::Kind::Array {
            if let Some(t) = top
                .array()
                .iter()
                .map(|p| p.get("text").str().to_owned())
                .find(|t| !t.is_empty())
            {
                system = truncate(&t);
            }
        } else if top.kind() == gjson::Kind::String {
            system = truncate(top.str());
        }
    }
    // ponytail: Gemini `contents` and Responses `input` roots are not hashed here;
    // the Claude executor receives Messages-shaped originals for its own formats.
    if user.is_empty() {
        return String::new();
    }
    let hash = |assistant: &str| {
        let mut h: u64 = 0xcbf29ce484222325;
        let mut feed = |s: String| {
            for b in s.bytes() {
                h ^= u64::from(b);
                h = h.wrapping_mul(0x100000001b3);
            }
        };
        if !system.is_empty() {
            feed(format!("sys:{system}\n"));
        }
        feed(format!("usr:{user}\n"));
        if !assistant.is_empty() {
            feed(format!("ast:{assistant}\n"));
        }
        format!("msg:{h:016x}")
    };
    hash(&assistant)
}

/// Request facts that select the agent-conversation identity.
pub(crate) struct Inputs<'a> {
    pub headers: &'a HeaderMap,
    pub original: &'a str,
    pub translated: &'a str,
    /// `derived:` identity from `session.Enrich` when the caller sent none.
    pub derived: &'a str,
    /// Execution-session metadata (`ExecutionSessionMetadataKey`), normalized.
    pub execution: &'a str,
}

/// `ClaudeAgentSessionUUIDForRequest`: unconfirmed callers cannot choose the
/// identity through the session header or metadata.user_id.
pub(crate) fn agent_session_uuid(inputs: &Inputs<'_>, confirmed: bool) -> String {
    let mut headers = inputs.headers.clone();
    let (mut original, mut translated) = (inputs.original.to_owned(), inputs.translated.to_owned());
    if !confirmed {
        headers.remove("x-claude-code-session-id");
        original = rawjson::delete(&original, "metadata.user_id");
        translated = rawjson::delete(&translated, "metadata.user_id");
    }
    let extract = |payload: &str| {
        let explicit = explicit_session(&headers, payload);
        if !explicit.is_empty() {
            return explicit;
        }
        if !inputs.execution.is_empty() {
            return bound(format!("execution:{}", inputs.execution));
        }
        let derived = normalize(inputs.derived);
        if !derived.is_empty() {
            return format!("derived:{derived}");
        }
        if payload.is_empty() {
            String::new()
        } else {
            message_hash(payload)
        }
    };
    let mut identity = extract(&original);
    if identity.is_empty() && !translated.is_empty() {
        identity = extract(&translated);
    }
    if identity.is_empty() {
        return new_v4();
    }
    if let Some(u) = identity
        .strip_prefix("claude:")
        .and_then(|s| uuid::Uuid::parse_str(s).ok())
    {
        return u.to_string();
    }
    if let Ok(u) = uuid::Uuid::parse_str(&identity) {
        return u.to_string();
    }
    uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_OID,
        format!("cli-proxy-api\0claude\0agent-conversation\0{identity}").as_bytes(),
    )
    .to_string()
}

/// A random RFC 4122 v4 UUID.
pub(crate) fn new_v4() -> String {
    let mut bytes = [0u8; 16];
    let _ = getrandom::fill(&mut bytes);
    uuid::Builder::from_random_bytes(bytes).into_uuid().to_string()
}

/// `CanonicalSessionID` as `$CPA-SESSION-ID` resolves it: the caller's explicit
/// identity, else the execution session, else the derived one, else the message hash.
pub(crate) fn canonical(headers: &HeaderMap, original: &str, execution: &str, derived: &str) -> String {
    let explicit = explicit_session(headers, original);
    if !explicit.is_empty() {
        return explicit;
    }
    if !execution.is_empty() {
        return bound(format!("execution:{execution}"));
    }
    let derived = normalize(derived);
    if !derived.is_empty() {
        return bound(format!("derived:{derived}"));
    }
    if original.is_empty() {
        String::new()
    } else {
        bound(message_hash(original))
    }
}

/// Per-key values renewed for an hour on every use (Go's local session/user ID caches).
type KeyedCache = Mutex<HashMap<[u8; 32], (String, Instant)>>;

fn cached(cache: &'static OnceLock<KeyedCache>, api_key: &str, make: impl FnOnce() -> String) -> String {
    let key: [u8; 32] = Sha256::digest(api_key.as_bytes()).into();
    let now = Instant::now();
    let mut cache = cache.get_or_init(KeyedCache::default).lock().expect("id cache");
    cache.retain(|_, (_, until)| *until > now);
    let entry = cache.entry(key).or_insert_with(|| (make(), now));
    entry.1 = now + Duration::from_secs(3600);
    entry.0.clone()
}

/// `CachedSessionIDRequired`: one session UUID per key, renewed for an hour on use.
pub(crate) fn cached_session_id(api_key: &str) -> String {
    static CACHE: OnceLock<KeyedCache> = OnceLock::new();
    if api_key.is_empty() {
        return new_v4();
    }
    cached(&CACHE, api_key, new_v4)
}

/// `CachedUserIDRequired`: one complete fake user ID per key (cloak cache-user-id).
pub(crate) fn cached_user_id(api_key: &str, make: impl FnOnce() -> String) -> String {
    static CACHE: OnceLock<KeyedCache> = OnceLock::new();
    if api_key.is_empty() {
        return make();
    }
    cached(&CACHE, api_key, make)
}

/// `ClaudeDeterministicPromptID`: a v4-shaped UUID from sha256(seed).
pub(crate) fn deterministic_prompt_id(seed: &str) -> String {
    let mut d: [u8; 32] = Sha256::digest(seed.as_bytes()).into();
    d[6] = (d[6] & 0x0f) | 0x40;
    d[8] = (d[8] & 0x3f) | 0x80;
    format!(
        "{}-{}-{}-{}-{}",
        hex(&d[0..4]),
        hex(&d[4..6]),
        hex(&d[6..8]),
        hex(&d[8..10]),
        hex(&d[10..16])
    )
}

pub(crate) fn valid_prompt_id(id: &str) -> bool {
    uuid::Uuid::try_parse(id.trim())
        .is_ok_and(|u| id.trim().len() == 36 && u.get_version_num() == 4 && u.get_variant() == uuid::Variant::RFC4122)
}

pub(crate) fn valid_request_id(id: &str) -> bool {
    id.strip_prefix("req_").is_some_and(|rest| {
        (1..=36).contains(&rest.len())
            && rest
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    })
}

/// Billing/diagnostics continuity for one request (`ClaudeContinuityContext`).
#[derive(Debug, Clone, Default)]
pub(crate) struct Continuity {
    pub key: String,
    pub sequence: u64,
    pub previous_message_id: String,
    pub previous_request_id: String,
    pub prompt_id: String,
    pub initialized: bool,
}

#[derive(Default)]
struct Entry {
    previous_message_id: String,
    previous_request_id: String,
    prompt_id: String,
    minimum_sequence: u64,
    committed_sequence: u64,
    last_access: u64,
    expires: Option<Instant>,
}

#[derive(Default)]
struct Store {
    entries: HashMap<String, Entry>,
    last_cleanup: Option<Instant>,
    next_sequence: u64,
    next_access: u64,
}

const CONTINUITY_TTL: Duration = Duration::from_secs(3600);

fn store() -> &'static Mutex<Store> {
    static STORE: OnceLock<Mutex<Store>> = OnceLock::new();
    STORE.get_or_init(Mutex::default)
}

/// `BeginClaudeContinuity`. Returns (key, sequence, previous message, previous request, prompt).
pub(crate) fn begin(credential: &str, session: &str, new_turn: bool, explicit_prompt: &str) -> Continuity {
    let (credential, session) = (credential.trim(), session.trim());
    if credential.is_empty() || session.is_empty() {
        return Continuity::default();
    }
    let key = hex(&Sha256::digest(format!("{credential}\0{session}").as_bytes()));
    let now = Instant::now();
    let mut s = store().lock().expect("continuity store");
    if s.last_cleanup
        .is_none_or(|t| now.duration_since(t) >= Duration::from_secs(900))
    {
        s.entries.retain(|_, e| e.expires.is_none_or(|t| now <= t));
        s.last_cleanup = Some(now);
    }
    let found = s.entries.contains_key(&key);
    let expired = s.entries.get(&key).is_some_and(|e| e.expires.is_some_and(|t| now > t));
    if !found && s.entries.len() >= 4096 {
        let mut order: Vec<_> = s.entries.iter().map(|(k, e)| (e.last_access, k.clone())).collect();
        order.sort();
        for (_, k) in order.into_iter().take(256) {
            s.entries.remove(&k);
        }
    }
    s.next_sequence += 1;
    let sequence = s.next_sequence;
    s.next_access += 1;
    let access = s.next_access;
    let entry = s.entries.entry(key.clone()).or_default();
    if !found || expired {
        *entry = Entry {
            minimum_sequence: sequence,
            ..Entry::default()
        };
    }
    let explicit = explicit_prompt.trim();
    let prompt = if !explicit.is_empty() && valid_prompt_id(explicit) {
        explicit.to_lowercase()
    } else if new_turn || entry.prompt_id.is_empty() {
        new_v4()
    } else {
        entry.prompt_id.clone()
    };
    entry.last_access = access;
    entry.expires = Some(now + CONTINUITY_TTL);
    Continuity {
        key,
        sequence,
        previous_message_id: entry.previous_message_id.clone(),
        previous_request_id: entry.previous_request_id.clone(),
        prompt_id: prompt,
        initialized: false,
    }
}

/// `CommitClaudeContinuity` after a completed response.
pub(crate) fn commit(key: &str, sequence: u64, message_id: &str, request_id: &str, prompt_id: &str) {
    let (key, message_id, request_id) = (key.trim(), message_id.trim(), request_id.trim());
    if key.is_empty() || sequence == 0 || message_id.is_empty() {
        return;
    }
    let now = Instant::now();
    let mut s = store().lock().expect("continuity store");
    s.next_access += 1;
    let access = s.next_access;
    let Some(entry) = s.entries.get_mut(key) else { return };
    if entry.expires.is_some_and(|t| now > t)
        || sequence < entry.minimum_sequence
        || sequence < entry.committed_sequence
    {
        return;
    }
    entry.previous_message_id = message_id.into();
    entry.previous_request_id = if valid_request_id(request_id) {
        request_id.into()
    } else {
        String::new()
    };
    if valid_prompt_id(prompt_id.trim()) {
        entry.prompt_id = prompt_id.trim().to_lowercase();
    }
    entry.committed_sequence = sequence;
    entry.last_access = access;
    entry.expires = Some(now + CONTINUITY_TTL);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_float_formatting() {
        assert_eq!(go_float(1.0), "1");
        assert_eq!(go_float(0.5), "0.5");
        assert_eq!(go_float(1e21), "1e+21");
        assert_eq!(go_float(1.5e-7), "1.5e-07");
        assert_eq!(go_float(123456789.0), "123456789");
    }

    #[test]
    fn harness_message_hash_identity_matches_go_capture() {
        // From the Go differential capture: unconfirmed caller with an explicit (and
        // therefore stripped) session header falls through to the message hash.
        let body =
            r#"{"model":"claude-sonnet-4-6","max_tokens":17,"messages":[{"role":"user","content":"Local question"}]}"#;
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-claude-code-session-id",
            "11111111-2222-4333-8444-555555555555".parse().unwrap(),
        );
        let inputs = Inputs {
            headers: &headers,
            original: body,
            translated: body,
            derived: "",
            execution: "",
        };
        assert_eq!(
            agent_session_uuid(&inputs, false),
            "dd01238e-cdb5-5572-8f27-a28d98fe9075"
        );
        assert_eq!(
            agent_session_uuid(&inputs, true),
            "11111111-2222-4333-8444-555555555555"
        );
        assert_eq!(
            deterministic_prompt_id("cpa:prompt:Local question"),
            "83ec619f-ab81-4d70-9c67-44c3865291b9"
        );
    }
}
