//! Local ephemeral keys (client_secret.go): `POST /v1/realtime/client_secrets` and the
//! legacy `POST /v1/realtime/sessions`. A key is `ek_` + 32 random bytes, lives 10 s to
//! 2 h (default 10 min) and carries the session config every call made with it uses.
//! At most 1024 keys exist, 64 per issuing client key.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::Extension;
use axum::body::Body;
use axum::http::{HeaderValue, header};
use axum::response::Response;
use base64::Engine;
use cpa_common::json::GoValue;
use cpa_exec::codex_live::{self as live, SessionError};

use super::{Live, Principal, realtime_error};

/// `clientSecretPrefix`.
pub(super) const PREFIX: &str = "ek_";
/// `clientSecretMaxBodySize`.
const MAX_BODY: usize = 64 << 10;
/// `clientSecretMaxEntries` and `clientSecretMaxEntriesPerIssuer`.
const MAX_ENTRIES: usize = 1024;
const MAX_PER_ISSUER: usize = 64;

/// What an ephemeral key grants (`ClientSecretAuthorization`).
#[derive(Debug)]
pub(crate) struct Grant {
    /// `sess_...`: the call-scope identity of every call made with the key.
    pub principal: String,
    pub issuer_key: String,
    pub issuer_provider: String,
    /// The upstream (Codex model) session config.
    pub session: Vec<u8>,
}

#[derive(Default)]
pub(super) struct Secrets {
    entries: Mutex<HashMap<String, (Arc<Grant>, Instant)>>,
}

struct Created {
    token: String,
    grant: Arc<Grant>,
    expires_at: i64,
}

impl Secrets {
    /// `clientSecretStore.create`: `None` when the store or the issuer is at capacity.
    fn create(&self, session: Vec<u8>, lifetime: Duration, issuer_key: &str, issuer_provider: &str) -> Option<Created> {
        let token = random_id(PREFIX, 32);
        let grant = Arc::new(Grant {
            principal: random_id("sess_", 18),
            issuer_key: issuer_key.trim().to_owned(),
            issuer_provider: issuer_provider.trim().to_owned(),
            session,
        });
        let now = Instant::now();
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries.retain(|_, (_, expires)| *expires > now);
        if entries.len() >= MAX_ENTRIES {
            return None;
        }
        if !grant.issuer_key.is_empty()
            && entries
                .values()
                .filter(|(g, _)| g.issuer_key == grant.issuer_key && g.issuer_provider == grant.issuer_provider)
                .count()
                >= MAX_PER_ISSUER
        {
            return None;
        }
        entries.insert(token.clone(), (grant.clone(), now + lifetime));
        let expires_at = SystemTime::now()
            .checked_add(lifetime)
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs() as i64);
        Some(Created {
            token,
            grant,
            expires_at,
        })
    }

    /// `clientSecretStore.authenticate`: an expired key is forgotten on use.
    pub(super) fn authenticate(&self, token: &str) -> Option<Arc<Grant>> {
        if !token.starts_with(PREFIX) {
            return None;
        }
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        match entries.get(token) {
            Some((grant, expires)) if *expires > Instant::now() => Some(grant.clone()),
            _ => {
                entries.remove(token);
                None
            }
        }
    }
}

/// `randomRealtimeID`.
fn random_id(prefix: &str, size: usize) -> String {
    let mut bytes = vec![0u8; size];
    getrandom::fill(&mut bytes).expect("system randomness");
    format!(
        "{prefix}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    )
}

/// `readClientSecretBody`'s failures as `realtimeError`.
fn body_error(error: super::http::ReadError) -> Response {
    match error {
        super::http::ReadError::TooLarge => realtime_error(
            413,
            "Codex live request body too large",
            "invalid_request_error",
            "invalid_request",
        ),
        super::http::ReadError::Read(e) => realtime_error(
            400,
            &format!("failed to read Realtime client secret request: {e}"),
            "invalid_request_error",
            "invalid_request",
        ),
    }
}

/// `CreateClientSecret`.
pub(super) async fn create(
    Extension(live): Extension<Arc<Live>>,
    Extension(principal): Extension<Principal>,
    body: Body,
) -> Response {
    let body = match super::http::read_limited(body, MAX_BODY).await {
        Ok(body) => body,
        Err(error) => return body_error(error),
    };
    let Some(request) = live::decode_secret_request(&body) else {
        return realtime_error(
            400,
            "Invalid Realtime client secret request",
            "invalid_request_error",
            "invalid_request",
        );
    };
    issue(
        &live,
        &principal,
        &request.session,
        request.expires_after.as_ref(),
        false,
    )
}

/// `CreateLegacySession`: the body is the session itself.
pub(super) async fn legacy(
    Extension(live): Extension<Arc<Live>>,
    Extension(principal): Extension<Principal>,
    body: Body,
) -> Response {
    match super::http::read_limited(body, MAX_BODY).await {
        Ok(body) => issue(&live, &principal, &body, None, true),
        Err(error) => body_error(error),
    }
}

/// `createClientSecret`.
fn issue(
    live: &Live,
    principal: &Principal,
    session: &[u8],
    expires_after: Option<&(String, i64)>,
    legacy: bool,
) -> Response {
    let lifetime = match live::client_secret_lifetime(expires_after) {
        Ok(lifetime) => lifetime,
        Err(e) => return realtime_error(400, &e, "invalid_request_error", "invalid_expires_after"),
    };
    let (client, upstream) = match live::normalize_client_secret_session(session) {
        Ok(sessions) => sessions,
        Err(SessionError::Unsupported(e)) => {
            return realtime_error(501, &e, "not_supported_error", "realtime_capability_not_supported");
        }
        Err(SessionError::Invalid(e)) => return realtime_error(400, &e, "invalid_request_error", "invalid_session"),
    };
    let Some(created) = live
        .secrets
        .create(upstream, lifetime, &principal.key, &principal.provider)
    else {
        let mut response = realtime_error(
            429,
            "Realtime client secret capacity exhausted",
            "rate_limit_error",
            "realtime_client_secret_capacity_exhausted",
        );
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
        return response;
    };
    let Some(GoValue::Object(mut session)) =
        live::session_response(&client, &created.grant.principal, created.expires_at)
    else {
        return realtime_error(
            500,
            "Failed to encode Realtime session",
            "server_error",
            "realtime_session_failed",
        );
    };
    let body = if legacy {
        let secret = [
            ("expires_at".to_owned(), GoValue::Number(created.expires_at.to_string())),
            ("value".to_owned(), GoValue::String(created.token)),
        ];
        session.insert("client_secret".into(), GoValue::Object(secret.into_iter().collect()));
        GoValue::Object(session).marshal()
    } else {
        // `clientSecretCreateResponse`: struct field order.
        let mut out = b"{\"value\":".to_vec();
        cpa_common::json::marshal_str(&mut out, created.token.as_bytes(), true);
        out.extend_from_slice(format!(",\"expires_at\":{},\"session\":", created.expires_at).as_bytes());
        out.extend_from_slice(&GoValue::Object(session).marshal());
        out.push(b'}');
        out
    };
    let mut response = crate::respond::gin_json(200, String::from_utf8_lossy(&body).into_owned());
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_expire_and_capacity_is_bounded() {
        let secrets = Secrets::default();
        let created = secrets
            .create(b"{}".to_vec(), Duration::from_secs(60), "issuer", "config-inline")
            .unwrap();
        assert!(created.token.starts_with("ek_") && created.token.len() == 3 + 43);
        assert!(created.grant.principal.starts_with("sess_") && created.grant.principal.len() == 5 + 24);
        assert_eq!(
            secrets.authenticate(&created.token).unwrap().principal,
            created.grant.principal
        );
        assert!(secrets.authenticate("ek_unknown").is_none());
        assert!(secrets.authenticate(&created.token[1..]).is_none(), "prefix required");
        // Expiry on its own token: a slow runner can never see this one still valid,
        // nor the long-lived one above already expired.
        let short = secrets
            .create(b"{}".to_vec(), Duration::from_millis(1), "issuer", "config-inline")
            .unwrap();
        std::thread::sleep(Duration::from_millis(20));
        assert!(secrets.authenticate(&short.token).is_none(), "expired");

        for _ in 0..MAX_PER_ISSUER {
            assert!(secrets.create(vec![], Duration::from_secs(60), "busy", "p").is_some());
        }
        assert!(
            secrets.create(vec![], Duration::from_secs(60), "busy", "p").is_none(),
            "per issuer"
        );
        assert!(
            secrets
                .create(vec![], Duration::from_secs(60), "busy", "other")
                .is_some(),
            "issuer is key and provider"
        );
        assert!(
            secrets.create(vec![], Duration::from_secs(60), "", "").is_some(),
            "no issuer, no per-issuer limit"
        );
        while secrets.create(vec![], Duration::from_secs(60), "", "").is_some() {}
        assert_eq!(secrets.entries.lock().unwrap().len(), MAX_ENTRIES, "store-wide cap");
    }
}
