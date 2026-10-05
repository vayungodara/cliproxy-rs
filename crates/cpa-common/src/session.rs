//! Session identity (sdk/cliproxy/session/info.go and identity.go, plus the affinity
//! fallbacks in sdk/cliproxy/auth/selector.go).
//!
//! [`extract_session_info`] resolves the explicit identity a client sent (Claude Code,
//! Codex, Antigravity, OpenCode, pi, task, conversation and thread headers, then body
//! fields) with its parent, fork and subagent relationships. [`session_ids`] adds Go's
//! fallbacks for affinity: the execution session, the context-derived identity
//! ([`derive_id`]) and the first-messages hash.
//!
//! The server sets `ExecRequest::session` to [`extract_session_id`]'s result, so
//! executors read Go's `ExtractSessionID` from there. A value `derived:<id>` carries
//! Go's `derived_session_id` metadata `<id>`.
//!
//! With session affinity on, a request from an authenticated caller without an
//! explicit session is bound by its conversation prefix first (Go's Merkle LCP
//! matcher, ported in cpa-server's `lcp` module); these fallbacks apply when that
//! matcher has nothing to work with.

use std::collections::BTreeMap;

use cpa_core::format::Format;
use http::HeaderMap;
use sha2::{Digest, Sha256};

use crate::json::{self, GoValue, Kind, Res};

/// Go `SessionInfo`, minus the fields only Home and LCP fill.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionInfo {
    pub session_id: String,
    pub parent_session_id: String,
    pub agent_name: String,
    pub client_type: &'static str,
    pub is_fork: bool,
    pub is_subagent: bool,
}

/// Go `NormalizeExplicitID`: printable, trimmed, at most 256 bytes.
pub fn normalize_explicit_id(raw: &str) -> String {
    if raw.chars().any(char::is_control) {
        return String::new();
    }
    let raw = raw.trim();
    if raw.is_empty() || raw.len() > 256 {
        return String::new();
    }
    raw.to_owned()
}

/// Go `BoundSessionIdentity`: identifiers over 256 bytes become a 190-byte UTF-8 safe
/// prefix, `#`, and the SHA-256 of the whole identifier.
pub fn bound_session_identity(id: &str) -> String {
    if id.len() <= 256 {
        return id.to_owned();
    }
    let hash = hex(&Sha256::digest(id.as_bytes()));
    let mut end = (255 - 1 - hash.len()).min(id.len());
    while !id.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}#{hash}", &id[..end])
}

/// Go `knownSessionPrefixes` (sdk/cliproxy/session/identity.go), stripped iteratively.
const KNOWN_SESSION_PREFIXES: [&str; 20] = [
    "lcp:v1:",
    "lcp:",
    "ctx:v1:",
    "ctx:",
    "codex:",
    "claude:",
    "header:",
    "session:",
    "affinity:",
    "slot:",
    "task:",
    "conv:",
    "thread:",
    "clientreq:",
    "geminicache:",
    "pck:",
    "user:",
    "execution:",
    "agy:",
    "derived:",
];

fn is_canonical_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// Go `NormalizeToCanonicalUUID`: a lowercase UUID as is (after stripping known
/// session prefixes, or after one generic `prefix:`), otherwise an RFC 9562 UUIDv8
/// projected from SHA-256. Empty when nothing identifying remains.
pub fn normalize_to_canonical_uuid(raw: &str) -> String {
    let mut clean = raw.trim();
    if clean.is_empty() {
        return String::new();
    }
    if is_canonical_uuid(clean) {
        return clean.to_ascii_lowercase();
    }
    while let Some(rest) = KNOWN_SESSION_PREFIXES.iter().find_map(|p| clean.strip_prefix(p)) {
        clean = rest.trim();
    }
    if clean.is_empty() {
        return String::new();
    }
    if is_canonical_uuid(clean) {
        return clean.to_ascii_lowercase();
    }
    if let Some(idx) = clean.find(':').filter(|i| *i > 0) {
        let candidate = clean[idx + 1..].trim();
        if is_canonical_uuid(candidate) {
            return candidate.to_ascii_lowercase();
        }
    }
    let mut hasher = Sha256::new();
    hasher.update(b"cpa:canonical-uuid:v1\0");
    hasher.update(clean.as_bytes());
    let mut u: [u8; 16] = hasher.finalize()[..16].try_into().expect("16 bytes");
    u[6] = (u[6] & 0x0f) | 0x80;
    u[8] = (u[8] & 0x3f) | 0x80;
    let h = hex(&u);
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

/// Go `CallerScope`: an irreversible namespace for a downstream client key.
pub fn caller_scope(value: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        return String::new();
    }
    let mut hasher = Sha256::new();
    hasher.update(b"cli-proxy-api:caller-scope:v1\0");
    hasher.update(value.as_bytes());
    hex(&hasher.finalize())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A request body viewed the way Go's session extractors read it: gjson paths on the
/// root, and on `request` when the body is an envelope without top-level `contents`.
struct Body<'a> {
    json: &'a [u8],
    nested: bool,
    exists: bool,
    /// The raw values of the [`INDEXED_ROOTS`] members of a valid JSON object, borrowed from
    /// the body: `None` while a root is absent, `Some(None)` for a duplicate key (gjson tries
    /// each in turn, so those lookups scan). Most lookups here ask for a root the body lacks
    /// or one after `messages`, which gjson finds only by scanning the whole object (a
    /// coding agent's prompt is hundreds of KB). Other members are skipped without being
    /// decoded; a body with an escaped key gets no index.
    roots: Option<[Option<Option<&'a [u8]>>; INDEXED_ROOTS.len()]>,
}

/// The top-level keys session extraction looks up through [`Body`]. A root missing here
/// only means that lookup scans the body as gjson does.
const INDEXED_ROOTS: [&str; 52] = [
    "actionID",
    "actionId",
    "action_id",
    "cachedContent",
    "cached_content",
    "chatId",
    "chat_id",
    "childSessionId",
    "child_session_id",
    "contents",
    "conversation",
    "conversationId",
    "conversation_id",
    "extra_body",
    "forkSource",
    "fork_source",
    "forked_from_id",
    "forked_from_thread_id",
    "metadata",
    "parentActionID",
    "parentActionId",
    "parentConversationID",
    "parentConversationId",
    "parentID",
    "parentId",
    "parentSession",
    "parentSessionID",
    "parentSessionId",
    "parentSubagentId",
    "parentTaskID",
    "parentTaskId",
    "parentThreadID",
    "parentThreadId",
    "parent_action_id",
    "parent_conversation_id",
    "parent_id",
    "parent_session",
    "parent_session_id",
    "parent_subagent_id",
    "parent_task_id",
    "parent_thread_id",
    "previousSessionId",
    "previous_session_id",
    "promptCacheKey",
    "prompt_cache_key",
    "request",
    "sessionID",
    "sessionId",
    "session_id",
    "taskID",
    "taskId",
    "task_id",
];

impl<'a> Body<'a> {
    fn new(payload: &'a [u8]) -> Self {
        let object = payload.iter().find(|b| !b.is_ascii_whitespace()) == Some(&b'{');
        let roots = (object && json::valid(payload)).then(|| {
            let mut roots = [None; INDEXED_ROOTS.len()];
            let mut escaped = false;
            json::object_members(payload, |key, esc, value| {
                escaped |= esc;
                if let Some(at) = INDEXED_ROOTS.iter().position(|r| r.as_bytes() == key) {
                    roots[at] = Some(if roots[at].is_some() { None } else { Some(value) });
                }
                !escaped
            });
            (!escaped).then_some(roots)
        });
        let mut body = Self {
            json: payload,
            nested: false,
            exists: false,
            roots: roots.flatten(),
        };
        if !payload.is_empty() {
            body.nested = body.get("request").exists() && !body.get("contents").exists();
            body.exists = json::parse(payload).exists();
        }
        body
    }

    fn get(&self, path: &str) -> Res<'a> {
        let plain = |segment: &str| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        };
        let (first, rest) = path.split_once('.').map_or((path, None), |(f, r)| (f, Some(r)));
        if let Some(roots) = &self.roots
            && path.split('.').all(plain)
            && let Some(at) = INDEXED_ROOTS.iter().position(|r| *r == first)
        {
            match (roots[at], rest) {
                (None, _) => return Res::default(),
                (Some(Some(value)), None) => return json::parse(value),
                (Some(Some(value)), Some(rest)) if value.first() == Some(&b'{') => return json::get(value, rest),
                _ => {}
            }
        }
        json::get(self.json, path)
    }

    /// The normalized root value at `path` (gjson `root.Get(path).String()`).
    fn root(&self, path: &str) -> String {
        if !self.exists {
            return String::new();
        }
        normalize_explicit_id(&self.get(path).str())
    }

    /// The normalized nested `request` value at `path`, when the body is an envelope.
    fn req(&self, path: &str) -> String {
        if !self.nested {
            return String::new();
        }
        normalize_explicit_id(&self.get("request").get(path).str())
    }

    /// The first path with a value, checking root then `request` for each path.
    fn first(&self, paths: &[&str]) -> String {
        for path in paths {
            let value = self.root(path);
            if !value.is_empty() {
                return value;
            }
            let value = self.req(path);
            if !value.is_empty() {
                return value;
            }
        }
        String::new()
    }

    /// Root paths in order, then the same paths under `request`.
    fn root_then_req(&self, paths: &[&str]) -> String {
        paths
            .iter()
            .map(|p| self.root(p))
            .chain(paths.iter().map(|p| self.req(p)))
            .find(|v| !v.is_empty())
            .unwrap_or_default()
    }
}

/// Go `sessionHeaderValue`: the first normalized value of `name`, case-insensitively.
fn header(headers: &HeaderMap, name: &str) -> String {
    headers
        .get_all(name)
        .iter()
        .map(|v| normalize_explicit_id(&String::from_utf8_lossy(v.as_bytes())))
        .find(|v| !v.is_empty())
        .unwrap_or_default()
}

fn first_header(headers: &HeaderMap, names: &[&str]) -> String {
    names
        .iter()
        .map(|n| header(headers, n))
        .find(|v| !v.is_empty())
        .unwrap_or_default()
}

/// Go `ClaudeMetadataIdentities`: session, parent and agent from Claude `user_id`
/// metadata (JSON or the legacy `..._session_<id>` suffix).
pub fn claude_metadata_identities(payload: &[u8]) -> (String, String, String) {
    if payload.is_empty() {
        return Default::default();
    }
    let mut user_id = json::get(payload, "metadata.user_id").str().trim().to_owned();
    if user_id.is_empty() && json::get(payload, "request").exists() && !json::get(payload, "contents").exists() {
        user_id = json::get(payload, "request.metadata.user_id").str().trim().to_owned();
    }
    if user_id.is_empty() {
        return Default::default();
    }
    let first = |source: &[u8], paths: &[&str]| {
        paths
            .iter()
            .map(|p| normalize_explicit_id(&json::get(source, p).str()))
            .find(|v| !v.is_empty())
            .unwrap_or_default()
    };
    if user_id.starts_with('{') {
        let parsed = user_id.as_bytes();
        return (
            first(parsed, &["session_id"]),
            first(parsed, &["parent_session_id", "parent_agent_id", "parent_id"]),
            first(parsed, &["agent_id", "subagent_id"]),
        );
    }
    if let Some(session) = legacy_claude_session(&user_id) {
        return (
            normalize_explicit_id(session),
            first(
                payload,
                &[
                    "metadata.parent_agent_id",
                    "metadata.parent_session_id",
                    "metadata.parent_id",
                ],
            ),
            first(payload, &["metadata.agent_id", "metadata.subagent_id"]),
        );
    }
    Default::default()
}

/// Go `_session_([a-f0-9-]+)$`: the leftmost marker whose remainder is all session
/// characters.
fn legacy_claude_session(user_id: &str) -> Option<&str> {
    const MARKER: &str = "_session_";
    user_id.match_indices(MARKER).find_map(|(i, _)| {
        let rest = &user_id[i + MARKER.len()..];
        (!rest.is_empty() && rest.bytes().all(|b| matches!(b, b'a'..=b'f' | b'0'..=b'9' | b'-'))).then_some(rest)
    })
}

const PARENT_PATHS: &[&str] = &[
    "parent_session_id",
    "parentSessionId",
    "parentSessionID",
    "parent_thread_id",
    "parentThreadId",
    "parentThreadID",
    "forked_from_thread_id",
    "forked_from_id",
    "parent_conversation_id",
    "parentConversationId",
    "parentConversationID",
    "parent_id",
    "parentId",
    "parentID",
    "parent_task_id",
    "parentTaskId",
    "parentTaskID",
    "parent_action_id",
    "parentActionId",
    "parentActionID",
    "parent_session",
    "parentSession",
    "parent_subagent_id",
    "parentSubagentId",
    "forkSource.sessionId",
    "fork_source.session_id",
    "previousSessionId",
    "previous_session_id",
    "metadata.parent_session_id",
    "metadata.parentSessionId",
    "metadata.parentSessionID",
    "metadata.parent_thread_id",
    "metadata.parentThreadId",
    "metadata.forked_from_thread_id",
    "metadata.forked_from_id",
    "metadata.parent_id",
    "metadata.parentId",
    "metadata.parentID",
    "metadata.parent_task_id",
    "metadata.parentTaskId",
    "metadata.parentTaskID",
    "metadata.parent_action_id",
    "metadata.parentActionId",
    "metadata.parent_subagent_id",
    "metadata.parentSubagentId",
    "metadata.parent_session",
    "metadata.parentSession",
    "metadata.parent_agent_id",
    "metadata.parentAgentId",
    "metadata.forkSource.sessionId",
    "metadata.previousSessionId",
    "extra_body.parent_session_id",
    "extra_body.parentSessionId",
    "extra_body.parentSessionID",
    "extra_body.parent_thread_id",
    "extra_body.parentThreadId",
    "extra_body.forked_from_thread_id",
    "extra_body.forked_from_id",
    "extra_body.parent_id",
    "extra_body.parentId",
    "extra_body.parentID",
    "extra_body.parent_task_id",
    "extra_body.parentTaskId",
    "extra_body.parent_action_id",
    "extra_body.parentActionId",
    "extra_body.parent_subagent_id",
    "extra_body.parentSubagentId",
    "extra_body.parent_session",
    "extra_body.parentSession",
];

const FORK_PATHS: &[&str] = &[
    "forked_from_thread_id",
    "forked_from_id",
    "forkSource.sessionId",
    "fork_source.session_id",
    "previousSessionId",
    "previous_session_id",
    "metadata.forked_from_thread_id",
    "metadata.forked_from_id",
    "metadata.forkSource.sessionId",
    "metadata.previousSessionId",
    "extra_body.forked_from_thread_id",
    "extra_body.forked_from_id",
    "extra_body.forkSource.sessionId",
    "extra_body.previousSessionId",
];

/// A header-keyed client family: its session header, prefix, parent headers and the
/// agent name used when there is no parent.
struct Family {
    header: &'static [&'static str],
    client: &'static str,
    prefix: &'static str,
    parents: &'static [&'static str],
    main: &'static str,
}

/// Go steps 5 (generic headers) in priority order.
const FAMILIES: &[Family] = &[
    Family {
        header: &["X-Session-ID"],
        client: "generic",
        prefix: "header:",
        parents: &[
            "X-Parent-Session-ID",
            "X-Parent-Session-Id",
            "X-Parent-ID",
            "X-Parent-Id",
        ],
        main: "main",
    },
    Family {
        header: &["X-Session-Affinity"],
        client: "opencode",
        prefix: "affinity:",
        parents: &[
            "X-Parent-Session-Affinity",
            "X-Parent-Session-ID",
            "X-Parent-ID",
            "X-Parent-Id",
        ],
        main: "main",
    },
    Family {
        header: &["X-Slot-Session-Id"],
        client: "pi",
        prefix: "slot:",
        parents: &[
            "X-Parent-Slot-Session-Id",
            "X-Parent-Session-ID",
            "X-Parent-Session-Id",
            "X-Parent-ID",
            "X-Parent-Id",
        ],
        main: "slot",
    },
    Family {
        header: &["X-Task-ID", "X-Task-Id", "X-Task_ID"],
        client: "task",
        prefix: "task:",
        parents: &[
            "X-Parent-Task-ID",
            "X-Parent-Task-Id",
            "X-Parent-Session-ID",
            "X-Parent-Session-Id",
            "X-Parent-ID",
            "X-Parent-Id",
        ],
        main: "main",
    },
    Family {
        header: &["X-Conversation-Id"],
        client: "conv",
        prefix: "conv:",
        parents: &["X-Parent-Conversation-Id", "X-Parent-ID"],
        main: "main",
    },
    Family {
        header: &["X-Thread-Id"],
        client: "openai-thread",
        prefix: "thread:",
        parents: &["X-Parent-Thread-Id", "X-Parent-ID"],
        main: "main",
    },
    Family {
        header: &["X-Client-Request-Id"],
        client: "generic",
        prefix: "clientreq:",
        parents: &["X-Parent-Session-ID", "X-Parent-ID", "X-Parent-Id"],
        main: "main",
    },
];

/// Go `ExtractSessionInfo` for a request's headers, body and execution session.
pub fn extract_session_info(
    headers: &HeaderMap,
    payload: &[u8],
    execution_session: Option<&str>,
) -> Option<SessionInfo> {
    let body = Body::new(payload);
    let mut info = SessionInfo::default();
    let mut parent_candidate = String::new();
    if !payload.is_empty() {
        parent_candidate = body.first(PARENT_PATHS);
        if parent_candidate.is_empty() {
            parent_candidate = claude_metadata_identities(payload).1;
        }
    }
    let parent = parent_candidate.as_str();

    // 1. Claude Code headers.
    let sid = header(headers, "X-Claude-Code-Session-Id");
    if !sid.is_empty() {
        info.client_type = "claude";
        let mut agent = header(headers, "X-Claude-Code-Agent-Id");
        if agent.is_empty() && body.exists {
            agent = body.root_then_req(&["metadata.agent_id", "metadata.subagent_id"]);
        }
        if agent.is_empty() {
            agent = claude_metadata_identities(payload).2;
        }
        let mut parent_agent = header(headers, "X-Claude-Code-Parent-Agent-Id");
        if parent_agent.is_empty() && body.exists {
            parent_agent = body.root_then_req(&["metadata.parent_agent_id", "metadata.parentAgentId"]);
        }
        if !agent.is_empty() && agent != "main" {
            info.agent_name = agent.clone();
            info.parent_session_id = format!("claude:{sid}");
            if !parent_agent.is_empty() && parent_agent != "main" && parent_agent != agent {
                info.parent_session_id = format!("claude:{sid}:agent:{parent_agent}");
            } else if !parent.is_empty() && parent != sid {
                info.parent_session_id = format!("claude:{parent}");
            }
            info.session_id = format!("claude:{sid}:agent:{agent}");
        } else {
            info.agent_name = "main".into();
            info.session_id = format!("claude:{sid}");
            if !parent.is_empty() && parent != sid {
                info.parent_session_id = format!("claude:{parent}");
                info.agent_name = "subagent".into();
            }
        }
        return finalize(info);
    }

    // 2. Claude Code metadata.user_id outranks generic headers.
    if !payload.is_empty() {
        let (sid, parent_sid, mut agent) = claude_metadata_identities(payload);
        if !sid.is_empty() {
            info.client_type = "claude";
            if agent.is_empty() {
                agent = header(headers, "X-Claude-Code-Agent-Id");
            }
            if agent.is_empty() && body.exists {
                agent = body.root_then_req(&["metadata.agent_id", "metadata.subagent_id"]);
            }
            let mut parent_agent = header(headers, "X-Claude-Code-Parent-Agent-Id");
            if parent_agent.is_empty() && body.exists {
                parent_agent = body.root_then_req(&["metadata.parent_agent_id", "metadata.parentAgentId"]);
            }
            if !agent.is_empty() && agent != "main" {
                info.session_id = format!("claude:{sid}:agent:{agent}");
                info.parent_session_id = format!("claude:{sid}");
                if !parent_agent.is_empty() && parent_agent != "main" && parent_agent != agent {
                    info.parent_session_id = format!("claude:{sid}:agent:{parent_agent}");
                } else if !parent_sid.is_empty() && parent_sid != sid {
                    info.parent_session_id = format!("claude:{parent_sid}");
                } else if !parent.is_empty() && parent != sid {
                    info.parent_session_id = format!("claude:{parent}");
                }
                info.agent_name = agent;
            } else {
                info.session_id = format!("claude:{sid}");
                if !parent_sid.is_empty() && parent_sid != sid {
                    info.parent_session_id = format!("claude:{parent_sid}");
                    info.agent_name = "subagent".into();
                } else if !parent.is_empty() && parent != sid {
                    info.parent_session_id = format!("claude:{parent}");
                    info.agent_name = "subagent".into();
                } else {
                    info.agent_name = "main".into();
                }
            }
            return finalize(info);
        }
    }

    // 3. OpenAI / Codex CLI headers.
    if let Some(info) = codex(headers, &body, parent) {
        return finalize(info);
    }

    // 4. Antigravity CLI.
    let sid = header(headers, "X-Http-Session-Id");
    if !sid.is_empty() {
        info.client_type = "agy";
        info.session_id = format!("agy:{sid}");
        let parent_sid = first_header(
            headers,
            &[
                "X-Parent-Session-ID",
                "X-Parent-Session-Id",
                "X-Parent-ID",
                "X-Parent-Id",
            ],
        );
        child_of(&mut info, "agy:", &sid, &parent_sid, parent, "main");
        return finalize(info);
    }

    // 5. OpenCode, pi slot, task, conversation, thread and client-request headers.
    for family in FAMILIES {
        let sid = first_header(headers, family.header);
        if sid.is_empty() {
            continue;
        }
        info.client_type = family.client;
        info.session_id = format!("{}{sid}", family.prefix);
        let parent_sid = first_header(headers, family.parents);
        child_of(&mut info, family.prefix, &sid, &parent_sid, parent, family.main);
        return finalize(info);
    }

    // 6. Payload inspection.
    if !payload.is_empty()
        && body.exists
        && let Some(info) = payload_session(headers, &body, parent)
    {
        return finalize(info);
    }

    // 7. Execution session metadata.
    if let Some(id) = execution_session.map(normalize_explicit_id).filter(|id| !id.is_empty()) {
        info.client_type = "generic";
        info.session_id = format!("execution:{id}");
        info.agent_name = "main".into();
        return finalize(info);
    }
    None
}

/// Shared parent handling of the header families: an explicit parent header, else the
/// body parent candidate, else the family's main agent name.
fn child_of(info: &mut SessionInfo, prefix: &str, sid: &str, parent_sid: &str, parent: &str, main: &str) {
    if !parent_sid.is_empty() && parent_sid != sid {
        info.parent_session_id = format!("{prefix}{parent_sid}");
        info.agent_name = "subagent".into();
    } else if !parent.is_empty() && parent != sid {
        info.parent_session_id = format!("{prefix}{parent}");
        info.agent_name = "subagent".into();
    } else {
        info.agent_name = main.into();
    }
}

fn codex(headers: &HeaderMap, body: &Body<'_>, parent: &str) -> Option<SessionInfo> {
    let mut sid = first_header(headers, &["Session-Id", "Session_id"]);
    let mut tid = first_header(headers, &["Thread-Id", "Thread_id"]);
    let turn = headers
        .get("X-Codex-Turn-Metadata")
        .map(|v| String::from_utf8_lossy(v.as_bytes()).trim().to_owned())
        .unwrap_or_default();
    let turn_exists = !turn.is_empty() && json::parse(turn.as_bytes()).exists();
    let turn_get = |path: &str| {
        if turn_exists {
            normalize_explicit_id(&json::get(turn.as_bytes(), path).str())
        } else {
            String::new()
        }
    };
    if sid.is_empty() {
        sid = turn_get("session_id");
    }
    if tid.is_empty() {
        tid = turn_get("thread_id");
    }
    if tid.is_empty() && !sid.is_empty() && body.exists {
        tid = body.first(&["thread_id", "threadId", "metadata.thread_id"]);
    }
    if sid.is_empty() && tid.is_empty() {
        return None;
    }
    let mut info = SessionInfo {
        client_type: "codex",
        ..Default::default()
    };
    let mut parent_thread = first_header(headers, &["x-codex-parent-thread-id", "X-Codex-Parent-Thread-Id"]);
    if parent_thread.is_empty() {
        parent_thread = turn_get("parent_thread_id");
    }
    let mut forked_from = turn_get("forked_from_thread_id");
    if forked_from.is_empty() {
        forked_from = turn_get("forked_from_id");
    }
    if forked_from.is_empty() && body.exists {
        forked_from = body.first(&[
            "forked_from_thread_id",
            "forked_from_id",
            "metadata.forked_from_thread_id",
            "metadata.forked_from_id",
            "extra_body.forked_from_thread_id",
            "extra_body.forked_from_id",
        ]);
    }
    let mut agent_name = String::new();
    if turn_exists {
        let raw = json::get(turn.as_bytes(), "agent_name").str().into_owned();
        let raw = raw.strip_prefix("/root/").unwrap_or(&raw);
        let raw = raw.strip_prefix('/').unwrap_or(raw);
        let raw = normalize_explicit_id(raw.trim());
        if !raw.is_empty() && raw != "root" && raw != "main" {
            agent_name = raw;
        }
    }
    let sub = header(headers, "X-Openai-Subagent");
    let mut subagent = !sub.is_empty() && !sub.eq_ignore_ascii_case("false") && sub != "0";
    if turn_exists && json::get(turn.as_bytes(), "subagent_kind").str() == "thread_spawn" {
        subagent = true;
    }

    if !forked_from.is_empty() {
        let mut fork_session = if tid.is_empty() { sid.clone() } else { tid.clone() };
        if fork_session == forked_from && !sid.is_empty() && sid != forked_from {
            fork_session = sid.clone();
        }
        info.session_id = format!("codex:{fork_session}");
        info.parent_session_id = format!("codex:{forked_from}");
        info.agent_name = "main".into();
        info.is_fork = true;
        return Some(info);
    }

    if subagent
        || (!tid.is_empty() && !sid.is_empty() && tid != sid)
        || (!parent_thread.is_empty() && parent_thread != tid && parent_thread != sid)
    {
        let child = if tid.is_empty() { sid.clone() } else { tid.clone() };
        let parent_sid = if parent_thread.is_empty() {
            sid.clone()
        } else {
            parent_thread
        };
        if !agent_name.is_empty() && !sid.is_empty() {
            info.session_id = format!("codex:{sid}:agent:{agent_name}");
            info.agent_name = agent_name;
            if !parent_sid.is_empty() {
                info.parent_session_id = format!("codex:{parent_sid}");
            } else if !parent.is_empty() && parent != sid {
                info.parent_session_id = format!("codex:{parent}");
            }
        } else {
            info.session_id = format!("codex:{child}");
            info.agent_name = if agent_name.is_empty() {
                "subagent".into()
            } else {
                agent_name
            };
            if !parent_sid.is_empty() && parent_sid != child {
                info.parent_session_id = format!("codex:{parent_sid}");
            } else if !parent.is_empty() && parent != child {
                info.parent_session_id = format!("codex:{parent}");
            }
        }
        info.is_subagent = true;
        return Some(info);
    }

    let session = if sid.is_empty() { tid } else { sid };
    info.session_id = format!("codex:{session}");
    if !parent_thread.is_empty() && parent_thread != session {
        info.parent_session_id = format!("codex:{parent_thread}");
        info.agent_name = "subagent".into();
        info.is_subagent = true;
    } else if !parent.is_empty() && parent != session {
        info.parent_session_id = format!("codex:{parent}");
        info.agent_name = "subagent".into();
        info.is_subagent = true;
    } else {
        info.agent_name = "main".into();
    }
    Some(info)
}

fn is_body_fork(body: &Body<'_>) -> bool {
    body.exists && !body.first(FORK_PATHS).is_empty()
}

/// A body-keyed child: a fork when the body names a fork source, else a subagent.
fn body_child(info: &mut SessionInfo, body: &Body<'_>) {
    if is_body_fork(body) {
        info.is_fork = true;
        info.is_subagent = false;
        info.agent_name = "main".into();
    } else {
        info.agent_name = "subagent".into();
        info.is_subagent = true;
    }
}

fn payload_session(headers: &HeaderMap, body: &Body<'_>, parent: &str) -> Option<SessionInfo> {
    let mut info = SessionInfo::default();

    // Gemini context caching.
    for path in ["cachedContent", "cached_content"] {
        let mut cache = body.root(path);
        if cache.is_empty() {
            cache = body.req(path);
        }
        if !cache.is_empty() {
            info.client_type = "gemini";
            info.session_id = format!("geminicache:{cache}");
            if !parent.is_empty() && parent != cache {
                info.parent_session_id = format!("geminicache:{parent}");
                info.agent_name = "subagent".into();
            } else {
                info.agent_name = "main".into();
            }
            return Some(info);
        }
    }

    // OpenAI thread in the body.
    let tid = body.first(&["thread_id", "threadId", "metadata.thread_id"]);
    if !tid.is_empty() {
        info.client_type = "openai-thread";
        info.session_id = format!("thread:{tid}");
        if !parent.is_empty() && parent != tid {
            info.parent_session_id = format!("thread:{parent}");
            body_child(&mut info, body);
        } else {
            info.agent_name = "main".into();
        }
        return Some(info);
    }

    // Generic session in the body.
    let mut agent = body.root("metadata.agent_id");
    if agent.is_empty() {
        agent = body.root("metadata.subagent_id");
    }
    if agent.is_empty() {
        agent = first_header(headers, &["X-Claude-Code-Agent-Id", "x-agent-id"]);
    }
    if agent.is_empty() {
        agent = body.req("metadata.agent_id");
    }
    if agent.is_empty() {
        agent = body.req("metadata.subagent_id");
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
        info.client_type = "generic";
        if !agent.is_empty() && agent != "main" {
            info.session_id = format!("session:{sid}:agent:{agent}");
            info.parent_session_id = format!("session:{sid}");
            if !parent.is_empty() && parent != sid {
                info.parent_session_id = format!("session:{parent}");
            }
            info.agent_name = agent;
        } else {
            info.session_id = format!("session:{sid}");
            if !parent.is_empty() && parent != sid {
                info.parent_session_id = format!("session:{parent}");
                body_child(&mut info, body);
            } else {
                info.agent_name = "main".into();
            }
        }
        return Some(info);
    }

    // Task or action in the body (Roo Code, Cline, OpenHands).
    let tid = body.first(&[
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
    if !tid.is_empty() {
        info.client_type = "task";
        info.session_id = format!("task:{tid}");
        if !parent.is_empty() && parent != tid {
            info.parent_session_id = format!("task:{parent}");
            body_child(&mut info, body);
        } else {
            info.agent_name = "main".into();
        }
        return Some(info);
    }

    // Prompt cache key, then the conversation object.
    let conversation = conversation_alias(body);
    let mut pck = body.root("prompt_cache_key");
    if pck.is_empty() {
        pck = body.root("promptCacheKey");
    }
    if pck.is_empty() {
        pck = body.req("prompt_cache_key");
        if pck.is_empty() {
            pck = body.req("promptCacheKey");
        }
    }
    if !pck.is_empty() {
        info.client_type = "generic";
        info.session_id = format!("pck:{pck}");
        if !parent.is_empty() && parent != pck {
            info.parent_session_id = format!("pck:{parent}");
            info.agent_name = "subagent".into();
        } else {
            info.agent_name = "main".into();
        }
        return Some(info);
    }
    if !conversation.is_empty() {
        info.client_type = "conv";
        if !parent.is_empty() && format!("conv:{parent}") != conversation {
            info.parent_session_id = format!("conv:{parent}");
            info.agent_name = "subagent".into();
        } else {
            info.agent_name = "main".into();
        }
        info.session_id = conversation;
        return Some(info);
    }

    // Plain metadata.user_id.
    let mut user = body.root("metadata.user_id");
    if user.is_empty() {
        user = body.req("metadata.user_id");
    }
    if !user.is_empty() {
        info.client_type = "generic";
        info.session_id = format!("user:{user}");
        info.agent_name = "main".into();
        return Some(info);
    }

    // Legacy conversation paths.
    let cid = body.first(&[
        "conversation_id",
        "conversationId",
        "chat_id",
        "chatId",
        "metadata.conversation_id",
        "extra_body.conversation_id",
    ]);
    if !cid.is_empty() {
        info.client_type = "conv";
        info.session_id = format!("conv:{cid}");
        if !parent.is_empty() && parent != cid {
            info.parent_session_id = format!("conv:{parent}");
            info.agent_name = "subagent".into();
        } else {
            info.agent_name = "main".into();
        }
        return Some(info);
    }
    None
}

/// `conv:<id>` from `conversation.id` or a string `conversation` (root, else the
/// envelope's), or empty.
fn conversation_alias(body: &Body<'_>) -> String {
    let mut conversation = body.get("conversation");
    if !conversation.exists() && body.nested {
        conversation = body.get("request").get("conversation");
    }
    let id = normalize_explicit_id(&conversation.get("id").str());
    if !id.is_empty() {
        return format!("conv:{id}");
    }
    if conversation.kind == Kind::String {
        let id = normalize_explicit_id(&conversation.str());
        if !id.is_empty() {
            return format!("conv:{id}");
        }
    }
    String::new()
}

fn finalize(mut info: SessionInfo) -> Option<SessionInfo> {
    if info.session_id.is_empty() {
        return None;
    }
    info.session_id = bound_session_identity(&info.session_id);
    if !info.parent_session_id.is_empty() {
        info.parent_session_id = bound_session_identity(&info.parent_session_id);
    }
    if info.agent_name.is_empty() {
        info.agent_name = "main".into();
    }
    if info.client_type.is_empty() {
        info.client_type = "generic";
    }
    if info.parent_session_id == info.session_id {
        info.parent_session_id.clear();
    }
    Some(info)
}

/// Go `$CPA-SESSION-ID` for custom headers: the session the conductor binds to the
/// executor context (`ensureCanonicalSessionMetadata` then `syncMetadataSessionToContext`):
/// `BoundSessionIdentity(CanonicalSessionID)`, which falls through the whole
/// `ExtractSessionID` chain, including the `derived:` and first-messages `msg:` fallbacks.
/// `session` is `ExecRequest::session`, which carries the LCP session when session
/// affinity bound the attempt by conversation prefix. Empty when Go's chain finds nothing.
pub fn cpa_session_id(session: Option<&str>) -> Option<String> {
    session
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(bound_session_identity)
}

/// Go metadata the session fallbacks read.
#[derive(Debug, Clone, Default)]
pub struct Meta<'a> {
    /// `execution_session_id`.
    pub execution_session: Option<&'a str>,
    /// `derived_session_id` (from [`derive_id`]).
    pub derived: Option<&'a str>,
}

/// Go `extractExplicitSessionIDs`: the explicit identity and its fallback (the parent,
/// or the conversation alias of a prompt-cache-key session), plus the fork flag.
pub fn explicit_session_ids(headers: &HeaderMap, payload: &[u8], meta: &Meta<'_>) -> (String, String, bool) {
    let Some(info) = extract_session_info(headers, payload, meta.execution_session) else {
        return Default::default();
    };
    let mut fallback = info.parent_session_id;
    if fallback.is_empty() && info.session_id.starts_with("pck:") && !payload.is_empty() {
        fallback = conversation_alias(&Body::new(payload));
    }
    (info.session_id, fallback, info.is_fork)
}

/// Go `extractSessionIDs`: explicit identity, then `derived:<id>`, then the
/// first-messages hash. Returns `(primary, fallback)`.
pub fn session_ids(headers: &HeaderMap, payload: &[u8], meta: &Meta<'_>) -> (String, String) {
    let (primary, fallback, _) = explicit_session_ids(headers, payload, meta);
    if !primary.is_empty() {
        return (primary, fallback);
    }
    let derived = normalize_explicit_id(meta.derived.unwrap_or_default());
    if !derived.is_empty() {
        return (format!("derived:{derived}"), String::new());
    }
    if payload.is_empty() {
        return Default::default();
    }
    message_hash_ids(payload)
}

/// Go `ExtractSessionID`.
pub fn extract_session_id(headers: &HeaderMap, payload: &[u8], meta: &Meta<'_>) -> String {
    session_ids(headers, payload, meta).0
}

/// Go `isSubagentSession`.
pub fn is_subagent_session(primary: &str, fallback: &str) -> bool {
    if primary.contains(":agent:") {
        return true;
    }
    if fallback.is_empty() || primary.is_empty() || primary == fallback {
        return false;
    }
    match (primary.find(':'), fallback.find(':')) {
        (Some(a), Some(b)) if a > 0 && b > 0 && primary[..a] == fallback[..b] => true,
        (None, None) => true,
        _ => false,
    }
}

/// Go `hasExplicitSession`: whether headers or body carry any session signal, so the
/// derived identity is not computed.
pub fn has_explicit_session(headers: &HeaderMap, payload: &[u8]) -> bool {
    const HEADERS: &[&str] = &[
        "X-Claude-Code-Session-Id",
        "X-Claude-Code-Agent-Id",
        "X-Claude-Code-Parent-Agent-Id",
        "Session-Id",
        "Session_id",
        "x-codex-parent-thread-id",
        "X-Codex-Turn-Metadata",
        "X-Openai-Subagent",
        "X-Http-Session-Id",
        "X-Session-ID",
        "X-Session-Affinity",
        "X-Parent-Session-ID",
        "X-Parent-Session-Affinity",
        "X-Parent-ID",
        "X-Slot-Session-Id",
        "X-Parent-Slot-Session-Id",
        "X-Task-ID",
        "X-Parent-Task-ID",
        "X-Conversation-Id",
        "X-Parent-Conversation-Id",
        "X-Thread-Id",
        "X-Parent-Thread-Id",
        "Thread-Id",
        "X-Client-Request-Id",
    ];
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
    if HEADERS.iter().any(|h| !header(headers, h).is_empty()) {
        return true;
    }
    if payload.is_empty() {
        return false;
    }
    let body = Body::new(payload);
    if !body.first(PATHS).is_empty() {
        return true;
    }
    if !claude_metadata_identities(payload).0.is_empty() {
        return true;
    }
    let mut user = body.get("metadata.user_id").str().trim().to_owned();
    if user.is_empty() && body.nested {
        user = body.get("request").get("metadata.user_id").str().trim().to_owned();
    }
    !normalize_explicit_id(&user).is_empty() || !conversation_alias(&body).is_empty()
}

/// Go `Enrich`'s derived identity: computed only when the request carries no explicit
/// session signal and no execution session.
pub fn derived_id(
    format: Format,
    headers: &HeaderMap,
    payload: &[u8],
    execution_session: Option<&str>,
    caller_scope: &str,
) -> Option<String> {
    let execution = execution_session.map(normalize_explicit_id).unwrap_or_default();
    if !execution.is_empty() || has_explicit_session(headers, payload) {
        return None;
    }
    Some(derive_id(format, payload, caller_scope)).filter(|id| !id.is_empty())
}

/// Go `DeriveID`: a stable identity from the leading instructions and the first
/// complete user input, scoped to the caller. Empty when there is no user input.
pub fn derive_id(format: Format, payload: &[u8], caller_scope: &str) -> String {
    if payload.is_empty() {
        return String::new();
    }
    // Go `json.Unmarshal` into `map[string]any`: a JSON null decodes to an empty map.
    let body = match GoValue::parse_f64(payload) {
        Some(GoValue::Object(body)) => body,
        Some(GoValue::Null) => Object::new(),
        _ => return String::new(),
    };
    let resource = if matches!(format, Format::Gemini | Format::Antigravity) {
        let request = match body.get("request") {
            Some(GoValue::Object(request)) => request,
            _ => &body,
        };
        string_field(request, &["cachedContent", "cached_content"])
    } else {
        String::new()
    };
    let (instructions, user) = match format {
        Format::Gemini | Format::Antigravity => gemini_root(&body),
        Format::Interactions => interactions_root(&body),
        Format::OpenAIResponse | Format::Codex => responses_root(&body),
        Format::Claude => messages_root(&body, true),
        Format::OpenAI => messages_root(&body, false),
    };
    if user.is_empty() {
        return String::new();
    }
    // Go `canonicalRoot` field order and omitempty rules.
    let string = |out: &mut Vec<u8>, s: &str| json::marshal_str(out, s.as_bytes(), true);
    let mut out = b"{\"version\":\"cpa-session-root-v1\",\"format\":".to_vec();
    string(&mut out, format.as_str());
    out.extend_from_slice(b",\"caller_scope\":");
    string(&mut out, caller_scope.trim());
    if !instructions.is_empty() {
        out.extend_from_slice(b",\"instructions\":[");
        for (i, instruction) in instructions.iter().enumerate() {
            if i > 0 {
                out.push(b',');
            }
            string(&mut out, instruction);
        }
        out.push(b']');
    }
    out.extend_from_slice(b",\"user\":[");
    for (i, part) in user.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        out.extend_from_slice(b"{\"kind\":");
        string(&mut out, &part.kind);
        if !part.mime.is_empty() {
            out.extend_from_slice(b",\"mime\":");
            string(&mut out, &part.mime);
        }
        out.extend_from_slice(b",\"value\":");
        string(&mut out, &part.value);
        out.push(b'}');
    }
    out.push(b']');
    if !resource.is_empty() {
        out.extend_from_slice(b",\"resource\":");
        string(&mut out, &resource);
    }
    out.push(b'}');
    format!("ctx:v1:{}", hex(&Sha256::digest(&out)))
}

type Object = BTreeMap<String, GoValue>;

struct Part {
    kind: String,
    mime: String,
    value: String,
}

/// Go `normalizedString`: a string value, trimmed and lowercased; empty otherwise.
fn lower(v: Option<&GoValue>) -> String {
    match v {
        Some(GoValue::String(s)) => s.trim().to_lowercase(),
        _ => String::new(),
    }
}

fn first_field<'v>(object: &'v Object, keys: &[&str]) -> Option<&'v GoValue> {
    keys.iter().find_map(|k| object.get(*k))
}

fn string_field(object: &Object, keys: &[&str]) -> String {
    match first_field(object, keys) {
        Some(GoValue::String(s)) => s.trim().to_owned(),
        _ => String::new(),
    }
}

fn content_value(value: &GoValue) -> &GoValue {
    match value {
        GoValue::Object(object) => ["content", "parts", "text"]
            .iter()
            .find_map(|k| object.get(*k))
            .unwrap_or(value),
        other => other,
    }
}

fn parts(value: &GoValue) -> Vec<Part> {
    let mut out = Vec::new();
    append_parts(&mut out, value);
    out
}

fn text_part(out: &mut Vec<Part>, text: &str) {
    if !text.is_empty() {
        out.push(Part {
            kind: "text".into(),
            mime: String::new(),
            value: text.to_owned(),
        });
    }
}

fn json_part(out: &mut Vec<Part>, value: &GoValue) {
    out.push(Part {
        kind: "json".into(),
        mime: String::new(),
        value: String::from_utf8_lossy(&value.marshal()).into_owned(),
    });
}

fn append_parts(out: &mut Vec<Part>, value: &GoValue) {
    match value {
        GoValue::Null => {}
        GoValue::String(text) => text_part(out, text),
        GoValue::Array(items) => items.iter().for_each(|item| append_parts(out, item)),
        GoValue::Object(object) => {
            if let Some(GoValue::String(text)) = object.get("text") {
                return text_part(out, text);
            }
            if let Some(nested) = object.get("content").or_else(|| object.get("parts")) {
                return append_parts(out, nested);
            }
            if let Some(image) = object.get("image_url") {
                return media(out, "image", image, "");
            }
            if let Some(inline) = first_field(object, &["inlineData", "inline_data"]) {
                return media(out, "inline_data", inline, "");
            }
            if let Some(file) = first_field(object, &["fileData", "file_data"]) {
                return media(out, "file", file, "");
            }
            if let Some(source) = object.get("source") {
                return media(
                    out,
                    &lower(object.get("type")),
                    source,
                    &lower(object.get("media_type")),
                );
            }
            json_part(out, &without_cache_control(value));
        }
        scalar => json_part(out, scalar),
    }
}

fn media(out: &mut Vec<Part>, kind: &str, value: &GoValue, fallback_mime: &str) {
    let kind = match kind.trim() {
        "" => "media",
        kind => kind,
    };
    match value {
        GoValue::String(text) => {
            if !text.is_empty() {
                out.push(Part {
                    kind: kind.into(),
                    mime: fallback_mime.into(),
                    value: text.clone(),
                });
            }
        }
        GoValue::Object(object) => {
            let mut mime = string_field(object, &["mimeType", "mime_type", "media_type"]);
            if mime.is_empty() {
                mime = fallback_mime.into();
            }
            let value = string_field(object, &["url", "uri", "fileUri", "file_uri", "data"]);
            if !value.is_empty() {
                out.push(Part {
                    kind: kind.into(),
                    mime,
                    value,
                });
            }
        }
        other => append_parts(out, other),
    }
}

fn without_cache_control(value: &GoValue) -> GoValue {
    match value {
        GoValue::Object(object) => GoValue::Object(
            object
                .iter()
                .filter(|(k, _)| !k.trim().eq_ignore_ascii_case("cache_control"))
                .map(|(k, v)| (k.clone(), without_cache_control(v)))
                .collect(),
        ),
        GoValue::Array(items) => GoValue::Array(items.iter().map(without_cache_control).collect()),
        other => other.clone(),
    }
}

/// Go `appendInstruction`: the value's text parts joined by newlines, first 50 runes.
fn append_instruction(instructions: &mut Vec<String>, value: &GoValue) {
    let text: Vec<String> = parts(value)
        .into_iter()
        .filter(|p| p.kind == "text" && !p.value.is_empty())
        .map(|p| p.value)
        .collect();
    if !text.is_empty() {
        instructions.push(text.join("\n").chars().take(50).collect());
    }
}

type Root = (Vec<String>, Vec<Part>);

fn array(value: Option<&GoValue>) -> &[GoValue] {
    match value {
        Some(GoValue::Array(items)) => items,
        _ => &[],
    }
}

fn messages_root(body: &Object, top_level_system: bool) -> Root {
    let mut instructions = Vec::new();
    if top_level_system && let Some(system) = body.get("system") {
        append_instruction(&mut instructions, system);
    }
    for message in array(body.get("messages")) {
        let GoValue::Object(message) = message else {
            continue;
        };
        let content = message.get("content").unwrap_or(&GoValue::Null);
        match lower(message.get("role")).as_str() {
            "system" | "developer" => append_instruction(&mut instructions, content),
            "user" => {
                let user = parts(content);
                if !user.is_empty() {
                    return (instructions, user);
                }
            }
            _ => {}
        }
    }
    (instructions, Vec::new())
}

fn responses_root(body: &Object) -> Root {
    let mut instructions = Vec::new();
    if let Some(value) = body.get("instructions") {
        append_instruction(&mut instructions, value);
    }
    match body.get("input") {
        None => (instructions, Vec::new()),
        Some(input @ GoValue::String(_)) => (instructions, parts(input)),
        Some(input) => {
            for item in array(Some(input)) {
                let GoValue::Object(item) = item else {
                    continue;
                };
                let content = item.get("content").unwrap_or(&GoValue::Null);
                match lower(item.get("role")).as_str() {
                    "system" | "developer" => append_instruction(&mut instructions, content),
                    "user" => {
                        let user = parts(content);
                        if !user.is_empty() {
                            return (instructions, user);
                        }
                    }
                    _ => {}
                }
            }
            (instructions, Vec::new())
        }
    }
}

fn gemini_root(body: &Object) -> Root {
    let body = match body.get("request") {
        Some(GoValue::Object(request)) => request,
        _ => body,
    };
    let mut instructions = Vec::new();
    if let Some(value) = first_field(body, &["systemInstruction", "system_instruction"]) {
        append_instruction(&mut instructions, content_value(value));
    }
    for content in array(body.get("contents")) {
        let GoValue::Object(object) = content else {
            continue;
        };
        if lower(object.get("role")) != "user" {
            continue;
        }
        let user = parts(content_value(content));
        if !user.is_empty() {
            return (instructions, user);
        }
    }
    (instructions, Vec::new())
}

fn interactions_root(body: &Object) -> Root {
    let mut instructions = Vec::new();
    if let Some(value) = first_field(body, &["system_instruction", "systemInstruction"]) {
        append_instruction(&mut instructions, content_value(value));
    }
    let Some(input) = body.get("input") else {
        return (instructions, Vec::new());
    };
    if let GoValue::String(_) = input {
        return (instructions, parts(input));
    }
    for entry in flatten_interaction_entries(input) {
        let step = match &entry {
            GoValue::String(_) => return (instructions, parts(&entry)),
            GoValue::Object(step) => step,
            _ => continue,
        };
        let role = lower(step.get("role"));
        let kind = lower(step.get("type"));
        if matches!(role.as_str(), "system" | "developer")
            || matches!(kind.as_str(), "system_instruction" | "developer_instruction")
        {
            append_instruction(&mut instructions, content_value(&entry));
            continue;
        }
        if role == "user" || kind == "user_input" || (matches!(kind.as_str(), "message" | "") && role.is_empty()) {
            return (instructions, parts(content_value(&entry)));
        }
    }
    (instructions, Vec::new())
}

/// Go `flattenInteractionEntries`: steps flattened depth-first, inheriting the role.
fn flatten_interaction_entries(value: &GoValue) -> Vec<GoValue> {
    fn walk(value: &GoValue, inherited: &str, out: &mut Vec<GoValue>) {
        match value {
            GoValue::Array(items) => items.iter().for_each(|item| walk(item, inherited, out)),
            GoValue::Object(object) => {
                let own = lower(object.get("role"));
                let role = if own.is_empty() {
                    inherited.to_owned()
                } else {
                    own.clone()
                };
                if let Some(GoValue::Array(steps)) = object.get("steps") {
                    steps.iter().for_each(|step| walk(step, &role, out));
                    return;
                }
                if !role.is_empty() && own.is_empty() {
                    let mut cloned = object.clone();
                    cloned.insert("role".into(), GoValue::String(role));
                    out.push(GoValue::Object(cloned));
                } else {
                    out.push(value.clone());
                }
            }
            other => out.push(other.clone()),
        }
    }
    let mut out = Vec::new();
    walk(value, "", &mut out);
    out
}

/// Go `extractMessageHashIDs`: an FNV-64a hash of the system prompt and first user
/// message (primary), and with the first assistant reply (primary, short hash as
/// fallback) once the conversation has one.
fn message_hash_ids(payload: &[u8]) -> (String, String) {
    let (mut system, mut user, mut assistant) = (Vec::new(), Vec::new(), Vec::new());
    // Go `truncateString(s, 100)` slices bytes, which may split a rune.
    let take = |s: &[u8]| s[..s.len().min(100)].to_vec();
    let messages = json::get(payload, "messages");
    if messages.is_array() {
        messages.each(|_, message| {
            let content = message_content(&message.get("content"));
            if content.is_empty() {
                return true;
            }
            match &*message.get("role").str() {
                "system" if system.is_empty() => system = take(&content),
                "user" if user.is_empty() => user = take(&content),
                "assistant" if assistant.is_empty() => assistant = take(&content),
                _ => {}
            }
            system.is_empty() || user.is_empty() || assistant.is_empty()
        });
    }
    if system.is_empty() {
        let top = json::get(payload, "system");
        if top.is_array() {
            top.each(|_, part| {
                let text = part.get("text").bytes();
                if !text.is_empty() && system.is_empty() {
                    system = take(&text);
                    return false;
                }
                true
            });
        } else if top.kind == Kind::String {
            system = take(&top.bytes());
        }
    }
    if system.is_empty() && user.is_empty() {
        let instruction = json::get(payload, "systemInstruction.parts");
        if instruction.is_array() {
            instruction.each(|_, part| {
                let text = part.get("text").bytes();
                if !text.is_empty() && system.is_empty() {
                    system = take(&text);
                    return false;
                }
                true
            });
        }
        let contents = json::get(payload, "contents");
        if contents.is_array() {
            contents.each(|_, message| {
                let role = message.get("role").str().into_owned();
                message.get("parts").each(|_, part| {
                    let text = part.get("text").bytes();
                    if text.is_empty() {
                        return true;
                    }
                    match role.as_str() {
                        "user" if user.is_empty() => user = take(&text),
                        "model" if assistant.is_empty() => assistant = take(&text),
                        _ => {}
                    }
                    false
                });
                user.is_empty() || assistant.is_empty()
            });
        }
    }
    if system.is_empty() && user.is_empty() {
        let instructions = json::get(payload, "instructions").bytes();
        if !instructions.is_empty() {
            system = take(&instructions);
        }
        let input = json::get(payload, "input");
        if input.is_array() {
            input.each(|_, item| {
                let kind = item.get("type").str().into_owned();
                if kind == "reasoning" || (!kind.is_empty() && kind != "message") {
                    return true;
                }
                let role = item.get("role").str().into_owned();
                if kind.is_empty() && role.is_empty() {
                    return true;
                }
                let content = item.get("content");
                let text = if content.kind == Kind::String {
                    content.bytes().into_owned()
                } else {
                    joined_text(&content, &["input_text", "output_text", "text"])
                };
                if text.is_empty() {
                    return true;
                }
                match role.as_str() {
                    "developer" | "system" if system.is_empty() => system = take(&text),
                    "user" if user.is_empty() => user = take(&text),
                    "assistant" if assistant.is_empty() => assistant = take(&text),
                    _ => {}
                }
                user.is_empty() || assistant.is_empty()
            });
        }
    }
    if user.is_empty() {
        return Default::default();
    }
    let short = session_hash(&system, &user, &[]);
    if assistant.is_empty() {
        return (short, String::new());
    }
    (session_hash(&system, &user, &assistant), short)
}

/// Go `extractMessageContent`.
fn message_content(content: &Res<'_>) -> Vec<u8> {
    if content.kind == Kind::String {
        return content.bytes().into_owned();
    }
    joined_text(content, &["text"])
}

/// The `text` of array parts whose `type` is one of `types`, joined by spaces (Go
/// `extractMessageContent` and `extractResponsesAPIContent`).
fn joined_text(content: &Res<'_>, types: &[&str]) -> Vec<u8> {
    if !content.is_array() {
        return Vec::new();
    }
    let mut texts: Vec<Vec<u8>> = Vec::new();
    content.each(|_, part| {
        if types.contains(&&*part.get("type").str()) {
            let text = part.get("text").bytes();
            if !text.is_empty() {
                texts.push(text.into_owned());
            }
        }
        true
    });
    texts.join(&b' ')
}

/// Go `computeSessionHash`: FNV-64a over the labelled parts.
fn session_hash(system: &[u8], user: &[u8], assistant: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut write = |bytes: &[u8]| {
        for b in bytes {
            hash ^= u64::from(*b);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    };
    for (label, value) in [(&b"sys:"[..], system), (b"usr:", user), (b"ast:", assistant)] {
        if !value.is_empty() {
            write(label);
            write(value);
            write(b"\n");
        }
    }
    format!("msg:{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::cpa_session_id;

    /// Go `ensureCanonicalSessionMetadata` + `syncMetadataSessionToContext`: every
    /// `ExtractSessionID` result reaches `$CPA-SESSION-ID`, the first-messages hash included,
    /// bounded like `BoundSessionIdentity`.
    #[test]
    fn cpa_session_id_is_the_bound_canonical_session() {
        assert_eq!(cpa_session_id(Some("claude:s1")).as_deref(), Some("claude:s1"));
        assert_eq!(
            cpa_session_id(Some("execution:ws-1")).as_deref(),
            Some("execution:ws-1")
        );
        assert_eq!(
            cpa_session_id(Some("derived:ctx:v1:ab")).as_deref(),
            Some("derived:ctx:v1:ab")
        );
        assert_eq!(
            cpa_session_id(Some("msg:2f68951593d97234")).as_deref(),
            Some("msg:2f68951593d97234")
        );
        assert_eq!(cpa_session_id(Some("  ")), None);
        let long = format!("header:{}", "x".repeat(300));
        let bounded = cpa_session_id(Some(&long)).unwrap();
        assert_eq!(bounded.len(), 255);
        assert_eq!(bounded, super::bound_session_identity(&long));
        assert_eq!(cpa_session_id(None), None);
    }

    /// Allocations on this thread (see [`crate::alloc_count`]).
    fn allocated(f: impl FnOnce()) -> usize {
        let before = crate::alloc_count::bytes();
        f();
        crate::alloc_count::bytes() - before
    }

    /// The index borrows raw slices and decodes on lookup, so a large escaped `system`
    /// string and many top-level keys session extraction never asks for cost no memory:
    /// building the index and the extractors' lookups allocate exactly what they do on a
    /// body with only the queried keys. The previous index decoded every member (the
    /// escaped string included) and stored each key.
    #[test]
    fn body_index_allocates_nothing_for_unrelated_keys() {
        use crate::json;
        let small = r#"{"metadata":{"user_id":"u1"},"model":"m"}"#.to_owned();
        let mut big = String::from(r#"{"metadata":{"user_id":"u1"},"model":"m","system":""#);
        big.push_str(&r#"line \"quoted\" \u00e9\n"#.repeat(50_000));
        big.push('"');
        for i in 0..2_000 {
            big.push_str(&format!(r#","unrelated_{i}":{{"x":"\u0041{i}"}}"#));
        }
        big.push('}');
        assert!(json::valid(big.as_bytes()));
        let lookups = |payload: &str| {
            let body = super::Body::new(payload.as_bytes());
            assert!(body.roots.is_some());
            assert_eq!(body.root("metadata.user_id"), "u1");
            assert_eq!(body.first(super::PARENT_PATHS), "");
            assert_eq!(body.first(&["conversation_id", "chat_id"]), "");
        };
        let (small_bytes, big_bytes) = (allocated(|| lookups(&small)), allocated(|| lookups(&big)));
        assert_eq!(big_bytes, small_bytes, "allocations independent of unrelated members");
    }

    /// `Body` answers lookups for absent top-level keys without scanning. gjson matches
    /// unescaped keys, so an escaped key must still be found, a key nested under another
    /// must not count as top-level, and an invalid body keeps gjson's own scan.
    #[test]
    fn body_key_shortcut_keeps_gjson_matches() {
        let id = |payload: &str| super::Body::new(payload.as_bytes()).first(&["conversation_id", "chat_id"]);
        assert_eq!(id(r#"{"messages":[],"conversation_id":"c1"}"#), "c1");
        assert_eq!(id(r#"{"messages":[],"conversation_\u0069d":"c2"}"#), "c2");
        assert_eq!(
            id(r#"{"messages":[{"conversation_id":"nested"}],"chat_id":"c3"}"#),
            "c3"
        );
        assert_eq!(id(r#"{"metadata":{"conversation_id":"m"}}"#), "");
        // Not valid JSON (a trailing comma): no index, gjson's scan still finds it.
        assert_eq!(id(r#"{"messages":[1,],"conversation_id":"c4"}"#), "c4");
        let nested = super::Body::new(br#"{"request":{"conversation_id":"r1"}}"#);
        assert!(nested.nested);
        assert_eq!(nested.first(&["conversation_id"]), "r1");
        let body = super::Body::new(br#"{"metadata":{"user_id":"u1"}}"#);
        assert_eq!(body.root("metadata.user_id"), "u1");
        assert!(body.roots.is_some());
        // Every lookup agrees with a plain gjson lookup on the whole document, including
        // duplicate keys (gjson moves on to the next one when the first lacks the rest),
        // a string where an object was expected, and a key after a long `messages`.
        let long = format!(
            r#"{{"messages":[{{"content":"{}"}}],"metadata":{{"user_id":"u3"}}}}"#,
            "x".repeat(5000)
        );
        let payloads = [
            r#"{"metadata":{"a":1},"metadata":{"user_id":"u2"}}"#,
            r#"{"metadata":"{\"user_id\":\"s\"}","x":{"user_id":"no"}}"#,
            r#"{"metadata":{"user_id":{"x":1}},"prompt_cache_key":17,"thread_id":true}"#,
            r#"{"request":{"metadata":{"user_id":"r2"}},"contents":[]}"#,
            " {\n \"metadata\" :\t{ \"user_id\" : \"w\" } , \"thread_id\" : 5 , \"prompt_cache_key\" : null } ",
            long.as_str(),
        ];
        let paths = [
            "metadata",
            "metadata.user_id",
            "metadata.user_id.x",
            "prompt_cache_key",
            "thread_id",
            "request",
            "request.metadata.user_id",
            "contents",
            "missing",
            "metadata.missing",
        ];
        for payload in payloads {
            let body = super::Body::new(payload.as_bytes());
            assert!(body.roots.is_some(), "{payload}");
            for path in paths {
                let (fast, slow) = (body.get(path), crate::json::get(payload.as_bytes(), path));
                assert_eq!(
                    (fast.exists(), fast.str()),
                    (slow.exists(), slow.str()),
                    "{path} in {payload}"
                );
            }
        }
    }
}
