//! Model listings in Home mode: the catalog Home answers for the calling client, in
//! the shape each route serves (Go internal/api/server_routes.go `handleHomeModels`,
//! `handleGrokModels`, `handleHomeGeminiModels`, `loadHomeModelEntries`,
//! `decodeHomeModels`).

use axum::http::HeaderMap;
use axum::response::Response;
use serde_json::{Map, Value, json};

use crate::registry::Spec;
use crate::remote::{ModelsError, RemoteDispatch};
use crate::{gojson, respond};

/// Go `homeModelEntry`.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Entry {
    pub id: String,
    pub created: i64,
    pub owned_by: String,
    pub display_name: String,
    pub context_length: i64,
    pub max_context_length: i64,
    pub max_completion_tokens: i64,
    pub providers: Vec<String>,
}

impl Entry {
    fn spec(&self) -> Spec {
        Spec {
            id: self.id.clone(),
            created: self.created,
            owned_by: self.owned_by.clone(),
            display_name: self.display_name.clone(),
            context_length: self.context_length,
            max_completion_tokens: self.max_completion_tokens,
            ..Spec::default()
        }
    }
}

/// Go `homeModelInt64Value`: the first key holding a number or a numeric string.
fn int(model: &Map<String, Value>, keys: &[&str]) -> i64 {
    for key in keys {
        match model.get(*key) {
            Some(Value::Number(n)) => {
                return n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)).unwrap_or(0);
            }
            Some(Value::String(s)) => {
                if let Ok(n) = s.trim().parse::<i64>() {
                    return n;
                }
            }
            _ => {}
        }
    }
    0
}

fn text(model: &Map<String, Value>, key: &str) -> String {
    model
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_owned()
}

/// Go `decodeHomeModels`: one entry per ID across provider sections, sorted by ID.
// ponytail: Go walks the sections map in random order, so the provider list and the
// metadata of an ID served by several sections follow it; sections are taken in
// sorted order here, one of Go's possible outcomes.
pub(crate) fn decode(raw: &[u8]) -> Result<Vec<Entry>, String> {
    if raw.is_empty() {
        return Err("home models payload is empty".into());
    }
    // Go `map[string][]map[string]any`: null sections and null entries are empty.
    let parse_error = |e: serde_json::Error| format!("parse home models payload: {e}");
    let sections: Option<Map<String, Value>> = serde_json::from_slice(raw).map_err(parse_error)?;
    let sections = sections.unwrap_or_default();
    let mut parsed: Vec<(String, Vec<Map<String, Value>>)> = Vec::with_capacity(sections.len());
    for (section, models) in sections {
        let models: Option<Vec<Option<Map<String, Value>>>> = serde_json::from_value(models).map_err(parse_error)?;
        parsed.push((section, models.unwrap_or_default().into_iter().flatten().collect()));
    }
    if parsed.is_empty() {
        return Err("home models payload has no sections".into());
    }
    parsed.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out: Vec<Entry> = Vec::new();
    for (section, models) in parsed {
        let provider = section.trim().to_lowercase();
        for model in models {
            let mut id = text(&model, "id");
            if id.is_empty() {
                id = text(&model, "name").trim_start_matches("models/").to_owned();
            }
            if id.is_empty() {
                continue;
            }
            if let Some(existing) = out.iter_mut().find(|e| e.id == id) {
                if !provider.is_empty() && !existing.providers.contains(&provider) {
                    existing.providers.push(provider.clone());
                }
                continue;
            }
            let mut display_name = text(&model, "display_name");
            if display_name.is_empty() {
                display_name = text(&model, "displayName");
            }
            out.push(Entry {
                id,
                created: int(&model, &["created"]),
                owned_by: text(&model, "owned_by"),
                display_name,
                context_length: int(
                    &model,
                    &["context_length", "contextLength", "inputTokenLimit", "max_input_tokens"],
                ),
                max_context_length: int(&model, &["max_context_length", "maxContextLength"]),
                max_completion_tokens: int(
                    &model,
                    &[
                        "max_completion_tokens",
                        "maxCompletionTokens",
                        "outputTokenLimit",
                        "max_tokens",
                    ],
                ),
                providers: if provider.is_empty() {
                    Vec::new()
                } else {
                    vec![provider.clone()]
                },
            });
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    if out.is_empty() {
        return Err("home models payload contains no models".into());
    }
    Ok(out)
}

/// Go `homeModelsAuthStatus` / `homeModelsErrorMessage`: a Home error envelope.
fn error_envelope(raw: &[u8]) -> Option<(u16, String)> {
    let top: Map<String, Value> = serde_json::from_slice(raw).ok()?;
    let error = top.get("error")?;
    let kind = error.get("type")?.as_str()?.trim().to_owned();
    if kind.is_empty() {
        return None;
    }
    let status = if kind == "no_credentials" || kind == "invalid_credential" {
        401
    } else {
        502
    };
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .unwrap_or("home models request failed")
        .to_owned();
    Some((status, message))
}

/// Go `loadHomeModelEntries`.
pub(crate) async fn load(
    remote: &dyn RemoteDispatch,
    headers: &HeaderMap,
    uri: &axum::http::Uri,
) -> Result<Vec<Entry>, Box<Response>> {
    let headers = headers
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_owned(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect();
    let query = url_query(uri);
    let raw = match remote.models(headers, query).await {
        Ok(raw) => raw,
        Err(ModelsError::Unavailable) => {
            return Err(Box::new(respond::error_detail(
                503,
                "home control center unavailable",
                "server_error",
            )));
        }
        Err(ModelsError::Failed(message)) => {
            return Err(Box::new(respond::error_detail(502, &message, "server_error")));
        }
    };
    if let Some((status, message)) = error_envelope(&raw) {
        return Err(Box::new(respond::error_detail(
            status,
            &message,
            "authentication_error",
        )));
    }
    decode(&raw).map_err(|message| Box::new(respond::error_detail(502, &message, "server_error")))
}

fn url_query(uri: &axum::http::Uri) -> Vec<(String, String)> {
    axum::extract::Query::<Vec<(String, String)>>::try_from_uri(uri)
        .map(|q| q.0)
        .unwrap_or_default()
}

/// Go `handleHomeModels`, OpenAI shape: `owned_by` only when Home set one.
pub(crate) fn openai(entries: &[Entry]) -> String {
    let data: Vec<Value> = entries
        .iter()
        .map(|e| {
            let mut model = Map::new();
            model.insert("id".into(), e.id.clone().into());
            model.insert("object".into(), "model".into());
            if e.created > 0 {
                model.insert("created".into(), e.created.into());
            }
            if !e.owned_by.is_empty() {
                model.insert("owned_by".into(), e.owned_by.clone().into());
            }
            Value::Object(model)
        })
        .collect();
    gojson::sorted(&json!({ "object": "list", "data": data }))
}

/// Go `formatHomeClaudeModels` + `claudemodels.BuildResponse`.
pub(crate) fn claude(entries: &[Entry], disable_cloaking: bool) -> String {
    let specs: Vec<Spec> = entries.iter().map(Entry::spec).collect();
    crate::models::claude_list(specs.iter(), disable_cloaking)
}

/// Go `grokModelsFromHomeEntries` + `grokbuild.BuildResponse`.
pub(crate) fn grok(entries: &[Entry]) -> String {
    let data: Vec<Value> = entries
        .iter()
        .map(|e| {
            let name = if e.display_name.is_empty() {
                &e.id
            } else {
                &e.display_name
            };
            let mut entry = Map::new();
            entry.insert("id".into(), e.id.clone().into());
            entry.insert("model".into(), e.id.clone().into());
            entry.insert("name".into(), name.clone().into());
            if e.context_length > 0 {
                entry.insert("context_window".into(), e.context_length.into());
            }
            entry.insert("api_backend".into(), "responses".into());
            entry.insert("supported_in_api".into(), true.into());
            Value::Object(entry)
        })
        .collect();
    // Go marshals the struct: fields in declaration order, not sorted.
    serde_json::to_string(&json!({ "object": "list", "data": data })).unwrap_or_default()
}

/// Go `formatHomeGeminiModel`.
pub(crate) fn gemini(entry: &Entry) -> Value {
    let name = if entry.id.starts_with("models/") {
        entry.id.clone()
    } else {
        format!("models/{}", entry.id)
    };
    let display = if entry.display_name.is_empty() {
        entry.id.clone()
    } else {
        entry.display_name.clone()
    };
    json!({
        "name": name,
        "displayName": display,
        "description": display,
        "supportedGenerationMethods": ["generateContent"],
    })
}

/// Go `homeGeminiModelMatches`.
pub(crate) fn gemini_matches(entry: &Entry, action: &str) -> bool {
    let id = entry.id.trim();
    if id.is_empty() || action.is_empty() {
        return false;
    }
    action == id
        || action == format!("models/{id}")
        || action.trim_start_matches("models/") == id.trim_start_matches("models/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_merges_sections_and_sorts_like_go() {
        let raw = br#"{"Codex":[{"id":"gpt-5","owned_by":"openai","created":5,"context_length":"400000"}],"claude":[{"id":"gpt-5"},{"name":"models/claude-x","displayName":"Claude X","max_tokens":9}],"x":[{"id":""}]}"#;
        let entries = decode(raw).unwrap();
        assert_eq!(
            entries.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            ["claude-x", "gpt-5"]
        );
        assert_eq!(entries[1].providers, ["codex", "claude"]);
        assert_eq!(entries[1].context_length, 400_000, "numeric strings count");
        assert_eq!(
            (entries[0].display_name.as_str(), entries[0].max_completion_tokens),
            ("Claude X", 9)
        );
        assert_eq!(decode(b"").unwrap_err(), "home models payload is empty");
        assert_eq!(decode(b"{}").unwrap_err(), "home models payload has no sections");
        // Go accepts null sections and entries.
        let entries = decode(br#"{"claude":null,"codex":[null,{"id":"gpt-5"}]}"#).unwrap();
        assert_eq!(entries.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["gpt-5"]);
        assert_eq!(decode(b"null").unwrap_err(), "home models payload has no sections");
        assert_eq!(
            decode(br#"{"a":[{"id":" "}]}"#).unwrap_err(),
            "home models payload contains no models"
        );
        assert!(
            decode(br#"{"a":1}"#)
                .unwrap_err()
                .starts_with("parse home models payload: ")
        );
    }

    #[test]
    fn renderers_follow_go_shapes() {
        let entries = decode(br#"{"xai":[{"id":"grok-4","display_name":"Grok 4","context_length":256000,"owned_by":"xai"},{"id":"g2"}]}"#).unwrap();
        assert_eq!(
            openai(&entries),
            r#"{"data":[{"id":"g2","object":"model"},{"id":"grok-4","object":"model","owned_by":"xai"}],"object":"list"}"#
        );
        assert_eq!(
            grok(&entries),
            r#"{"object":"list","data":[{"id":"g2","model":"g2","name":"g2","api_backend":"responses","supported_in_api":true},{"id":"grok-4","model":"grok-4","name":"Grok 4","context_window":256000,"api_backend":"responses","supported_in_api":true}]}"#
        );
        assert_eq!(gemini(&entries[1])["name"], "models/grok-4");
        assert!(gemini_matches(&entries[1], "models/grok-4") && !gemini_matches(&entries[1], "grok"));
        assert_eq!(
            error_envelope(br#"{"error":{"type":"no_credentials"}}"#),
            Some((401, "home models request failed".into()))
        );
        assert_eq!(
            error_envelope(br#"{"error":{"type":"x","message":"m"}}"#),
            Some((502, "m".into()))
        );
        assert_eq!(error_envelope(br#"{"codex":[]}"#), None);
    }
}
