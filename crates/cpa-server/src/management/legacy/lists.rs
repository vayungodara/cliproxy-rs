//! Go's v0 writes of the provider key lists and OAuth maps (`config_lists.go`).
//!
//! Each handler edits Go's runtime list (the GET /config view), runs Go's family
//! normalizer and sanitizer, and persists the result as v8 groups through the shared
//! config writer. As Go's saver (`restoreV8Layout`), a family is regrouped only when
//! its list changed, one group per entry (`groupLegacyKeys`), so GET /config after a
//! write shows what Go keeps in memory.
use std::sync::Arc;

use axum::http::{Method, StatusCode};
use axum::response::Response;
use serde_json::{Map, Value, json};

use super::decode;
use super::view::{self, Shape};
use super::{Management, go_json, go_trim, keys, query_first, remove, sscanf_int, touch, write};

const META_BASE_URL: &str = "https://api.meta.ai/v1";

/// A Go error answer: its status and `error` text.
struct Fail(StatusCode, String);

impl Fail {
    fn bad(message: impl Into<String>) -> Self {
        Fail(StatusCode::BAD_REQUEST, message.into())
    }

    fn not_found(message: &str) -> Self {
        Fail(StatusCode::NOT_FOUND, message.into())
    }

    fn response(self) -> Response {
        go_json(self.0, &json!({ "error": self.1 }))
    }
}

fn text(o: &Map<String, Value>, k: &str) -> String {
    o.get(k).and_then(Value::as_str).unwrap_or_default().to_owned()
}

fn trimmed(o: &Map<String, Value>, k: &str) -> String {
    go_trim(&text(o, k)).to_owned()
}

fn entries(list: &Value) -> Vec<Map<String, Value>> {
    list.as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e.as_object().cloned())
        .collect()
}

fn array(list: Vec<Map<String, Value>>) -> Value {
    Value::Array(list.into_iter().map(Value::Object).collect())
}

/// Go `append([]T(nil), v...)`: an empty list becomes nil.
fn nil_if_empty(list: Vec<Map<String, Value>>) -> Value {
    if list.is_empty() { Value::Null } else { array(list) }
}

/// Go `append([]T(nil), v...)` on a decoded JSON list.
fn copied(v: &Value) -> Value {
    match v {
        Value::Array(a) if !a.is_empty() => v.clone(),
        _ => Value::Null,
    }
}

fn v8_family(family: &str) -> &'static str {
    view::KEY_FAMILIES
        .iter()
        .find(|(legacy, _)| *legacy == family)
        .map_or("", |(_, v8)| v8)
}

/// A JSON-view value as Go's `yaml.Marshal` writes it: YAML field names; nils,
/// non-pointer zero scalars and empty `omitempty` collections left out (each decodes
/// back to the same runtime value).
fn yaml(shape: &Shape, v: &Value) -> Value {
    let dropped = |f: &Shape, v: &Value| match v {
        Value::Null => true,
        Value::Bool(b) => !b && !f.ptr,
        Value::Number(n) => n.as_i64() == Some(0) && !f.ptr,
        Value::String(s) => s.is_empty() && !f.ptr,
        Value::Array(a) => a.is_empty() && f.omit,
        Value::Object(o) => o.is_empty() && f.omit && f.kind == "map",
    };
    match (shape.kind.as_str(), v) {
        ("struct", Value::Object(o)) => Value::Object(
            shape
                .fields
                .iter()
                .filter_map(|f| {
                    let x = o.get(&f.json).filter(|x| !dropped(f, x))?;
                    Some((f.yaml.clone(), yaml(f, x)))
                })
                .collect(),
        ),
        ("slice", Value::Array(a)) => {
            let elem = shape.elem.as_deref().expect("slice element");
            Value::Array(a.iter().map(|x| yaml(elem, x)).collect())
        }
        ("map", Value::Object(o)) => {
            let elem = shape.elem.as_deref().expect("map element");
            Value::Object(o.iter().map(|(k, x)| (k.clone(), yaml(elem, x))).collect())
        }
        _ => v.clone(),
    }
}

/// Go `groupLegacyKeys`: one v8 group per legacy entry. OpenAI-compatible entries are
/// groups already (`api-key-entries` become `keys`); other families move `base-url`
/// and the shared fields onto a group named `<family>-<n>`.
fn groups(family: &str, list: &[Value]) -> Value {
    let elem = view::field(family).elem.as_deref().expect("list element");
    let v8 = v8_family(family);
    let groups = list.iter().enumerate().map(|(i, e)| {
        let Value::Object(mut key) = yaml(elem, e) else {
            return Value::Null;
        };
        if family == "openai-compatibility" {
            let keys = key.shift_remove("api-key-entries").unwrap_or_else(|| json!([]));
            key.insert("keys".into(), keys);
            return Value::Object(key);
        }
        let mut group = Map::new();
        group.insert("name".into(), format!("{v8}-{}", i + 1).into());
        let shared: Vec<String> = key
            .keys()
            .filter(|k| view::SHARED_KEY_FIELDS.contains(&k.as_str()))
            .cloned()
            .collect();
        for k in shared {
            if let Some(v) = key.shift_remove(&k) {
                group.insert(k, v);
            }
        }
        group.insert("keys".into(), json!([key]));
        Value::Object(group)
    });
    Value::Array(groups.collect())
}

/// Go's sanitizer, then the save. An unchanged family keeps its groups but is still
/// saved, so a failing disk fails the request as Go's `persistLocked` does.
async fn persist_list(state: Arc<Management>, family: &str, old: &Value, new: Value) -> Response {
    let new = keys::sanitize(family, &new);
    let shape = view::field(family);
    let path = format!("api-keys/{}", v8_family(family));
    if yaml(shape, &new) == yaml(shape, old) {
        return touch(state, path).await;
    }
    match &new {
        Value::Array(list) => write(state, path, groups(family, list)).await,
        _ => remove(state, path).await,
    }
}

/// Go `rejectInvalidCredentialWeight` (`credentialweight.Normalize`).
fn check_weight(field: &str, weight: Option<&Value>) -> Result<(), Fail> {
    match weight.and_then(Value::as_i64) {
        Some(w) if w > 1_000_000 => Err(Fail::bad(format!("{field}: weight must not exceed 1000000"))),
        _ => Ok(()),
    }
}

/// Go `NormalizeClaudeFingerprintProfile`: the canonical value and whether it is known.
fn fingerprint_profile(raw: &str) -> (&'static str, bool) {
    match keys::lower(go_trim(raw)).as_str() {
        "claude-code-cli" | "oauth-cli" => ("claude-code-cli", true),
        "" => ("", true),
        _ => ("", false),
    }
}

/// Go `rejectInvalidFingerprintProfile`.
fn check_profile(field: &str, raw: &str) -> Result<(), Fail> {
    if fingerprint_profile(raw).1 {
        return Ok(());
    }
    Err(Fail::bad(format!(
        "{field}: unsupported fingerprint-profile {} (supported: \"claude-code-cli\" or empty)",
        cpa_common::gostr::quote(go_trim(raw))
    )))
}

fn trim_field(e: &mut Map<String, Value>, k: &str) {
    if let Some(Value::String(s)) = e.get_mut(k) {
        *s = go_trim(s).to_owned();
    }
}

fn normalize_field(e: &mut Map<String, Value>, k: &str, f: fn(&Value) -> Value) {
    if let Some(v) = e.get(k).map(f) {
        e.insert(k.into(), v);
    }
}

/// Model names and aliases trimmed; Vertex drops a model missing either, the other
/// families one missing both.
fn normalize_models(e: &mut Map<String, Value>, need_both: bool) {
    let Some(Value::Array(models)) = e.get_mut("models") else {
        return;
    };
    models.retain_mut(|m| {
        let Some(m) = m.as_object_mut() else { return true };
        trim_field(m, "name");
        trim_field(m, "alias");
        let (name, alias) = (text(m, "name"), text(m, "alias"));
        if need_both {
            !name.is_empty() && !alias.is_empty()
        } else {
            !name.is_empty() || !alias.is_empty()
        }
    });
}

/// Go `NormalizeCloakConfig`.
fn normalize_cloak(e: &mut Map<String, Value>) {
    let Some(Value::Object(cloak)) = e.get_mut("cloak") else {
        return;
    };
    let mode = trimmed(cloak, "mode");
    cloak.insert("mode".into(), mode.into());
    let words: Vec<Value> = cloak
        .get("sensitive-words")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|w| Some(go_trim(w.as_str()?)).filter(|w| !w.is_empty()).map(Value::from))
        .collect();
    if words.is_empty() {
        cloak.shift_remove("sensitive-words");
    } else {
        cloak.insert("sensitive-words".into(), Value::Array(words));
    }
}

/// Go's per-family entry normalizers (`normalizeClaudeKey`, `normalizeCodexKey`,
/// `normalizeVertexCompatKey`, `normalizeOpenAICompatibilityEntry`).
fn normalize(family: &str, e: &mut Map<String, Value>) {
    match family {
        "claude-api-key" => {
            trim_field(e, "api-key");
            let profile = text(e, "fingerprint-profile");
            let profile = match fingerprint_profile(&profile) {
                (canonical, true) => canonical.to_owned(),
                _ => go_trim(&profile).to_owned(),
            };
            e.insert("fingerprint-profile".into(), profile.into());
            trim_field(e, "base-url");
            trim_field(e, "proxy-url");
            normalize_field(e, "headers", keys::normalize_headers);
            normalize_field(e, "excluded-models", keys::normalize_excluded);
            normalize_cloak(e);
            normalize_models(e, false);
        }
        "openai-compatibility" => {
            trim_field(e, "base-url");
            normalize_field(e, "headers", keys::normalize_headers);
            if let Some(Value::Array(list)) = e.get_mut("api-key-entries") {
                for k in list.iter_mut().filter_map(Value::as_object_mut) {
                    let key = trimmed(k, "api-key");
                    k.insert("api-key".into(), key.into());
                }
            }
        }
        "vertex-api-key" | "codex-api-key" | "xai-api-key" | "meta-api-key" => {
            for k in ["api-key", "prefix", "base-url", "proxy-url"] {
                trim_field(e, k);
            }
            normalize_field(e, "headers", keys::normalize_headers);
            normalize_field(e, "excluded-models", keys::normalize_excluded);
            normalize_models(e, family == "vertex-api-key");
        }
        _ => {}
    }
}

/// Go `findExistingClaudeKey`: the one entry with the same trimmed identity.
fn existing_claude<'a>(list: &'a [Map<String, Value>], e: &Map<String, Value>) -> Option<&'a Map<String, Value>> {
    let id = |o: &Map<String, Value>| ["api-key", "base-url", "prefix", "proxy-url"].map(|k| trimmed(o, k));
    let mut hits = list.iter().filter(|o| id(o) == id(e));
    match (hits.next(), hits.next()) {
        (Some(one), None) => Some(one),
        _ => None,
    }
}

fn cloak_mode(o: &Map<String, Value>) -> Option<String> {
    let cloak = o.get("cloak")?.as_object()?;
    Some(text(cloak, "mode"))
}

/// PUT: the replacement list (before the sanitizer), or Go's 400.
fn put(family: &str, old: &Value, body: &[u8]) -> Result<Value, Fail> {
    let Some(list) = decode::put_collection(view::field(family), body, true) else {
        return Err(Fail::bad("invalid body"));
    };
    let nil = list.is_null();
    let mut list = entries(&list);
    let weight = |i: usize, e: &Map<String, Value>| check_weight(&format!("{family}[{i}].weight"), e.get("weight"));
    match family {
        "gemini-api-key" | "interactions-api-key" => {
            for (i, e) in list.iter().enumerate() {
                weight(i, e)?;
            }
            Ok(nil_if_empty(list))
        }
        "claude-api-key" => {
            let existing = entries(old);
            for (i, e) in list.iter_mut().enumerate() {
                let inherited = existing_claude(&existing, e)
                    .and_then(cloak_mode)
                    .filter(|m| !m.is_empty());
                match e.get_mut("cloak") {
                    Some(Value::Object(cloak)) => {
                        if trimmed(cloak, "mode").is_empty()
                            && let Some(mode) = inherited
                        {
                            cloak.insert("mode".into(), mode.into());
                        }
                    }
                    _ => {
                        if let Some(mode) = inherited {
                            e.insert("cloak".into(), json!({ "mode": mode }));
                        }
                    }
                }
                normalize(family, e);
                weight(i, e)?;
                check_profile(
                    &format!("claude-api-key[{i}].fingerprint-profile"),
                    &text(e, "fingerprint-profile"),
                )?;
            }
            Ok(if nil { Value::Null } else { array(list) })
        }
        "openai-compatibility" => {
            let mut kept = Vec::new();
            for (i, mut e) in list.into_iter().enumerate() {
                normalize(family, &mut e);
                if text(&e, "base-url").is_empty() {
                    continue;
                }
                let keys = e.get("api-key-entries").and_then(Value::as_array);
                for (k, key) in keys.into_iter().flatten().enumerate() {
                    check_weight(
                        &format!("openai-compatibility[{i}].api-key-entries[{k}].weight"),
                        key.get("weight"),
                    )?;
                }
                kept.push(e);
            }
            Ok(array(kept))
        }
        "vertex-api-key" => {
            for (i, e) in list.iter_mut().enumerate() {
                normalize(family, e);
                if text(e, "api-key").is_empty() {
                    return Err(Fail::bad(format!("vertex-api-key[{i}].api-key is required")));
                }
                weight(i, e)?;
            }
            Ok(nil_if_empty(list))
        }
        _ => {
            // codex, xai and meta share Go's CodexKey type and normalizer.
            let mut kept = Vec::new();
            for (i, mut e) in list.into_iter().enumerate() {
                normalize(family, &mut e);
                if text(&e, "base-url").is_empty() {
                    if family != "meta-api-key" {
                        continue;
                    }
                    e.insert("base-url".into(), META_BASE_URL.into());
                }
                weight(i, &e)?;
                kept.push(e);
            }
            Ok(array(kept))
        }
    }
}

/// Go's patch fields per family, in the order its handler applies them.
fn patch_fields(family: &str) -> &'static [&'static str] {
    match family {
        "gemini-api-key" | "interactions-api-key" => &[
            "api-key",
            "priority",
            "weight",
            "prefix",
            "base-url",
            "proxy-url",
            "headers",
            "excluded-models",
            "disable-cooling",
            "request-retry",
            "request-scoped-errors",
        ],
        "claude-api-key" => &[
            "api-key",
            "priority",
            "fingerprint-profile",
            "weight",
            "prefix",
            "base-url",
            "proxy-url",
            "models",
            "headers",
            "excluded-models",
            "rebuild-mid-system-message",
            "disable-cooling",
            "request-retry",
            "request-scoped-errors",
            "cloak",
        ],
        "openai-compatibility" => &[
            "name",
            "priority",
            "prefix",
            "disabled",
            "disable-cooling",
            "request-retry",
            "base-url",
            "api-key-entries",
            "models",
            "headers",
            "support-prompt-cache-key",
            "request-scoped-errors",
        ],
        "vertex-api-key" => &[
            "api-key",
            "priority",
            "weight",
            "prefix",
            "base-url",
            "proxy-url",
            "headers",
            "models",
            "excluded-models",
            "disable-cooling",
            "request-retry",
        ],
        "codex-api-key" => &[
            "api-key",
            "priority",
            "weight",
            "prefix",
            "base-url",
            "proxy-url",
            "alpha-search",
            "models",
            "headers",
            "excluded-models",
            "disable-cooling",
            "disable-codex-cloaking",
            "request-retry",
            "request-scoped-errors",
        ],
        "xai-api-key" => &[
            "api-key",
            "priority",
            "weight",
            "prefix",
            "base-url",
            "websockets",
            "proxy-url",
            "models",
            "headers",
            "excluded-models",
            "disable-cooling",
            "request-retry",
            "request-scoped-errors",
        ],
        _ => &[
            "api-key",
            "priority",
            "weight",
            "prefix",
            "base-url",
            "proxy-url",
            "models",
            "headers",
            "excluded-models",
            "disable-cooling",
            "request-retry",
            "request-scoped-errors",
        ],
    }
}

/// `json.RawMessage` patch fields; the rest are pointers to the entry's field type.
const RAW_FIELDS: [&str; 4] = ["weight", "disable-cooling", "disable-codex-cloaking", "cloak"];

/// Go's PATCH body: `{index, match | name, value: <family patch>}`.
fn patch_shape(family: &str) -> Shape {
    let elem = view::field(family).elem.as_deref().expect("list element");
    let fields = patch_fields(family)
        .iter()
        .map(|name| {
            if RAW_FIELDS.contains(name) {
                decode::field(name, "raw", false, None)
            } else {
                let f = elem
                    .fields
                    .iter()
                    .find(|f| f.json == *name)
                    .expect("patch field in Go's entry");
                decode::pointer_to(name, f)
            }
        })
        .collect();
    let lookup = if family == "openai-compatibility" {
        "name"
    } else {
        "match"
    };
    decode::record(vec![
        decode::field(lookup, "string", true, None),
        decode::field("index", "int", true, None),
        decode::pointer_to("value", &decode::record(fields)),
    ])
}

/// Go `parseCredentialWeightPatch`.
fn weight_patch(raw: &str) -> Result<Value, Fail> {
    if go_trim(raw) == "null" {
        return Ok(Value::Null);
    }
    match decode::whole(raw.as_bytes()).and_then(decode::int_token) {
        Some(w) if w > 1_000_000 => Err(Fail::bad("weight must not exceed 1000000")),
        Some(w) => Ok(w.into()),
        None => Err(Fail::bad("weight must be an integer")),
    }
}

/// Go `applyDisableCoolingPatch` / `applyDisableCodexCloakingPatch`.
fn bool_patch(name: &str, raw: &str) -> Result<Value, Fail> {
    if go_trim(raw) == "null" {
        return Ok(Value::Null);
    }
    match raw {
        "true" => Ok(Value::Bool(true)),
        "false" => Ok(Value::Bool(false)),
        _ => Err(Fail::bad(format!("{name} must be a boolean or null"))),
    }
}

/// Go's Claude `cloak` patch: null clears it; an object edits a copy (a fresh one when
/// the identity changed) where a blank mode keeps the old one.
fn cloak_patch(
    e: &mut Map<String, Value>,
    old: &Map<String, Value>,
    raw: &str,
    identity_changed: bool,
) -> Result<(), Fail> {
    if go_trim(raw) == "null" {
        e.insert("cloak".into(), Value::Null);
        return Ok(());
    }
    let cloak = view::nested("claude-api-key", "cloak").expect("cloak shape");
    let shape = decode::record(cloak.fields.iter().map(|f| decode::pointer_to(&f.json, f)).collect());
    let patch = decode::whole(raw.as_bytes()).and_then(|r| decode::decode(&shape, r, view::zero(&shape)));
    let Some(Value::Object(p)) = patch else {
        return Err(Fail::bad("invalid cloak config"));
    };
    let given = |k: &str| p.get(k).filter(|v| !v.is_null());
    let mut c = match e.get("cloak") {
        Some(Value::Object(c)) if !identity_changed => c.clone(),
        _ => Map::new(),
    };
    if let Some(mode) = given("mode") {
        let mode = go_trim(mode.as_str().unwrap_or_default()).to_owned();
        let mode = match cloak_mode(old) {
            Some(previous) if mode.is_empty() && !identity_changed => previous,
            _ => mode,
        };
        c.insert("mode".into(), mode.into());
    } else if identity_changed {
        c.insert("mode".into(), "".into());
    }
    for k in ["strict-mode", "sensitive-words", "cache-user-id"] {
        if let Some(v) = given(k) {
            c.insert(k.into(), v.clone());
        }
    }
    e.insert("cloak".into(), Value::Object(c));
    normalize_cloak(e);
    Ok(())
}

enum Patched {
    Remove,
    Entry(Map<String, Value>),
}

/// One entry patched as Go's `Patch*Key` does, or removed where Go removes it.
fn apply(family: &str, old: &Map<String, Value>, p: &Map<String, Value>) -> Result<Patched, Fail> {
    let given = |k: &str| p.get(k).filter(|v| !v.is_null());
    let trim_given = |k: &str| given(k).map(|v| go_trim(v.as_str().unwrap_or_default()).to_owned());
    let identity_changed = family == "claude-api-key"
        && ["api-key", "base-url", "prefix", "proxy-url"]
            .iter()
            .any(|k| trim_given(k).is_some_and(|v| v != text(old, k)));
    let mut e = old.clone();
    for &name in patch_fields(family) {
        let Some(v) = given(name) else { continue };
        let raw = v.as_str().unwrap_or_default();
        let value = match name {
            "api-key" | "name" | "prefix" | "proxy-url" | "base-url" => {
                let t = go_trim(raw).to_owned();
                let removes = match name {
                    "api-key" => family == "vertex-api-key",
                    "base-url" => matches!(family, "codex-api-key" | "xai-api-key" | "openai-compatibility"),
                    _ => false,
                };
                if t.is_empty() && removes {
                    return Ok(Patched::Remove);
                }
                if t.is_empty() && name == "base-url" && family == "meta-api-key" {
                    META_BASE_URL.into()
                } else {
                    t.into()
                }
            }
            "weight" => weight_patch(raw)?,
            "disable-cooling" | "disable-codex-cloaking" => bool_patch(name, raw)?,
            "headers" => keys::normalize_headers(v),
            "excluded-models" => keys::normalize_excluded(v),
            "models" | "request-scoped-errors" => copied(v),
            "api-key-entries" => {
                for (k, key) in v.as_array().into_iter().flatten().enumerate() {
                    check_weight(&format!("api-key-entries[{k}].weight"), key.get("weight"))?;
                }
                copied(v)
            }
            "fingerprint-profile" => {
                check_profile("fingerprint-profile", raw)?;
                fingerprint_profile(raw).0.into()
            }
            "cloak" => {
                cloak_patch(&mut e, old, raw, identity_changed)?;
                continue;
            }
            _ => v.clone(),
        };
        e.insert(name.into(), value);
    }
    if family == "claude-api-key" && given("cloak").is_none() && identity_changed {
        if let Some(Value::Object(c)) = e.get_mut("cloak") {
            c.insert("mode".into(), "".into());
        }
        normalize_cloak(&mut e);
    }
    if matches!(family, "gemini-api-key" | "interactions-api-key") {
        if text(&e, "api-key").is_empty() && text(&e, "base-url").is_empty() {
            return Ok(Patched::Remove);
        }
        return Ok(Patched::Entry(e));
    }
    normalize(family, &mut e);
    Ok(Patched::Entry(e))
}

/// Go's PATCH target when `index` is absent or out of range.
fn patch_target(
    family: &str,
    list: &[Map<String, Value>],
    body: &Map<String, Value>,
    query: Option<&str>,
) -> Result<Option<usize>, Fail> {
    let compat = family == "openai-compatibility";
    let lookup = if compat { "name" } else { "match" };
    let Some(wanted) = body.get(lookup).and_then(Value::as_str).map(|m| go_trim(m).to_owned()) else {
        return Ok(None);
    };
    match family {
        "gemini-api-key" | "interactions-api-key" => {
            if wanted.is_empty() {
                return Ok(None);
            }
            let base = query_first(query, "base-url").map(|b| go_trim(&b).to_owned());
            let hits: Vec<usize> = (0..list.len())
                .filter(|&i| trimmed(&list[i], "api-key") == wanted)
                .filter(|&i| base.as_ref().is_none_or(|b| trimmed(&list[i], "base-url") == *b))
                .collect();
            if hits.len() > 1 {
                return Err(Fail::bad("multiple items match; index is required"));
            }
            Ok(hits.first().copied())
        }
        "vertex-api-key" if wanted.is_empty() => Ok(None),
        _ => {
            let field = if compat { "name" } else { "api-key" };
            Ok(list.iter().position(|e| text(e, field) == wanted))
        }
    }
}

fn patch(family: &str, old: &Value, query: Option<&str>, body: &[u8]) -> Result<Value, Fail> {
    let Some(b) = decode::bind(&patch_shape(family), body) else {
        return Err(Fail::bad("invalid body"));
    };
    let Some(Value::Object(p)) = b.get("value") else {
        return Err(Fail::bad("invalid body"));
    };
    let mut list = entries(old);
    let index = b
        .get("index")
        .and_then(Value::as_i64)
        .filter(|&i| i >= 0 && (i as usize) < list.len())
        .map(|i| i as usize);
    let target = match index {
        Some(i) => Some(i),
        None => patch_target(family, &list, &b, query)?,
    };
    let Some(target) = target else {
        return Err(Fail::not_found("item not found"));
    };
    match apply(family, &list[target], p)? {
        Patched::Remove => {
            list.remove(target);
        }
        Patched::Entry(e) => list[target] = e,
    }
    Ok(array(list))
}

fn delete(family: &str, old: &Value, query: Option<&str>) -> Result<Value, Fail> {
    let mut list = entries(old);
    let missing = if family == "openai-compatibility" {
        if let Some(name) = query_first(query, "name").filter(|n| !n.is_empty()) {
            list.retain(|e| text(e, "name") != name);
            return Ok(array(list));
        }
        "missing name or index"
    } else {
        let key = query_first(query, "api-key").map(|k| go_trim(&k).to_owned());
        if let Some(key) = key.filter(|k| !k.is_empty()) {
            let strict = matches!(family, "gemini-api-key" | "interactions-api-key");
            let mut hits: Vec<usize> = (0..list.len())
                .filter(|&i| trimmed(&list[i], "api-key") == key)
                .collect();
            if let Some(base) = query_first(query, "base-url").map(|b| go_trim(&b).to_owned()) {
                hits.retain(|&i| trimmed(&list[i], "base-url") == base);
                if strict && hits.is_empty() {
                    return Err(Fail::not_found("item not found"));
                }
                if strict && hits.len() > 1 {
                    return Err(Fail::bad("multiple items match; index is required"));
                }
                // Go rebuilds the list (`make([]T, 0)`), so a nil list becomes empty.
                for &i in hits.iter().rev() {
                    list.remove(i);
                }
                return Ok(array(list));
            }
            if strict && hits.is_empty() {
                return Err(Fail::not_found("item not found"));
            }
            if hits.len() > 1 {
                return Err(Fail::bad("multiple items match api-key; base-url is required"));
            }
            return match hits.first() {
                Some(&i) => {
                    list.remove(i);
                    Ok(array(list))
                }
                None => Ok(old.clone()),
            };
        }
        "missing api-key or index"
    };
    if let Some(i) = query_first(query, "index")
        .filter(|s| !s.is_empty())
        .and_then(|s| sscanf_int(&s))
        && i >= 0
        && (i as usize) < list.len()
    {
        list.remove(i as usize);
        return Ok(array(list));
    }
    Err(Fail::bad(missing))
}

/// v8 path of an OAuth map route.
fn map_path(route: &str) -> &'static str {
    match route {
        "oauth-excluded-models" => "oauth/excluded-models",
        "oauth-model-alias" => "oauth/model-alias",
        _ => "oauth/request-scoped-errors",
    }
}

/// Go's sanitizer for one OAuth map; nil when nothing is left.
fn sanitize_map(route: &str, v: &Value) -> Value {
    let out = match route {
        "oauth-excluded-models" => keys::oauth_excluded(v),
        "oauth-model-alias" => keys::oauth_alias(v),
        _ => keys::oauth_scoped_errors(v),
    };
    if out.as_object().is_some_and(Map::is_empty) {
        Value::Null
    } else {
        out
    }
}

/// The OAuth map after a PUT, PATCH or DELETE, or Go's error.
fn map_change(route: &str, old: &Value, method: &Method, query: Option<&str>, body: &[u8]) -> Result<Value, Fail> {
    let shape = view::field(route);
    let excluded = route == "oauth-excluded-models";
    let (what, missing_entry) = if excluded {
        ("provider", "provider not found")
    } else {
        ("channel", "channel not found")
    };
    let key = match *method {
        Method::PUT => {
            return match decode::put_collection(shape, body, false) {
                Some(entries) => Ok(sanitize_map(route, &entries)),
                None => Err(Fail::bad("invalid body")),
            };
        }
        Method::PATCH => {
            let list = match route {
                "oauth-excluded-models" => "models",
                "oauth-model-alias" => "aliases",
                _ => "rules",
            };
            let mut fields = vec![decode::field("provider", "string", true, None)];
            if !excluded {
                fields.push(decode::field("channel", "string", true, None));
            }
            fields.push(Shape {
                json: list.into(),
                ..shape.elem.as_deref().expect("map element").clone()
            });
            let Some(b) = decode::bind(&decode::record(fields), body) else {
                return Err(Fail::bad("invalid body"));
            };
            let raw = match (
                b.get("channel").and_then(Value::as_str),
                b.get("provider").and_then(Value::as_str),
            ) {
                (Some(c), _) => c,
                (None, Some(p)) => p,
                (None, None) if excluded => return Err(Fail::bad("invalid body")),
                (None, None) => "",
            };
            let key = keys::lower(go_trim(raw));
            if key.is_empty() {
                return Err(Fail::bad(format!("invalid {what}")));
            }
            let normalized = if excluded {
                keys::normalize_excluded(&b[list])
            } else {
                let one = sanitize_map(route, &json!({ key.clone(): b[list] }));
                one.get(&key).cloned().unwrap_or_default()
            };
            if !normalized.is_null() {
                let mut map = old.as_object().cloned().unwrap_or_default();
                map.insert(key, normalized);
                return Ok(Value::Object(map));
            }
            key
        }
        _ => {
            let query_key = |name: &str| keys::lower(go_trim(&query_first(query, name).unwrap_or_default()));
            let mut key = query_key(what);
            if key.is_empty() && !excluded {
                key = query_key("provider");
            }
            if key.is_empty() {
                return Err(Fail::bad(format!("missing {what}")));
            }
            key
        }
    };
    // A PATCH that leaves nothing, or a DELETE: the entry must exist.
    let mut map = old.as_object().cloned().unwrap_or_default();
    if map.shift_remove(&key).is_none() {
        return Err(Fail::not_found(missing_entry));
    }
    Ok(if map.is_empty() {
        Value::Null
    } else {
        Value::Object(map)
    })
}

/// PUT, PATCH and DELETE of a v0 key list or OAuth map route.
pub(super) async fn change(
    state: Arc<Management>,
    route: &str,
    method: Method,
    query: Option<&str>,
    body: &[u8],
) -> Response {
    let _serial = super::WRITES.lock().await;
    let old = super::current(&state).get(route).cloned().unwrap_or_default();
    if route.starts_with("oauth-") {
        let new = match map_change(route, &old, &method, query, body) {
            Ok(v) => v,
            Err(fail) => return fail.response(),
        };
        let shape = view::field(route);
        if yaml(shape, &new) == yaml(shape, &old) {
            return touch(state, map_path(route).into()).await;
        }
        return match new {
            Value::Object(_) => write(state, map_path(route).into(), yaml(shape, &new)).await,
            _ => remove(state, map_path(route).into()).await,
        };
    }
    let new = match method {
        Method::PUT => put(route, &old, body),
        Method::PATCH => patch(route, &old, query, body),
        _ => delete(route, &old, query),
    };
    match new {
        Ok(new) => persist_list(state, route, &old, new).await,
        Err(fail) => fail.response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_follow_go_group_legacy_keys() {
        let list = vec![
            json!({"api-key": "k", "priority": 2, "base-url": "https://b", "headers": {"A": "1"}, "weight": 0}),
            json!({"api-key": "j", "prefix": ""}),
        ];
        assert_eq!(
            groups("gemini-api-key", &list),
            json!([
                {"name": "gemini-1", "priority": 2, "base-url": "https://b", "headers": {"A": "1"},
                 "keys": [{"api-key": "k", "weight": 0}]},
                {"name": "gemini-2", "keys": [{"api-key": "j"}]},
            ])
        );
        let compat = vec![json!({"name": "c", "base-url": "http://x", "api-key-entries": [{"api-key": "a"}]})];
        assert_eq!(
            groups("openai-compatibility", &compat),
            json!([{"name": "c", "base-url": "http://x", "keys": [{"api-key": "a"}]}])
        );
    }

    #[test]
    fn weight_and_bool_patches_follow_go() {
        assert_eq!(weight_patch("null").ok(), Some(Value::Null));
        assert_eq!(weight_patch("-4").ok(), Some(json!(-4)));
        for bad in ["1.0", "\"2\"", "true", "1000001"] {
            assert!(weight_patch(bad).is_err(), "{bad}");
        }
        assert_eq!(bool_patch("disable-cooling", "false").ok(), Some(json!(false)));
        assert!(bool_patch("disable-cooling", "0").is_err());
    }
}
