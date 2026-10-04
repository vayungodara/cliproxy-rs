//! A request's session identity for affinity and executors (Go `session.Enrich` and
//! `SessionAffinitySelector.Pick`'s identity step). The extraction rules live in
//! `cpa_common::session` so executors share them.

use axum::http::HeaderMap;
use cpa_common::session::{self as identity, Meta};
use cpa_core::format::Format;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Session {
    /// Go `ExtractSessionID`: the affinity session and `ExecRequest::session`.
    pub id: Option<String>,
    /// The parent or prompt-cache conversation alias (Go fallback ID).
    pub parent: Option<String>,
    /// The session forked from `parent`.
    pub fork: bool,
    /// Go `derived_session_id` (see `ExecRequest::derived_session`).
    pub derived: Option<String>,
    /// A client header, body field or execution session named the session (Go
    /// `extractExplicitSessionIDs`). Without one, session affinity asks the LCP matcher
    /// first (`crate::lcp`).
    pub explicit: bool,
}

/// Resolves the session of a request in `format` from its client headers and body.
/// `caller` is the matched client key; it scopes the derived identity.
pub fn resolve(format: Format, headers: &HeaderMap, body: &[u8], execution: Option<&str>, caller: &str) -> Session {
    let scope = identity::caller_scope(caller);
    let derived = identity::derived_id(format, headers, body, execution, &scope);
    let meta = Meta {
        execution_session: execution,
        derived: derived.as_deref(),
    };
    let (mut primary, mut parent, fork) = identity::explicit_session_ids(headers, body, &meta);
    let explicit = !primary.is_empty();
    if !explicit {
        (primary, parent) = identity::session_ids(headers, body, &meta);
    }
    let some = |s: String| (!s.is_empty()).then(|| identity::bound_session_identity(&s));
    Session {
        id: some(primary),
        parent: some(parent),
        fork,
        derived,
        explicit,
    }
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderName, HeaderValue};
    use cpa_common::session::{self as identity, Meta};
    use cpa_core::format::Format;
    use serde_json::Value;

    /// Goldens from Go `session.ExtractSessionInfo`, `session.Enrich` and
    /// `auth.ExtractSessionID` (tests/reference/server/main.go).
    #[test]
    fn identity_matches_go() {
        let fixture: Value = serde_json::from_str(include_str!("../tests/fixtures/server_go.json")).unwrap();
        let cases = fixture["session"].as_array().unwrap();
        assert!(cases.len() >= 50);
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let mut headers = super::HeaderMap::new();
            for pair in case["headers"].as_array().into_iter().flatten() {
                let name = HeaderName::from_bytes(pair[0].as_str().unwrap().as_bytes()).unwrap();
                // Values hyper would reject (control characters) never reach a route.
                if let Ok(value) = HeaderValue::from_bytes(pair[1].as_str().unwrap().as_bytes()) {
                    headers.append(name, value);
                }
            }
            let payload = case["payload"].as_str().unwrap().as_bytes();
            let execution = case["execution"].as_str().filter(|s| !s.is_empty());
            let format = Format::parse(case["format"].as_str().unwrap()).unwrap();
            let scope = identity::caller_scope(case["client_key"].as_str().unwrap());
            assert_eq!(scope, case["caller_scope"].as_str().unwrap(), "{name} caller scope");

            let info = identity::extract_session_info(&headers, payload, execution);
            match (&info, case["info"].as_object()) {
                (None, None) => {}
                (Some(info), Some(go)) => {
                    let s = |k: &str| go.get(k).and_then(Value::as_str).unwrap_or_default();
                    let b = |k: &str| go.get(k).and_then(Value::as_bool).unwrap_or_default();
                    assert_eq!(info.session_id, s("session_id"), "{name} session");
                    assert_eq!(info.parent_session_id, s("parent_session_id"), "{name} parent");
                    assert_eq!(info.agent_name, s("agent_name"), "{name} agent");
                    assert_eq!(info.client_type, s("client_type"), "{name} client");
                    assert_eq!(info.is_fork, b("is_fork"), "{name} fork");
                    assert_eq!(info.is_subagent, b("is_subagent"), "{name} subagent");
                }
                (rust, go) => panic!("{name}: rust {rust:?}, go {go:?}"),
            }

            let derived = identity::derived_id(format, &headers, payload, execution, &scope);
            assert_eq!(
                derived.as_deref().unwrap_or_default(),
                case["derived"].as_str().unwrap(),
                "{name} derived"
            );
            // Enrich's canonical and parent metadata: the explicit (or execution) session.
            let explicit = execution.is_some() || identity::has_explicit_session(&headers, payload);
            let canonical = info.as_ref().filter(|_| explicit);
            assert_eq!(
                canonical.map_or("", |i| i.session_id.as_str()),
                case["canonical"].as_str().unwrap(),
                "{name} canonical"
            );
            assert_eq!(
                canonical.map_or("", |i| i.parent_session_id.as_str()),
                case["parent"].as_str().unwrap(),
                "{name} parent metadata"
            );
            let meta = Meta {
                execution_session: execution,
                derived: derived.as_deref(),
            };
            assert_eq!(
                identity::extract_session_id(&headers, payload, &meta),
                case["session_id"].as_str().unwrap(),
                "{name} session id"
            );
            let bare = Meta {
                execution_session: execution,
                derived: None,
            };
            assert_eq!(
                identity::extract_session_id(&headers, payload, &bare),
                case["hash_session_id"].as_str().unwrap(),
                "{name} hash session id"
            );
            let resolved = super::resolve(
                format,
                &headers,
                payload,
                execution,
                case["client_key"].as_str().unwrap(),
            );
            assert_eq!(
                resolved.id.as_deref().unwrap_or_default(),
                case["session_id"].as_str().unwrap(),
                "{name} resolved"
            );
        }
    }
}
