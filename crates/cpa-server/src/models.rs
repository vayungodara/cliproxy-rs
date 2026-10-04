//! Model listing routes: unified `GET /v1/models` and the Gemini catalog
//! (internal/api/server_routes.go, internal/registry/model_registry.go).

use std::sync::Arc;

use axum::extract::{OriginalUri, Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use cpa_common::gostr::GoStr;
use serde_json::{Map, Value, json};

use crate::registry::Spec;
use crate::{Runtime, gojson, respond};

const CLAUDE_MAX_INPUT: i64 = 200_000;
const CLAUDE_MAX_OUTPUT: i64 = 64_000;

/// `GET /v1/models`: Grok Shell (a `grok-shell` User-Agent) gets its own catalog, Codex
/// clients (any `client_version` query key) the Codex client catalog, Anthropic clients
/// (an `Anthropic-Version` header or a `claude-cli` User-Agent) the Anthropic catalog, and
/// everyone else the OpenAI one.
pub async fn unified(State(rt): State<Arc<Runtime>>, OriginalUri(uri): OriginalUri, headers: HeaderMap) -> Response {
    let header = |name: &str| {
        headers
            .get(name)
            .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
    };
    let anthropic = header("anthropic-version").is_some_and(|v| !v.is_empty())
        || header("user-agent").is_some_and(|ua| ua.starts_with("claude-cli"));
    let disable_cloaking = || {
        rt.config()
            .document
            .get("oauth")
            .and_then(|o| o.get("providers"))
            .and_then(|p| p.get("claude"))
            .and_then(|c| c.get("claude-code"))
            .and_then(|c| c.get("disable-cloaking-model-list"))
            .and_then(serde_yaml_ng::Value::as_bool)
            .unwrap_or(false)
    };
    // Go `WriteModelListResponse`: plugin response interceptors see every list.
    let intercept = |source: &'static str, response: Response| {
        let rt = rt.clone();
        let headers = headers.clone();
        async move { crate::plugins::interceptors::model_list(&rt, source, &headers, response).await }
    };
    // Go Home mode: the catalog Home answers for this client.
    if let Some(remote) = rt.remote_dispatch() {
        let entries = match crate::home_models::load(remote.as_ref(), &headers, &uri).await {
            Ok(entries) => entries,
            Err(response) => return *response,
        };
        let grok_shell = header("user-agent").is_some_and(|ua| ua.to_lowercase().contains("grok-shell"));
        // ponytail: Go builds the Codex client catalog (`client_version`) from Home's
        // model IDs with its template metadata; here such clients get the OpenAI list.
        let (source, body) = if grok_shell {
            ("openai", crate::home_models::grok(&entries))
        } else if anthropic && query_value(&uri, "client_version").is_none() {
            ("claude", crate::home_models::claude(&entries, disable_cloaking()))
        } else {
            ("openai", crate::home_models::openai(&entries))
        };
        return intercept(source, respond::gin_json(200, body)).await;
    }
    if header("user-agent").is_some_and(|ua| ua.go_lower().contains("grok-shell")) {
        let registry = rt.registry();
        let list = grok_list(registry.available_with(|c, m| rt.suspension(c, m)));
        return intercept("openai", respond::gin_json(200, list)).await;
    }
    if let Some(version) = query_value(&uri, "client_version") {
        return intercept("openai", crate::codex_models::response(&rt, &version)).await;
    }
    let registry = rt.registry();
    let (source, body) = if anthropic {
        (
            "claude",
            claude_list(registry.available_with(|c, m| rt.suspension(c, m)), disable_cloaking()),
        )
    } else {
        (
            "openai",
            openai_list(registry.available_with(|c, m| rt.suspension(c, m))),
        )
    };
    intercept(source, respond::gin_json(200, body)).await
}

/// Go `c.Request.URL.Query()[key]` present, with `c.Query(key)`'s first value.
fn query_value(uri: &axum::http::Uri, key: &str) -> Option<String> {
    let axum::extract::Query(pairs) = axum::extract::Query::<Vec<(String, String)>>::try_from_uri(uri).ok()?;
    pairs.into_iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

/// Go `OpenAIModels`: id, object, created and owned_by only.
pub fn openai_list<'a>(models: impl Iterator<Item = &'a Spec>) -> String {
    let data: Vec<Value> = models
        .map(|m| {
            let mut entry = Map::new();
            entry.insert("id".into(), m.id.clone().into());
            entry.insert("object".into(), "model".into());
            entry.insert("owned_by".into(), m.owned_by.clone().into());
            if m.created > 0 {
                entry.insert("created".into(), m.created.into());
            }
            Value::Object(entry)
        })
        .collect();
    gojson::sorted(&json!({ "object": "list", "data": data }))
}

/// Go `grokbuild.BuildResponse` over `GetAvailableModelInfos` (sorted by trimmed ID),
/// marshalled in struct field order.
pub fn grok_list<'a>(models: impl Iterator<Item = &'a Spec>) -> String {
    let mut models: Vec<&Spec> = models.collect();
    models.sort_by(|a, b| a.id.trim().cmp(b.id.trim()));
    let data: Vec<String> = models
        .into_iter()
        .map(|m| {
            let name = if m.display_name.is_empty() {
                &m.id
            } else {
                &m.display_name
            };
            let mut entry = gojson::Obj::new()
                .str("id", &m.id)
                .str("model", &m.id)
                .str("name", name);
            if m.context_length > 0 {
                entry = entry.raw("context_window", &m.context_length.to_string());
            }
            entry = entry.str("api_backend", "responses").raw("supported_in_api", "true");
            let efforts: Vec<String> = m
                .thinking
                .iter()
                .flat_map(|t| &t.levels)
                .map(|level| level.trim())
                .filter(|level| !level.is_empty())
                .map(|level| gojson::Obj::new().str("value", level).finish())
                .collect();
            if !efforts.is_empty() {
                entry = entry.raw("reasoning_efforts", &format!("[{}]", efforts.join(",")));
            }
            entry.finish()
        })
        .collect();
    gojson::Obj::new()
        .str("object", "list")
        .raw("data", &format!("[{}]", data.join(",")))
        .finish()
}

/// Go `convertModelToMap(model, "claude")` plus `claudemodels.BuildResponse`.
pub fn claude_list<'a>(models: impl Iterator<Item = &'a Spec>, disable_cloaking: bool) -> String {
    let mut data: Vec<(String, String, Value)> = models
        .map(|m| {
            let id = if disable_cloaking {
                m.id.clone()
            } else {
                crate::claude::ensure_dd(&m.id)
            };
            let display = if m.display_name.is_empty() {
                m.id.clone()
            } else {
                m.display_name.clone()
            };
            let mut entry = Map::new();
            entry.insert("id".into(), id.clone().into());
            entry.insert("object".into(), "model".into());
            entry.insert("owned_by".into(), m.owned_by.clone().into());
            if m.created > 0 {
                entry.insert("created_at".into(), rfc3339(m.created).into());
            }
            entry.insert("type".into(), "model".into());
            entry.insert("display_name".into(), display.clone().into());
            let input = if m.context_length > 0 {
                m.context_length
            } else {
                CLAUDE_MAX_INPUT
            };
            let output = if m.max_completion_tokens > 0 {
                m.max_completion_tokens
            } else {
                CLAUDE_MAX_OUTPUT
            };
            entry.insert("max_input_tokens".into(), input.into());
            entry.insert("max_tokens".into(), output.into());
            (display, id, Value::Object(entry))
        })
        .collect();
    data.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let first = data.first().map(|d| d.1.clone()).unwrap_or_default();
    let last = data.last().map(|d| d.1.clone()).unwrap_or_default();
    let data: Vec<Value> = data.into_iter().map(|d| d.2).collect();
    gojson::sorted(&json!({ "data": data, "has_more": false, "first_id": first, "last_id": last }))
}

/// `time.Unix(s, 0).UTC().Format(time.RFC3339)`.
pub fn rfc3339(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let secs = seconds.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    )
}

/// Go `convertModelToMap(model, "gemini")`.
fn gemini_entry(m: &Spec) -> Map<String, Value> {
    let mut entry = Map::new();
    entry.insert(
        "name".into(),
        if m.name.is_empty() {
            m.id.clone()
        } else {
            m.name.clone()
        }
        .into(),
    );
    let mut put = |k: &str, v: Value, keep: bool| {
        if keep {
            entry.insert(k.into(), v);
        }
    };
    put("version", m.version.clone().into(), !m.version.is_empty());
    put("displayName", m.display_name.clone().into(), !m.display_name.is_empty());
    put("description", m.description.clone().into(), !m.description.is_empty());
    put("inputTokenLimit", m.input_token_limit.into(), m.input_token_limit > 0);
    put(
        "outputTokenLimit",
        m.output_token_limit.into(),
        m.output_token_limit > 0,
    );
    put(
        "supportedGenerationMethods",
        m.supported_generation_methods.clone().into(),
        !m.supported_generation_methods.is_empty(),
    );
    put(
        "supportedInputModalities",
        m.supported_input_modalities.clone().into(),
        !m.supported_input_modalities.is_empty(),
    );
    put(
        "supportedOutputModalities",
        m.supported_output_modalities.clone().into(),
        !m.supported_output_modalities.is_empty(),
    );
    entry
}

/// `GET /v1beta/models` (Go `GeminiModels`).
pub async fn gemini_list(
    State(rt): State<Arc<Runtime>>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response {
    // Go `handleHomeGeminiModels`.
    if let Some(remote) = rt.remote_dispatch() {
        return match crate::home_models::load(remote.as_ref(), &headers, &uri).await {
            Ok(entries) => {
                let models: Vec<Value> = entries.iter().map(crate::home_models::gemini).collect();
                let response = respond::gin_json(200, gojson::sorted(&json!({ "models": models })));
                crate::plugins::interceptors::model_list(&rt, "gemini", &headers, response).await
            }
            Err(response) => *response,
        };
    }
    let registry = rt.registry();
    let models: Vec<Value> = registry
        .available_with(|c, m| rt.suspension(c, m))
        .map(|m| {
            let mut entry = gemini_entry(m);
            if let Some(name) = entry.get("name").and_then(Value::as_str).map(str::to_owned)
                && !name.is_empty()
            {
                if !name.starts_with("models/") {
                    entry.insert("name".into(), format!("models/{name}").into());
                }
                for key in ["displayName", "description"] {
                    if entry.get(key).and_then(Value::as_str).is_none_or(str::is_empty) {
                        entry.insert(key.into(), name.clone().into());
                    }
                }
            }
            entry
                .entry("supportedGenerationMethods")
                .or_insert_with(|| json!(["generateContent"]));
            Value::Object(entry)
        })
        .collect();
    let response = respond::gin_json(200, gojson::sorted(&json!({ "models": models })));
    crate::plugins::interceptors::model_list(&rt, "gemini", &headers, response).await
}

/// `GET /v1beta/models/*action` (Go `GeminiGetHandler`).
pub async fn gemini_get(
    State(rt): State<Arc<Runtime>>,
    Path(action): Path<String>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response {
    let action = action.trim_start_matches('/');
    // Go `handleHomeGeminiModel`.
    if let Some(remote) = rt.remote_dispatch() {
        let action = action.trim();
        return match crate::home_models::load(remote.as_ref(), &headers, &uri).await {
            Ok(entries) => match entries.iter().find(|e| crate::home_models::gemini_matches(e, action)) {
                Some(entry) => respond::gin_json(200, gojson::sorted(&crate::home_models::gemini(entry))),
                None => respond::error_detail(404, "Not Found", "not_found"),
            },
            Err(response) => *response,
        };
    }
    let registry = rt.registry();
    let found = registry
        .available_with(|c, m| rt.suspension(c, m))
        .map(gemini_entry)
        .find(|entry| {
            let name = entry.get("name").and_then(Value::as_str).unwrap_or_default();
            name == action || name == format!("models/{action}")
        });
    match found {
        Some(mut entry) => {
            if let Some(name) = entry.get("name").and_then(Value::as_str).map(str::to_owned)
                && !name.is_empty()
                && !name.starts_with("models/")
            {
                entry.insert("name".into(), format!("models/{name}").into());
            }
            respond::gin_json(200, gojson::sorted(&Value::Object(entry)))
        }
        None => respond::error_detail(404, "Not Found", "not_found"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_matches_go() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_759_276_800), "2025-10-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_709_210_096), "2024-02-29T12:34:56Z");
    }

    #[test]
    fn list_shapes_follow_go_marshal() {
        let spec = Spec {
            id: "gpt-x".into(),
            owned_by: "openai".into(),
            created: 5,
            display_name: "B".into(),
            ..Spec::default()
        };
        let claude = Spec {
            id: "claude-a".into(),
            owned_by: "anthropic".into(),
            display_name: "A".into(),
            context_length: 10,
            ..Spec::default()
        };
        assert_eq!(
            openai_list([&spec].into_iter()),
            r#"{"data":[{"created":5,"id":"gpt-x","object":"model","owned_by":"openai"}],"object":"list"}"#
        );
        assert_eq!(
            claude_list([&spec, &claude].into_iter(), false),
            r#"{"data":[{"display_name":"A","id":"claude-a","max_input_tokens":10,"max_tokens":64000,"object":"model","owned_by":"anthropic","type":"model"},{"created_at":"1970-01-01T00:00:05Z","display_name":"B","id":"claude-fable-5-dd-x-tpg","max_input_tokens":200000,"max_tokens":64000,"object":"model","owned_by":"openai","type":"model"}],"first_id":"claude-a","has_more":false,"last_id":"claude-fable-5-dd-x-tpg"}"#
        );
    }
}
