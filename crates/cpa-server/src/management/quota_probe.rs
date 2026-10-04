//! The declarative quota probe (plugin_quota.go `executeQuotaProbe`): a credential's
//! `quota_probe` metadata names an HTTP request whose JSON answer is read as a
//! normalized quota, directly or through a gjson path mapping. `quota/fetch` uses it
//! when no plugin quota provider handles the credential.

use std::time::SystemTime;

use axum::body::Bytes;
use cpa_common::json::{self as gjson, Kind, Res};
use cpa_core::credential::Credential;
use cpa_exec::proxy::{self, GoHeaders, Route};
use cpa_plugin::api::{QuotaBucket, QuotaFetchResponse, QuotaGroup, QuotaMetric, QuotaSubscription};
use futures_util::StreamExt;
use serde_json::{Map, Value};

use super::Management;

/// ponytail: Go reads the probe answer without a bound; the API-call limit applies.
const MAX_BODY: usize = 64 << 20;

/// Go `executeQuotaProbe`: `None` when the probe is not usable (no URL, or a request Go
/// cannot build), else the quota or the error to report.
pub(super) async fn execute(
    state: &Management,
    auth: &Credential,
    probe: &Map<String, Value>,
) -> Option<Result<QuotaFetchResponse, String>> {
    let text = |key: &str| probe.get(key).and_then(Value::as_str).unwrap_or_default();
    let mut url = text("url").trim().to_owned();
    if url.is_empty() {
        return None;
    }
    let method = match go_to_upper(text("method").trim()) {
        m if m.is_empty() => "GET".to_owned(),
        m => m,
    };
    let mut data = text("data").to_owned();
    let headers = match probe.get("header").and_then(Value::as_object) {
        Some(h) => Some(h),
        None => probe.get("headers").and_then(Value::as_object),
    };
    let needs_token = url.contains("$TOKEN$")
        || data.contains("$TOKEN$")
        || headers
            .into_iter()
            .flatten()
            .any(|(_, v)| v.as_str().is_some_and(|v| v.contains("$TOKEN$")));
    // ponytail: Go refreshes antigravity, meta and xai tokens here when due (as for
    // api-call); the stored token is used.
    let token = super::api_call::token_for(auth);
    if needs_token {
        if token.is_empty() {
            return Some(Err("probe authentication token not found for credential".into()));
        }
        url = url.replace("$TOKEN$", &token);
        data = data.replace("$TOKEN$", &token);
    }
    // http.NewRequestWithContext: a bad method or URL is not a handled probe.
    let valid_method = !method.is_empty()
        && method
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b));
    let parsed = cpa_exec::xai_url::parse(&url).ok().filter(|_| valid_method)?;
    let Ok(wire_method) = axum::http::Method::from_bytes(method.as_bytes()) else {
        return None;
    };
    let fail = |e: String| Some(Err(e));
    // req.Header.Set for each string value: canonical names, the last value wins.
    let mut header = cpa_plugin::gojson::Header::new();
    for (key, value) in headers.into_iter().flatten() {
        if let Some(value) = value.as_str() {
            let value = if needs_token {
                value.replace("$TOKEN$", &token)
            } else {
                value.to_owned()
            };
            header.insert(proxy::canonical_header(key), vec![value]);
        }
    }
    // client.Do refuses what Go's transport would not send, before dialing.
    if let Err(cause) = cpa_plugin::go_preflight(&url, &parsed, &header) {
        return fail(format!(
            "probe request failed: {}",
            cpa_plugin::go_url_error_text(&method, &url, &cause)
        ));
    }
    // Go's request writer takes Host from the URL and frames the body itself.
    let mut go_headers = GoHeaders::new();
    for (name, values) in header {
        if !matches!(
            name.as_str(),
            "Host" | "Content-Length" | "Transfer-Encoding" | "Trailer"
        ) {
            for value in values {
                go_headers.add_raw(&name, value);
            }
        }
    }
    let Some(client) = super::api_call::client_for(state, Some(auth), "") else {
        return fail("probe request failed: no transport".into());
    };
    let body = (!data.is_empty()).then(|| Bytes::from(data));
    let sent = proxy::send_request_raw(
        &|_| {
            Ok(Route {
                client: client.clone(),
                order: None,
            })
        },
        wire_method,
        &url,
        go_headers,
        body,
        None,
    )
    .await;
    let upstream = match sent {
        Ok(u) => u,
        Err(e) => {
            return fail(format!(
                "probe request failed: {}",
                cpa_plugin::go_url_error(&method, e)
            ));
        }
    };
    let date = upstream
        .headers
        .get(axum::http::header::DATE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let status = upstream.status;
    let mut body = upstream.body;
    let mut raw = Vec::new();
    while let Some(chunk) = body.next().await {
        match chunk {
            Ok(chunk) if raw.len() + chunk.len() <= MAX_BODY => raw.extend_from_slice(&chunk),
            Ok(_) => return fail("read probe response: response too large".into()),
            Err(e) => return fail(format!("read probe response: {}", cpa_plugin::go_read_error_text(&e))),
        }
    }
    if !(200..300).contains(&status) {
        return fail(format!(
            "probe returned status {status}: {}",
            String::from_utf8_lossy(&raw)
        ));
    }
    if cpa_plugin::gojson::parse(&raw).is_err() {
        return fail("upstream probe response is not valid JSON".into());
    }
    let offset = date.and_then(|d| server_offset_ms(&d)).unwrap_or(0);
    if let Some(mapping) = probe.get("mapping").and_then(Value::as_object) {
        return Some(match map_response(&raw, mapping) {
            Ok(mut out) => {
                if out.server_time_offset_ms == 0 {
                    out.server_time_offset_ms = offset;
                }
                Ok(out)
            }
            Err(e) => Err(format!("probe response mapping failed: {e}")),
        });
    }
    if let Some(mut out) = normalized_response(&raw) {
        if out.server_time_offset_ms == 0 {
            out.server_time_offset_ms = offset;
        }
        return Some(Ok(out));
    }
    fail("upstream probe response does not match normalized quota shape or declared mapping".into())
}

/// Go `strings.ToUpper`: simple per-rune case mapping (a rune whose uppercase is
/// several runes, like `ß`, stays as it is).
fn go_to_upper(s: &str) -> String {
    s.chars()
        .map(|c| {
            let mut upper = c.to_uppercase();
            match (upper.next(), upper.next()) {
                (Some(u), None) => u,
                _ => c,
            }
        })
        .collect()
}

/// `http.ParseTime(date).Sub(time.Now()).Milliseconds()`.
fn server_offset_ms(date: &str) -> Option<i64> {
    let parsed = [
        "%a, %d %b %Y %H:%M:%S GMT",
        "%A, %d-%b-%y %H:%M:%S GMT",
        "%a %b %e %H:%M:%S %Y",
    ]
    .iter()
    .find_map(|layout| chrono::NaiveDateTime::parse_from_str(date, layout).ok())?
    .and_utc();
    let now = chrono::DateTime::<chrono::Utc>::from(SystemTime::now());
    Some((parsed - now).num_milliseconds())
}

/// A JSON number or numeric string that is finite (Go `parseNumericFraction`).
fn numeric_fraction(res: &Res<'_>) -> Option<f64> {
    if !res.exists() {
        return None;
    }
    let value = match res.kind {
        Kind::Number => res.float(),
        Kind::String => {
            let s = res.str();
            let s = s.trim();
            if s.is_empty() {
                return None;
            }
            cpa_plugin::cli::parse_float(s)?
        }
        _ => return None,
    };
    value.is_finite().then_some(value)
}

/// The response's top-level members as Go's `map[string]json.RawMessage` holds them:
/// the last of duplicate keys wins.
fn members(raw: &[u8]) -> Option<Vec<(String, cpa_plugin::gojson::Node)>> {
    // `Node` frees itself iteratively (it implements `Drop`), so its members are taken
    // out rather than moved by the pattern.
    let mut node = cpa_plugin::gojson::parse(raw).ok()?;
    let cpa_plugin::gojson::Node::Object(members) = &mut node else {
        return None;
    };
    let mut out: Vec<(String, cpa_plugin::gojson::Node)> = Vec::new();
    for (k, v) in std::mem::take(members) {
        out.retain(|(key, _)| *key != k);
        out.push((k, v));
    }
    Some(out)
}

/// Go `filterUsableQuotaSummary`: well-formed metrics of the `summary` array (the
/// last `summary` member, else one spelled in another case).
fn usable_summary(raw: &[u8]) -> Vec<QuotaMetric> {
    let top = gjson::parse(raw);
    if !top.is_object() {
        return Vec::new();
    }
    let members = top.map();
    let summary = members
        .iter()
        .rev()
        .find(|(k, _)| k == b"summary")
        .or_else(|| members.iter().find(|(k, _)| k.eq_ignore_ascii_case(b"summary")))
        .map(|(_, v)| v.raw().to_vec());
    let Some(summary) = summary else {
        return Vec::new();
    };
    let parsed = gjson::parse(&summary);
    if !parsed.is_array() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for metric in parsed.array() {
        let key = metric.get("key");
        let label = metric.get("label");
        let value = metric.get("value");
        let (key_text, label_text) = (key.str().trim().to_owned(), label.str().trim().to_owned());
        if key.kind != Kind::String
            || label.kind != Kind::String
            || key_text.is_empty()
            || label_text.is_empty()
            || value.kind != Kind::Number
            || !value.float().is_finite()
        {
            continue;
        }
        let mut m = QuotaMetric {
            key: key_text,
            label: label_text,
            value: value.float(),
            ..Default::default()
        };
        let unit = metric.get("unit");
        if unit.kind == Kind::String {
            m.unit = unit.str().trim().to_owned();
        }
        let format = metric.get("format");
        if format.kind == Kind::String {
            match format.str().trim() {
                "number" => m.format = "number".into(),
                "currency" => {
                    let currency = metric.get("currency");
                    if currency.kind == Kind::String {
                        let code = currency.str().trim().to_uppercase();
                        if super::iso_currency::CODES.binary_search(&code.as_str()).is_ok() {
                            m.format = "currency".into();
                            m.currency = code;
                        }
                    }
                }
                _ => {}
            }
        }
        out.push(m);
    }
    out
}

/// The probe answer read as Go's `QuotaFetchResponse` (without `summary`, which is
/// filtered separately), keeping only buckets with a usable remaining fraction.
fn normalized_response(raw: &[u8]) -> Option<QuotaFetchResponse> {
    let mut members = members(raw)?;
    members.retain(|(k, _)| !k.eq_ignore_ascii_case("summary"));
    // Go re-marshals the map before decoding: keys in sorted order.
    members.sort_by(|a, b| a.0.cmp(&b.0));
    let core = cpa_plugin::gojson::Node::Object(members);
    let mut out = cpa_plugin::api::quota_fetch_from_value(&core).ok()?;
    let has_plan = out.subscription.as_ref().is_some_and(|s| !s.plan.trim().is_empty());
    let mut groups = Vec::new();
    let raw_groups = gjson::get(raw, "groups");
    if raw_groups.is_array() {
        for (index, raw_group) in raw_groups.array().iter().enumerate() {
            let Some(group) = out.groups.get(index) else { break };
            let raw_buckets = raw_group.get("buckets");
            if !raw_buckets.is_array() {
                continue;
            }
            let mut buckets = Vec::new();
            for (b, raw_bucket) in raw_buckets.array().iter().enumerate() {
                let Some(bucket) = group.buckets.get(b) else { break };
                let mut fraction = raw_bucket.get("remainingFraction");
                if !fraction.exists() {
                    fraction = raw_bucket.get("remaining_fraction");
                }
                if let Some(value) = numeric_fraction(&fraction) {
                    let mut bucket = bucket.clone();
                    bucket.remaining_fraction = value;
                    buckets.push(bucket);
                }
            }
            if !buckets.is_empty() {
                let mut group = group.clone();
                group.buckets = buckets;
                groups.push(group);
            }
        }
    }
    out.groups = groups;
    out.summary = usable_summary(raw);
    (has_plan || !out.groups.is_empty() || !out.summary.is_empty()).then_some(out)
}

/// Go `mapProbeResponse`.
fn map_response(raw: &[u8], mapping: &Map<String, Value>) -> Result<QuotaFetchResponse, String> {
    let mut out = QuotaFetchResponse::default();
    let text = |m: &Map<String, Value>, key: &str| m.get(key).and_then(Value::as_str).map(str::to_owned);
    let found = |path: &str| {
        let res = gjson::get(raw, path);
        (res.exists() && !res.str().trim().is_empty()).then(|| res.str().into_owned())
    };
    let mut subscription =
        |f: &mut dyn FnMut(&mut QuotaSubscription)| f(out.subscription.get_or_insert_with(Default::default));
    if let Some(v) = text(mapping, "plan").filter(|p| !p.is_empty()).and_then(|p| found(&p)) {
        subscription(&mut |s| s.plan = v.clone());
    }
    // Go reads `tier_name` when it is a non-empty string, else `tierName` (likewise ids).
    for (snake, camel, is_name) in [("tier_name", "tierName", true), ("tier_id", "tierId", false)] {
        let path = text(mapping, snake)
            .filter(|p| !p.is_empty())
            .or_else(|| text(mapping, camel).filter(|p| !p.is_empty()));
        if let Some(v) = path.and_then(|p| found(&p)) {
            subscription(&mut |s| {
                if is_name {
                    s.tier_name = v.clone();
                } else {
                    s.tier_id = v.clone();
                }
            });
        }
    }
    if let Some(Value::Array(groups)) = mapping.get("groups") {
        for g in groups {
            let Some(gm) = g.as_object() else { continue };
            let mut group = QuotaGroup::default();
            let name = text(gm, "display_name").or_else(|| text(gm, "displayName"));
            if let Some(name) = name {
                group.display_name = found(&name).unwrap_or(name);
            }
            if let Some(path) = text(gm, "buckets_path").filter(|p| !p.is_empty()) {
                let items = gjson::get(raw, &path);
                if items.is_array() && !items.array().is_empty() {
                    let key = |k: &str, default: &str| match text(gm, k) {
                        Some(v) if !v.is_empty() => v,
                        _ => default.to_owned(),
                    };
                    let window = key("window_key", "window");
                    let fraction_key = key("remaining_fraction_key", "remaining_fraction");
                    let remaining_key = text(gm, "remaining_amount_key").unwrap_or_default();
                    let total_key = text(gm, "total_amount_key").unwrap_or_default();
                    let reset = key("reset_time_key", "reset_time");
                    let description = key("description_key", "description");
                    for item in items.array() {
                        let mut fraction = numeric_fraction(&item.get(&fraction_key));
                        if fraction.is_none() && !remaining_key.is_empty() && !total_key.is_empty() {
                            fraction = match (
                                numeric_fraction(&item.get(&remaining_key)),
                                numeric_fraction(&item.get(&total_key)),
                            ) {
                                (Some(rem), Some(total)) if total > 0.0 => Some(rem / total),
                                _ => None,
                            };
                        }
                        let Some(fraction) = fraction else { continue };
                        group.buckets.push(QuotaBucket {
                            window: item.get(&window).str().into_owned(),
                            remaining_fraction: fraction,
                            reset_time: item.get(&reset).str().into_owned(),
                            description: item.get(&description).str().into_owned(),
                        });
                    }
                }
            }
            if let Some(Value::Array(buckets)) = gm.get("buckets") {
                for b in buckets {
                    let Some(bm) = b.as_object() else { continue };
                    let path_value = |k: &str| text(bm, k).filter(|p| !p.is_empty());
                    let mut fraction =
                        path_value("remaining_fraction").and_then(|p| numeric_fraction(&gjson::get(raw, &p)));
                    if fraction.is_none()
                        && let (Some(rem), Some(total)) = (path_value("remaining_amount"), path_value("total_amount"))
                    {
                        fraction = match (
                            numeric_fraction(&gjson::get(raw, &rem)),
                            numeric_fraction(&gjson::get(raw, &total)),
                        ) {
                            (Some(r), Some(t)) if t > 0.0 => Some(r / t),
                            _ => None,
                        };
                    }
                    let Some(fraction) = fraction else { continue };
                    // A path that resolves gives its value; otherwise the text itself.
                    let literal = |k: &str| {
                        text(bm, k).map(|v| {
                            let res = gjson::get(raw, &v);
                            if res.exists() { res.str().into_owned() } else { v }
                        })
                    };
                    group.buckets.push(QuotaBucket {
                        remaining_fraction: fraction,
                        window: literal("window").unwrap_or_default(),
                        description: literal("description").unwrap_or_default(),
                        reset_time: literal("reset_time").unwrap_or_default(),
                    });
                }
            }
            if !group.buckets.is_empty() {
                out.groups.push(group);
            }
        }
    }
    let has_plan = out.subscription.as_ref().is_some_and(|s| !s.plan.trim().is_empty());
    out.summary = usable_summary(raw);
    if out.groups.iter().all(|g| g.buckets.is_empty()) && !has_plan && out.summary.is_empty() {
        return Err("response mapping did not match any valid quota fields in upstream response".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_offset_reads_go_http_time_formats() {
        let now = chrono::Utc::now();
        let later = now + chrono::Duration::seconds(90);
        for date in [
            later.format("%a, %d %b %Y %H:%M:%S GMT").to_string(),
            later.format("%A, %d-%b-%y %H:%M:%S GMT").to_string(),
            later.format("%a %b %e %H:%M:%S %Y").to_string(),
        ] {
            let offset = server_offset_ms(&date).unwrap_or_else(|| panic!("{date}"));
            // Second precision: between 89 and 90 seconds ahead.
            assert!((88_000..=90_000).contains(&offset), "{date}: {offset}");
        }
        assert_eq!(server_offset_ms("yesterday"), None);
    }

    #[test]
    fn numeric_fraction_follows_parse_float() {
        let doc = br#"{"a":0.5,"b":" 0.25 ","c":"x","d":null,"e":"","f":"1e400","g":true}"#;
        let f = |k: &str| numeric_fraction(&gjson::get(doc, k));
        assert_eq!(f("a"), Some(0.5));
        assert_eq!(f("b"), Some(0.25));
        for k in ["c", "d", "e", "f", "g", "missing"] {
            assert_eq!(f(k), None, "{k}");
        }
    }
}
