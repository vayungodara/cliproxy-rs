//! Field shapes generated from Go structs at 6fecc6e. Runtime defaults and provider
//! sanitizers are separate; this does not claim to replace those normalizers.
use anyhow::{Context, bail};
use serde_json::Value as Schema;
use serde_yaml_ng::Value;
use std::sync::LazyLock;

static SCHEMA: LazyLock<Schema> =
    LazyLock::new(|| serde_json::from_str(include_str!("schema.json")).expect("generated schema"));

pub fn validate(value: &Value, reject_unknown: bool) -> anyhow::Result<()> {
    walk(value, &SCHEMA, "config", reject_unknown)
}

pub(super) fn is_struct(path: &str) -> bool {
    let mut schema = &*SCHEMA;
    for part in path.split('.') {
        let Some(next) = schema.get("fields").and_then(|f| f.get(part)) else {
            return false;
        };
        schema = next;
    }
    schema.get("fields").is_some()
}

fn walk(value: &Value, schema: &Schema, path: &str, strict: bool) -> anyhow::Result<()> {
    if value.is_null() {
        if schema.get("fields").is_some() && path != "config.routing" {
            bail!("{path} must be a mapping");
        }
        if schema.get("list").is_some() && path.starts_with("config.api-keys.") && !path.contains('[') {
            bail!("{path} must be a list");
        }
        return Ok(());
    }
    if let Some(kind) = schema.as_str() {
        let valid = match kind {
            "opaque" => true,
            // yaml.v3 decodes numeric/bool scalar spellings into Go string fields.
            "string" => value.is_string() || value.is_number() || value.is_bool(),
            "bool" => value.is_bool(),
            "int" => value.as_i64().is_some(),
            "port" => value.as_i64().is_some(), // Config bounds listener ports separately.
            "version" => value.as_i64() == Some(8),
            "duration" => value.is_string(),
            "image-mode" => {
                value.is_bool()
                    || value
                        .as_str()
                        .is_some_and(|s| ["chat", "passthrough", "true", "false"].contains(&s))
            }
            _ => false,
        };
        if !valid {
            bail!("{path} has an invalid type");
        }
        if path.ends_with(".weight") && value.as_i64().is_some_and(|v| v > 1_000_000) {
            bail!("credential weight must not exceed 1000000");
        }
        return Ok(());
    }
    if let Some(inner) = schema.get("optional") {
        return walk(value, inner, path, strict);
    }
    if let Some(inner) = schema.get("list") {
        for (i, item) in value
            .as_sequence()
            .with_context(|| format!("{path} must be a list"))?
            .iter()
            .enumerate()
        {
            walk(item, inner, &format!("{path}[{i}]"), strict)?;
        }
        return Ok(());
    }
    let map = value
        .as_mapping()
        .with_context(|| format!("{path} must be a mapping"))?;
    let group = path.starts_with("config.api-keys.")
        && path.ends_with(']')
        && schema.get("fields").and_then(|s| s.get("keys")).is_some();
    if group && map.get("keys").and_then(Value::as_sequence).is_none() {
        bail!("{path}.keys must be a list");
    }
    let native_group = group && !path.starts_with("config.api-keys.openai-compatibility[");
    let native_key = path.starts_with("config.api-keys.")
        && !path.starts_with("config.api-keys.openai-compatibility[")
        && path.contains("].keys[")
        && path.matches('[').count() == 2
        && path.ends_with(']');
    for (key, value) in map {
        let key = key.as_str().context("config keys must be strings")?;
        if native_key && key == "base-url" {
            bail!("{path}: base-url belongs to the group");
        }
        if let Some(inner) = schema.get("map") {
            walk(value, inner, &format!("{path}.{key}"), strict)?;
        } else if let Some(inner) = schema.get("fields").and_then(|f| f.get(key)) {
            walk(value, inner, &format!("{path}.{key}"), strict)?;
        } else if strict || native_group {
            bail!("unknown setting {path}.{key}");
        }
    }
    Ok(())
}

/// Go's `commentUnknownV8Sections`: removes keys that are not fields of the struct at
/// their position and returns them as `(dotted path, value)`. Only struct mappings
/// are walked; lists, user maps and `api-keys` keep their content (strict validation
/// rejects unknown fields there instead).
pub(super) fn archive_unknown(value: &mut Value) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    walk_unknown(value, &SCHEMA, "", &mut out);
    out
}

fn walk_unknown(value: &mut Value, schema: &Schema, path: &str, out: &mut Vec<(String, Value)>) {
    let (Some(map), Some(fields)) = (value.as_mapping_mut(), schema.get("fields")) else {
        return;
    };
    let unknown: Vec<Value> = map
        .keys()
        .filter(|k| k.as_str().is_none_or(|k| fields.get(k).is_none()))
        .cloned()
        .collect();
    for key in unknown {
        let name = key.as_str().map(str::to_owned).unwrap_or_else(|| format!("{key:?}"));
        let section = if path.is_empty() {
            name
        } else {
            format!("{path}.{name}")
        };
        if let Some(v) = map.remove(&key) {
            out.push((section, v));
        }
    }
    for (key, child) in map.iter_mut() {
        let Some(name) = key.as_str() else { continue };
        if path.is_empty() && name == "api-keys" {
            continue;
        }
        let inner = &fields[name];
        let inner = inner.get("optional").unwrap_or(inner);
        let child_path = if path.is_empty() {
            name.to_owned()
        } else {
            format!("{path}.{name}")
        };
        walk_unknown(child, inner, &child_path, out);
    }
}

/// Go's saver writes the typed value of the written keys: a null on a non-pointer
/// scalar keeps the value Go pre-set before decoding (see [`null_default`]) and a
/// number or bool in a string field becomes a string. `path` is one path the request
/// wrote; the rest of the document stays as written.
pub(super) fn typed_projection(value: &mut Value, path: &[&str]) {
    if path.first().is_none_or(|p| *p == "oauth")
        && let Some(oauth) = value.as_mapping_mut().and_then(|m| m.get_mut("oauth"))
    {
        super::sanitize::oauth_maps(oauth, path.get(1).copied(), path.get(2).copied());
    }
    // api-keys groups keep null key overrides: they mean "inherit the group value".
    if path.first() == Some(&"api-keys") {
        return;
    }
    let mut prefix: Vec<String> = Vec::new();
    if path.is_empty() {
        if let Some(map) = value.as_mapping_mut() {
            for (key, child) in map.iter_mut() {
                if let Some((key, field)) = key
                    .as_str()
                    .filter(|k| *k != "api-keys")
                    .and_then(|k| Some((k, SCHEMA["fields"].get(k)?)))
                {
                    prefix.push(key.to_owned());
                    project(child, field, &mut prefix);
                    prefix.pop();
                }
            }
        }
        return;
    }
    let mut schema = &*SCHEMA;
    let mut node = value;
    for part in path {
        let Some(next) = schema.get("fields").and_then(|f| f.get(*part)) else {
            return;
        };
        schema = next;
        let Some(child) = node.as_mapping_mut().and_then(|m| m.get_mut(*part)) else {
            return;
        };
        node = child;
        prefix.push((*part).to_owned());
    }
    project(node, schema, &mut prefix);
}

fn project(value: &mut Value, schema: &Schema, path: &mut Vec<String>) {
    if let Some(kind) = schema.as_str() {
        if value.is_null() {
            if let Some(v) = null_default(&path.join(".")).or_else(|| zero_of(kind)) {
                *value = v;
            }
        } else if kind == "string" && (value.is_number() || value.is_bool()) {
            *value = Value::from(super::go_string(value));
        }
        return;
    }
    match value {
        Value::Mapping(map) => {
            let Some(fields) = schema.get("fields") else { return };
            for (key, child) in map.iter_mut() {
                if let Some((key, field)) = key.as_str().and_then(|k| Some((k, fields.get(k)?))) {
                    path.push(key.to_owned());
                    project(child, field, path);
                    path.pop();
                }
            }
        }
        Value::Sequence(items) => {
            if let Some(inner) = schema.get("list") {
                // List items have no Go defaults; their paths never match a default.
                path.push("[]".to_owned());
                for item in items {
                    project(item, inner, path);
                }
                path.pop();
            }
        }
        _ => {}
    }
}

/// The value yaml.v3 leaves in a non-pointer field for an explicit null: what
/// `ParseConfigBytes` set before decoding (internal/config/parse.go and
/// `DefaultCredentialInFlightConfig`), after its normalizers. Verified against Go's
/// handler by the `null_writes_take_go_defaults` replay.
fn null_default(path: &str) -> Option<Value> {
    Some(match path {
        "oauth.providers.aistudio.ws-auth" => Value::Bool(true),
        "observability.logs.error-logs-max-files" => Value::from(10),
        "observability.usage.redis-usage-queue-retention-seconds" => Value::from(60),
        "observability.pprof.addr" => Value::from("127.0.0.1:8316"),
        "management.panel-github-repository" => {
            Value::from("https://github.com/router-for-me/Cli-Proxy-API-Management-Center")
        }
        "server.discovery.service-type" => Value::from("_ai-gateway._tcp"),
        "credentials.in-flight.snapshot-interval" => Value::from("2s"),
        "credentials.in-flight.stale-after" => Value::from("10s"),
        "credentials.in-flight.staging-retention" => Value::from("1m"),
        "credentials.in-flight.max-part-bytes" => Value::from(262_144),
        "credentials.in-flight.max-part-count" => Value::from(64),
        "credentials.in-flight.max-revision-bytes" => Value::from(16_777_216),
        "credentials.in-flight.max-aggregate-groups" => Value::from(100_000),
        "credentials.in-flight.max-details" => Value::from(10_000),
        "credentials.in-flight.max-string-bytes" => Value::from(256),
        _ => return None,
    })
}

fn zero_of(kind: &str) -> Option<Value> {
    match kind {
        "bool" | "image-mode" => Some(Value::Bool(false)),
        "int" | "port" => Some(Value::from(0)),
        "string" => Some(Value::from("")),
        // Go marshals a zero time.Duration as "0s".
        "duration" => Some(Value::from("0s")),
        _ => None,
    }
}
