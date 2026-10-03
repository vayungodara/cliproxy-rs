//! Plugin-owned Management API and resource routes (internal/pluginhost/management.go,
//! internal/htmlsanitize).
//!
//! Plugins declare exact routes under `/v0/management/` (management-authenticated) and
//! GET resources under `/v0/resource/plugins/<id>/` (unauthenticated, browser
//! navigable). Higher-priority plugins win collisions; built-in routes always win.

use std::collections::{BTreeMap, HashSet};

use bytes::Bytes;
use cpa_common::json::GoValue;

use crate::abi::{self, method};
use crate::api::{
    ManagementRegistrationRequest, ManagementRegistrationResponse, ManagementResponse, ManagementRoute, ResourceRoute,
};
use crate::callbacks::RequestScope;
use crate::gojson::{Header, NonNilBytes};
use crate::host::Host;

pub const MANAGEMENT_BASE: &str = "/v0/management";
pub const RESOURCE_BASE: &str = "/v0/resource/plugins";
const LEGACY_PLUGIN_PREFIX: &str = "/plugins";

#[derive(Debug, Clone, PartialEq)]
pub struct RouteRecord {
    pub plugin_id: String,
    pub path: std::path::PathBuf,
    pub version: String,
    pub schema_version: u32,
    pub route: ManagementRoute,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResourceRecord {
    pub plugin_id: String,
    pub path: std::path::PathBuf,
    pub version: String,
    pub route: ResourceRoute,
}

/// Go `managementRouteKey`.
pub fn route_key(method: &str, path: &str) -> String {
    format!("{} {}", method.trim().to_ascii_uppercase(), path.trim())
}

fn has_space(s: &str) -> bool {
    s.contains([' ', '\t', '\r', '\n'])
}

/// Go `normalizeManagementRoute`: `(METHOD, /v0/management/...)`.
pub fn normalize_route(route: &ManagementRoute) -> Option<(String, String)> {
    let mut method = route.method.trim().to_ascii_uppercase();
    if method.is_empty() {
        method = "GET".into();
    }
    if has_space(&method) {
        return None;
    }
    let mut path = route.path.trim().to_owned();
    if path.is_empty() {
        return None;
    }
    if !path.starts_with('/') {
        path.insert(0, '/');
    }
    if path.starts_with(&format!("{MANAGEMENT_BASE}/")) {
        path = path[MANAGEMENT_BASE.len()..].to_owned();
    }
    let path = path.trim_end_matches('/');
    if path.is_empty() {
        return None;
    }
    let full = format!("{MANAGEMENT_BASE}{path}");
    if !full.starts_with(&format!("{MANAGEMENT_BASE}/")) || has_space(&full) || full.contains(':') || full.contains('*')
    {
        return None;
    }
    Some((method, full))
}

/// Go `normalizeResourceRoute`.
pub fn normalize_resource(plugin_id: &str, route: &ResourceRoute) -> Option<String> {
    let plugin_id = plugin_id.trim();
    if plugin_id.is_empty() {
        return None;
    }
    let mut path = route.path.trim().to_owned();
    if path.is_empty() {
        return None;
    }
    if !path.starts_with('/') {
        path.insert(0, '/');
    }
    let base = format!("{RESOURCE_BASE}/{plugin_id}");
    let legacy = format!("{LEGACY_PLUGIN_PREFIX}/{plugin_id}");
    if path.starts_with(&format!("{base}/")) {
        path = path[base.len()..].to_owned();
    } else if path.starts_with(&format!("{legacy}/")) {
        path = path[legacy.len()..].to_owned();
    }
    let path = path.trim_end_matches('/');
    if path.is_empty() {
        return None;
    }
    let full = format!("{base}{path}");
    if !full.starts_with(&format!("{base}/"))
        || has_space(&full)
        || full.contains(':')
        || full.contains('*')
        || full.contains("..")
    {
        return None;
    }
    Some(full)
}

/// A response for the HTTP layer to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub status: u16,
    /// In the plugin's order; names as the plugin spelled them (Go canonicalizes on
    /// write).
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
}

impl Reply {
    /// Go `http.Error`.
    pub fn error(status: u16, message: &str) -> Self {
        Self {
            status,
            headers: vec![
                ("Content-Type".into(), "text/plain; charset=utf-8".into()),
                ("X-Content-Type-Options".into(), "nosniff".into()),
            ],
            body: Bytes::from(format!("{message}\n")),
        }
    }

    fn from_plugin(resp: ManagementResponse) -> Self {
        let headers = resp
            .headers
            .into_iter()
            .flat_map(|(name, values)| values.into_iter().map(move |v| (name.clone(), v)))
            .collect();
        Self {
            status: if resp.status_code == 0 {
                200
            } else {
                u16::try_from(resp.status_code).unwrap_or(500)
            },
            headers,
            body: resp.body,
        }
    }
}

/// One inbound management or resource request.
#[derive(Debug, Clone, Default)]
pub struct Inbound {
    pub method: String,
    /// The decoded URL path.
    pub path: String,
    /// Go canonical header names.
    pub headers: Header,
    pub query: Header,
    pub body: Bytes,
    pub scope: RequestScope,
}

crate::go_struct! {
    /// `rpcManagementRequest`.
    pub struct ManagementCall("pluginhost.rpcManagementRequest") {
        "Method" => method: String,
        "Path" => path: String,
        "Headers" => headers: Header,
        "Query" => query: Header,
        /// Go reads management bodies with `io.ReadAll` (non-nil); resource requests
        /// carry none (nil).
        "Body" => body: Option<NonNilBytes>,
        "host_callback_id" omitempty => host_callback_id: String,
    }
}

impl Host {
    /// Go `RegisterManagementRoutes`: asks every active plugin with the capability for its
    /// routes and rebuilds both tables. `reserved` holds built-in `METHOD /path` keys.
    pub async fn register_management_routes(&self, reserved: &HashSet<String>) {
        let mut routes: BTreeMap<String, RouteRecord> = BTreeMap::new();
        let mut resources: BTreeMap<String, ResourceRecord> = BTreeMap::new();
        for record in self.active_records() {
            if !record.plugin.caps.management_api || self.is_fused(&record.id) || !self.record_current(&record) {
                continue;
            }
            let request = ManagementRegistrationRequest {
                plugin: record.plugin.metadata.clone(),
                base_path: MANAGEMENT_BASE.into(),
                resource_base_path: format!("{RESOURCE_BASE}/{}", record.id),
            };
            let resp: ManagementRegistrationResponse =
                match self.call(&record, method::MANAGEMENT_REGISTER, &request).await {
                    Ok(resp) => resp,
                    Err(e) => {
                        tracing::warn!("pluginhost: management registrar {} failed: {e}", record.id);
                        continue;
                    }
                };
            let register_resource = |resources: &mut BTreeMap<String, ResourceRecord>, route: ResourceRoute| -> bool {
                let Some(path) = normalize_resource(&record.id, &route) else {
                    return false;
                };
                let key = route_key("GET", &path);
                if resources.contains_key(&key) {
                    tracing::warn!(
                        "pluginhost: plugin {} resource route {key} conflicts with a higher-priority plugin and was skipped",
                        record.id
                    );
                    return true;
                }
                resources.insert(
                    key,
                    ResourceRecord {
                        plugin_id: record.id.clone(),
                        path: record.path.clone(),
                        version: record.version.clone(),
                        route: ResourceRoute { path, ..route },
                    },
                );
                true
            };
            for item in resp.routes {
                let Some((method, path)) = normalize_route(&item) else {
                    tracing::warn!(
                        "pluginhost: plugin {} declared invalid management route {} {}",
                        record.id,
                        item.method,
                        item.path
                    );
                    continue;
                };
                if method.eq_ignore_ascii_case("GET") && !item.menu.trim().is_empty() {
                    let resource = ResourceRoute {
                        path: item.path.clone(),
                        menu: item.menu.clone(),
                        description: item.description.clone(),
                    };
                    if !register_resource(&mut resources, resource) {
                        tracing::warn!(
                            "pluginhost: plugin {} declared invalid resource route {}",
                            record.id,
                            item.path
                        );
                    }
                    continue;
                }
                let key = route_key(&method, &path);
                if reserved.contains(&key) {
                    tracing::warn!(
                        "pluginhost: plugin {} management route {key} conflicts with an existing route and was skipped",
                        record.id
                    );
                    continue;
                }
                if routes.contains_key(&key) {
                    tracing::warn!(
                        "pluginhost: plugin {} management route {key} conflicts with a higher-priority plugin and was skipped",
                        record.id
                    );
                    continue;
                }
                routes.insert(
                    key,
                    RouteRecord {
                        plugin_id: record.id.clone(),
                        path: record.path.clone(),
                        version: record.version.clone(),
                        schema_version: record.plugin.schema_version,
                        route: ManagementRoute { method, path, ..item },
                    },
                );
            }
            for item in resp.resources {
                let path = item.path.clone();
                if !register_resource(&mut resources, item) {
                    tracing::warn!(
                        "pluginhost: plugin {} declared invalid resource route {path}",
                        record.id
                    );
                }
            }
        }
        let mut state = self.state();
        state.management_routes = routes;
        state.resource_routes = resources;
    }

    /// Plugin management routes currently registered, by key.
    pub fn management_routes(&self) -> Vec<RouteRecord> {
        self.state().management_routes.values().cloned().collect()
    }

    pub fn resource_routes(&self) -> Vec<ResourceRecord> {
        self.state().resource_routes.values().cloned().collect()
    }

    /// Whether [`Self::serve_management`] would call a plugin for this method and decoded
    /// path (Go checks this before reading the body).
    pub fn has_management_route(&self, method: &str, path: &str) -> bool {
        let record = self.state().management_routes.get(&route_key(method, path)).cloned();
        record.is_some_and(|r| !self.is_fused(&r.plugin_id))
    }

    /// Go `ServeManagementHTTP`: `None` when no plugin route matches.
    pub async fn serve_management(&self, req: Inbound) -> Option<Reply> {
        let key = route_key(&req.method, &req.path);
        let record = self.state().management_routes.get(&key).cloned()?;
        if self.is_fused(&record.plugin_id) {
            return None;
        }
        let request = ManagementCall {
            method: req.method.clone(),
            path: req.path.clone(),
            headers: req.headers.clone(),
            query: req.query.clone(),
            body: Some(NonNilBytes(req.body.clone())),
            host_callback_id: String::new(),
        };
        match self
            .call_route_handler(&record.plugin_id, &record.path, &record.version, request, req.scope)
            .await
        {
            Err(e) => {
                tracing::warn!("pluginhost: management handler {} failed: {e}", record.plugin_id);
                Some(Reply::error(502, "plugin management handler failed"))
            }
            Ok(mut resp) => {
                if record.schema_version < abi::SCHEMA_RAW_MANAGEMENT_RESPONSE {
                    let content_type = header_get(&resp.headers, "Content-Type");
                    if let Some(body) = escape_json_body_if_likely(&resp.body, content_type) {
                        resp.body = body;
                    }
                }
                Some(Reply::from_plugin(resp))
            }
        }
    }

    /// Go `ServeResourceHTTP`: GET only, no body forwarded.
    pub async fn serve_resource(&self, req: Inbound) -> Option<Reply> {
        if !req.method.eq_ignore_ascii_case("GET") {
            return None;
        }
        let record = self
            .state()
            .resource_routes
            .get(&route_key("GET", &req.path))
            .cloned()?;
        if self.is_fused(&record.plugin_id) {
            return None;
        }
        let request = ManagementCall {
            method: "GET".into(),
            path: req.path.clone(),
            headers: req.headers.clone(),
            query: req.query.clone(),
            body: None,
            host_callback_id: String::new(),
        };
        match self
            .call_route_handler(&record.plugin_id, &record.path, &record.version, request, req.scope)
            .await
        {
            Err(e) => {
                tracing::warn!("pluginhost: resource handler {} failed: {e}", record.plugin_id);
                Some(Reply::error(502, "plugin resource handler failed"))
            }
            Ok(resp) => Some(Reply::from_plugin(resp)),
        }
    }

    /// `management.handle` with a callback context. A plugin that is no longer the
    /// current identity yields an empty response, as in Go.
    async fn call_route_handler(
        &self,
        plugin_id: &str,
        path: &std::path::Path,
        version: &str,
        mut call: ManagementCall,
        scope: RequestScope,
    ) -> Result<ManagementResponse, crate::rpc::CallError> {
        let Some(record) = self.record(plugin_id) else {
            return Ok(ManagementResponse::default());
        };
        if record.path != path || record.version != version {
            return Ok(ManagementResponse::default());
        }
        let guard = self
            .inner
            .callbacks
            .open(plugin_id, Some(record.client.instance().clone()), scope);
        call.host_callback_id = guard.id().to_owned();
        let result = self.call(&record, method::MANAGEMENT_HANDLE, &call).await;
        drop(guard);
        result
    }
}

/// `http.Header.Get`: canonicalizes the key, then looks it up exactly. A decoded map
/// whose key is spelled `content-type` does not match.
pub fn header_get<'a>(headers: &'a Header, name: &str) -> &'a str {
    headers
        .get(&cpa_exec::proxy::canonical_header(name))
        .and_then(|v| v.first())
        .map(String::as_str)
        .unwrap_or_default()
}

/// Go `htmlsanitize.JSONBodyIfLikely`: HTML-escapes every string value of a JSON body
/// (schema < 6 management responses). `None` when the body is left as is.
pub fn escape_json_body_if_likely(body: &[u8], content_type: &str) -> Option<Bytes> {
    if !(is_json_content_type(content_type) || looks_like_json(body)) {
        return None;
    }
    let trimmed = body.trim_ascii();
    if trimmed.is_empty() {
        return None;
    }
    let value = GoValue::parse(trimmed)?;
    Some(Bytes::from(escape_strings(value).marshal_no_html()))
}

fn escape_strings(v: GoValue) -> GoValue {
    match v {
        GoValue::String(s) => GoValue::String(html_escape(&s)),
        GoValue::Array(items) => GoValue::Array(items.into_iter().map(escape_strings).collect()),
        GoValue::Object(map) => GoValue::Object(map.into_iter().map(|(k, v)| (k, escape_strings(v))).collect()),
        other => other,
    }
}

/// Go `html.EscapeString`.
pub fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '\'' => out.push_str("&#39;"),
            '"' => out.push_str("&#34;"),
            c => out.push(c),
        }
    }
    out
}

/// Go `htmlsanitize.IsJSONContentType` (`mime.ParseMediaType`, falling back to the raw
/// value when the parameters do not parse).
fn is_json_content_type(content_type: &str) -> bool {
    let content_type = content_type.trim();
    let media = match content_type.split_once(';') {
        Some((media, params)) if params.split(';').all(|p| p.trim().is_empty() || p.contains('=')) => media.trim(),
        Some(_) => content_type,
        None => content_type,
    };
    let media = media.to_ascii_lowercase();
    media == "application/json" || media.ends_with("+json")
}

fn looks_like_json(body: &[u8]) -> bool {
    matches!(body.trim_ascii().first(), Some(b'{' | b'['))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Go `TestNormalizeManagementRoute*` / resource normalization.
    #[test]
    fn route_normalization_matches_go() {
        let route = |m: &str, p: &str| ManagementRoute {
            method: m.into(),
            path: p.into(),
            ..Default::default()
        };
        assert_eq!(
            normalize_route(&route("", "status/")),
            Some(("GET".into(), "/v0/management/status".into()))
        );
        assert_eq!(
            normalize_route(&route("post", "/v0/management/x")),
            Some(("POST".into(), "/v0/management/x".into()))
        );
        assert_eq!(normalize_route(&route("GET", "/v0/management/")), None);
        assert_eq!(normalize_route(&route("GET", "/a/:id")), None);
        assert_eq!(normalize_route(&route("GE T", "/a")), None);
        let res = |p: &str| ResourceRoute {
            path: p.into(),
            ..Default::default()
        };
        assert_eq!(
            normalize_resource("p", &res("status")),
            Some("/v0/resource/plugins/p/status".into())
        );
        assert_eq!(
            normalize_resource("p", &res("/plugins/p/a/")),
            Some("/v0/resource/plugins/p/a".into())
        );
        assert_eq!(
            normalize_resource("p", &res("/v0/resource/plugins/p/b")),
            Some("/v0/resource/plugins/p/b".into())
        );
        assert_eq!(normalize_resource("p", &res("/a/../b")), None);
    }

    /// Go `TestJSONBodyEscapesStringValues` / `TestJSONBodyIfLikelySkipsNonJSONHTML`.
    #[test]
    fn legacy_json_escaping_matches_htmlsanitize() {
        let got = escape_json_body_if_likely(br#" {"title":"<b>","n":1.50,"items":["a & b",{"z":"'"}]} "#, "").unwrap();
        assert_eq!(
            &got[..],
            br#"{"items":["a &amp; b",{"z":"&#39;"}],"n":1.50,"title":"&lt;b&gt;"}"#
        );
        assert_eq!(
            escape_json_body_if_likely(b"<!doctype html>", "text/html; charset=utf-8"),
            None
        );
        assert_eq!(escape_json_body_if_likely(b"{bad", "application/json"), None);
        assert!(is_json_content_type("application/problem+json; charset=utf-8"));
        // Go's Header.Get does not find a lowercase key decoded from JSON.
        let lower: Header = [("content-type".to_owned(), vec!["application/json".to_owned()])].into();
        assert_eq!(header_get(&lower, "Content-Type"), "");
        assert_eq!(
            escape_json_body_if_likely(br#""<b>""#, header_get(&lower, "Content-Type")),
            None
        );
    }
}
