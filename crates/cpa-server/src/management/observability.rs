//! `GET /server/latest-version`, `GET /observability/usage/api-keys` and
//! `GET /observability/usage/queue` (Go config_basic.go, api_key_usage.go, usage.go).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::response::Response;
use cpa_exec::proxy::Proxy;
use serde_json::{Map, Value, json};

use super::auth_files::{Query, auth_kind, fail, recent_requests, reply};
use super::{Management, json as respond};

/// Public redirect endpoint, not the authenticated GitHub API.
pub const LATEST_RELEASE_URL: &str = "https://github.com/vayungodara/cliproxy-rs/releases/latest";
const LATEST_RELEASE_USER_AGENT: &str = "cliproxy-rs";
const RELEASE_CACHE_TTL: Duration = Duration::from_secs(12 * 60 * 60);

/// Explicit checks only. One HEAD, no redirects, cookies or GitHub/store tokens.
/// Cost: one lock per check; a timestamp and tag only after success. No idle work.
pub(crate) async fn latest_version(State(state): State<Arc<Management>>) -> Response {
    if state.update_check_disabled {
        return reply(
            StatusCode::SERVICE_UNAVAILABLE,
            [
                ("error", "update_check_disabled".into()),
                (
                    "message",
                    "Update checks are disabled by CLIPROXY_NO_UPDATE_CHECK=1.".into(),
                ),
            ],
        );
    }
    // The lock is held across the HEAD on purpose: concurrent clicks share one
    // request and its cached result instead of each reaching GitHub.
    let mut cache = state.latest_release.lock().await;
    if let Some((at, version)) = cache.as_ref()
        && release_cache_fresh(*at, SystemTime::now())
    {
        return reply(StatusCode::OK, [("latest-version", version.clone().into())]);
    }
    // Fresh headers and no cookie jar: only proxy authentication belongs to the
    // transport. Never copy incoming headers or read token environment variables.
    let cfg = state.rt.config();
    let proxy = Proxy::parse(
        cfg.document
            .get("requests")
            .and_then(|r| r.get("proxy-url"))
            .and_then(serde_yaml_ng::Value::as_str)
            .unwrap_or_default(),
    );
    let client = match proxy
        .apply(wreq::Client::builder().redirect(wreq::redirect::Policy::none()), true)
        .and_then(wreq::ClientBuilder::build)
    {
        Ok(client) => client,
        Err(_) => return gateway("request_failed", "Could not create the release client.".into()),
    };
    let upstream = match client
        .head(state.latest_release_url.as_ref())
        .header("User-Agent", LATEST_RELEASE_USER_AGENT)
        .timeout(Duration::from_secs(10))
        .send()
        .await
    {
        Ok(u) => u,
        Err(_) => return gateway("request_failed", "Could not check the latest release.".into()),
    };
    if !upstream.status().is_redirection() {
        return gateway("unexpected_status", format!("status {}", upstream.status().as_u16()));
    }
    let version = upstream
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("https://github.com/vayungodara/cliproxy-rs/releases/tag/"))
        .filter(|v| {
            !v.is_empty() && v.len() <= 128 && v.bytes().all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b))
        });
    let Some(version) = version else {
        return gateway("invalid_response", "missing release version".to_owned());
    };
    *cache = Some((SystemTime::now(), version.to_owned()));
    reply(StatusCode::OK, [("latest-version", version.into())])
}

/// Fresh for under 12 hours of wall-clock time. A timestamp in the future (the clock
/// went backwards) counts as expired.
fn release_cache_fresh(at: SystemTime, now: SystemTime) -> bool {
    now.duration_since(at).is_ok_and(|age| age < RELEASE_CACHE_TTL)
}

fn gateway(error: &str, message: String) -> Response {
    respond(StatusCode::BAD_GATEWAY, &json!({"error": error, "message": message}))
}

/// Go `GetAPIKeyUsage`: API-key credentials grouped by provider (the OpenAI-compatible
/// name when set) and keyed by `base_url|api_key`, with summed counters.
pub(crate) async fn api_key_usage(State(state): State<Arc<Management>>) -> Response {
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
    // apiKeyUsageEntry and RecentRequestBucket structs in sorted maps.
    super::json_ordered(StatusCode::OK, &Value::Object(body))
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
pub(crate) async fn usage_queue(State(state): State<Arc<Management>>, RawQuery(raw): RawQuery) -> Response {
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
    // Records are embedded as stored (usageQueueRecord.MarshalJSON).
    super::json_ordered(StatusCode::OK, &Value::Array(records))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn release_cache_expires_after_twelve_hours() {
        let dir = std::env::temp_dir().join(format!("release-cache-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("auth")).unwrap();
        let config = cpa_core::config::Config::parse(&format!(
            "config-version: 8\noauth:\n  auth-dir: '{}'\n",
            dir.join("auth").display()
        ))
        .unwrap();
        let rt = Arc::new(crate::testing::runtime(
            config,
            vec![],
            cpa_exec::Executors {
                claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
                codex: Default::default(),
                devices: Default::default(),
                openai: Default::default(),
                google: Default::default(),
            },
        ));
        let state = Management::with_options(
            rt,
            dir.join("config.yaml"),
            super::super::Options {
                latest_release_url: Some("http://127.0.0.1:0/latest".into()),
                update_check_disabled: Some(false),
                ..Default::default()
            },
        );
        let hours = |h: u64| Duration::from_secs(h * 60 * 60);
        let now = SystemTime::now();
        for (at, expected) in [
            (now - (hours(12) - Duration::from_secs(5)), StatusCode::OK),
            (now - hours(12), StatusCode::BAD_GATEWAY),
            (now - hours(13), StatusCode::BAD_GATEWAY),
            // A clock that went backwards: the cached time is in the future.
            (now + hours(1), StatusCode::BAD_GATEWAY),
        ] {
            *state.latest_release.lock().await = Some((at, "v1.2.3".into()));
            let response = latest_version(State(state.clone())).await;
            assert_eq!(response.status(), expected);
            if expected == StatusCode::OK {
                let body = axum::body::to_bytes(response.into_body(), 1024).await.unwrap();
                assert_eq!(
                    serde_json::from_slice::<Value>(&body).unwrap(),
                    json!({"latest-version":"v1.2.3"})
                );
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn release_cache_freshness_uses_wall_clock() {
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let ttl = RELEASE_CACHE_TTL;
        assert!(release_cache_fresh(at, at));
        assert!(release_cache_fresh(at, at + ttl - Duration::from_nanos(1)));
        assert!(!release_cache_fresh(at, at + ttl));
        assert!(!release_cache_fresh(at, at + ttl * 3));
        assert!(!release_cache_fresh(at, at - Duration::from_secs(1)));
    }

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
