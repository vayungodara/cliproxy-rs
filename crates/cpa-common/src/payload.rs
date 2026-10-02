//! Config payload rules applied to the final provider body (Go
//! internal/runtime/executor/helps/payload_helpers.go `ApplyPayloadConfigWith*`), plus
//! the Codex-client tool schema integer normalization (Go
//! internal/client/codex/tool-schema `NormalizeCodexToolIntegerTypes`).
//!
//! Order, as in Go:
//! 1. Codex clients (User-Agent contains `codex`) sent to a non-Codex executor get
//!    `number` tool parameters of known Codex tools rewritten to `integer`.
//! 2. `disable-image-generation` strips the `image_generation` tool and tool choice.
//! 3. `requests.payload` rules, for the upstream model, the client's model and its
//!    suffix-free base: `default` and `default-raw` set fields missing from the original
//!    request (first write wins), `override` and `override-raw` set fields (last write
//!    wins), `filter` deletes fields.
//!
//! Edits go through `crate::json` (tidwall sjson semantics), so untouched bytes stay
//! byte for byte.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use cpa_core::config::Config;
use http::HeaderMap;
use serde_yaml_ng::Value as Yaml;

use crate::json::{self, GoValue, Kind, Res};

/// One request's context for payload rules (Go's parameters to
/// `ApplyPayloadConfigWithTrackedPathsForExecutor`).
#[derive(Debug, Clone, Copy, Default)]
pub struct Request<'a> {
    /// The executor receiving the body (`codex` and `codex-websockets` skip the Codex
    /// integer normalization).
    pub target_executor: &'a str,
    /// Upstream model after alias resolution.
    pub model: &'a str,
    /// The model the client asked for (Go `requested_model` metadata), suffix included.
    pub requested_model: &'a str,
    /// Target protocol (`openai`, `claude`, `gemini`, `codex`, ...).
    pub protocol: &'a str,
    /// Source protocol of the client request.
    pub from_protocol: &'a str,
    /// Path all rule paths are relative to (`request` for Antigravity envelopes).
    pub root: &'a str,
    /// The client's original body; defaults check it for existing fields.
    pub original: &'a [u8],
    /// The inbound route path (`/v1/images/generations` keeps image generation in
    /// `chat` mode).
    pub request_path: &'a str,
    /// Inbound client headers (rule header gates, Codex detection).
    pub headers: Option<&'a HeaderMap>,
}

/// A configured value, as Go's YAML decoder yields it into `any`.
#[derive(Debug, Clone, PartialEq)]
enum Param {
    Null,
    Bool(bool),
    Int(i64),
    Uint(u64),
    Float(f64),
    Str(String),
    /// A sequence or mapping (Go `[]any` / `map[string]any`).
    Json(GoValue),
}

impl Param {
    fn from_yaml(v: &Yaml) -> Self {
        match v {
            Yaml::Null => Param::Null,
            Yaml::Bool(b) => Param::Bool(*b),
            Yaml::Number(n) => match (n.as_i64(), n.as_u64(), n.as_f64()) {
                (Some(i), _, _) => Param::Int(i),
                (None, Some(u), _) => Param::Uint(u),
                (None, None, Some(f)) => Param::Float(f),
                _ => Param::Null,
            },
            Yaml::String(s) => Param::Str(s.clone()),
            Yaml::Tagged(t) => Param::from_yaml(&t.value),
            other => Param::Json(go_value(other)),
        }
    }

    /// `json.Marshal(value)`.
    fn marshal(&self) -> Option<Vec<u8>> {
        Some(match self {
            Param::Null => b"null".to_vec(),
            Param::Bool(b) => b.to_string().into_bytes(),
            Param::Int(i) => i.to_string().into_bytes(),
            Param::Uint(u) => u.to_string().into_bytes(),
            Param::Float(f) => json::json_float(*f)?.into_bytes(),
            Param::Str(s) => json::quote(s),
            Param::Json(v) => v.marshal(),
        })
    }

    /// The raw JSON `sjson.SetBytes(_, path, value)` writes for non-string values.
    fn sjson_raw(&self) -> Option<Vec<u8>> {
        match self {
            // sjson formats float64 with strconv 'f', not encoding/json.
            Param::Float(f) => f.is_finite().then(|| json::fmt_float(*f).into_bytes()),
            other => other.marshal(),
        }
    }
}

/// Go yaml.v3 into `any`, then the `json.Marshal` model.
fn go_value(v: &Yaml) -> GoValue {
    match v {
        Yaml::Null => GoValue::Null,
        Yaml::Bool(b) => GoValue::Bool(*b),
        Yaml::Number(n) => match (n.as_i64(), n.as_u64(), n.as_f64()) {
            (Some(i), _, _) => GoValue::Number(i.to_string()),
            (None, Some(u), _) => GoValue::Number(u.to_string()),
            (None, None, Some(f)) => json::json_float(f).map_or(GoValue::Null, GoValue::Number),
            _ => GoValue::Null,
        },
        Yaml::String(s) => GoValue::String(s.clone()),
        Yaml::Sequence(items) => GoValue::Array(items.iter().map(go_value).collect()),
        Yaml::Mapping(map) => GoValue::Object(
            map.iter()
                .map(|(k, v)| (yaml_key(k), go_value(v)))
                .collect::<BTreeMap<_, _>>(),
        ),
        Yaml::Tagged(t) => go_value(&t.value),
    }
}

fn yaml_key(k: &Yaml) -> String {
    match k {
        Yaml::String(s) => s.clone(),
        Yaml::Bool(b) => b.to_string(),
        Yaml::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

fn yaml_str(v: Option<&Yaml>) -> String {
    match v {
        Some(Yaml::String(s)) => s.clone(),
        Some(Yaml::Number(n)) => n.to_string(),
        Some(Yaml::Bool(b)) => b.to_string(),
        _ => String::new(),
    }
}

/// Go `PayloadModelRule`.
#[derive(Debug, Clone, Default)]
struct ModelRule {
    name: String,
    protocol: String,
    from_protocol: String,
    headers: Vec<(String, String)>,
    matches: Vec<Vec<(String, Param)>>,
    not_matches: Vec<Vec<(String, Param)>>,
    exist: Vec<String>,
    not_exist: Vec<String>,
}

#[derive(Debug, Clone, Default)]
struct Rule {
    models: Vec<ModelRule>,
    params: Vec<(String, Param)>,
}

#[derive(Debug, Clone, Default)]
struct FilterRule {
    models: Vec<ModelRule>,
    params: Vec<String>,
}

/// Go `DisableImageGenerationMode`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ImageGeneration {
    #[default]
    Off,
    All,
    Chat,
    Passthrough,
}

/// `requests.payload` and `multimedia.disable-image-generation`. Use [`Rules::of`] to
/// share one parse per config snapshot.
#[derive(Debug, Clone, Default)]
pub struct Rules {
    default: Vec<Rule>,
    default_raw: Vec<Rule>,
    overrides: Vec<Rule>,
    override_raw: Vec<Rule>,
    filter: Vec<FilterRule>,
    pub image_generation: ImageGeneration,
}

impl Rules {
    /// The rules of one config snapshot, parsed once per snapshot and shared (cached in
    /// `Config::derived`). Executors call this per request with the `cfg` they were
    /// given.
    pub fn of(cfg: &Config) -> std::sync::Arc<Rules> {
        cfg.derived(Rules::from_config)
    }

    pub fn from_config(cfg: &Config) -> Self {
        let doc = &cfg.document;
        let payload = doc.get("requests").and_then(|r| r.get("payload"));
        let section = |name: &str| -> Vec<&Yaml> {
            payload
                .and_then(|p| p.get(name))
                .and_then(Yaml::as_sequence)
                .map(|s| s.iter().collect())
                .unwrap_or_default()
        };
        let rules = |name: &str, raw: bool| -> Vec<Rule> {
            section(name)
                .into_iter()
                .map(|r| Rule {
                    models: model_rules(r.get("models")),
                    params: r
                        .get("params")
                        .and_then(Yaml::as_mapping)
                        .into_iter()
                        .flatten()
                        .map(|(k, v)| (yaml_key(k), Param::from_yaml(v)))
                        .collect(),
                })
                // Go `SanitizePayloadRules`: a raw rule with no params or an invalid raw
                // JSON string param is dropped whole.
                .filter(|r| {
                    !raw || (!r.params.is_empty()
                        && r.params.iter().all(|(_, v)| match v {
                            Param::Str(s) => {
                                let t = s.trim().as_bytes();
                                !t.is_empty() && json::valid(t)
                            }
                            _ => true,
                        }))
                })
                .collect()
        };
        let filter = section("filter")
            .into_iter()
            .map(|r| FilterRule {
                models: model_rules(r.get("models")),
                params: r
                    .get("params")
                    .and_then(Yaml::as_sequence)
                    .into_iter()
                    .flatten()
                    .map(|p| yaml_str(Some(p)))
                    .collect(),
            })
            .collect();
        let image_generation = match doc.get("multimedia").and_then(|m| m.get("disable-image-generation")) {
            Some(Yaml::Bool(true)) => ImageGeneration::All,
            Some(Yaml::String(s)) => match s.trim().to_lowercase().as_str() {
                "true" | "1" | "on" | "yes" => ImageGeneration::All,
                "chat" => ImageGeneration::Chat,
                "passthrough" => ImageGeneration::Passthrough,
                _ => ImageGeneration::Off,
            },
            _ => ImageGeneration::Off,
        };
        Self {
            default: rules("default", false),
            default_raw: rules("default-raw", true),
            overrides: rules("override", false),
            override_raw: rules("override-raw", true),
            filter,
            image_generation,
        }
    }

    fn has_rules(&self) -> bool {
        !(self.default.is_empty()
            && self.default_raw.is_empty()
            && self.overrides.is_empty()
            && self.override_raw.is_empty()
            && self.filter.is_empty())
    }
}

fn model_rules(v: Option<&Yaml>) -> Vec<ModelRule> {
    let conditions = |m: &Yaml, key: &str| -> Vec<Vec<(String, Param)>> {
        m.get(key)
            .and_then(Yaml::as_sequence)
            .into_iter()
            .flatten()
            .map(|c| {
                c.as_mapping()
                    .into_iter()
                    .flatten()
                    .map(|(k, v)| (yaml_key(k), Param::from_yaml(v)))
                    .collect()
            })
            .collect()
    };
    let strings = |m: &Yaml, key: &str| -> Vec<String> {
        m.get(key)
            .and_then(Yaml::as_sequence)
            .into_iter()
            .flatten()
            .map(|s| yaml_str(Some(s)))
            .collect()
    };
    v.and_then(Yaml::as_sequence)
        .into_iter()
        .flatten()
        .map(|m| ModelRule {
            name: yaml_str(m.get("name")),
            protocol: yaml_str(m.get("protocol")),
            from_protocol: yaml_str(m.get("from-protocol")),
            headers: m
                .get("headers")
                .and_then(Yaml::as_mapping)
                .into_iter()
                .flatten()
                .map(|(k, v)| (yaml_key(k), yaml_str(Some(v))))
                .collect(),
            matches: conditions(m, "match"),
            not_matches: conditions(m, "not-match"),
            exist: strings(m, "exist"),
            not_exist: strings(m, "not-exist"),
        })
        .collect()
}

/// [`apply_tracked`] without tracking.
pub fn apply(rules: &Rules, req: &Request<'_>, payload: Vec<u8>) -> Vec<u8> {
    apply_tracked(rules, req, payload, &[]).0
}

/// Go `ApplyPayloadConfigWithTrackedPathsForExecutor`: the edited body and the tracked
/// paths an applied rule targeted (the path itself, an ancestor or a descendant).
pub fn apply_tracked(
    rules: &Rules,
    req: &Request<'_>,
    payload: Vec<u8>,
    tracked: &[&str],
) -> (Vec<u8>, BTreeSet<String>) {
    let mut touched = BTreeSet::new();
    if payload.is_empty() {
        return (payload, touched);
    }
    let mut out = payload;
    if let Some(headers) = req.headers
        && is_codex_user_agent(headers)
        && !matches!(
            req.target_executor.trim().to_lowercase().as_str(),
            "codex" | "codex-websockets" | "codex_websockets"
        )
    {
        out = normalize_codex_tool_integer_types(&out, headers);
    }
    let mut mark = |resolved: &str| {
        for t in tracked.iter().map(|t| t.trim()).filter(|t| !t.is_empty()) {
            if targets_path(resolved, t) {
                touched.insert(t.to_owned());
            }
        }
    };
    let out = edit(rules, req, out, &mut mark);
    (out, touched)
}

fn edit(rules: &Rules, req: &Request<'_>, mut out: Vec<u8>, mark: &mut dyn FnMut(&str)) -> Vec<u8> {
    // Go: defaults check the original request, else the body as it entered (after the
    // Codex normalization, before image-generation stripping).
    let source = if req.original.is_empty() {
        out.clone()
    } else {
        req.original.to_vec()
    };
    if strip_image_generation(rules.image_generation, req.request_path) {
        out = remove_tool_type(&out, &build_path(req.root, "tools"), "image_generation");
        out = remove_tool_choice(&out, &build_path(req.root, "tool_choice"), "image_generation");
    }
    let (model, requested) = (req.model.trim(), req.requested_model.trim());
    if !rules.has_rules() || (model.is_empty() && requested.is_empty()) {
        return out;
    }
    let candidates = model_candidates(model, requested);
    let matches = |models: &[ModelRule], out: &[u8]| rules_match(models, req, out, &candidates);

    let mut defaulted: HashSet<String> = HashSet::new();
    for rule in &rules.default {
        if !matches(&rule.models, &out) {
            continue;
        }
        for (path, value) in &rule.params {
            let full = build_path(req.root, path);
            if full.is_empty() {
                continue;
            }
            for resolved in resolve_paths(&out, &full) {
                if json::get(&source, &resolved).exists() || defaulted.contains(&resolved) {
                    continue;
                }
                if sjson_set(&mut out, &resolved, value) {
                    defaulted.insert(resolved.clone());
                    mark(&resolved);
                }
            }
        }
    }
    for rule in &rules.default_raw {
        if !matches(&rule.models, &out) {
            continue;
        }
        for (path, value) in &rule.params {
            let full = build_path(req.root, path);
            if full.is_empty() {
                continue;
            }
            for resolved in resolve_paths(&out, &full) {
                if json::get(&source, &resolved).exists() || defaulted.contains(&resolved) {
                    continue;
                }
                let Some(raw) = raw_value(value) else { continue };
                if let Ok(next) = json::try_set_raw(&out, &resolved, &raw) {
                    out = next;
                    defaulted.insert(resolved.clone());
                    mark(&resolved);
                }
            }
        }
    }
    for rule in &rules.overrides {
        if !matches(&rule.models, &out) {
            continue;
        }
        for (path, value) in &rule.params {
            let full = build_path(req.root, path);
            if full.is_empty() {
                continue;
            }
            for resolved in resolve_paths(&out, &full) {
                if set_if_different(&mut out, &resolved, value) {
                    mark(&resolved);
                }
            }
        }
    }
    for rule in &rules.override_raw {
        if !matches(&rule.models, &out) {
            continue;
        }
        for (path, value) in &rule.params {
            let full = build_path(req.root, path);
            if full.is_empty() {
                continue;
            }
            let Some(raw) = raw_value(value) else { continue };
            for resolved in resolve_paths(&out, &full) {
                let current = json::get(&out, &resolved);
                let same = current.exists()
                    && current.indexes.as_ref().is_none_or(Vec::is_empty)
                    && current.raw() == raw.as_slice();
                if same {
                    mark(&resolved);
                } else if let Ok(next) = json::try_set_raw(&out, &resolved, &raw) {
                    out = next;
                    mark(&resolved);
                }
            }
        }
    }
    for rule in &rules.filter {
        if !matches(&rule.models, &out) {
            continue;
        }
        for path in &rule.params {
            let full = build_path(req.root, path);
            if full.is_empty() {
                continue;
            }
            for resolved in resolve_paths(&out, &full).into_iter().rev() {
                if let Ok(next) = json::try_delete(&out, &resolved) {
                    out = next;
                    mark(&resolved);
                }
            }
        }
    }
    out
}

/// `sjson.SetBytes(out, path, value)` for a configured value.
fn sjson_set(out: &mut Vec<u8>, path: &str, value: &Param) -> bool {
    match value {
        Param::Str(s) => json::try_set_str(out, path, s),
        other => match other.sjson_raw() {
            Some(raw) => json::try_set_raw(out, path, raw),
            None => return false,
        },
    }
    .map(|next| *out = next)
    .is_ok()
}

/// Go `setPayloadValueIfDifferentTracked`: an equal value counts as applied.
fn set_if_different(out: &mut Vec<u8>, path: &str, value: &Param) -> bool {
    let current = json::get(out, path);
    let same = match value {
        Param::Str(s) => current.kind == Kind::String && *current.str() == **s,
        Param::Bool(b) => current.kind == if *b { Kind::True } else { Kind::False },
        Param::Null => current.raw() == b"null",
        other => {
            let Some(expected) = other.sjson_raw() else {
                return false;
            };
            if current.indexes.as_ref().is_none_or(Vec::is_empty) && current.raw() == expected.as_slice() {
                return true;
            }
            return match json::try_set_raw(out, path, expected) {
                Ok(next) => {
                    *out = next;
                    true
                }
                Err(_) => false,
            };
        }
    };
    same || sjson_set(out, path, value)
}

/// Go `payloadRawValue`: strings are raw JSON as written; other values are marshaled.
fn raw_value(value: &Param) -> Option<Vec<u8>> {
    match value {
        Param::Null => None,
        Param::Str(s) => Some(s.as_bytes().to_vec()),
        other => other.marshal(),
    }
}

fn strip_image_generation(mode: ImageGeneration, request_path: &str) -> bool {
    match mode {
        ImageGeneration::All => true,
        ImageGeneration::Chat => !is_images_path(request_path),
        _ => false,
    }
}

fn is_images_path(path: &str) -> bool {
    let path = path.trim();
    !path.is_empty() && (path.ends_with("/images/generations") || path.ends_with("/images/edits"))
}

fn rules_match(models: &[ModelRule], req: &Request<'_>, payload: &[u8], candidates: &[String]) -> bool {
    candidates.iter().any(|model| {
        models.iter().any(|entry| {
            let name = entry.name.trim();
            if name.is_empty() {
                return false;
            }
            let protocol = entry.protocol.trim();
            if !protocol.is_empty() && !req.protocol.is_empty() && !protocol.eq_ignore_ascii_case(req.protocol) {
                return false;
            }
            from_protocol_matches(&entry.from_protocol, req.from_protocol)
                && headers_match(req.headers, &entry.headers)
                && match_pattern(name, model)
                && conditions_match(payload, req.root, entry)
        })
    })
}

fn conditions_match(payload: &[u8], root: &str, rule: &ModelRule) -> bool {
    let each = |conditions: &[Vec<(String, Param)>], want: bool| {
        conditions.iter().flatten().all(|(path, value)| {
            path.trim().is_empty() || path_matches_value(payload, &build_path(root, path), value) == want
        })
    };
    let exists = |paths: &[String], want: bool| {
        paths
            .iter()
            .all(|p| p.trim().is_empty() || path_exists(payload, &build_path(root, p)) == want)
    };
    each(&rule.matches, true)
        && each(&rule.not_matches, false)
        && exists(&rule.exist, true)
        && exists(&rule.not_exist, false)
}

fn path_matches_value(payload: &[u8], path: &str, value: &Param) -> bool {
    let Some(expected) = value.marshal().and_then(|m| GoValue::parse_f64(&m)) else {
        return false;
    };
    resolve_paths(payload, path).iter().any(|p| {
        let r = json::get(payload, p);
        r.exists() && GoValue::parse_f64(r.raw().trim_ascii()).is_some_and(|actual| actual == expected)
    })
}

fn path_exists(payload: &[u8], path: &str) -> bool {
    resolve_paths(payload, path).iter().any(|p| {
        let r = json::get(payload, p);
        r.exists() && r.kind != Kind::Null
    })
}

fn normalize_from_protocol(p: &str) -> String {
    match p.trim().to_lowercase().as_str() {
        "openai-response" | "openai-responses" | "response" => "responses".into(),
        other => other.to_owned(),
    }
}

fn from_protocol_matches(pattern: &str, from: &str) -> bool {
    let pattern = normalize_from_protocol(pattern);
    if pattern.is_empty() {
        return true;
    }
    let from = normalize_from_protocol(from);
    !from.is_empty() && pattern.eq_ignore_ascii_case(&from)
}

fn headers_match(headers: Option<&HeaderMap>, rules: &[(String, String)]) -> bool {
    rules.iter().all(|(key, pattern)| {
        let key = key.trim();
        if key.is_empty() {
            return true;
        }
        let Some(headers) = headers else { return false };
        headers
            .get_all(key)
            .iter()
            .any(|v| match_pattern(pattern, &String::from_utf8_lossy(v.as_bytes())))
    })
}

/// Go `payloadModelCandidates`: the upstream model, the client's suffix-free model and,
/// when it has a thinking suffix, the client's model as written (case-insensitive dedup).
fn model_candidates(model: &str, requested: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut add = |v: &str| {
        let v = v.trim();
        if !v.is_empty() && !out.iter().any(|c| c.eq_ignore_ascii_case(v)) {
            out.push(v.to_owned());
        }
    };
    add(model);
    if !requested.is_empty() {
        let parsed = crate::thinking::parse_suffix(requested);
        add(&parsed.model_name);
        if parsed.has_suffix {
            add(requested);
        }
    }
    out
}

/// Go `buildPayloadPath`.
fn build_path(root: &str, path: &str) -> String {
    let (r, p) = (root.trim(), path.trim());
    if r.is_empty() {
        return p.to_owned();
    }
    if p.is_empty() {
        return r.to_owned();
    }
    format!("{r}.{}", p.strip_prefix('.').unwrap_or(p))
}

/// Go `payloadRuleTargetsPath`.
fn targets_path(path: &str, tracked: &str) -> bool {
    !tracked.is_empty()
        && !path.is_empty()
        && (path == tracked || path.starts_with(&format!("{tracked}.")) || tracked.starts_with(&format!("{path}.")))
}

/// Go `resolvePayloadRulePaths`: `#(query)` and `#(query)#` segments become the matching
/// array indexes; other paths are used as written.
fn resolve_paths(payload: &[u8], path: &str) -> Vec<String> {
    let path = path.trim();
    if path.is_empty() {
        return Vec::new();
    }
    if !path.contains("#(") {
        return vec![path.to_owned()];
    }
    let mut paths = vec![String::new()];
    for part in split_path(path) {
        let Some((query, all)) = query_part(part) else {
            paths.iter_mut().for_each(|p| *p = join(p, part));
            continue;
        };
        let mut next = Vec::new();
        for base in &paths {
            let array = if base.is_empty() {
                json::parse(payload)
            } else {
                json::get(payload, base)
            };
            if !array.is_array() {
                continue;
            }
            for (index, item) in array.array().iter().enumerate() {
                if !query_matches(item, query) {
                    continue;
                }
                next.push(join(base, &index.to_string()));
                if !all {
                    break;
                }
            }
        }
        paths = next;
        if paths.is_empty() {
            return paths;
        }
    }
    paths
}

fn join(path: &str, part: &str) -> String {
    match (path.is_empty(), part.is_empty()) {
        (true, _) => part.to_owned(),
        (_, true) => path.to_owned(),
        _ => format!("{path}.{part}"),
    }
}

/// Splits on `.` outside parentheses and quotes; `\` escapes the next byte.
fn split_path(path: &str) -> Vec<&str> {
    let bytes = path.as_bytes();
    let (mut parts, mut start, mut depth, mut quote, mut escaped) = (Vec::new(), 0, 0usize, 0u8, false);
    for (i, &c) in bytes.iter().enumerate() {
        if escaped {
            escaped = false;
        } else if c == b'\\' {
            escaped = true;
        } else if quote != 0 {
            if c == quote {
                quote = 0;
            }
        } else if c == b'"' || c == b'\'' {
            quote = c;
        } else if c == b'(' {
            depth += 1;
        } else if c == b')' {
            depth = depth.saturating_sub(1);
        } else if c == b'.' && depth == 0 {
            parts.push(&path[start..i]);
            start = i + 1;
        }
    }
    parts.push(&path[start..]);
    parts
}

/// `#(query)` or `#(query)#` → the query and whether all matches are wanted.
fn query_part(part: &str) -> Option<(&str, bool)> {
    if !part.starts_with("#(") {
        return None;
    }
    let bytes = part.as_bytes();
    let (mut quote, mut escaped, mut depth) = (0u8, false, 1usize);
    let mut close = None;
    for (i, &c) in bytes.iter().enumerate().skip(2) {
        if escaped {
            escaped = false;
        } else if c == b'\\' {
            escaped = true;
        } else if quote != 0 {
            if c == quote {
                quote = 0;
            }
        } else if c == b'"' || c == b'\'' {
            quote = c;
        } else if c == b'(' {
            depth += 1;
        } else if c == b')' {
            depth -= 1;
            if depth == 0 {
                close = Some(i);
                break;
            }
        }
    }
    let close = close?;
    let suffix = &part[close + 1..];
    (suffix.is_empty() || suffix == "#").then(|| (part[2..close].trim(), suffix == "#"))
}

/// Splits on `operator` outside quotes.
fn split_logical<'q>(query: &'q str, operator: &str) -> Vec<&'q str> {
    let bytes = query.as_bytes();
    let (mut parts, mut start, mut quote, mut escaped) = (Vec::new(), 0, 0u8, false);
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if escaped {
            escaped = false;
        } else if c == b'\\' {
            escaped = true;
        } else if quote != 0 {
            if c == quote {
                quote = 0;
            }
        } else if c == b'"' || c == b'\'' {
            quote = c;
        } else if bytes[i..].starts_with(operator.as_bytes()) {
            parts.push(query[start..i].trim());
            i += operator.len();
            start = i;
            continue;
        }
        i += 1;
    }
    parts.push(query[start..].trim());
    parts
}

/// Go `payloadQueryMatches`: `||` of `&&` of gjson query terms, each evaluated on the
/// item wrapped in a one-element array.
fn query_matches(item: &Res<'_>, query: &str) -> bool {
    split_logical(query, "||").into_iter().any(|or| {
        let terms = split_logical(or, "&&");
        !terms.is_empty()
            && terms.into_iter().all(|term| {
                let term = term.trim();
                if term.is_empty() || item.raw().is_empty() {
                    return false;
                }
                let wrapped = format!("[{}]", String::from_utf8_lossy(item.raw()));
                // ponytail: gjson queries come from the `gjson` crate (tidwall's own Rust
                // port); `crate::json` does not implement `#(...)`.
                gjson::get(&wrapped, &format!("#({term})")).exists()
            })
    })
}

fn remove_tool_type(payload: &[u8], tools_path: &str, tool_type: &str) -> Vec<u8> {
    let tools = json::get(payload, tools_path);
    if !tools.is_array() {
        return payload.to_vec();
    }
    let items = tools.array();
    if !items.iter().any(|t| *t.get("type").str() == *tool_type) {
        return payload.to_vec();
    }
    let kept: Vec<&[u8]> = items
        .iter()
        .filter(|t| *t.get("type").str() != *tool_type)
        .map(Res::raw)
        .collect();
    let mut joined = vec![b'['];
    joined.extend(kept.join(&b','));
    joined.push(b']');
    json::try_set_raw(payload, tools_path, joined).unwrap_or_else(|_| payload.to_vec())
}

fn remove_tool_choice(payload: &[u8], path: &str, tool_type: &str) -> Vec<u8> {
    let choice = json::get(payload, path);
    let delete = match choice.kind {
        Kind::String => choice.str().trim().eq_ignore_ascii_case(tool_type),
        Kind::Json => {
            let kind = choice.get("type").str().trim().to_owned();
            kind.eq_ignore_ascii_case(tool_type)
                || (kind.eq_ignore_ascii_case("tool")
                    && choice.get("name").str().trim().eq_ignore_ascii_case(tool_type))
        }
        _ => false,
    };
    if delete {
        json::try_delete(payload, path).unwrap_or_else(|_| payload.to_vec())
    } else {
        payload.to_vec()
    }
}

/// Go `matchModelPattern`: `*` matches any run of bytes; whole-string match.
pub fn match_pattern(pattern: &str, model: &str) -> bool {
    let (p, s) = (pattern.trim().as_bytes(), model.trim().as_bytes());
    if p.is_empty() {
        return false;
    }
    if p == b"*" {
        return true;
    }
    let (mut pi, mut si, mut star, mut mark) = (0, 0, None, 0);
    while si < s.len() {
        if pi < p.len() && p[pi] == s[si] {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            mark = si;
            pi += 1;
        } else if let Some(star) = star {
            pi = star + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

/// Go `IsCodexUserAgent`: the first non-empty User-Agent contains `codex`.
pub fn is_codex_user_agent(headers: &HeaderMap) -> bool {
    headers
        .get_all(http::header::USER_AGENT)
        .iter()
        .map(|v| String::from_utf8_lossy(v.as_bytes()).trim().to_owned())
        .find(|v| !v.is_empty())
        .is_some_and(|ua| ua.to_lowercase().contains("codex"))
}

/// Codex client tools whose `number` parameters Codex deserializes as integers.
fn codex_integer_fields(tool: &str) -> &'static [&'static str] {
    let base = tool.trim();
    let base = base
        .strip_prefix("functions__")
        .or_else(|| base.strip_prefix("collab__"))
        .unwrap_or(base);
    match base {
        "exec_command" => &["yield_time_ms", "max_output_tokens", "timeout_ms"],
        "write_stdin" => &["session_id", "yield_time_ms", "max_output_tokens"],
        "sleep" => &["duration_ms"],
        "wait_agent" => &["timeout_ms"],
        "wait" => &["yield_time_ms", "max_tokens"],
        "tool_search" => &["limit"],
        "test_sync_tool" => &["sleep_before_ms", "sleep_after_ms", "participants", "timeout_ms"],
        _ => &[],
    }
}

/// Go `NormalizeCodexToolIntegerTypes`: for Codex clients, `number` (also inside a type
/// list) becomes `integer` on known Codex tool parameters, in OpenAI, Claude
/// `input_schema`, Gemini `function_declarations` and namespace tools, top-level and in
/// `input[].additional_tools`. Unchanged tools keep their bytes.
pub fn normalize_codex_tool_integer_types(body: &[u8], headers: &HeaderMap) -> Vec<u8> {
    let mut body = body.to_vec();
    if body.is_empty() || !is_codex_user_agent(headers) {
        return body;
    }
    let tools = json::get(&body, "tools");
    if let Some(updated) = normalize_tool_array(&tools)
        && let Ok(next) = json::try_set_raw(&body, "tools", updated)
    {
        body = next;
    }
    let input = json::get(&body, "input").into_owned();
    if input.is_array() {
        for (index, item) in input.array().iter().enumerate() {
            if *item.get("type").str() != *"additional_tools" {
                continue;
            }
            if let Some(updated) = normalize_tool_array(&item.get("tools"))
                && let Ok(next) = json::try_set_raw(&body, &format!("input.{index}.tools"), updated)
            {
                body = next;
            }
        }
    }
    body
}

/// Rewrites changed elements of a tool array, copying the bytes between them verbatim.
fn normalize_tool_array(tools: &Res<'_>) -> Option<Vec<u8>> {
    if !tools.is_array() {
        return None;
    }
    let raw = tools.raw();
    let mut out: Option<Vec<u8>> = None;
    let mut offset = 0;
    for tool in tools.array() {
        let Some(updated) = normalize_tool(&tool) else {
            continue;
        };
        let start = tool.index.saturating_sub(tools.index);
        let buf = out.get_or_insert_with(|| Vec::with_capacity(raw.len()));
        buf.extend_from_slice(&raw[offset..start]);
        buf.extend_from_slice(&updated);
        offset = start + tool.raw().len();
    }
    let mut out = out?;
    out.extend_from_slice(&raw[offset..]);
    Some(out)
}

fn normalize_tool(tool: &Res<'_>) -> Option<Vec<u8>> {
    let mut raw = tool.raw().to_vec();
    if *tool.get("type").str() == *"namespace" {
        let updated = normalize_tool_array(&tool.get("tools"))?;
        return json::try_set_raw(&raw, "tools", updated).ok();
    }
    for key in ["function_declarations", "functionDeclarations"] {
        let decls = tool.get(key);
        if decls.is_array() {
            let updated = normalize_tool_array(&decls)?;
            return json::try_set_raw(&raw, key, updated).ok();
        }
    }
    let mut name = tool.get("name").str().into_owned();
    let mut params = tool.get("parameters");
    let mut path = "parameters";
    if !params.is_object() {
        let function = tool.get("function.parameters");
        let schema = tool.get("input_schema");
        let json_schema = tool.get("parametersJsonSchema");
        if function.is_object() {
            path = "function.parameters";
            params = function;
            if name.is_empty() {
                name = tool.get("function.name").str().into_owned();
            }
        } else if schema.is_object() {
            path = "input_schema";
            params = schema;
        } else if json_schema.is_object() {
            path = "parametersJsonSchema";
            params = json_schema;
        } else {
            return None;
        }
    }
    let fields = codex_integer_fields(&name);
    if fields.is_empty() {
        return None;
    }
    let mut updated = params.raw().to_vec();
    let mut changed = false;
    let properties = json::get(&updated, "properties").into_owned();
    if !properties.is_object() {
        return None;
    }
    for field in fields {
        let kind = properties.get(&escape_key(field)).get("type");
        let path = format!("properties.{}.type", escape_key(field));
        if kind.kind == Kind::String && *kind.str() == *"number" {
            changed |= json::set_str(&mut updated, &path, "integer");
        } else if kind.is_array() {
            let mut types: Vec<String> = Vec::new();
            let mut has_number = false;
            for item in kind.array() {
                let mut t = item.str().into_owned();
                if t == "number" {
                    has_number = true;
                    t = "integer".into();
                }
                if !types.contains(&t) {
                    types.push(t);
                }
            }
            if has_number {
                changed |= json::set_raw(&mut updated, &path, json::quote_all(&types));
            }
        }
    }
    if !changed {
        return None;
    }
    raw = json::try_set_raw(&raw, path, updated).ok()?;
    Some(raw)
}

/// Go `escapeCodexSjsonKey`.
fn escape_key(key: &str) -> String {
    key.replace('\\', "\\\\").replace('.', "\\.").replace(':', "\\:")
}
