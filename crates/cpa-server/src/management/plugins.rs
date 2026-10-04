//! Plugin management routes (internal/api/handlers/management/plugins.go) and the
//! NoRoute fallback that serves plugin-declared management and resource routes
//! (internal/api/server_management.go `pluginManagementNoRoute`).

use std::collections::BTreeMap;
use std::sync::{Arc, PoisonError};

use axum::body::{Body, Bytes};
use axum::extract::{Path as UrlPath, Request, State};
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use cpa_core::config::{Config, ConfigDocument, archive_comments};
use cpa_plugin::config::{self as pcfg, ItemConfig};
use cpa_plugin::gojson::{self as pjson, Node};
use serde_json::{Value, json};
use serde_yaml_ng::{Mapping, Value as Yaml};
use tower::ServiceExt as _;

use super::{Management, access};

// ---- Go JSON responses -------------------------------------------------------------

/// gin `c.JSON` of a `gin.H` or `map[string]any`: keys sorted at every level.
pub(super) fn map_json(status: StatusCode, value: &Value) -> Response {
    body_json(status, crate::gojson::sorted(value))
}

/// gin `c.JSON` of a struct: fields in declaration order (the `json!` order here).
pub(super) fn struct_json(status: StatusCode, value: &Value) -> Response {
    let mut out = String::new();
    ordered(value, &mut out);
    body_json(status, out)
}

fn body_json(status: StatusCode, body: String) -> Response {
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

/// Go `url.ParseQuery` as `URL.Query()` uses it: pairs in order; a pair with a raw
/// `;` or a bad percent escape is dropped, the rest kept; `+` is a space.
pub(crate) fn go_query(raw: &str) -> Vec<(String, String)> {
    fn unescape(s: &str) -> Option<String> {
        let b = s.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            match b[i] {
                b'%' => {
                    let hex = b.get(i + 1..i + 3)?;
                    out.push(u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?);
                    i += 3;
                }
                b'+' => {
                    out.push(b' ');
                    i += 1;
                }
                c => {
                    out.push(c);
                    i += 1;
                }
            }
        }
        Some(String::from_utf8_lossy(&out).into_owned())
    }
    raw.split('&')
        .filter(|pair| !pair.is_empty() && !pair.contains(';'))
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            Some((unescape(k)?, unescape(v)?))
        })
        .collect()
}

/// A struct-shaped value as Go's `encoding/json` writes it: fields in order.
pub(super) fn ordered_json(v: &Value) -> String {
    let mut out = String::new();
    ordered(v, &mut out);
    out
}

fn ordered(v: &Value, out: &mut String) {
    match v {
        Value::Object(map) => {
            out.push('{');
            for (i, (k, v)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&crate::gojson::string(k));
                out.push(':');
                ordered(v, out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, v) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                ordered(v, out);
            }
            out.push(']');
        }
        Value::String(s) => out.push_str(&crate::gojson::string(s)),
        other => out.push_str(&other.to_string()),
    }
}

/// An early response; boxed to keep `Result`s small.
pub(super) type Res<T> = Result<T, Box<Response>>;

pub(super) fn error(status: StatusCode, code: &str, message: &str) -> Response {
    map_json(status, &json!({"error": code, "message": message}))
}

fn not_found_plugin() -> Response {
    error(StatusCode::NOT_FOUND, "plugin_not_found", "plugin not found")
}

/// Go `htmlsanitize.String` (`html.EscapeString`).
pub(super) fn esc(s: &str) -> String {
    cpa_plugin::management::html_escape(s)
}

// ---- config view ---------------------------------------------------------------------

/// The `plugins:` section as the management handlers read it from the live config.
pub(super) struct PluginsView {
    pub(super) enabled: bool,
    /// Go `normalizedPluginsDir`, before resolving.
    pub(super) dir: String,
    /// Configured plugins by ID, with their raw subtree.
    pub(super) configs: BTreeMap<String, (ItemConfig, Yaml)>,
}

impl PluginsView {
    pub(super) fn of(cfg: &Config) -> Self {
        let plugins = cfg.document.get("plugins");
        let enabled = plugins
            .and_then(|p| p.get("enabled"))
            .and_then(pcfg::yaml_bool)
            .unwrap_or(false);
        let dir = pcfg::configured_dir(&cfg.document).trim().to_owned();
        let dir = if dir.is_empty() { "plugins".to_owned() } else { dir };
        let mut configs = BTreeMap::new();
        if let Some(Yaml::Mapping(items)) = plugins.and_then(|p| p.get("configs")) {
            for (key, node) in items {
                let id = pcfg::yaml_string(key);
                configs.insert(id.clone(), (pcfg::item_config(&id, node), node.clone()));
            }
        }
        Self { enabled, dir, configs }
    }

    /// Go `pluginStoreDesiredVersions`: discovery then drops versions it rejects.
    pub(super) fn desired_versions(&self) -> BTreeMap<String, String> {
        self.configs
            .iter()
            .filter_map(|(id, (_, raw))| {
                let version = store_desired_version(raw);
                (!id.trim().is_empty() && !version.is_empty()).then(|| (id.trim().to_owned(), version))
            })
            .collect()
    }
}

/// Go `pluginStoreDesiredVersion`: `store.version`, else `store.release-tag`, with one
/// leading `v` dropped; empty unless it starts with a digit.
fn store_desired_version(raw: &Yaml) -> String {
    let normalize = |v: &str| {
        let v = v.trim();
        let v = if v.len() > 1 && v.starts_with(['v', 'V']) {
            &v[1..]
        } else {
            v
        };
        if v.starts_with(|c: char| c.is_ascii_digit()) {
            v.to_owned()
        } else {
            String::new()
        }
    };
    let Some(store) = raw.get("store").filter(|s| s.is_mapping()) else {
        return String::new();
    };
    // Go reads the scalar's text whatever its tag; containers give "".
    let scalar = |key: &str| store.get(key).map(pcfg::yaml_string).unwrap_or_default();
    let version = normalize(&scalar("version"));
    if !version.is_empty() {
        return version;
    }
    normalize(&scalar("release-tag"))
}

pub(super) fn resolved_dir(dir: &str) -> Res<std::path::PathBuf> {
    pcfg::resolve_dir(dir)
        .map_err(|e| Box::new(error(StatusCode::INTERNAL_SERVER_ERROR, "plugin_directory_invalid", &e)))
}

pub(super) fn discover(
    dir: &std::path::Path,
    desired: &BTreeMap<String, String>,
) -> Res<Vec<cpa_plugin::platform::PluginFile>> {
    cpa_plugin::platform::discover(dir, desired).map_err(|e| {
        Box::new(error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "plugin_discovery_failed",
            &e.to_string(),
        ))
    })
}

/// Go `pluginIDFromRequest`.
pub(super) fn plugin_id(raw: &str) -> Res<String> {
    let id = raw.trim();
    if !cpa_plugin::platform::valid_id(id) {
        return Err(Box::new(error(
            StatusCode::BAD_REQUEST,
            "invalid_plugin_id",
            "invalid plugin id",
        )));
    }
    Ok(id.to_owned())
}

// ---- GET /plugins --------------------------------------------------------------------

#[derive(Default)]
struct Entry {
    id: String,
    path: String,
    configured: bool,
    registered: bool,
    enabled: bool,
    supports_oauth: bool,
    oauth_provider: String,
    supports_quota: bool,
    quota_provider: String,
    logo: String,
    config_fields: Vec<Value>,
    menus: Vec<Value>,
    metadata: Option<Value>,
}

fn config_fields(fields: &[cpa_plugin::api::ConfigField]) -> Vec<Value> {
    fields
        .iter()
        .map(|f| {
            json!({
                "name": esc(&f.name),
                "type": esc(&f.field_type),
                "enum_values": f.enum_values.iter().map(|v| esc(v)).collect::<Vec<_>>(),
                "description": esc(&f.description),
            })
        })
        .collect()
}

/// Go `ListPlugins`: discovered files, configured entries and registered plugins, by ID.
pub(crate) async fn list(State(state): State<Arc<Management>>) -> Response {
    let cfg = state.rt.config();
    let view = PluginsView::of(&cfg);
    let dir = match resolved_dir(&view.dir) {
        Ok(d) => d,
        Err(r) => return *r,
    };
    let files = match discover(&dir, &view.desired_versions()) {
        Ok(f) => f,
        Err(r) => return *r,
    };
    let mut entries: BTreeMap<String, Entry> = BTreeMap::new();
    for file in files {
        entries.insert(
            file.id.clone(),
            Entry {
                id: esc(&file.id),
                path: esc(&file.path.to_string_lossy()),
                ..Default::default()
            },
        );
    }
    for (id, (item, _)) in &view.configs {
        let entry = entries.entry(id.clone()).or_default();
        entry.id = esc(id);
        entry.configured = true;
        entry.enabled = item.enabled;
    }
    for info in state.rt.plugins().registered_plugins() {
        let entry = entries.entry(info.id.clone()).or_default();
        let meta = &info.metadata;
        entry.id = esc(&info.id);
        entry.registered = true;
        entry.supports_oauth = info.supports_oauth;
        entry.oauth_provider = esc(&info.oauth_provider);
        entry.supports_quota = info.supports_quota;
        entry.quota_provider = esc(&info.quota_provider);
        entry.logo = esc(&meta.logo);
        entry.config_fields = config_fields(meta.config_field_list());
        entry.menus = info
            .menus
            .iter()
            .map(|m| json!({"path": esc(&m.path), "menu": esc(&m.menu), "description": esc(&m.description)}))
            .collect();
        entry.metadata = Some(json!({
            "name": esc(&meta.name),
            "version": esc(&meta.version),
            "author": esc(&meta.author),
            "github_repository": esc(&meta.github_repository),
            "logo": esc(&meta.logo),
            "config_fields": config_fields(meta.config_field_list()),
        }));
    }
    let plugins: Vec<Value> = entries
        .into_values()
        .map(|e| {
            let mut v = json!({
                "id": e.id,
                "path": e.path,
                "configured": e.configured,
                "registered": e.registered,
                "enabled": e.enabled,
                "effective_enabled": view.enabled && e.enabled && e.registered,
                "supports_oauth": e.supports_oauth,
                "oauth_provider": e.oauth_provider,
                "supports_quota": e.supports_quota,
            });
            let map = v.as_object_mut().expect("object");
            if !e.quota_provider.is_empty() {
                map.insert("quota_provider".into(), e.quota_provider.into());
            }
            map.insert("logo".into(), e.logo.into());
            map.insert("config_fields".into(), e.config_fields.into());
            map.insert("menus".into(), e.menus.into());
            map.insert("metadata".into(), e.metadata.unwrap_or(Value::Null));
            v
        })
        .collect();
    struct_json(
        StatusCode::OK,
        &json!({
            "plugins_enabled": view.enabled,
            "plugins_dir": esc(&dir.to_string_lossy()),
            "plugins": plugins,
        }),
    )
}

// ---- GET /plugins/:id/config ---------------------------------------------------------

/// Go `pluginConfigNode`: the raw subtree when it is a mapping, else one built from the
/// host-owned fields. yaml.v3 skips `UnmarshalYAML` for a null entry, which therefore
/// has no `enabled` either.
fn config_node(item: &ItemConfig, raw: &Yaml) -> Mapping {
    match raw {
        Yaml::Mapping(m) => return m.clone(),
        Yaml::Null => return Mapping::new(),
        _ => {}
    }
    let mut node = Mapping::new();
    node.insert("enabled".into(), item.enabled.into());
    if item.priority != 0 {
        node.insert("priority".into(), item.priority.into());
    }
    node
}

/// Go `yamlNodeToJSONValue` (gin then sorts the keys).
fn yaml_to_json(v: &Yaml) -> Value {
    match v {
        Yaml::Null => Value::Null,
        Yaml::Bool(b) => (*b).into(),
        Yaml::Number(n) => serde_json::to_value(n).unwrap_or(Value::Null),
        Yaml::String(s) => s.clone().into(),
        Yaml::Sequence(items) => items.iter().map(yaml_to_json).collect(),
        Yaml::Mapping(m) => Value::Object(m.iter().map(|(k, v)| (pcfg::yaml_string(k), yaml_to_json(v))).collect()),
        Yaml::Tagged(t) => yaml_to_json(&t.value),
    }
}

pub(crate) async fn get_config(State(state): State<Arc<Management>>, UrlPath(raw): UrlPath<String>) -> Response {
    let id = match plugin_id(&raw) {
        Ok(id) => id,
        Err(r) => return *r,
    };
    let cfg = state.rt.config();
    let view = PluginsView::of(&cfg);
    if let Some((item, raw)) = view.configs.get(&id) {
        return map_json(StatusCode::OK, &yaml_to_json(&Yaml::Mapping(config_node(item, raw))));
    }
    if state.rt.plugins().registered_plugins().iter().any(|p| p.id == id) {
        return map_json(StatusCode::OK, &json!({}));
    }
    let dir = match resolved_dir(&view.dir) {
        Ok(d) => d,
        Err(r) => return *r,
    };
    // Go `pluginDiscovered` asks without desired versions.
    match discover(&dir, &BTreeMap::new()) {
        Ok(files) if files.iter().any(|f| f.id == id) => map_json(StatusCode::OK, &json!({})),
        Ok(_) => not_found_plugin(),
        Err(r) => *r,
    }
}

// ---- config writes -------------------------------------------------------------------

/// Why a plugin config write failed.
pub(super) enum Fail {
    /// A response the edit produced.
    Response(Box<Response>),
    /// Reading, rendering or writing the file failed (Go's saver error).
    Save(String),
}

impl Fail {
    fn into_response(self) -> Response {
        match self {
            Fail::Response(r) => *r,
            Fail::Save(e) => map_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                &json!({"error": format!("failed to save config: {e}")}),
            ),
        }
    }
}

/// Reads, edits and saves the config file the way `config_sync` does, then publishes
/// it. Go's typed saver also writes `plugins.dir` resolved and cleaned (it was resolved
/// at load), so the edit does the same.
pub(super) fn save(state: &Management, edit: impl FnOnce(&mut ConfigDocument) -> Res<()>) -> Result<(), Fail> {
    let failed = |e: &dyn std::fmt::Display| Fail::Save(e.to_string());
    let _guard = state.disk.lock().unwrap_or_else(PoisonError::into_inner);
    let original = std::fs::read_to_string(&state.path).map_err(|e| failed(&e))?;
    let mut doc = ConfigDocument::parse(&original).map_err(|e| failed(&e))?;
    let basis = doc.migrated_text(&original).unwrap_or_else(|| original.clone());
    let archived = doc.archive_unknown();
    // Go `NormalizePluginsConfig` gives a nil configs map a value before the edit.
    for path in [&["plugins"][..], &["plugins", "configs"][..]] {
        if doc.get(path).is_some_and(Yaml::is_null) {
            doc.update(path, Yaml::Mapping(Mapping::new()), false)
                .map_err(|e| failed(&e))?;
        }
    }
    edit(&mut doc).map_err(Fail::Response)?;
    // Go's saver writes each plugin config through `MarshalYAML`: a null entry (never
    // unmarshalled) becomes an empty mapping.
    let nulls: Vec<String> = match doc.get(&["plugins", "configs"]) {
        Some(Yaml::Mapping(items)) => items
            .iter()
            .filter(|(_, v)| v.is_null())
            .map(|(k, _)| pcfg::yaml_string(k))
            .collect(),
        _ => Vec::new(),
    };
    for id in nulls {
        doc.update(&["plugins", "configs", &id], Yaml::Mapping(Mapping::new()), false)
            .map_err(|e| failed(&e))?;
    }
    let dir = doc.get(&["plugins", "dir"]).map(pcfg::yaml_string).unwrap_or_default();
    if let Ok(resolved) = pcfg::resolve_dir(&dir) {
        let resolved = resolved.to_string_lossy().into_owned();
        if doc.get(&["plugins", "dir"]).and_then(Yaml::as_str) != Some(resolved.as_str()) {
            doc.update(&["plugins", "dir"], resolved.into(), false)
                .map_err(|e| failed(&e))?;
        }
    }
    let text = doc.render_preserving(&basis).map_err(|e| failed(&e))? + &archive_comments(&archived);
    let cfg = Config::parse(&text).map_err(|e| failed(&e))?;
    ConfigDocument::write(&state.path, &text).map_err(|e| failed(&e))?;
    state.publish(cfg, None);
    Ok(())
}

/// Runs a [`save`] off the async runtime and answers Go's `{"status":"ok"}`.
async fn save_and_answer(
    state: Arc<Management>,
    edit: impl FnOnce(&mut ConfigDocument) -> Res<()> + Send + 'static,
) -> Response {
    match tokio::task::spawn_blocking(move || save(&state, edit)).await {
        Ok(Ok(())) => ok_status(),
        Ok(Err(fail)) => fail.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

fn ok_status() -> Response {
    map_json(StatusCode::OK, &json!({"status": "ok"}))
}

/// The current subtree for `id` as Go's `pluginConfigNode` sees it.
pub(super) fn current_node(doc: &ConfigDocument, id: &str) -> Mapping {
    let raw = doc.get(&["plugins", "configs", id]).cloned().unwrap_or(Yaml::Null);
    config_node(&pcfg::item_config(id, &raw), &raw)
}

pub(super) fn set_node(doc: &mut ConfigDocument, id: &str, node: Mapping) -> Res<()> {
    doc.update(&["plugins", "configs", id], Yaml::Mapping(node), false)
        .map_err(|e| Box::new(Fail::Save(e.to_string()).into_response()))
}

/// Go `readPluginConfigObject`: a JSON object with numbers kept as written, read with
/// `json.Decoder.Decode` (the first value; anything after it is never read).
fn config_object(body: &[u8]) -> Res<Vec<(String, Node)>> {
    let invalid = |message: &str| Err(Box::new(error(StatusCode::BAD_REQUEST, "invalid_body", message)));
    let kind = match pjson::parse_first(body) {
        Ok(mut node) => match &mut node {
            Node::Object(fields) => return Ok(std::mem::take(fields)),
            Node::Null => return invalid("body must be a JSON object"),
            Node::Array(_) => "array",
            Node::String(_) => "string",
            Node::Number(_) => "number",
            Node::Bool(_) => "bool",
        },
        Err(e) => return invalid(&e.to_string()),
    };
    invalid(&format!(
        "json: cannot unmarshal {kind} into Go value of type map[string]interface {{}}"
    ))
}

/// Go `yamlNodeFromJSONValue`: numbers stay integers when `ParseInt` takes them, else
/// floats; a float out of range is `invalid number`.
fn json_to_yaml(v: &Node) -> Result<Yaml, String> {
    Ok(match v {
        Node::Null => Yaml::Null,
        Node::Bool(b) => (*b).into(),
        Node::Number(n) => match n.parse::<i64>() {
            Ok(i) => i.into(),
            // Go `json.Number.Float64` is `strconv.ParseFloat`: out of range fails.
            Err(_) => match cpa_plugin::cli::parse_float(n) {
                Some(f) => f.into(),
                None => return Err(format!("invalid number {}", crate::gojson::string(n))),
            },
        },
        Node::String(s) => s.clone().into(),
        Node::Array(items) => Yaml::Sequence(items.iter().map(json_to_yaml).collect::<Result<_, _>>()?),
        Node::Object(fields) => Yaml::Mapping(object_to_yaml(fields)?),
    })
}

/// Go's `map[string]any` of an object: the last duplicate wins.
fn collapse(fields: &[(String, Node)]) -> BTreeMap<&str, &Node> {
    fields.iter().map(|(k, v)| (k.as_str(), v)).collect()
}

/// Go `yamlNodeFromJSONObject`: keys sorted; an error names its key.
fn object_to_yaml(fields: &[(String, Node)]) -> Result<Mapping, String> {
    collapse(fields)
        .into_iter()
        .map(|(k, v)| Ok((Yaml::from(k), json_to_yaml(v).map_err(|e| format!("{k}: {e}"))?)))
        .collect()
}

fn invalid_body(e: &str) -> Response {
    error(StatusCode::BAD_REQUEST, "invalid_body", e)
}

fn invalid_config(e: String) -> Response {
    error(StatusCode::BAD_REQUEST, "invalid_config", &e)
}

/// Go `PatchPluginEnabled`.
pub(crate) async fn patch_enabled(
    State(state): State<Arc<Management>>,
    UrlPath(raw): UrlPath<String>,
    body: Bytes,
) -> Response {
    let id = match plugin_id(&raw) {
        Ok(id) => id,
        Err(r) => return *r,
    };
    // gin ShouldBindJSON into `struct{ Enabled *bool }`.
    let enabled = match serde_json::from_slice::<Value>(&body) {
        Ok(Value::Object(map)) => match map.get("enabled") {
            Some(Value::Bool(b)) => Some(*b),
            _ => None,
        },
        _ => None,
    };
    let Some(enabled) = enabled else {
        return error(StatusCode::BAD_REQUEST, "invalid_body", "enabled is required");
    };
    save_and_answer(state, move |doc| {
        let mut node = current_node(doc, &id);
        set_key(&mut node, "enabled", enabled.into());
        set_node(doc, &id, node)
    })
    .await
}

/// Go `setYAMLMappingValue`: replace in place, else append.
pub(super) fn set_key(node: &mut Mapping, key: &str, value: Yaml) {
    node.insert(Yaml::from(key), value);
}

/// Go `PutPluginConfig`: replaces the subtree.
pub(crate) async fn put_config(
    State(state): State<Arc<Management>>,
    UrlPath(raw): UrlPath<String>,
    body: Bytes,
) -> Response {
    let id = match plugin_id(&raw) {
        Ok(id) => id,
        Err(r) => return *r,
    };
    let fields = match config_object(&body) {
        Ok(f) => f,
        Err(r) => return *r,
    };
    let node = match object_to_yaml(&fields) {
        Ok(node) => node,
        Err(e) => return invalid_body(&e),
    };
    if let Err(e) = pcfg::check_json_item(&fields) {
        return invalid_config(e);
    }
    save_and_answer(state, move |doc| set_node(doc, &id, node)).await
}

/// Go `PatchPluginConfig`: shallow merge; `null` deletes a key.
pub(crate) async fn patch_config(
    State(state): State<Arc<Management>>,
    UrlPath(raw): UrlPath<String>,
    body: Bytes,
) -> Response {
    let id = match plugin_id(&raw) {
        Ok(id) => id,
        Err(r) => return *r,
    };
    let fields = match config_object(&body) {
        Ok(f) => f,
        Err(r) => return *r,
    };
    // Go decodes into a map (the last duplicate wins), then walks the sorted keys:
    // `null` deletes, anything else converts or fails the request.
    let mut changes: Vec<(String, Option<Yaml>)> = Vec::new();
    for (key, value) in collapse(&fields) {
        if value.is_null() {
            changes.push((key.to_owned(), None));
            continue;
        }
        match json_to_yaml(value) {
            Ok(v) => changes.push((key.to_owned(), Some(v))),
            Err(e) => return invalid_body(&e),
        }
    }
    let set: Vec<(String, Node)> = collapse(&fields)
        .into_iter()
        .filter(|(_, v)| !v.is_null())
        .map(|(k, v)| (k.to_owned(), v.clone()))
        .collect();
    if let Err(e) = pcfg::check_json_item(&set) {
        return invalid_config(e);
    }
    save_and_answer(state, move |doc| {
        let mut node = current_node(doc, &id);
        for (key, value) in changes {
            match value {
                None => {
                    node.remove(Yaml::from(key.as_str()));
                }
                Some(value) => set_key(&mut node, &key, value),
            }
        }
        set_node(doc, &id, node)
    })
    .await
}

// ---- DELETE /plugins/:id -------------------------------------------------------------

/// `DELETE /v8/management/plugins/store`: gin's DELETE tree has only `plugins/:id`, so
/// Go deletes the plugin with the ID `store`.
pub(crate) async fn delete_store_id(State(state): State<Arc<Management>>) -> Response {
    delete(State(state), UrlPath("store".to_owned())).await
}

/// Go `DeletePlugin`: removes the selected plugin file and its saved config.
pub(crate) async fn delete(State(state): State<Arc<Management>>, UrlPath(raw): UrlPath<String>) -> Response {
    let id = match plugin_id(&raw) {
        Ok(id) => id,
        Err(r) => return *r,
    };
    let cfg = state.rt.config();
    let view = PluginsView::of(&cfg);
    let configured = view.configs.contains_key(&id);
    let dir = match resolved_dir(&view.dir) {
        Ok(d) => d,
        Err(r) => return *r,
    };
    let desired = if configured {
        view.desired_versions().into_iter().filter(|(k, _)| *k == id).collect()
    } else {
        BTreeMap::new()
    };
    let path = match discover(&dir, &desired) {
        Ok(files) => files
            .into_iter()
            .find(|f| f.id == id)
            .map(|f| f.path.to_string_lossy().into_owned())
            .unwrap_or_default(),
        Err(r) => return *r,
    };
    if path.is_empty() && !configured {
        return not_found_plugin();
    }
    let host = state.rt.plugins();
    if host.plugin_busy(&id) && !host.unload_plugin(&id, None).await && host.plugin_busy(&id) {
        return map_json(
            StatusCode::CONFLICT,
            &json!({
                "error": "plugin_delete_requires_restart",
                "message": "loaded plugin cannot be deleted while the server is running",
                "restart_required": true,
            }),
        );
    }
    let mut file_deleted = false;
    if !path.is_empty() {
        match std::fs::remove_file(&path) {
            Ok(()) => file_deleted = true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "plugin_delete_failed",
                    &e.to_string(),
                );
            }
        }
    }
    let saved = {
        let state = state.clone();
        let id = id.clone();
        tokio::task::spawn_blocking(move || {
            if !configured {
                // Go reloads even without a save, so the host drops the deleted file.
                let _guard = state.disk.lock().unwrap_or_else(PoisonError::into_inner);
                state.publish((*state.rt.config()).clone(), None);
                return Ok(());
            }
            match save(&state, |doc| {
                doc.delete(&["plugins", "configs", &id]);
                Ok(())
            }) {
                Err(Fail::Save(e)) => Err(e),
                _ => Ok(()),
            }
        })
        .await
        .unwrap_or_else(|_| Err("save task failed".into()))
    };
    if let Err(e) = saved {
        return map_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            &json!({
                "error": "config_save_failed",
                "message": format!("plugin deleted but saving config failed: {e}"),
                "file_deleted": file_deleted,
                "path": path,
            }),
        );
    }
    map_json(
        StatusCode::OK,
        &json!({
            "status": "deleted",
            "id": esc(&id),
            "path": esc(&path),
            "file_deleted": file_deleted,
            "configured_removed": configured,
            "restart_required": false,
        }),
    )
}

// ---- NoRoute -------------------------------------------------------------------------

/// Go `pluginManagementNoRoute`: unknown paths (and unregistered methods on known ones)
/// under `/v0/management` go to plugin routes after the management guard; plugin
/// resources are public; anything else is a bare 404.
/// ponytail: Go's plugin OAuth `auth-url` handling (`ServePluginAuthURL`) joins with
/// the plugin auth-file login flow.
pub(crate) async fn no_route(State(state): State<Arc<Management>>, req: Request) -> Response {
    // gin matches NoRoute prefixes on the decoded path.
    let path = super::percent_decode(req.uri().path());
    if path.starts_with("/v0/resource/plugins/") {
        // Go also refuses resources in Home mode, which this server never runs in
        // (`-home-jwt` is refused at startup); `home` in the YAML is ignored, as in Go.
        let (parts, _) = req.into_parts();
        let inbound = inbound(&parts, Bytes::new());
        return reply(state.rt.plugins().serve_resource(inbound).await);
    }
    if path != "/v0/management" && !path.starts_with("/v0/management/") {
        return access::not_found();
    }
    let inner: axum::routing::MethodRouter<(), std::convert::Infallible> = axum::routing::any(serve_management)
        .layer(axum::middleware::from_fn_with_state(state.clone(), access::guard))
        .with_state(state);
    match inner.oneshot(req).await {
        Ok(r) => r,
        Err(never) => match never {},
    }
}

async fn serve_management(State(state): State<Arc<Management>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    // Go `ServePluginAuthURL` runs before the plugin-declared routes.
    let path = super::percent_decode(parts.uri.path());
    if let Some(response) =
        super::plugin_oauth::serve_auth_url(&state, &path, parts.uri.query().unwrap_or_default()).await
    {
        return response;
    }
    let host = state.rt.plugins();
    if !host.has_management_route(parts.method.as_str(), &super::percent_decode(parts.uri.path())) {
        return access::not_found();
    }
    // Go reads the whole body (no cap) once a route matches, and answers 400 when that
    // fails.
    let body = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(body) => body,
        Err(_) => {
            return reply(Some(cpa_plugin::management::Reply::error(
                400,
                "failed to read plugin management request body",
            )));
        }
    };
    let inbound = inbound(&parts, body);
    reply(host.serve_management(inbound).await)
}

fn inbound(parts: &axum::http::request::Parts, body: Bytes) -> cpa_plugin::management::Inbound {
    cpa_plugin::management::Inbound {
        method: parts.method.as_str().to_owned(),
        path: super::percent_decode(parts.uri.path()),
        headers: crate::plugins::go_request_header(&parts.headers),
        query: crate::plugins::go_values(parts.uri.query().unwrap_or_default()),
        body,
        scope: Default::default(),
    }
}

fn reply(reply: Option<cpa_plugin::management::Reply>) -> Response {
    let Some(reply) = reply else {
        return access::not_found();
    };
    let mut response = Response::new(Body::from(reply.body));
    *response.status_mut() = StatusCode::from_u16(reply.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    for (name, value) in reply.headers {
        let canonical = cpa_exec::proxy::canonical_header(&name);
        if let (Ok(name), Ok(value)) = (HeaderName::try_from(canonical), HeaderValue::from_str(&value)) {
            response.headers_mut().append(name, value);
        }
    }
    response
}
