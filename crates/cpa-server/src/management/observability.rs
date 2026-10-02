//! `GET /server/latest-version`, `GET /observability/usage/api-keys` and
//! `GET /observability/usage/queue` (Go config_basic.go, api_key_usage.go, usage.go).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::response::Response;
use cpa_exec::proxy::{self, GoHeaders, Proxy, Route};
use serde_json::{Map, Value, json};

use super::auth_files::{Query, auth_kind, fail, recent_requests, reply};
use super::{Management, json as respond};

/// Release lookup for `GET /server/latest-version`: the cliproxy-rs repository's
/// GitHub latest-release API. Empty until that repository is published; the endpoint
/// then answers with Go's failed-lookup shape instead of asking anyone.
pub const LATEST_RELEASE_URL: &str = "";
const LATEST_RELEASE_USER_AGENT: &str = "cliproxy-rs";

/// Go `GetLatestVersion`: one GET through `requests.proxy-url` (or the environment
/// proxies), 10 s timeout.
pub(super) async fn latest_version(State(state): State<Arc<Management>>) -> Response {
    let url = state.latest_release_url.clone();
    if url.is_empty() {
        return reply(
            StatusCode::BAD_GATEWAY,
            [
                ("error", "request_failed".into()),
                ("message", "no release repository is configured".into()),
            ],
        );
    }
    let cfg = state.rt.config();
    let global = cfg
        .document
        .get("requests")
        .and_then(|r| r.get("proxy-url"))
        .and_then(serde_yaml_ng::Value::as_str)
        .unwrap_or_default();
    let client = state.clients.get(&Proxy::parse(global));
    let mut headers = GoHeaders::new();
    headers.set("Accept", "application/vnd.github+json");
    headers.set("User-Agent", LATEST_RELEASE_USER_AGENT);
    // Go `util.ResolveGitHubToken`.
    let token = ["GITHUB_TOKEN", "github_token"]
        .iter()
        .filter_map(|n| std::env::var(n).ok())
        .map(|t| t.trim().to_owned())
        .find(|t| !t.is_empty())
        .or_else(|| {
            let git = std::env::var("GITSTORE_GIT_URL").unwrap_or_default().to_lowercase();
            git.contains("github.com").then(|| {
                std::env::var("GITSTORE_GIT_TOKEN")
                    .unwrap_or_default()
                    .trim()
                    .to_owned()
            })
        })
        .filter(|t| !t.is_empty());
    if let Some(token) = token {
        headers.set("Authorization", format!("Bearer {token}"));
    }
    let upstream = match proxy::send_request(
        &|_| {
            Ok(Route {
                client: client.clone(),
                order: None,
            })
        },
        axum::http::Method::GET,
        &url,
        headers,
        None,
        Some(Duration::from_secs(10)),
    )
    .await
    {
        Ok(u) => u,
        Err(e) => return gateway("request_failed", String::from_utf8_lossy(&e.body).into_owned()),
    };
    let ok = upstream.status == 200;
    let limit = if ok { 1 << 20 } else { 1024 };
    let body = proxy::read_all(upstream.body, limit, true).await.unwrap_or_default();
    if !ok {
        let text = String::from_utf8_lossy(&body);
        return gateway(
            "unexpected_status",
            format!("status {}: {}", upstream.status, text.trim()),
        );
    }
    let info: Value = match serde_json::Deserializer::from_slice(&body).into_iter::<Value>().next() {
        Some(Ok(v)) => v,
        Some(Err(e)) => return gateway("decode_failed", e.to_string()),
        None => return gateway("decode_failed", "EOF".to_owned()),
    };
    let field = |k: &str| {
        info.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned()
    };
    let version = Some(field("tag_name"))
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| field("name"));
    if version.is_empty() {
        return gateway("invalid_response", "missing release version".to_owned());
    }
    reply(StatusCode::OK, [("latest-version", version.into())])
}

fn gateway(error: &str, message: String) -> Response {
    respond(StatusCode::BAD_GATEWAY, &json!({"error": error, "message": message}))
}

/// Go `GetAPIKeyUsage`: API-key credentials grouped by provider (the OpenAI-compatible
/// name when set) and keyed by `base_url|api_key`, with summed counters.
pub(super) async fn api_key_usage(State(state): State<Arc<Management>>) -> Response {
    let store = state.rt.store();
    let mut out: BTreeMap<String, BTreeMap<String, (u64, u64, Value)>> = BTreeMap::new();
    for c in store.snapshot().iter() {
        if auth_kind(c) != Some("apikey") {
            continue;
        }
        let Some(key) = c.attributes.get("api_key").map(|k| k.trim()).filter(|k| !k.is_empty()) else {
            continue;
        };
        let base = ["base_url", "base-url"]
            .iter()
            .filter_map(|k| c.attributes.get(*k))
            .map(|v| v.trim())
            .find(|v| !v.is_empty())
            .unwrap_or_default();
        let provider = c
            .attributes
            .get("compat_name")
            .map(|n| n.trim())
            .filter(|n| !n.is_empty())
            .unwrap_or(c.provider.trim())
            .to_lowercase();
        let provider = if provider.is_empty() {
            "unknown".to_owned()
        } else {
            provider
        };
        let activity = store.activity(&c.id);
        let recent = recent_requests(&activity);
        let entry = out.entry(provider).or_default().entry(format!("{base}|{key}"));
        match entry {
            std::collections::btree_map::Entry::Vacant(v) => {
                v.insert((activity.success, activity.failed, recent));
            }
            std::collections::btree_map::Entry::Occupied(mut o) => {
                let (s, f, buckets) = o.get_mut();
                *s += activity.success;
                *f += activity.failed;
                merge_buckets(buckets, &recent);
            }
        }
    }
    let body: Map<String, Value> = out
        .into_iter()
        .map(|(provider, keys)| {
            let keys: Map<String, Value> = keys
                .into_iter()
                .map(|(k, (s, f, r))| (k, json!({"success": s, "failed": f, "recent_requests": r})))
                .collect();
            (provider, Value::Object(keys))
        })
        .collect();
    respond(StatusCode::OK, &Value::Object(body))
}

/// Go `mergeRecentRequestBuckets`: counts add up index by index.
fn merge_buckets(dst: &mut Value, src: &Value) {
    let (Some(dst), Some(src)) = (dst.as_array_mut(), src.as_array()) else {
        return;
    };
    for (d, s) in dst.iter_mut().zip(src) {
        for key in ["success", "failed"] {
            let sum = d[key].as_u64().unwrap_or(0) + s[key].as_u64().unwrap_or(0);
            d[key] = sum.into();
        }
    }
}

/// Go `GetUsageQueue`: pops up to `count` (default 1) queued usage records. A record
/// that is valid JSON is embedded as is, anything else as a JSON string.
pub(super) async fn usage_queue(State(state): State<Arc<Management>>, RawQuery(raw): RawQuery) -> Response {
    let q = Query::parse(raw);
    let value = q.first("count").trim();
    let count = if value.is_empty() {
        1
    } else {
        match value.parse::<i64>() {
            Ok(n) if n > 0 => usize::try_from(n).unwrap_or(usize::MAX),
            _ => return fail(StatusCode::BAD_REQUEST, "count must be a positive integer"),
        }
    };
    let records: Vec<Value> = state
        .rt
        .usage_queue()
        .pop_oldest(count)
        .into_iter()
        .map(|payload| {
            serde_json::from_slice::<Value>(&payload)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&payload).into_owned()))
        })
        .collect();
    respond(StatusCode::OK, &Value::Array(records))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_base_and_key_sum_bucket_by_bucket() {
        let mut dst = json!([{"time": "a", "success": 1, "failed": 0}, {"time": "b", "success": 0, "failed": 2}]);
        let src = json!([{"time": "a", "success": 3, "failed": 1}, {"time": "b", "success": 0, "failed": 5}]);
        merge_buckets(&mut dst, &src);
        assert_eq!(
            dst,
            json!([{"time": "a", "success": 4, "failed": 1}, {"time": "b", "success": 0, "failed": 7}])
        );
    }
}
