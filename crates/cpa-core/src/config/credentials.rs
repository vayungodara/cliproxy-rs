//! Runtime credentials derived from `config.yaml` API keys and `auth-dir` files, the
//! way Go's watcher synthesizers build them (internal/watcher/synthesizer/config.go,
//! file.go) after the loader's sanitizers (internal/config/config_normalization.go).
//!
//! Go's `Auth` fields without a slot on [`Credential`] live in `attributes`:
//! `prefix` and `proxy_url`. The Rust scheduler also reads two metadata keys that
//! Go keeps elsewhere, so config-backed credentials carry them in metadata as well:
//! `excluded_models` (Go: comma-joined `attributes.excluded_models`) and
//! `model_aliases` (Go: the model registry). Config-backed metadata is never persisted.
//!
//! ponytail: the `models_hash`/`excluded_models_hash` change-detection attributes,
//! plugin auth parsers and Kimi domain resolution are not reproduced; nothing in the
//! management API exposes them. Port them with the registry/Kimi executors.
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value as Json};
use serde_yaml_ng::Value;
use sha2::{Digest, Sha256};

use super::Config;
use crate::credential::{Credential, Source};

const WEIGHT_MAX: i64 = 1_000_000;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Go `StableIDGenerator`: `kind:sha256(kind, \0part...)[:12]`, `-N` on collisions.
#[derive(Default)]
struct Ids(HashMap<String, usize>);

impl Ids {
    fn next(&mut self, kind: &str, parts: &[&str]) -> (String, String) {
        let mut hasher = Sha256::new();
        hasher.update(kind.as_bytes());
        for part in parts {
            hasher.update([0]);
            hasher.update(part.trim().as_bytes());
        }
        let mut short = hex(&hasher.finalize())[..12].to_owned();
        let index = self.0.entry(format!("{kind}:{short}")).or_insert(0);
        if *index > 0 {
            short = format!("{short}-{index}");
        }
        *index += 1;
        (format!("{kind}:{short}"), short)
    }
}

/// `strings.TrimSpace` decoding of a YAML scalar into a Go string field.
fn text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        _ => String::new(),
    }
}

fn int(v: Option<&Value>) -> Option<i64> {
    v.and_then(Value::as_i64)
}

fn boolean(v: Option<&Value>) -> Option<bool> {
    v.and_then(Value::as_bool)
}

fn strings(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_sequence)
        .map(|s| s.iter().map(|v| text(Some(v))).collect())
        .unwrap_or_default()
}

/// Go `NormalizeHeaders` (trimmed, non-empty), kept sorted like `FormatSortedHeaders`.
fn headers(v: Option<&Value>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    if let Some(map) = v.and_then(Value::as_mapping) {
        for (k, v) in map {
            let (k, v) = (text(Some(k)).trim().to_owned(), text(Some(v)).trim().to_owned());
            if !k.is_empty() && !v.is_empty() {
                out.insert(k, v);
            }
        }
    }
    out
}

fn sorted_headers(h: &BTreeMap<String, String>) -> String {
    h.iter().map(|(k, v)| format!("{k}\0{v}\0")).collect()
}

/// Go `normalizeModelPrefix`.
pub fn normalize_prefix(prefix: &str) -> String {
    let p = prefix.trim().trim_matches('/');
    if p.contains('/') { String::new() } else { p.to_owned() }
}

/// Go `NormalizeExcludedModels`: lowercase, trimmed, de-duplicated, ordered.
pub fn normalize_excluded(models: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    models
        .iter()
        .map(|m| m.trim().to_lowercase())
        .filter(|m| !m.is_empty() && seen.insert(m.clone()))
        .collect()
}

/// One flattened API key after Go's `expandV8Groups` and the family sanitizer.
#[derive(Default, Clone)]
struct Key {
    index: usize,
    /// `(group, key)` position in `api-keys.<family>` of the v8 document.
    origin: (usize, usize),
    api_key: String,
    base_url: String,
    priority: i64,
    weight: Option<i64>,
    prefix: String,
    proxy_url: String,
    headers: BTreeMap<String, String>,
    excluded: Vec<String>,
    disable_cooling: Option<bool>,
    request_retry: Option<i64>,
    rules: Vec<Value>,
    models: Vec<(String, String)>,
    /// The `models` entries as configured (every field), for the registry.
    raw_models: Vec<Json>,
    websockets: bool,
    alpha_search: bool,
    disable_codex_cloaking: Option<bool>,
    rebuild_mid_system_message: bool,
    fingerprint_profile: String,
}

const SHARED: &[&str] = &[
    "priority",
    "prefix",
    "proxy-url",
    "headers",
    "models",
    "excluded-models",
    "disable-cooling",
    "request-retry",
    "request-scoped-errors",
];

fn groups<'a>(cfg: &'a Config, family: &str) -> &'a [Value] {
    cfg.document
        .get("api-keys")
        .and_then(|k| k.get(family))
        .and_then(Value::as_sequence)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

/// `expandV8Groups`: a key inherits the group's base-url and shared fields; a key's
/// own non-null value wins.
fn expand(cfg: &Config, family: &str) -> Vec<((usize, usize), serde_yaml_ng::Mapping)> {
    let mut out = Vec::new();
    for (g, group) in groups(cfg, family).iter().enumerate() {
        let Some(group) = group.as_mapping() else { continue };
        for (k, key) in group
            .get("keys")
            .and_then(Value::as_sequence)
            .into_iter()
            .flatten()
            .enumerate()
        {
            let mut item = serde_yaml_ng::Mapping::new();
            for (field, value) in group {
                let name = field.as_str().unwrap_or_default();
                if name == "base-url" || SHARED.contains(&name) {
                    item.insert(field.clone(), value.clone());
                }
            }
            for (field, value) in key.as_mapping().into_iter().flatten() {
                if !value.is_null() {
                    item.insert(field.clone(), value.clone());
                }
            }
            out.push(((g, k), item));
        }
    }
    out
}

fn decode(item: &serde_yaml_ng::Mapping) -> Key {
    let get = |k: &str| item.get(k);
    Key {
        index: 0,
        origin: (0, 0),
        api_key: text(get("api-key")),
        base_url: text(get("base-url")),
        priority: int(get("priority")).unwrap_or(0),
        weight: int(get("weight")),
        prefix: text(get("prefix")),
        proxy_url: text(get("proxy-url")),
        headers: headers(get("headers")),
        excluded: strings(get("excluded-models")),
        disable_cooling: boolean(get("disable-cooling")),
        request_retry: int(get("request-retry")),
        rules: get("request-scoped-errors")
            .and_then(Value::as_sequence)
            .cloned()
            .unwrap_or_default(),
        models: get("models")
            .and_then(Value::as_sequence)
            .into_iter()
            .flatten()
            .map(|m| (text(m.get("name")), text(m.get("alias"))))
            .collect(),
        raw_models: raw_models(get("models")),
        websockets: boolean(get("websockets")).unwrap_or(false),
        alpha_search: boolean(get("alpha-search")).unwrap_or(false),
        disable_codex_cloaking: boolean(get("disable-codex-cloaking")),
        rebuild_mid_system_message: boolean(get("rebuild-mid-system-message")).unwrap_or(false),
        fingerprint_profile: text(get("fingerprint-profile")),
    }
}

/// Family sanitizers from `LoadConfig`, returning keys with their runtime index.
fn sanitized(cfg: &Config, family: &str) -> Vec<Key> {
    let mut keys: Vec<Key> = expand(cfg, family)
        .iter()
        .map(|(origin, item)| Key {
            origin: *origin,
            ..decode(item)
        })
        .collect();
    let mut seen = HashSet::new();
    keys.retain_mut(|k| match family {
        "gemini" | "interactions" => {
            k.api_key = k.api_key.trim().to_owned();
            k.base_url = k.base_url.trim().to_owned();
            if k.api_key.is_empty() && k.base_url.is_empty() {
                return false;
            }
            k.prefix = normalize_prefix(&k.prefix);
            k.proxy_url = k.proxy_url.trim().to_owned();
            k.excluded = normalize_excluded(&k.excluded);
            let id = [
                k.api_key.as_str(),
                &k.base_url,
                &k.proxy_url,
                &k.prefix,
                &sorted_headers(&k.headers),
            ]
            .join("\0");
            seen.insert(id)
        }
        "vertex" => {
            k.api_key = k.api_key.trim().to_owned();
            if k.api_key.is_empty() {
                return false;
            }
            k.prefix = normalize_prefix(&k.prefix);
            k.base_url = k.base_url.trim().to_owned();
            k.proxy_url = k.proxy_url.trim().to_owned();
            k.excluded = normalize_excluded(&k.excluded);
            k.models = std::mem::take(&mut k.models)
                .into_iter()
                .map(|(n, a)| (n.trim().to_owned(), a.trim().to_owned()))
                .filter(|(n, a)| !n.is_empty() && !a.is_empty())
                .collect();
            // SanitizeVertexCompatKeys drops models without both a name and an alias.
            k.raw_models.retain(|m| {
                let t = |key: &str| m.get(key).and_then(Json::as_str).is_some_and(|v| !v.trim().is_empty());
                t("name") && t("alias")
            });
            seen.insert(format!("{}|{}", k.api_key, k.base_url))
        }
        "codex" | "xai" => {
            k.prefix = normalize_prefix(&k.prefix);
            k.base_url = k.base_url.trim().to_owned();
            k.excluded = normalize_excluded(&k.excluded);
            if family == "xai" {
                k.alpha_search = false;
            }
            !k.base_url.is_empty()
        }
        "meta" => {
            k.api_key = k.api_key.trim().to_owned();
            if k.api_key.is_empty() || k.api_key.starts_with("dca:") {
                return false;
            }
            k.prefix = normalize_prefix(&k.prefix);
            k.base_url = k.base_url.trim().to_owned();
            if k.base_url.is_empty() {
                k.base_url = "https://api.meta.ai/v1".into();
            }
            k.excluded = normalize_excluded(&k.excluded);
            k.alpha_search = false;
            true
        }
        _ => {
            // claude
            k.prefix = normalize_prefix(&k.prefix);
            k.excluded = normalize_excluded(&k.excluded);
            true
        }
    });
    for (i, k) in keys.iter_mut().enumerate() {
        k.index = i;
    }
    keys
}

/// Go `ApplyAuthExcludedModelsMeta`: lowercase de-duplicated sorted union.
fn excluded_union(lists: &[&[String]]) -> Vec<String> {
    let mut set: Vec<String> = lists
        .iter()
        .flat_map(|l| l.iter())
        .map(|m| m.trim().to_lowercase())
        .filter(|m| !m.is_empty())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    set.sort();
    set
}

fn rules_json(rules: &[Value]) -> Json {
    // Field order and omitempty follow Go's RequestScopedErrorRule JSON tags.
    Json::Array(
        rules
            .iter()
            .map(|r| {
                let mut out = Map::new();
                if let Some(status) = int(r.get("status")).filter(|s| *s != 0) {
                    out.insert("status".into(), status.into());
                }
                for key in ["match", "match-regexr"] {
                    let list = strings(r.get(key));
                    if !list.is_empty() {
                        out.insert(key.into(), list.into());
                    }
                }
                let action = text(r.get("action"));
                if !action.is_empty() {
                    out.insert("action".into(), action.into());
                }
                Json::Object(out)
            })
            .collect(),
    )
}

struct Draft {
    origin: (usize, usize),
    id: String,
    provider: String,
    label: String,
    section: &'static str,
    index: usize,
    prefix: String,
    proxy_url: String,
    attrs: BTreeMap<String, String>,
    meta: Map<String, Json>,
}

impl Draft {
    fn finish(mut self, excluded: Option<&[String]>, models: &[(String, String)], raw_models: &[Json]) -> Credential {
        if let Some(list) = excluded {
            let combined = excluded_union(&[list]);
            if !combined.is_empty() {
                self.attrs.insert("excluded_models".into(), combined.join(","));
                self.meta.insert("excluded_models".into(), combined.into());
            }
            self.attrs.insert("auth_kind".into(), "apikey".into());
        }
        let aliases: Vec<Json> = models
            .iter()
            .filter(|(n, a)| !n.trim().is_empty() && !a.trim().is_empty())
            .map(|(n, a)| serde_json::json!({"name": n.trim(), "alias": a.trim()}))
            .collect();
        if !aliases.is_empty() {
            self.meta.insert("model_aliases".into(), aliases.into());
        }
        // Go's registry reads the entry's whole `models` list from config by index
        // (name-only entries register under their name).
        if !raw_models.is_empty() {
            self.meta.insert("models".into(), raw_models.to_vec().into());
        }
        if !self.prefix.is_empty() {
            self.attrs.insert("prefix".into(), self.prefix);
        }
        if !self.proxy_url.is_empty() {
            self.attrs.insert("proxy_url".into(), self.proxy_url);
        }
        Credential {
            id: self.id,
            provider: self.provider,
            source: Source::Config {
                section: self.section.into(),
                index: self.index,
            },
            disabled: false,
            label: self.label,
            attributes: self.attrs,
            metadata: self.meta,
            revision: 0,
        }
    }
}

fn base_draft(ids: &mut Ids, kind: &str, source: &str, k: &Key, parts: &[&str]) -> Draft {
    let (id, token) = ids.next(kind, parts);
    let mut attrs = BTreeMap::new();
    attrs.insert("source".into(), format!("config:{source}[{token}]"));
    attrs.insert("config_index".into(), k.index.to_string());
    let mut meta = Map::new();
    if let Some(v) = k.disable_cooling {
        meta.insert("disable_cooling".into(), v.into());
    }
    if let Some(v) = k.request_retry.filter(|v| *v >= 0) {
        meta.insert("request_retry".into(), v.into());
    }
    if !k.rules.is_empty() {
        meta.insert("request_scoped_errors".into(), rules_json(&k.rules));
    }
    if k.priority != 0 {
        attrs.insert("priority".into(), k.priority.to_string());
    }
    if let Some(w) = k.weight {
        attrs.insert("weight".into(), w.max(0).to_string());
    }
    for (name, value) in &k.headers {
        attrs.insert(format!("header:{name}"), value.clone());
    }
    Draft {
        origin: k.origin,
        id,
        provider: String::new(),
        label: String::new(),
        section: "",
        index: k.index,
        prefix: k.prefix.trim().to_owned(),
        proxy_url: k.proxy_url.trim().to_owned(),
        attrs,
        meta,
    }
}

/// Go `util.OpenAICompatibleProviderKey`.
pub fn openai_compat_provider(name: &str) -> String {
    let name = name.trim().to_lowercase();
    if name.is_empty() {
        "openai-compatibility".into()
    } else if name == "openai-compatibility" || name.starts_with("openai-compatible-") {
        name
    } else {
        format!("openai-compatible-{name}")
    }
}

/// Config-backed API-key credentials in Go's synthesis order. Weight bounds are
/// already enforced by config validation.
pub fn from_config(cfg: &Config) -> Vec<Credential> {
    synthesize(cfg).into_iter().map(|(c, _)| c).collect()
}

/// Where a config-backed credential's key lives in the v8 document:
/// `api-keys.<family>[group].keys[key]`. `None` for OpenAI-compatibility entries
/// (Go cannot toggle them per key either) and unknown IDs.
pub fn config_key_location(cfg: &Config, id: &str) -> Option<KeyLocation> {
    synthesize(cfg)
        .into_iter()
        .find(|(c, _)| c.id == id)
        .and_then(|(_, origin)| origin)
}

/// Position of a config API key: `api-keys.<family>[group].keys[key]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyLocation {
    pub family: &'static str,
    pub group: usize,
    pub key: usize,
}

fn synthesize(cfg: &Config) -> Vec<(Credential, Option<KeyLocation>)> {
    let mut ids = Ids::default();
    let mut out: Vec<(Credential, Option<KeyLocation>)> = Vec::new();
    for (family, kind, source, label, provider, section) in [
        (
            "gemini",
            "gemini:apikey",
            "gemini",
            "gemini-apikey",
            "gemini",
            "gemini-api-key",
        ),
        (
            "interactions",
            "gemini-interactions:apikey",
            "interactions",
            "interactions-apikey",
            "gemini-interactions",
            "interactions-api-key",
        ),
    ] {
        for k in sanitized(cfg, family) {
            let headers = sorted_headers(&k.headers);
            let parts = [&*k.api_key, &k.base_url, &k.proxy_url, &k.prefix, &headers];
            let mut d = base_draft(&mut ids, kind, source, &k, &parts);
            if !k.api_key.trim().is_empty() {
                d.attrs.insert("api_key".into(), k.api_key.trim().into());
            }
            if !k.base_url.trim().is_empty() {
                d.attrs.insert("base_url".into(), k.base_url.trim().into());
            }
            (d.provider, d.label, d.section) = (provider.into(), label.into(), section);
            let origin = Some(KeyLocation {
                family,
                group: d.origin.0,
                key: d.origin.1,
            });
            out.push((d.finish(Some(&k.excluded), &k.models, &k.raw_models), origin));
        }
    }
    for k in sanitized(cfg, "claude") {
        let (key, base) = (k.api_key.trim(), k.base_url.trim());
        if key.is_empty() && base.is_empty() {
            continue;
        }
        let headers = sorted_headers(&k.headers);
        let mut d = base_draft(
            &mut ids,
            "claude:apikey",
            "claude",
            &k,
            &[key, base, &k.proxy_url, &k.prefix, &headers],
        );
        if !key.is_empty() {
            d.attrs.insert("api_key".into(), key.into());
        }
        if !base.is_empty() {
            d.attrs.insert("base_url".into(), base.into());
        }
        if k.rebuild_mid_system_message {
            d.attrs.insert("rebuild_mid_system_message".into(), "true".into());
        }
        let profile = k.fingerprint_profile.trim().to_lowercase();
        if !profile.is_empty() {
            d.attrs.insert("fingerprint_profile".into(), profile);
        }
        (d.provider, d.label, d.section) = ("claude".into(), "claude-apikey".into(), "claude-api-key");
        let origin = Some(KeyLocation {
            family: "claude",
            group: d.origin.0,
            key: d.origin.1,
        });
        out.push((d.finish(Some(&k.excluded), &k.models, &k.raw_models), origin));
    }
    for (family, section) in [
        ("codex", "codex-api-key"),
        ("xai", "xai-api-key"),
        ("meta", "meta-api-key"),
    ] {
        for k in sanitized(cfg, family) {
            let (key, base) = (k.api_key.trim(), k.base_url.trim());
            if key.is_empty() && base.is_empty() {
                continue;
            }
            let headers = sorted_headers(&k.headers);
            let mut d = base_draft(
                &mut ids,
                &format!("{family}:apikey"),
                family,
                &k,
                &[key, base, &k.proxy_url, &k.prefix, &headers],
            );
            if !key.is_empty() {
                d.attrs.insert("api_key".into(), key.into());
            }
            if !base.is_empty() {
                d.attrs.insert("base_url".into(), base.into());
            }
            if k.websockets {
                d.attrs.insert("websockets".into(), "true".into());
            }
            if family == "codex" && k.alpha_search {
                d.attrs.insert("codex_alpha_search".into(), "true".into());
            }
            if family == "codex"
                && let Some(v) = k.disable_codex_cloaking
            {
                d.attrs.insert("codex_disable_cloaking".into(), v.to_string());
            }
            (d.provider, d.label, d.section) = (family.into(), format!("{family}-apikey"), section);
            let origin = Some(KeyLocation {
                family,
                group: d.origin.0,
                key: d.origin.1,
            });
            out.push((d.finish(Some(&k.excluded), &k.models, &k.raw_models), origin));
        }
    }
    out.extend(openai_compat(cfg, &mut ids).into_iter().map(|c| (c, None)));
    for k in sanitized(cfg, "vertex") {
        let (key, base) = (k.api_key.trim(), k.base_url.trim());
        let (id, token) = ids.next("vertex:apikey", &[key, base, &k.proxy_url]);
        let mut attrs = BTreeMap::new();
        attrs.insert("source".into(), format!("config:vertex-apikey[{token}]"));
        attrs.insert("base_url".into(), base.into());
        attrs.insert("provider_key".into(), "vertex".into());
        attrs.insert("config_index".into(), k.index.to_string());
        if k.priority != 0 {
            attrs.insert("priority".into(), k.priority.to_string());
        }
        if let Some(w) = k.weight {
            attrs.insert("weight".into(), w.max(0).to_string());
        }
        if !key.is_empty() {
            attrs.insert("api_key".into(), key.into());
        }
        for (name, value) in &k.headers {
            attrs.insert(format!("header:{name}"), value.clone());
        }
        let mut meta = Map::new();
        if let Some(v) = k.disable_cooling {
            meta.insert("disable_cooling".into(), v.into());
        }
        if let Some(v) = k.request_retry.filter(|v| *v >= 0) {
            meta.insert("request_retry".into(), v.into());
        }
        let d = Draft {
            origin: k.origin,
            id,
            provider: "vertex".into(),
            label: "vertex-apikey".into(),
            section: "vertex-api-key",
            index: k.index,
            prefix: k.prefix.trim().into(),
            proxy_url: k.proxy_url.trim().into(),
            attrs,
            meta,
        };
        let origin = Some(KeyLocation {
            family: "vertex",
            group: d.origin.0,
            key: d.origin.1,
        });
        out.push((d.finish(Some(&k.excluded), &k.models, &k.raw_models), origin));
    }
    out
}

fn openai_compat(cfg: &Config, ids: &mut Ids) -> Vec<Credential> {
    let mut out = Vec::new();
    let mut index = 0;
    for group in groups(cfg, "openai-compatibility") {
        let Some(g) = group.as_mapping() else { continue };
        let base = text(g.get("base-url")).trim().to_owned();
        if base.is_empty() {
            continue; // SanitizeOpenAICompatibility
        }
        let i = index;
        index += 1;
        if boolean(g.get("disabled")).unwrap_or(false) {
            continue;
        }
        let name = text(g.get("name")).trim().to_owned();
        let provider_name = if name.is_empty() {
            "openai-compatibility".to_owned()
        } else {
            name.to_lowercase()
        };
        let provider = openai_compat_provider(&provider_name);
        let group_key = Key {
            index: i,
            priority: int(g.get("priority")).unwrap_or(0),
            prefix: normalize_prefix(&text(g.get("prefix"))),
            headers: headers(g.get("headers")),
            disable_cooling: boolean(g.get("disable-cooling")),
            request_retry: int(g.get("request-retry")),
            rules: g
                .get("request-scoped-errors")
                .and_then(Value::as_sequence)
                .cloned()
                .unwrap_or_default(),
            ..Key::default()
        };
        let models: Vec<(String, String)> = g
            .get("models")
            .and_then(Value::as_sequence)
            .into_iter()
            .flatten()
            .map(|m| (text(m.get("name")), text(m.get("alias"))))
            .collect();
        let group_raw_models = raw_models(g.get("models"));
        let kind = format!("openai-compatibility:{provider_name}");
        let entries: Vec<&Value> = g
            .get("keys")
            .and_then(Value::as_sequence)
            .into_iter()
            .flatten()
            .collect();
        let mut push = |entry: Option<&Value>| {
            let key = entry.map(|e| text(e.get("api-key"))).unwrap_or_default();
            let key = key.trim();
            let proxy = entry.map(|e| text(e.get("proxy-url"))).unwrap_or_default();
            let proxy = proxy.trim();
            let parts: Vec<&str> = if entry.is_some() {
                vec![key, &base, proxy]
            } else {
                vec![&base]
            };
            let mut k = Key {
                proxy_url: proxy.to_owned(),
                weight: entry.and_then(|e| int(e.get("weight"))),
                ..group_key.clone()
            };
            k.headers = group_key.headers.clone();
            let mut d = base_draft(ids, &kind, &provider_name, &k, &parts);
            d.attrs.insert("base_url".into(), base.clone());
            d.attrs.insert("compat_name".into(), name.clone());
            d.attrs.insert("provider_key".into(), provider.clone());
            if !key.is_empty() {
                d.attrs.insert("api_key".into(), key.into());
            }
            (d.provider, d.label, d.section) = (provider.clone(), name.clone(), "openai-compatibility");
            out.push(d.finish(None, &models, &group_raw_models));
        };
        if entries.is_empty() {
            push(None);
        }
        for entry in entries {
            push(Some(entry));
        }
    }
    out
}

/// One sanitized `api-keys.<family>` entry (Go `cfg.GeminiKey[i]`, `cfg.CodexKey[i]`,
/// ...), as `LoadConfig` leaves it. `index` is its position, which config-backed
/// credentials carry as the `config_index` attribute.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiKeyEntry {
    pub index: usize,
    pub api_key: String,
    pub base_url: String,
    pub prefix: String,
    pub proxy_url: String,
    pub headers: BTreeMap<String, String>,
    pub excluded_models: Vec<String>,
    /// The entry's `models` as configured (config field names, every field).
    pub models: Vec<Json>,
}

/// The sanitized entries of one family, index-aligned with `config_index`.
/// `family` is the section under `api-keys`: `gemini`, `interactions`, `vertex`,
/// `claude`, `codex`, `xai` or `meta` (OpenAI compatibility has its own shape).
pub fn api_key_entries(cfg: &Config, family: &str) -> Vec<ApiKeyEntry> {
    if !matches!(
        family,
        "gemini" | "interactions" | "vertex" | "claude" | "codex" | "xai" | "meta"
    ) {
        return Vec::new();
    }
    sanitized(cfg, family)
        .into_iter()
        .map(|k| ApiKeyEntry {
            index: k.index,
            api_key: k.api_key,
            base_url: k.base_url,
            prefix: k.prefix,
            proxy_url: k.proxy_url,
            headers: k.headers,
            excluded_models: k.excluded,
            models: k.raw_models,
        })
        .collect()
}

/// Go `resolveAPIKeyConfig` (sdk/cliproxy/auth/conductor_models.go): the entry a
/// credential binds to. A config-backed credential takes the entry at its
/// `config_index` when that entry still matches its key and base URL; otherwise the
/// first entry matching credentials, prefix and proxy, then credentials alone, then
/// the key alone. Comparisons are trimmed and case-insensitive like Go's.
pub fn resolve_api_key_entry(cfg: &Config, family: &str, c: &Credential) -> Option<ApiKeyEntry> {
    let mut entries = api_key_entries(cfg, family);
    if entries.is_empty() {
        return None;
    }
    let attr = |k: &str| c.attributes.get(k).map(|v| v.trim()).unwrap_or_default();
    let fold = |a: &str, b: &str| a.trim().to_lowercase() == b.trim().to_lowercase();
    let (key, base) = (attr("api_key"), attr("base_url"));
    let matches = |e: &ApiKeyEntry| {
        let (k, b) = (e.api_key.trim(), e.base_url.trim());
        if !key.is_empty() && !base.is_empty() {
            fold(k, key) && fold(b, base)
        } else if !key.is_empty() {
            fold(k, key) && (b.is_empty() || fold(b, base))
        } else {
            !base.is_empty() && fold(b, base)
        }
    };
    let take = |entries: &mut Vec<ApiKeyEntry>, i: usize| Some(entries.swap_remove(i));
    if matches!(c.source, Source::Config { .. })
        && let Ok(i) = attr("config_index").parse::<usize>()
        && entries.get(i).is_some_and(matches)
    {
        return take(&mut entries, i);
    }
    let (prefix, proxy) = (attr("prefix"), attr("proxy_url"));
    if let Some(i) = entries
        .iter()
        .position(|e| matches(e) && fold(&e.prefix, prefix) && fold(&e.proxy_url, proxy))
    {
        return take(&mut entries, i);
    }
    if let Some(i) = entries.iter().position(matches) {
        return take(&mut entries, i);
    }
    if !key.is_empty()
        && let Some(i) = entries.iter().position(|e| fold(&e.api_key, key))
    {
        return take(&mut entries, i);
    }
    None
}

/// Go `proxyURLFromAPIKeyConfig` (management `api-call`): the `proxy-url` of the first
/// config entry matching an API-key credential's key and base URL, which is not
/// always the entry the credential came from when keys repeat. Empty when none
/// matches; callers check that the credential is an API key.
pub fn api_key_config_proxy(cfg: &Config, c: &Credential) -> String {
    let attr = |k: &str| c.attributes.get(k).map(|v| v.trim()).unwrap_or_default();
    let fold = |a: &str, b: &str| a.to_lowercase() == b.to_lowercase();
    let (key, base) = (attr("api_key"), attr("base_url"));
    let compat = attr("compat_name");
    if !compat.is_empty() || fold(c.provider.trim(), "openai-compatibility") {
        if key.is_empty() {
            return String::new();
        }
        let candidates: Vec<&str> = [compat, attr("provider_key"), c.provider.trim()]
            .into_iter()
            .filter(|v| !v.is_empty())
            .collect();
        for group in groups(cfg, "openai-compatibility") {
            let Some(g) = group.as_mapping() else { continue };
            // SanitizeOpenAICompatibility drops entries without a base URL.
            if text(g.get("base-url")).trim().is_empty() || boolean(g.get("disabled")).unwrap_or(false) {
                continue;
            }
            let name = text(g.get("name")).trim().to_owned();
            if !candidates.iter().any(|cand| fold(cand, &name)) {
                continue;
            }
            return g
                .get("keys")
                .and_then(Value::as_sequence)
                .into_iter()
                .flatten()
                .find(|e| fold(text(e.get("api-key")).trim(), key))
                .map(|e| text(e.get("proxy-url")).trim().to_owned())
                .unwrap_or_default();
        }
        return String::new();
    }
    let family = match c.provider.trim().to_lowercase().as_str() {
        "gemini" => "gemini",
        "gemini-interactions" => "interactions",
        "claude" => "claude",
        "codex" => "codex",
        "xai" => "xai",
        "meta" => "meta",
        _ => return String::new(),
    };
    // Go `resolveAPIKeyConfig`.
    let entries = sanitized(cfg, family);
    let matched = entries.iter().find(|e| {
        let (k, b) = (e.api_key.trim(), e.base_url.trim());
        if !key.is_empty() && !base.is_empty() {
            fold(k, key) && fold(b, base)
        } else if !key.is_empty() {
            fold(k, key) && (b.is_empty() || fold(b, base))
        } else {
            !base.is_empty() && fold(b, base)
        }
    });
    let matched = matched.or_else(|| {
        (!key.is_empty())
            .then(|| entries.iter().find(|e| fold(e.api_key.trim(), key)))
            .flatten()
    });
    matched.map(|e| e.proxy_url.trim().to_owned()).unwrap_or_default()
}

/// A `models` YAML sequence as JSON entries (mappings only).
fn raw_models(v: Option<&Value>) -> Vec<Json> {
    v.and_then(Value::as_sequence)
        .into_iter()
        .flatten()
        .filter(|m| m.is_mapping())
        .filter_map(|m| serde_json::to_value(m).ok())
        .collect()
}

/// Go `NormalizeCredentialMetadata`: config-style aliases become snake_case unless the
/// canonical key is present.
fn normalize_metadata(meta: &mut Map<String, Json>) {
    for (alias, canonical) in [
        ("api-key", "api_key"),
        ("base-url", "base_url"),
        ("disable-cooling", "disable_cooling"),
        ("excluded-models", "excluded_models"),
        ("fingerprint-profile", "fingerprint_profile"),
        ("model-aliases", "model_aliases"),
        ("proxy-url", "proxy_url"),
        ("request-retry", "request_retry"),
        ("request-scoped-errors", "request_scoped_errors"),
        ("tool-prefix-disabled", "tool_prefix_disabled"),
    ] {
        if let Some(value) = meta.remove(alias) {
            meta.entry(canonical).or_insert(value);
        }
    }
}

/// Go `credentialweight.ParseValue` for JSON metadata.
pub fn parse_weight(value: &Json) -> Result<i64, String> {
    let normalize = |w: i64| {
        if w <= 0 {
            Ok(0)
        } else if w > WEIGHT_MAX {
            Err(format!("weight must not exceed {WEIGHT_MAX}"))
        } else {
            Ok(w)
        }
    };
    match value {
        Json::Number(n) => {
            let f = n.as_f64().unwrap_or(f64::NAN);
            if !f.is_finite() || f.trunc() != f {
                Err("weight must be an integer".into())
            } else if f <= 0.0 {
                Ok(0)
            } else if f > WEIGHT_MAX as f64 {
                Err(format!("weight must not exceed {WEIGHT_MAX}"))
            } else {
                Ok(f as i64)
            }
        }
        Json::String(s) if s.trim().is_empty() => Ok(1),
        Json::String(s) => s
            .trim()
            .parse::<i64>()
            .map_err(|e| format!("weight must be an integer: {e}"))
            .and_then(normalize),
        _ => Err("weight must be an integer".into()),
    }
}

/// Builds one auth-file credential as Go's `FileSynthesizer` does. `Ok(None)` is a
/// silently ignored file; `Err` is a skipped file worth a warning.
pub fn from_file(cfg: &Config, auth_dir: &Path, path: &Path, data: &[u8]) -> Result<Option<Credential>, String> {
    if data.is_empty() {
        return Ok(None);
    }
    let Ok(mut meta) = serde_json::from_slice::<Map<String, Json>>(data) else {
        return Ok(None);
    };
    normalize_metadata(&mut meta);
    let weight = match meta.get("weight") {
        Some(w) => Some(parse_weight(w).map_err(|e| format!("invalid metadata weight: {e}"))?),
        None => None,
    };
    let provider = meta
        .get("type")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    if provider.is_empty() || provider == "gemini" || provider == "gemini-cli" {
        return Ok(None);
    }
    let full = path.display().to_string();
    let mut attrs = BTreeMap::new();
    for key in ["source", "path"] {
        attrs.insert(key.into(), full.clone());
    }
    attrs.insert("source_backend".into(), "file".into());
    match meta.get("priority") {
        Some(Json::Number(n)) => {
            let p = n.as_f64().map(|f| f.trunc() as i64).unwrap_or(0);
            attrs.insert("priority".into(), p.to_string());
            attrs.insert("file_priority".into(), "true".into());
        }
        Some(Json::String(s)) if s.trim().parse::<i64>().is_ok() => {
            attrs.insert("priority".into(), s.trim().into());
            attrs.insert("file_priority".into(), "true".into());
        }
        _ => {}
    }
    if let Some(w) = weight {
        attrs.insert("weight".into(), w.to_string());
    }
    if let Some(note) = meta.get("note").and_then(Json::as_str).map(str::trim)
        && !note.is_empty()
    {
        attrs.insert("note".into(), note.into());
    }
    if let Some(Json::Object(h)) = meta.get("headers") {
        for (k, v) in h {
            if let (k, Some(v)) = (k.trim(), v.as_str().map(str::trim))
                && !k.is_empty()
                && !v.is_empty()
            {
                attrs.insert(format!("header:{k}"), v.into());
            }
        }
    }
    if let Some(aliases) = oauth_aliases(meta.get("model_aliases")) {
        attrs.insert("model_aliases".into(), aliases);
    }
    let per_account: Vec<String> = match meta.get("excluded_models") {
        Some(Json::Array(list)) => list
            .iter()
            .filter_map(Json::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    };
    let global = oauth_excluded(cfg).remove(&provider).unwrap_or_default();
    let combined = excluded_union(&[&per_account, &global]);
    if !combined.is_empty() {
        attrs.insert("excluded_models".into(), combined.join(","));
    }
    attrs.insert("auth_kind".into(), "oauth".into());
    if let Some(p) = meta
        .get("fingerprint_profile")
        .and_then(Json::as_str)
        .map(|s| s.trim().to_lowercase())
        && !p.is_empty()
    {
        attrs.insert("fingerprint_profile".into(), p);
    }
    if matches!(provider.as_str(), "kimi" | "kimi-ai" | "kimi.ai" | "kimi.com") {
        kimi_attributes(&provider, &meta, &mut attrs);
    }
    if provider == "codex" {
        let plan = meta
            .get("plan_type")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .or_else(|| {
                let token = meta.get("id_token").and_then(Json::as_str)?.trim();
                (!token.is_empty()).then(|| codex_plan(token))
            });
        if let Some(plan) = plan {
            attrs.insert("plan_type".into(), plan);
        }
    }
    // Always present for files, even empty: routing must not fall back to a raw
    // metadata prefix that Go's normalization rejected (for example "a/b").
    let prefix = meta.get("prefix").and_then(Json::as_str).map(normalize_prefix);
    attrs.insert("prefix".into(), prefix.unwrap_or_default());
    if let Some(proxy) = meta.get("proxy_url").and_then(Json::as_str)
        && !proxy.is_empty()
    {
        attrs.insert("proxy_url".into(), proxy.into());
    }
    let id = path
        .strip_prefix(auth_dir)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned();
    let mut cred = Credential {
        id,
        provider,
        source: Source::File(path.to_owned()),
        disabled: false,
        label: String::new(),
        attributes: attrs,
        metadata: meta,
        revision: 0,
    };
    cred.refresh_derived();
    Ok(Some(cred))
}

/// Go `buildAuthFromFileData`'s fallback for an uploaded file no synthesizer claims:
/// provider is the raw `type` (or `unknown`), label the email or provider, and only
/// `path`, `source` and custom headers as attributes. Go keeps it registered until
/// restart; the caller decides how long it lives. `None` for invalid JSON.
pub fn upload_fallback(auth_dir: &Path, path: &Path, data: &[u8]) -> Option<Credential> {
    let mut meta = serde_json::from_slice::<Map<String, Json>>(data).ok()?;
    normalize_metadata(&mut meta);
    let provider = match meta.get("type").and_then(Json::as_str) {
        Some(t) if !t.is_empty() => t.to_owned(),
        _ => "unknown".to_owned(),
    };
    let label = match meta.get("email").and_then(Json::as_str) {
        Some(e) if !e.is_empty() => e.to_owned(),
        _ => provider.clone(),
    };
    let full = path.display().to_string();
    let mut attributes = BTreeMap::from([("path".to_owned(), full.clone()), ("source".to_owned(), full)]);
    if let Some(Json::Object(h)) = meta.get("headers") {
        for (k, v) in h {
            if let (k, Some(v)) = (k.trim(), v.as_str().map(str::trim))
                && !k.is_empty()
                && !v.is_empty()
            {
                attributes.insert(format!("header:{k}"), v.into());
            }
        }
    }
    // Go `authIDForPath`: relative to the absolute auth dir (lexical, no symlinks).
    let absolute = |p: &Path| std::path::absolute(p).unwrap_or_else(|_| p.to_owned());
    let (dir, file) = (absolute(auth_dir), absolute(path));
    Some(Credential {
        id: file.strip_prefix(&dir).unwrap_or(&file).to_string_lossy().into_owned(),
        provider,
        source: Source::File(path.to_owned()),
        disabled: meta.get("disabled").and_then(Json::as_bool).unwrap_or(false),
        label,
        attributes,
        metadata: meta,
        revision: 0,
    })
}

fn oauth_aliases(raw: Option<&Json>) -> Option<String> {
    let mut seen = HashSet::new();
    let clean: Vec<Json> = raw?
        .as_array()?
        .iter()
        .filter_map(|e| {
            let name = e.get("name")?.as_str()?.trim();
            let alias = e.get("alias")?.as_str()?.trim();
            (!name.is_empty() && !alias.is_empty() && !name.eq_ignore_ascii_case(alias)).then_some((name, alias, e))
        })
        .filter(|(_, alias, _)| seen.insert(alias.to_lowercase()))
        .map(|(name, alias, e)| {
            let mut out = Map::new();
            out.insert("name".into(), name.into());
            out.insert("alias".into(), alias.into());
            if e.get("fork").and_then(Json::as_bool) == Some(true) {
                out.insert("fork".into(), true.into());
            }
            if let Some(d) = e.get("display-name").and_then(Json::as_str).map(str::trim)
                && !d.is_empty()
            {
                out.insert("display-name".into(), d.into());
            }
            if e.get("force-mapping").and_then(Json::as_bool) == Some(true) {
                out.insert("force-mapping".into(), true.into());
            }
            Json::Object(out)
        })
        .collect();
    (!clean.is_empty()).then(|| Json::Array(clean).to_string())
}

/// Go `codex.ParseJWTToken(...).GetPlanType()`, defaulting to "free".
const KIMI_COM: &str = "kimi.com";
const KIMI_AI: &str = "kimi.ai";

/// Go `FileSynthesizer`'s Kimi branch: keep `domain` and `base_url`, canonicalize
/// the domain and default the API base URL from the resolved domain.
fn kimi_attributes(provider: &str, meta: &Map<String, Json>, attrs: &mut BTreeMap<String, String>) {
    let text = |key: &str| {
        meta.get(key)
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    let (domain, base_url) = (text("domain"), text("base_url"));
    // Go `ResolveKimiDomainFromAuth`. Its metadata steps repeat the attribute
    // steps, `type` equals the provider, and every provider that reaches this
    // branch classifies, so the file-name fallback is unreachable.
    let resolved = domain
        .and_then(kimi_domain)
        .or_else(|| base_url.and_then(kimi_url_domain))
        .or_else(|| kimi_domain(provider))
        .unwrap_or(KIMI_COM);
    let normalized = domain.map_or(resolved, |d| {
        kimi_domain(d).filter(|d| *d == KIMI_AI).unwrap_or(KIMI_COM)
    });
    attrs.insert("domain".into(), normalized.into());
    let default_base = if resolved == KIMI_AI {
        "https://api.kimi.ai/coding"
    } else {
        "https://api.kimi.com/coding"
    };
    attrs.insert("base_url".into(), base_url.unwrap_or(default_base).into());
}

/// Go `IsKimiAIDomain` / `IsKimiComDomain`.
fn kimi_domain(s: &str) -> Option<&'static str> {
    let d = s.trim().to_lowercase();
    if matches!(d.as_str(), "kimi.ai" | "ai" | "kimi-ai") || d.ends_with(".kimi.ai") {
        Some(KIMI_AI)
    } else if matches!(d.as_str(), "kimi.com" | "com" | "kimi") || d.ends_with(".kimi.com") {
        Some(KIMI_COM)
    } else {
        None
    }
}

/// Go `isKimiAIHost` / `isKimiComHost`: a URL Go cannot parse classifies as neither.
fn kimi_url_domain(raw: &str) -> Option<&'static str> {
    let host = super::go_url::parse(raw.trim())?.hostname().trim().to_lowercase();
    if host == KIMI_AI || host.ends_with(".kimi.ai") {
        Some(KIMI_AI)
    } else if host == KIMI_COM || host.ends_with(".kimi.com") {
        Some(KIMI_COM)
    } else {
        None
    }
}

fn codex_plan(token: &str) -> String {
    use base64::Engine;
    let payload = token.split('.').collect::<Vec<_>>();
    payload
        .get(1)
        .filter(|_| payload.len() == 3)
        .and_then(|p| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(p.trim_end_matches('='))
                .ok()
        })
        .and_then(|bytes| serde_json::from_slice::<Json>(&bytes).ok())
        .and_then(|claims| {
            Some(
                claims
                    .get("https://api.openai.com/auth")?
                    .get("chatgpt_plan_type")?
                    .as_str()?
                    .trim()
                    .to_owned(),
            )
        })
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "free".into())
}

/// Go `NormalizeOAuthExcludedModels` over `oauth.excluded-models`.
pub fn oauth_excluded(cfg: &Config) -> HashMap<String, Vec<String>> {
    let mut out = HashMap::new();
    if let Some(map) = cfg
        .document
        .get("oauth")
        .and_then(|o| o.get("excluded-models"))
        .and_then(Value::as_mapping)
    {
        for (provider, models) in map {
            let provider = text(Some(provider)).trim().to_lowercase();
            let models = normalize_excluded(&strings(Some(models)));
            if !provider.is_empty() && !models.is_empty() {
                out.insert(provider, models);
            }
        }
    }
    out
}

/// Every auth-dir JSON file (case-insensitive extension, top level only), sorted by ID.
/// Like Go's `FileSynthesizer`, an unreadable or missing directory is an empty set:
/// it must never block publishing the rest of a valid config.
pub fn from_auth_dir(cfg: &Config) -> Vec<Credential> {
    let entries = match std::fs::read_dir(&cfg.auth_dir) {
        Ok(entries) => entries,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(dir = %cfg.auth_dir.display(), error = %e, "cannot read auth dir");
            }
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path: PathBuf = entry.path();
        if entry.file_type().is_ok_and(|t| t.is_dir())
            || !entry.file_name().to_string_lossy().to_lowercase().ends_with(".json")
        {
            continue;
        }
        let Ok(data) = std::fs::read(&path) else { continue };
        match from_file(cfg, &cfg.auth_dir, &path, &data) {
            Ok(Some(c)) => out.push(c),
            Ok(None) => {}
            Err(e) => tracing::warn!(path = %path.display(), error = %e, "skipping auth file"),
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// All runtime credentials for a config: auth files, then config API keys.
pub fn load(cfg: &Config) -> Vec<Credential> {
    let mut all = from_auth_dir(cfg);
    all.extend(from_config(cfg));
    all
}

/// The attribute carrying the auth index Home assigned to a dispatched credential (Go
/// sets `auth.Index` from the dispatch response).
pub const HOME_AUTH_INDEX: &str = "home_auth_index";

/// The attribute every credential Home dispatched carries: the auth's own `provider`
/// field. Executors read its presence as Go's `cfg.Home.Enabled`.
pub const HOME_PROVIDER: &str = "home_provider";

/// The attribute carrying the `credential_options` model Home mode selected for a
/// dispatched credential and its upstream model (Go `ResolvedHomeModelOptions`): the
/// JSON object of the matching `models` entry, `{}` when none matches. Absent when the
/// credential's options list no models.
pub const HOME_MODEL_OPTIONS: &str = "home_model_options";

/// Go `AccessTokenSHA256`: hex SHA-256 of the metadata access token (`access_token`
/// or `accessToken`, top level or under `token`/`Token`), empty without one.
pub fn access_token_sha256(c: &Credential) -> String {
    use sha2::{Digest, Sha256};
    let pick = |m: &serde_json::Map<String, Json>| {
        ["access_token", "accessToken"].iter().find_map(|k| {
            m.get(*k)
                .and_then(Json::as_str)
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
        })
    };
    let token = pick(&c.metadata).or_else(|| {
        ["token", "Token"]
            .iter()
            .find_map(|k| c.metadata.get(*k).and_then(Json::as_object).and_then(pick))
    });
    token.map_or_else(String::new, |t| {
        Sha256::digest(t.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    })
}

/// Go `Auth.EnsureIndex`: an assigned index (Home's, for dispatched credentials), else
/// 16 hex chars of sha256 over a stable identity seed.
pub fn auth_index(c: &Credential) -> String {
    let attr = |k: &str| c.attributes.get(k).map(|s| s.trim()).unwrap_or_default();
    if !attr(HOME_AUTH_INDEX).is_empty() {
        return attr(HOME_AUTH_INDEX).to_owned();
    }
    let seed = if !attr("auth_index_seed").is_empty() {
        format!("auth_index_seed:{}", attr("auth_index_seed"))
    } else {
        let provider = c.provider.trim().to_lowercase();
        let mut file = match &c.source {
            Source::File(p) => p.display().to_string(),
            Source::Config { .. } | Source::Runtime => String::new(),
        };
        for k in ["path", "source"] {
            if file.is_empty() {
                file = attr(k).to_owned();
            }
        }
        if file.is_empty() {
            file = c.id.trim().to_owned();
        }
        if file.to_lowercase().ends_with(".json") {
            let abs = std::path::absolute(&file).unwrap_or_else(|_| PathBuf::from(&file));
            let kind = c
                .str("type")
                .map(|t| t.trim().to_lowercase())
                .filter(|t| !t.is_empty())
                .unwrap_or(provider);
            format!("{kind}:{}", clean(&abs).display())
        } else {
            let (key, base) = (attr("api_key"), attr("base_url"));
            let prefix = if key.is_empty() {
                ""
            } else if !attr("compat_name").is_empty() || provider == "openai-compatibility" {
                "openai-compatibility"
            } else {
                match provider.as_str() {
                    "gemini" => "gemini-api-key",
                    "gemini-interactions" => "interactions-api-key",
                    "codex" => "codex-api-key",
                    "xai" => "xai-api-key",
                    "claude" => "claude-api-key",
                    "meta" => "meta-api-key",
                    _ => "",
                }
            };
            if !prefix.is_empty() {
                format!("{prefix}:{base}+{key}")
            } else if !c.id.trim().is_empty() {
                format!("id:{}", c.id.trim())
            } else {
                return String::new();
            }
        }
    };
    hex(&Sha256::digest(seed.as_bytes())[..8])
}

/// Go `filepath.Clean` for absolute Unix paths.
fn clean(path: &Path) -> PathBuf {
    let mut out = PathBuf::from("/");
    for part in path.components() {
        match part {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::Normal(p) => out.push(p),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Go `buildCodexConfigModels`: name-only entries register under their name, an
    /// alias with its display name; vertex keeps only entries with both (its sanitizer).
    #[test]
    fn config_models_reach_the_registry_like_go() {
        let cfg = Config::parse(
            "api-keys:\n  codex:\n    - base-url: https://c.example.invalid\n      models: [{name: gpt-name-only}, {name: gpt-real, alias: friendly, display-name: Friendly}]\n      keys: [{api-key: k}]\n  vertex:\n    - base-url: https://v.example.invalid\n      models: [{name: v-only}, {name: v1, alias: va}]\n      keys: [{api-key: vk}]\n",
        )
        .unwrap();
        let creds = from_config(&cfg);
        let aliases = crate::registry::dynamic::global_aliases(&cfg);
        let ids = |provider: &str| -> Vec<(String, String)> {
            let c = creds.iter().find(|c| c.provider == provider).unwrap();
            crate::registry::dynamic::models_for(&cfg, &aliases, c, 0)
                .into_iter()
                .map(|m| (m.id, m.display_name))
                .collect()
        };
        assert_eq!(
            ids("codex"),
            [
                ("gpt-name-only".to_owned(), "gpt-name-only".to_owned()),
                ("friendly".to_owned(), "Friendly".to_owned())
            ]
        );
        assert_eq!(
            ids("vertex").iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
            ["va"]
        );
    }

    /// Go `resolveAPIKeyConfig`: `config_index` binds exactly even when an earlier
    /// entry has the same key and base URL (differing only by headers), and a stale
    /// index falls back to matching.
    #[test]
    fn resolve_api_key_entry_prefers_the_matching_config_index() {
        let cfg = Config::parse(
            "api-keys:\n  gemini:\n    - base-url: https://g.example.invalid\n      headers: {X-A: one}\n      models: [{name: first}]\n      keys: [{api-key: same}]\n    - base-url: https://g.example.invalid\n      headers: {X-A: two}\n      models: [{name: second}]\n      keys: [{api-key: same}]\n",
        )
        .unwrap();
        let entries = api_key_entries(&cfg, "gemini");
        assert_eq!(entries.iter().map(|e| e.index).collect::<Vec<_>>(), [0, 1]);
        let creds = from_config(&cfg);
        assert_eq!(creds.len(), 2);
        let second = &creds[1];
        assert_eq!(second.attributes["config_index"], "1");
        let bound = resolve_api_key_entry(&cfg, "gemini", second).unwrap();
        assert_eq!((bound.index, bound.models[0]["name"].as_str()), (1, Some("second")));
        let mut stale = second.clone();
        stale.attributes.insert("config_index".into(), "9".into());
        assert_eq!(resolve_api_key_entry(&cfg, "gemini", &stale).unwrap().index, 0);
        assert!(api_key_entries(&cfg, "openai-compatibility").is_empty());
    }

    /// Go `proxyURLFromAPIKeyConfig`: the first matching entry wins, so a repeated key
    /// takes the earlier entry's `direct`, and compat keys match by name then key.
    #[test]
    fn api_key_config_proxy_takes_the_first_matching_entry() {
        let cfg = Config::parse(
            "api-keys:\n  claude:\n    - base-url: https://c.example.invalid\n      proxy-url: direct\n      keys: [{api-key: dup}]\n    - base-url: https://c.example.invalid\n      keys: [{api-key: dup, prefix: second}]\n  openai-compatibility:\n    - name: Off\n      disabled: true\n      base-url: https://o.example.invalid\n      keys: [{api-key: k, proxy-url: socks5://off.example.invalid:1}]\n    - name: One\n      base-url: https://o.example.invalid\n      keys: [{api-key: k, proxy-url: http://one.example.invalid:1}]\n",
        )
        .unwrap();
        let creds = from_config(&cfg);
        let second = creds
            .iter()
            .find(|c| c.attributes.get("prefix").map(String::as_str) == Some("second"))
            .unwrap();
        assert_eq!(second.attributes.get("proxy_url"), None);
        assert_eq!(api_key_config_proxy(&cfg, second), "direct");
        let compat = creds.iter().find(|c| c.attributes.contains_key("compat_name")).unwrap();
        assert_eq!(api_key_config_proxy(&cfg, compat), "http://one.example.invalid:1");
    }
}
