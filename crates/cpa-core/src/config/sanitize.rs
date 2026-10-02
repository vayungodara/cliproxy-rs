//! Go's loader sanitizers for the OAuth maps, applied to the persisted document the
//! way Go's saver writes them back (internal/config/config_normalization.go,
//! config_yaml.go pruneMappingToGeneratedKeys). An emptied map stays as `{}`.
use std::collections::HashSet;

use serde_yaml_ng::{Mapping, Value};

fn s(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.trim().to_owned(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        _ => String::new(),
    }
}

fn list(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_sequence)
        .map(|l| l.iter().map(|x| s(Some(x))).filter(|x| !x.is_empty()).collect())
        .unwrap_or_default()
}

fn channels(map: &Value, mut each: impl FnMut(&Value) -> Option<Value>) -> Value {
    let mut out = Mapping::new();
    for (channel, entries) in map.as_mapping().into_iter().flatten() {
        let channel = s(Some(channel)).to_lowercase();
        if channel.is_empty() {
            continue;
        }
        if let Some(clean) = each(entries) {
            out.insert(Value::from(channel), clean);
        }
    }
    Value::Mapping(out)
}

/// `NormalizeOAuthExcludedModels`.
fn excluded_models(map: &Value) -> Value {
    channels(map, |models| {
        let mut seen = HashSet::new();
        let clean: Vec<Value> = list(Some(models))
            .into_iter()
            .map(|m| m.to_lowercase())
            .filter(|m| seen.insert(m.clone()))
            .map(Value::from)
            .collect();
        (!clean.is_empty()).then_some(Value::Sequence(clean))
    })
}

/// `SanitizeOAuthModelAlias`.
fn model_alias(map: &Value) -> Value {
    channels(map, |aliases| {
        let mut seen = HashSet::new();
        let clean: Vec<Value> = aliases
            .as_sequence()
            .into_iter()
            .flatten()
            .filter_map(|e| {
                let (name, alias) = (s(e.get("name")), s(e.get("alias")));
                if name.is_empty() || alias.is_empty() || name.eq_ignore_ascii_case(&alias) {
                    return None;
                }
                if !seen.insert(alias.to_lowercase()) {
                    return None;
                }
                let mut out = Mapping::new();
                out.insert("name".into(), name.into());
                out.insert("alias".into(), alias.into());
                if e.get("fork").and_then(Value::as_bool) == Some(true) {
                    out.insert("fork".into(), true.into());
                }
                let display = s(e.get("display-name"));
                if !display.is_empty() {
                    out.insert("display-name".into(), display.into());
                }
                if e.get("force-mapping").and_then(Value::as_bool) == Some(true) {
                    out.insert("force-mapping".into(), true.into());
                }
                Some(Value::Mapping(out))
            })
            .collect();
        (!clean.is_empty()).then_some(Value::Sequence(clean))
    })
}

/// `SanitizeOAuthSettings`: the last entry for each name/alias pair wins, order kept.
fn settings(map: &Value) -> Value {
    channels(map, |entries| {
        let mut seen = HashSet::new();
        let mut reversed: Vec<Value> = Vec::new();
        for e in entries.as_sequence().into_iter().flatten().rev() {
            let (name, alias) = (s(e.get("name")), s(e.get("alias")));
            if name.is_empty() || !seen.insert(format!("{}->{}", name.to_lowercase(), alias.to_lowercase())) {
                continue;
            }
            let mut out = Mapping::new();
            out.insert("name".into(), name.into());
            if !alias.is_empty() {
                out.insert("alias".into(), alias.into());
            }
            if let Some(n) = e.get("max-context-length").and_then(Value::as_i64).filter(|n| *n != 0) {
                out.insert("max-context-length".into(), n.into());
            }
            reversed.push(Value::Mapping(out));
        }
        reversed.reverse();
        (!reversed.is_empty()).then_some(Value::Sequence(reversed))
    })
}

/// `SanitizeOAuthRequestScopedErrors`.
fn request_scoped_errors(map: &Value) -> Value {
    channels(map, |rules| {
        let clean: Vec<Value> = rules
            .as_sequence()
            .into_iter()
            .flatten()
            .filter_map(|r| {
                let status = r.get("status").and_then(Value::as_i64).unwrap_or(0);
                let action = s(r.get("action")).to_lowercase();
                let (m, re) = (list(r.get("match")), list(r.get("match-regexr")));
                if status <= 0 || (m.is_empty() && re.is_empty()) || action.is_empty() {
                    return None;
                }
                let mut out = Mapping::new();
                out.insert("status".into(), status.into());
                if !m.is_empty() {
                    out.insert("match".into(), m.into());
                }
                if !re.is_empty() {
                    out.insert("match-regexr".into(), re.into());
                }
                out.insert("action".into(), action.into());
                Some(Value::Mapping(out))
            })
            .collect();
        (!clean.is_empty()).then_some(Value::Sequence(clean))
    })
}

/// Rewrites the OAuth maps under `oauth` as Go persists them, limited to what a
/// write touched: one map (`only_map`) or one channel of it (`only_channel`).
pub(super) fn oauth_maps(oauth: &mut Value, only_map: Option<&str>, only_channel: Option<&str>) {
    let Some(map) = oauth.as_mapping_mut() else { return };
    for (key, f) in [
        ("excluded-models", excluded_models as fn(&Value) -> Value),
        ("model-alias", model_alias),
        ("settings", settings),
        ("request-scoped-errors", request_scoped_errors),
    ] {
        if only_map.is_some_and(|m| m != key) {
            continue;
        }
        let Some(v) = map.get_mut(key).and_then(Value::as_mapping_mut) else {
            continue;
        };
        let Some(channel) = only_channel else {
            *map.get_mut(key).expect("present") = f(&Value::Mapping(v.clone()));
            continue;
        };
        let Some(entries) = v.get(channel).cloned() else {
            continue;
        };
        let mut single = Mapping::new();
        single.insert(channel.into(), entries);
        let clean = f(&Value::Mapping(single));
        match clean.as_mapping().and_then(|m| m.iter().next()) {
            // Same key: replace in place so the channel keeps its position.
            Some((k, cleaned)) if k.as_str() == Some(channel) => {
                *v.get_mut(channel).expect("present") = cleaned.clone();
            }
            Some((k, cleaned)) => {
                v.remove(channel);
                v.insert(k.clone(), cleaned.clone());
            }
            None => {
                v.remove(channel);
            }
        }
    }
}
