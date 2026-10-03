//! Claude Code credential identity in metadata.user_id
//! (helps/claude_credential_identity.go, helps/claude_cli_identity_seed.go,
//! internal/auth/claude/identity.go).

use cpa_core::credential::Credential;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::session::hex;
use crate::rawjson;

fn valid_device(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `NormalizeDeviceIDPool` with a pool size of one.
fn first_device(raw: Option<&Value>) -> Option<String> {
    raw?.as_array()?
        .iter()
        .filter_map(Value::as_str)
        .map(|s| s.trim().to_lowercase())
        .find(|s| valid_device(s))
}

pub(crate) fn account_uuid(credential: &Credential) -> String {
    ["account_uuid", "accountUuid"]
        .iter()
        .filter_map(|k| credential.str(k))
        .map(str::trim)
        .find(|s| !s.is_empty())
        .unwrap_or_default()
        .to_owned()
}

/// Device and account used on the wire. API keys with the CLI fingerprint profile get
/// a stable identity derived from `seed` when the credential has none of its own.
pub(crate) fn wire_identity(credential: &Credential, seed: &str, synthesize: bool) -> (String, String) {
    let seed = if seed.trim().is_empty() {
        "anonymous"
    } else {
        seed.trim()
    };
    let mut account = account_uuid(credential);
    if synthesize && account.is_empty() {
        let namespace = uuid::Uuid::parse_str("6ba7b812-9dad-11d1-80b4-00c04fd430c8").expect("constant");
        account = uuid::Uuid::new_v5(&namespace, format!("cpa-claude-code-cli-account|{seed}").as_bytes()).to_string();
    }
    let raw = credential.metadata.get("claude_device_ids");
    let canonical = raw
        .and_then(Value::as_array)
        .is_some_and(|a| a.len() == 1 && a[0].as_str().is_some_and(|s| first_device(raw).as_deref() == Some(s)));
    let device = if synthesize && !canonical {
        hex(&Sha256::digest(format!("cpa-claude-code-cli-device|{seed}").as_bytes()))
    } else {
        // ponytail: an OAuth credential reaching execute without a pool (prepare not yet
        // committed) gets a request-local random device, as Go's in-memory ensure does.
        first_device(raw).unwrap_or_else(|| {
            let mut bytes = [0u8; 32];
            let _ = getrandom::fill(&mut bytes);
            hex(&bytes)
        })
    };
    (device, account)
}

/// Why [`apply`] failed, typed like Go's errors so a message never decides the class.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ApplyError {
    /// A plain Go error (missing identity): no status, no request scope.
    Plain(String),
    /// `claudeCredentialMetadataRequestError`: the caller's payload is malformed (400,
    /// request-scoped).
    Request(String),
}

/// `ApplyClaudeCredentialMetadata`: rewrites metadata.user_id as
/// `{"device_id","account_uuid","session_id",...caller extras}`.
pub(crate) fn apply(body: &str, device: &str, account: &str, session: &str) -> Result<String, ApplyError> {
    let existing = existing_user_id(body).map_err(ApplyError::Request)?;
    if session.trim().is_empty() {
        return Err(ApplyError::Plain("select Claude device ID: session ID is empty".into()));
    }
    if account.is_empty() {
        return Err(ApplyError::Plain(
            "apply Claude credential metadata: account UUID is empty".into(),
        ));
    }
    let mut out = format!(
        r#"{{"device_id":{},"account_uuid":{},"session_id":{}"#,
        rawjson::go_string(device),
        rawjson::go_string(account),
        rawjson::go_string(session)
    );
    let trimmed = existing.trim();
    if trimmed.starts_with('{') && gjson::valid(trimmed) {
        let mut seen = std::collections::HashSet::new();
        let mut duplicate = None;
        gjson::parse(trimmed).each(|k, v| {
            let key = k.str().to_owned();
            if !seen.insert(key.clone()) {
                duplicate = Some(key);
                return false;
            }
            if !matches!(key.as_str(), "device_id" | "account_uuid" | "session_id") {
                out.push(',');
                out.push_str(&rawjson::go_string(&key));
                out.push(':');
                // Go decodes into json.RawMessage, which keeps the raw bytes.
                out.push_str(v.json());
            }
            true
        });
        if let Some(key) = duplicate {
            return Err(ApplyError::Request(format!(
                "apply Claude credential metadata: metadata.user_id contains duplicate key {key:?}"
            )));
        }
    }
    out.push('}');
    Ok(rawjson::set_str(body, "metadata.user_id", &out))
}

/// The string value of a unique top-level `metadata.user_id`, rejecting duplicate
/// `metadata`/`user_id` members as a request error (Go returns 400).
fn existing_user_id(body: &str) -> Result<String, String> {
    let root = gjson::parse(body);
    if !gjson::valid(body) || root.kind() != gjson::Kind::Object {
        return Err("apply Claude credential metadata: request must be a JSON object".into());
    }
    let mut metadata = None;
    let mut dup = false;
    root.each(|k, v| {
        if k.str() == "metadata" {
            dup |= metadata.is_some();
            metadata = Some(v.json().to_owned());
        }
        true
    });
    if dup {
        return Err("apply Claude credential metadata: duplicate JSON object key \"metadata\"".into());
    }
    let Some(metadata) = metadata else {
        return Ok(String::new());
    };
    let m = gjson::parse(&metadata);
    if m.kind() != gjson::Kind::Object {
        return Ok(String::new());
    }
    let mut user = None;
    let mut dup = false;
    m.each(|k, v| {
        if k.str() == "user_id" {
            dup |= user.is_some();
            user = Some((v.kind(), v.str().to_owned()));
        }
        true
    });
    if dup {
        return Err("apply Claude credential metadata: metadata: duplicate JSON object key \"user_id\"".into());
    }
    Ok(match user {
        Some((gjson::Kind::String, s)) => s,
        _ => String::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebuild_keeps_caller_extras_after_identity() {
        let body =
            r#"{"metadata":{"user_id":"{\"session_id\":\"x\",\"parent_session_id\":\"p\",\"device_id\":\"d\"}"}}"#;
        let out = apply(body, "dev", "acct", "sess").unwrap();
        assert_eq!(
            rawjson::get(&out, "metadata.user_id").str(),
            r#"{"device_id":"dev","account_uuid":"acct","session_id":"sess","parent_session_id":"p"}"#
        );
        assert!(matches!(
            apply(r#"{"metadata":{},"metadata":{}}"#, "d", "a", "s"),
            Err(ApplyError::Request(_))
        ));
        let out = apply(r#"{"model":"m"}"#, "d", "a", "s").unwrap();
        assert_eq!(
            out,
            r#"{"model":"m","metadata":{"user_id":"{\"device_id\":\"d\",\"account_uuid\":\"a\",\"session_id\":\"s\"}"}}"#
        );
    }

    /// A caller-chosen key that spells a plain error's message stays a request error.
    #[test]
    fn error_class_is_typed_not_matched_on_text() {
        let body = r#"{"metadata":{"user_id":"{\"account UUID is empty\":1,\"account UUID is empty\":2}"}}"#;
        assert_eq!(
            apply(body, "d", "a", "s"),
            Err(ApplyError::Request(
                r#"apply Claude credential metadata: metadata.user_id contains duplicate key "account UUID is empty""#
                    .into()
            ))
        );
        assert!(matches!(apply(r#"{}"#, "d", "", "s"), Err(ApplyError::Plain(_))));
        assert!(matches!(apply(r#"{}"#, "d", "a", " "), Err(ApplyError::Plain(_))));
        assert!(matches!(apply("[]", "d", "", ""), Err(ApplyError::Request(_))));
    }
}
