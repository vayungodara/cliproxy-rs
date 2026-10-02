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
