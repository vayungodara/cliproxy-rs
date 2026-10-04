//! Plugin quota routes (internal/api/handlers/management/plugin_quota.go): the v0
//! `quota/providers`, `quota/fetch` and `quota/reset`, and per-plugin quota under
//! `plugins/{id}/quota` (v0 and v8).
//! Without a plugin quota provider, `quota/fetch` falls back to the credential's
//! declarative `quota_probe` (management/quota_probe.rs).

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path as UrlPath, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use cpa_core::config::credentials;
use cpa_core::credential::Credential;
use cpa_plugin::api::{QuotaFetchRequest, QuotaResetRequest};
use cpa_plugin::auth::AuthView;
use cpa_plugin::gojson::{self as pjson, Node};
use serde_json::{Value, json};

use super::Management;
use super::plugins::go_query;

fn map_json(status: StatusCode, value: &Value) -> Response {
    go_json(status, crate::gojson::sorted(value).into_bytes())
}

fn go_json(status: StatusCode, body: Vec<u8>) -> Response {
    (
        status,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json; charset=utf-8"),
        )],
        body,
    )
        .into_response()
}

fn fail(status: StatusCode, message: &str) -> Response {
    map_json(status, &json!({"error": message}))
}

/// Go `credentialQuotaRequest` as gin's `ShouldBindJSON` fills it: exact field names
/// first, then case-insensitive in struct order; `None` when binding fails.
#[derive(Default)]
struct QuotaBody {
    auth_index: [Option<String>; 3],
    plugin_id: String,
    provider: String,
}

impl QuotaBody {
    /// gin `ShouldBindJSON`: `None` when binding fails.
    fn decode(body: &[u8]) -> Option<Self> {
        let (out, ok) = Self::bind(body);
        ok.then_some(out)
    }

    /// `json.Decoder.Decode` into `credentialQuotaRequest`: the first JSON value only;
    /// exact field names first, then case-insensitive in struct order. A type error
    /// leaves that field as it was and decoding goes on (Go reports it at the end);
    /// a syntax error yields nothing.
    fn bind(body: &[u8]) -> (Self, bool) {
        const FIELDS: [&str; 5] = ["auth_index", "authIndex", "AuthIndex", "plugin_id", "provider"];
        let mut out = Self::default();
        let fields = match pjson::parse(first_value(body)) {
            Ok(mut node) => match &mut node {
                Node::Object(fields) => std::mem::take(fields),
                Node::Null => return (out, true),
                _ => return (out, false),
            },
            Err(_) => return (out, false),
        };
        let mut ok = true;
        for (key, mut v) in fields {
            let field = FIELDS
                .iter()
                .position(|f| *f == key)
                .or_else(|| FIELDS.iter().position(|f| f.eq_ignore_ascii_case(&key)));
            match (field, &mut v) {
                (Some(i @ 0..=2), Node::String(s)) => out.auth_index[i] = Some(std::mem::take(s)),
                (Some(i @ 0..=2), Node::Null) => out.auth_index[i] = None,
                (Some(3), Node::String(s)) => out.plugin_id = std::mem::take(s),
                (Some(4), Node::String(s)) => out.provider = std::mem::take(s),
                (Some(_), Node::Null) | (None, _) => {}
                (Some(_), _) => ok = false,
            }
        }
        (out, ok)
    }

    /// Go `resolveAuthIndex`.
    fn auth_index(&self) -> String {
        self.auth_index
            .iter()
            .flatten()
            .map(|s| s.trim())
            .find(|s| !s.is_empty())
            .unwrap_or_default()
            .to_owned()
    }
}

/// The bytes of the first JSON value (`json.Decoder` ignores what follows it).
fn first_value(body: &[u8]) -> &[u8] {
    let start = body.iter().position(|b| !b.is_ascii_whitespace()).unwrap_or(body.len());
    let rest = &body[start..];
    let (mut depth, mut in_string, mut escaped) = (0usize, false, false);
    for (i, &b) in rest.iter().enumerate() {
        if in_string {
            match (escaped, b) {
                (true, _) => escaped = false,
                (false, b'\\') => escaped = true,
                (false, b'"') => {
                    in_string = false;
                    if depth == 0 {
                        return &rest[..=i];
                    }
                }
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return &rest[..=i];
                }
            }
            b' ' | b'\t' | b'\r' | b'\n' | b',' if depth == 0 => return &rest[..i],
            _ => {}
        }
    }
    rest
}

/// Go `json.Unmarshal` into `map[string]any`: an object (or null) whose numbers all
/// fit a float64.
fn go_object(data: &[u8]) -> bool {
    fn numbers_fit(n: &Node) -> bool {
        match n {
            Node::Number(lit) => cpa_plugin::cli::parse_float(lit).is_some(),
            Node::Array(items) => items.iter().all(numbers_fit),
            Node::Object(fields) => fields.iter().all(|(_, v)| numbers_fit(v)),
            _ => true,
        }
    }
    matches!(pjson::parse(data), Ok(n @ (Node::Object(_) | Node::Null)) if numbers_fit(&n))
}

fn credential(state: &Management, index: &str) -> Option<Arc<Credential>> {
    state
        .rt
        .store()
        .snapshot()
        .into_iter()
        .find(|c| credentials::auth_index(c) == index)
}

/// Go `authPhysicalJSONByIndex`: the credential with its auth file's bytes, when the
/// `path` attribute names a readable JSON object file; `None` otherwise (the host then
/// fills nothing from it).
fn physical(c: &Credential) -> Option<AuthView> {
    let path = c.attributes.get("path").map(|p| p.trim()).filter(|p| !p.is_empty())?;
    let data = std::fs::read(path).ok()?;
    if data.iter().all(u8::is_ascii_whitespace) {
        return None;
    }
    if !go_object(&data) {
        return None;
    }
    Some(AuthView {
        id: c.id.clone(),
        provider: c.provider.clone(),
        storage_json: Bytes::from(data),
        metadata: c.metadata.clone().into_iter().collect(),
        attributes: go_attributes(c),
        ..Default::default()
    })
}

/// Go `Auth.Attributes` of a credential. cpa-core keeps Go's `Auth.Prefix` and
/// `Auth.ProxyURL` fields as the `prefix` and `proxy_url` attributes; Go's synthesizers
/// never put those keys in the attribute map plugins see.
pub(crate) fn go_attributes(c: &Credential) -> std::collections::BTreeMap<String, String> {
    let mut attributes = c.attributes.clone();
    attributes.remove("prefix");
    attributes.remove("proxy_url");
    attributes
}

fn fetch_request(c: &Credential, provider: &str) -> QuotaFetchRequest {
    QuotaFetchRequest {
        auth_index: credentials::auth_index(c),
        auth_id: c.id.clone(),
        provider: provider.to_owned(),
        metadata: c.metadata.clone().into_iter().collect(),
        attributes: go_attributes(c),
        ..Default::default()
    }
}

fn reset_request(c: &Credential, provider: &str) -> QuotaResetRequest {
    QuotaResetRequest {
        auth_index: credentials::auth_index(c),
        auth_id: c.id.clone(),
        provider: provider.to_owned(),
        metadata: c.metadata.clone().into_iter().collect(),
        attributes: go_attributes(c),
        ..Default::default()
    }
}

/// `GET /v0/management/quota/providers` (Go `GetQuotaProviders`).
pub(crate) async fn providers(State(state): State<Arc<Management>>) -> Response {
    let mut list = Vec::new();
    for p in state.rt.plugins().quota_providers().await {
        let mut v = json!({"plugin_id": p.plugin_id, "provider": p.provider});
        let map = v.as_object_mut().expect("object");
        if !p.display_name.is_empty() {
            map.insert("display_name".into(), p.display_name.into());
        }
        if !p.supported_providers.is_empty() {
            map.insert("supported_providers".into(), p.supported_providers.into());
        }
        map.insert("supports_reset".into(), p.supports_reset.into());
        list.push(v);
    }
    // gin.H: the top level is a map, each provider a struct in field order.
    let mut body = String::from("{\"providers\":[");
    for (i, p) in list.iter().enumerate() {
        if i > 0 {
            body.push(',');
        }
        body.push_str(&super::plugins::ordered_json(p));
    }
    body.push_str("]}");
    go_json(StatusCode::OK, body.into_bytes())
}

/// `POST /v0/management/quota/fetch` (Go `FetchCredentialQuota`).
pub(crate) async fn fetch(State(state): State<Arc<Management>>, body: Bytes) -> Response {
    let Some(body) = QuotaBody::decode(&body) else {
        return fail(StatusCode::BAD_REQUEST, "invalid request body");
    };
    let index = body.auth_index();
    if index.is_empty() {
        return fail(StatusCode::BAD_REQUEST, "auth_index is required");
    }
    let Some(auth) = credential(&state, &index) else {
        return fail(StatusCode::NOT_FOUND, "auth not found");
    };
    let plugin_id = body.plugin_id.trim();
    let provider = match body.provider.trim() {
        "" => auth.provider.clone(),
        p => p.to_owned(),
    };
    let host = state.rt.plugins();
    let req = fetch_request(&auth, &provider);
    let view = physical(&auth);
    let scope = Default::default();
    let result = if plugin_id.is_empty() {
        host.fetch_quota(req, view.as_ref(), &scope).await
    } else {
        host.fetch_quota_by_plugin(plugin_id, req, view.as_ref(), &scope).await
    };
    match result {
        Ok(Some(resp)) => return go_json(StatusCode::OK, cpa_plugin::gojson::to_vec(&resp)),
        Err(e) => return fail(StatusCode::BAD_GATEWAY, &format!("failed to fetch quota: {e}")),
        Ok(None) => {}
    }
    // Go: the declarative quota probe, when the metadata declares one.
    if let Some(probe) = auth.metadata.get("quota_probe").and_then(Value::as_object) {
        match super::quota_probe::execute(&state, &auth, probe).await {
            Some(Ok(resp)) => return go_json(StatusCode::OK, cpa_plugin::gojson::to_vec(&resp)),
            Some(Err(e)) => return fail(StatusCode::BAD_GATEWAY, &format!("quota probe failed: {e}")),
            None => {}
        }
    }
    fail(
        StatusCode::NOT_IMPLEMENTED,
        "no quota provider available for credential",
    )
}

/// `POST /v0/management/quota/reset` (Go `ResetCredentialQuota`).
pub(crate) async fn reset(State(state): State<Arc<Management>>, body: Bytes) -> Response {
    let Some(body) = QuotaBody::decode(&body) else {
        return fail(StatusCode::BAD_REQUEST, "invalid request body");
    };
    let index = body.auth_index();
    if index.is_empty() {
        return fail(StatusCode::BAD_REQUEST, "auth_index is required");
    }
    let Some(auth) = credential(&state, &index) else {
        return fail(StatusCode::NOT_FOUND, "auth not found");
    };
    let plugin_id = body.plugin_id.trim();
    let provider = match body.provider.trim() {
        "" => auth.provider.clone(),
        p => p.to_owned(),
    };
    let host = state.rt.plugins();
    let req = reset_request(&auth, &provider);
    let view = physical(&auth);
    let scope = Default::default();
    let result = if !plugin_id.is_empty() {
        if !host.has_quota_provider_for_plugin(plugin_id) {
            return fail(StatusCode::NOT_FOUND, "quota provider not found for plugin");
        }
        match host.reset_quota_by_plugin(plugin_id, req, view.as_ref(), &scope).await {
            Ok(None) => return fail(StatusCode::NOT_FOUND, "quota provider not found for plugin"),
            other => other,
        }
    } else {
        if !host.has_quota_provider(&provider).await {
            return fail(
                StatusCode::NOT_IMPLEMENTED,
                "no quota provider available for credential to reset",
            );
        }
        match host.reset_quota(req, view.as_ref(), &scope).await {
            Ok(None) => return fail(StatusCode::BAD_GATEWAY, "quota provider did not handle reset request"),
            other => other,
        }
    };
    let resp = match result {
        Err(e) => return fail(StatusCode::BAD_GATEWAY, &format!("plugin quota reset failed: {e}")),
        Ok(resp) => resp.unwrap_or_default(),
    };
    finish_reset(&state, &auth, &index, resp, "quota reset rejected by provider")
}

/// The success tail both resets share: a rejected reset is 502; otherwise the
/// credential's routing quota is cleared (Go `authManager.ResetQuota`).
fn finish_reset(
    state: &Management,
    auth: &Credential,
    index: &str,
    resp: cpa_plugin::api::QuotaResetResponse,
    rejected: &str,
) -> Response {
    if !resp.success {
        let message = if resp.message.is_empty() {
            rejected
        } else {
            &resp.message
        };
        return fail(StatusCode::BAD_GATEWAY, message);
    }
    state.rt.store().reset_cooldowns(&auth.id);
    let mut out = json!({"status": "ok", "auth_index": index});
    if !resp.message.is_empty() {
        out["message"] = resp.message.into();
    }
    map_json(StatusCode::OK, &out)
}

/// `c.Query("auth_index")`, else `c.Query("authIndex")`: first values from Go's
/// `URL.Query()`, trimmed.
fn query_index(raw: Option<&str>) -> String {
    let query = go_query(raw.unwrap_or_default());
    ["auth_index", "authIndex"]
        .iter()
        .filter_map(|k| query.iter().find(|(name, _)| name == k).map(|(_, v)| v.trim()))
        .find(|v| !v.is_empty())
        .unwrap_or_default()
        .to_owned()
}

/// `GET /plugins/{id}/quota` (Go `GetPluginQuota`). The ID is not validated, as in Go.
pub(crate) async fn get_plugin(
    State(state): State<Arc<Management>>,
    UrlPath(id): UrlPath<String>,
    uri: axum::http::Uri,
) -> Response {
    let index = query_index(uri.query());
    if index.is_empty() {
        return fail(StatusCode::BAD_REQUEST, "auth_index is required");
    }
    fetch_for_plugin(&state, id.trim(), &index).await
}

/// `POST /plugins/{id}/quota` (Go `FetchPluginQuota`).
pub(crate) async fn fetch_plugin(
    State(state): State<Arc<Management>>,
    UrlPath(id): UrlPath<String>,
    body: Bytes,
) -> Response {
    let Some(body) = QuotaBody::decode(&body) else {
        return fail(StatusCode::BAD_REQUEST, "invalid request body");
    };
    let index = body.auth_index();
    if index.is_empty() {
        return fail(StatusCode::BAD_REQUEST, "auth_index is required");
    }
    fetch_for_plugin(&state, id.trim(), &index).await
}

async fn fetch_for_plugin(state: &Management, plugin_id: &str, index: &str) -> Response {
    let Some(auth) = credential(state, index) else {
        return fail(StatusCode::NOT_FOUND, "auth not found");
    };
    let host = state.rt.plugins();
    if !host.has_quota_provider_for_plugin(plugin_id) {
        return fail(StatusCode::NOT_FOUND, "quota provider not found for plugin");
    }
    let req = fetch_request(&auth, &auth.provider);
    let view = physical(&auth);
    match host
        .fetch_quota_by_plugin(plugin_id, req, view.as_ref(), &Default::default())
        .await
    {
        Ok(Some(resp)) => go_json(StatusCode::OK, cpa_plugin::gojson::to_vec(&resp)),
        Ok(None) => fail(StatusCode::NOT_FOUND, "quota provider not found for plugin"),
        Err(e) => fail(StatusCode::BAD_GATEWAY, &format!("failed to fetch quota: {e}")),
    }
}

/// `DELETE /plugins/{id}/quota` and v0 `POST /plugins/{id}/quota/reset` (Go
/// `ResetPluginQuota`): the auth index from the query, else the body (bind errors
/// ignored).
pub(crate) async fn reset_plugin(
    State(state): State<Arc<Management>>,
    UrlPath(id): UrlPath<String>,
    req: Request,
) -> Response {
    let plugin_id = id.trim();
    let mut index = query_index(req.uri().query());
    if index.is_empty() {
        // Go reads the body only here and ignores binding errors, keeping whatever
        // fields did bind.
        let body = axum::body::to_bytes(req.into_body(), usize::MAX)
            .await
            .unwrap_or_default();
        index = QuotaBody::bind(&body).0.auth_index();
    }
    if index.is_empty() {
        return fail(StatusCode::BAD_REQUEST, "auth_index is required");
    }
    let Some(auth) = credential(&state, &index) else {
        return fail(StatusCode::NOT_FOUND, "auth not found");
    };
    let host = state.rt.plugins();
    if !host.has_quota_provider_for_plugin(plugin_id) {
        return fail(StatusCode::NOT_FOUND, "quota provider not found for plugin");
    }
    let req = reset_request(&auth, &auth.provider);
    let view = physical(&auth);
    let resp = match host
        .reset_quota_by_plugin(plugin_id, req, view.as_ref(), &Default::default())
        .await
    {
        Ok(Some(resp)) => resp,
        Ok(None) => return fail(StatusCode::NOT_FOUND, "quota provider not found for plugin"),
        Err(e) => return fail(StatusCode::BAD_GATEWAY, &format!("failed to reset quota: {e}")),
    };
    finish_reset(&state, &auth, &index, resp, "quota reset rejected by plugin")
}
