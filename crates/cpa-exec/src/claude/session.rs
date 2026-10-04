//! Session identity for the Claude wire: the agent-conversation UUID written to
//! metadata.user_id and X-Claude-Code-Session-Id, per-key cached session IDs, and the
//! billing/diagnostics continuity store.
//!
//! Session extraction (ExtractSessionInfo, ExtractSessionID, DeriveID) is the shared
//! `cpa_common::session`; this module adds helps/claude_credential_identity.go,
//! helps/session_id_cache.go and helps/claude_diagnostics.go.
//!
//! In Home mode the per-key session and user IDs live in Home KV
//! (`cpa:claude:session-id:*`, `cpa:claude:user-id:*`), so every node uses the same
//! identity for a key; the continuity store stays process-local.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use cpa_common::session::{self as shared, Meta};
use http::HeaderMap;
use sha2::{Digest, Sha256};

use crate::rawjson;

/// `NormalizeExplicitID`.
pub(crate) fn normalize(raw: &str) -> String {
    shared::normalize_explicit_id(raw)
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Request facts that select the agent-conversation identity.
pub(crate) struct Inputs<'a> {
    pub headers: &'a HeaderMap,
    pub original: &'a str,
    pub translated: &'a str,
    /// Go `derived_session_id` metadata, when `session.Enrich` set one.
    pub derived: &'a str,
    /// Execution-session metadata (`ExecutionSessionMetadataKey`).
    pub execution: &'a str,
}

impl Inputs<'_> {
    fn meta(&self) -> Meta<'_> {
        Meta {
            execution_session: Some(self.execution).filter(|s| !s.is_empty()),
            derived: Some(self.derived).filter(|s| !s.is_empty()),
        }
    }
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
    let meta = inputs.meta();
    let mut identity = shared::extract_session_id(&headers, original.as_bytes(), &meta);
    if identity.is_empty() && !translated.is_empty() {
        identity = shared::extract_session_id(&headers, translated.as_bytes(), &meta);
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

/// Go `sessionIDTTL` / `userIDTTL`.
const ID_TTL: Duration = Duration::from_secs(3600);

/// Go `CachedSessionIDRequired`: Home KV while a Home client is current, otherwise the
/// local cache. In Home mode an unreachable Home fails the request.
pub(crate) async fn cached_session_id_required(api_key: &str) -> Result<String, String> {
    if api_key.is_empty() {
        return Ok(new_v4());
    }
    match cpa_home::kv::current_client() {
        Ok(None) => Ok(cached_session_id(api_key)),
        Ok(Some(client)) => session_id_home(&client, api_key).await,
        Err(error) => Err(error.to_string()),
    }
}

/// Go `CachedUserIDRequired`: a complete fake user ID per key, built on the key's
/// cached session ID.
pub(crate) async fn cached_user_id_required(api_key: &str) -> Result<String, String> {
    match cpa_home::kv::current_client() {
        Ok(None) => Ok(cached_user_id(api_key, || {
            super::cloak::fake_user_id(&cached_session_id(api_key))
        })),
        Ok(Some(_)) if api_key.is_empty() => Ok(super::cloak::fake_user_id(&new_v4())),
        Ok(Some(client)) => user_id_home(&client, api_key).await,
        Err(error) => Err(error.to_string()),
    }
}

async fn session_id_home(client: &cpa_home::Client, api_key: &str) -> Result<String, String> {
    let key = format!("cpa:claude:session-id:{}", cpa_home::kv::hash_key_part(api_key));
    home_id(
        client,
        &key,
        |v| !v.is_empty(),
        || async { Ok(new_v4()) },
        "home kv session id missing after set",
    )
    .await
}

async fn user_id_home(client: &cpa_home::Client, api_key: &str) -> Result<String, String> {
    let key = format!("cpa:claude:user-id:{}", cpa_home::kv::hash_key_part(api_key));
    home_id(
        client,
        &key,
        super::detect::valid_user_id,
        || async { Ok(super::cloak::fake_user_id(&session_id_home(client, api_key).await?)) },
        "home kv user id missing after set",
    )
    .await
}

/// Go's Home branch of the ID caches: the stored value with its TTL renewed, or a new
/// one written with SETNX and read back, so concurrent nodes settle on one value.
async fn home_id<F, Fut>(
    client: &cpa_home::Client,
    key: &str,
    valid: impl Fn(&str) -> bool,
    make: F,
    missing: &str,
) -> Result<String, String>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<String, String>>,
{
    let read = || async {
        let raw = client.kv_get(key).await.map_err(|e| e.to_string())?;
        Ok::<_, String>(
            raw.map(|raw| String::from_utf8_lossy(&raw).trim().to_owned())
                .filter(|v| valid(v)),
        )
    };
    if let Some(value) = read().await? {
        client.kv_expire(key, ID_TTL).await.map_err(|e| e.to_string())?;
        return Ok(value);
    }
    let value = make().await?;
    client
        .kv_set_nx(key, value.as_bytes(), ID_TTL)
        .await
        .map_err(|e| e.to_string())?;
    read().await?.ok_or_else(|| missing.to_owned())
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

    /// Go `CachedSessionIDRequired` / `CachedUserIDRequired` in Home mode, recorded by
    /// the reference's zz_rustgolden_test.go: the KV calls in order (fresh UUIDs and
    /// user IDs normalized), the value or error, and whether a new user ID carries
    /// the session ID stored for the key.
    #[tokio::test]
    async fn home_kv_ids_match_go() {
        use std::sync::Arc;
        const VALID_SESSION: &str = "11111111-2222-4333-8444-555555555555";
        let valid_user = format!(
            r#"{{"device_id":"{}","account_uuid":"","session_id":"{VALID_SESSION}"}}"#,
            "ab".repeat(32)
        );
        let normalize = |v: &str| -> String {
            if uuid::Uuid::parse_str(v).is_ok() && v.len() == 36 && v != VALID_SESSION {
                "<uuid>".into()
            } else if super::super::detect::valid_user_id(v) && v != valid_user {
                "<user_id>".into()
            } else {
                v.to_owned()
            }
        };
        let golden: serde_json::Value = serde_json::from_str(include_str!("testdata/go_claude_ids_home.json")).unwrap();
        for case in golden["cases"].as_array().unwrap() {
            let scenario = &case["scenario"];
            let name = scenario["name"].as_str().unwrap();
            let api_key = scenario["api_key"].as_str().unwrap();
            let values = Arc::new(Mutex::new(HashMap::new()));
            if let Some(preset) = scenario["preset"].as_object() {
                for (k, v) in preset {
                    values.lock().unwrap().insert(k.clone(), v.as_str().unwrap().to_owned());
                }
            }
            let home = cpa_home::fake::FakeHome::start(super::super::kv_test::kv_home(values.clone())).await;
            let client = home.client();
            let got = match (scenario["kind"].as_str().unwrap(), api_key.is_empty()) {
                ("session", true) => Ok(new_v4()),
                ("session", false) => session_id_home(&client, api_key).await,
                (_, true) => Ok(super::super::cloak::fake_user_id(&new_v4())),
                (_, false) => user_id_home(&client, api_key).await,
            };
            let calls: Vec<serde_json::Value> = home
                .commands()
                .iter()
                .filter_map(|c| super::super::kv_test::as_go_call(c))
                .map(|mut call| {
                    if call[0] == "setnx" {
                        call[2] = normalize(call[2].as_str().unwrap()).into();
                    }
                    call
                })
                .collect();
            assert_eq!(&calls, case["calls"].as_array().unwrap(), "{name}: calls");
            match got {
                Ok(value) => {
                    assert_eq!(normalize(&value), case["value"].as_str().unwrap(), "{name}: value");
                    if let Some(carries) = case["carries_stored_session"].as_bool() {
                        let key = format!("cpa:claude:session-id:{}", cpa_home::kv::hash_key_part(api_key));
                        let stored = values.lock().unwrap().get(&key).cloned().unwrap_or_default();
                        let stored = stored.trim();
                        assert_eq!(!stored.is_empty() && value.contains(stored), carries, "{name}: session");
                    }
                }
                Err(error) => assert_eq!(error, case["error"].as_str().unwrap(), "{name}: error"),
            }
        }
    }
}
