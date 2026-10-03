//! Plugin quota routes (internal/api/handlers/management/plugin_quota.go): the v0
//! `quota/providers`, `quota/fetch` and `quota/reset`, and per-plugin quota under
//! `plugins/{id}/quota` (v0 and v8).
//! ponytail: the declarative `quota_probe` fallback of `quota/fetch` is not ported yet;
//! a credential without a plugin quota provider answers 501 as when it has no probe.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use cpa_core::config::credentials;
use cpa_core::credential::Credential;
use cpa_plugin::api::{QuotaFetchRequest, QuotaResetRequest};
use cpa_plugin::auth::AuthView;
use serde_json::{Value, json};

use super::Management;
use super::api_call::Members;

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
    fn decode(body: &[u8]) -> Option<Self> {
        const FIELDS: [&str; 5] = ["auth_index", "authIndex", "AuthIndex", "plugin_id", "provider"];
        let Members(members) = serde_json::Deserializer::from_slice(body)
            .into_iter::<Members>()
            .next()?
            .ok()?;
        let mut out = Self::default();
        for (key, v) in members.unwrap_or_default() {
            let field = FIELDS
                .iter()
                .position(|f| *f == key)
                .or_else(|| FIELDS.iter().position(|f| f.eq_ignore_ascii_case(&key)));
            let text = |slot: &mut String| match v.clone() {
                Value::String(s) => {
                    *slot = s;
                    Some(())
                }
                Value::Null => Some(()),
                _ => None,
            };
            match field {
                Some(i @ 0..=2) => {
                    out.auth_index[i] = match v {
                        Value::String(s) => Some(s),
                        Value::Null => None,
                        _ => return None,
                    }
                }
                Some(3) => text(&mut out.plugin_id)?,
                Some(4) => text(&mut out.provider)?,
                _ => {}
            }
        }
        Some(out)
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
    match serde_json::from_slice::<Value>(&data).ok()? {
        Value::Object(_) | Value::Null => {}
        _ => return None,
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
        Ok(Some(resp)) => go_json(StatusCode::OK, cpa_plugin::gojson::to_vec(&resp)),
        Err(e) => fail(StatusCode::BAD_GATEWAY, &format!("failed to fetch quota: {e}")),
        Ok(None) => fail(
            StatusCode::NOT_IMPLEMENTED,
            "no quota provider available for credential",
        ),
    }
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

/// `?auth_index=` then `?authIndex=` (first values, trimmed).
fn query_index(query: &[(String, String)]) -> String {
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
    Query(query): Query<Vec<(String, String)>>,
) -> Response {
    let index = query_index(&query);
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
    Query(query): Query<Vec<(String, String)>>,
    body: Bytes,
) -> Response {
    let plugin_id = id.trim();
    let mut index = query_index(&query);
    if index.is_empty() {
        index = QuotaBody::decode(&body).unwrap_or_default().auth_index();
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
