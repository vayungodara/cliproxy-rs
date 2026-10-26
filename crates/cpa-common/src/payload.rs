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

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use cpa_core::config::Config;
use http::HeaderMap;
use serde_yaml_ng::Value as Yaml;

use crate::gostr::GoStr;
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
            // A `time.Time`, which Go marshals as its RFC 3339 text, raw rules too.
            Yaml::Tagged(t) if t.tag == cpa_core::config::TIMESTAMP_TAG => Param::Json(go_value(&t.value)),
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
                // After Go's whole-rule check, which sees every parameter.
                .map(|mut r| {
                    r.params.retain(|(path, _)| !unported(path));
                    r
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
                    .filter(|path| !unported(path))
                    .collect(),
            })
            .collect();
        let image_generation = match doc.get("multimedia").and_then(|m| m.get("disable-image-generation")) {
            Some(Yaml::Bool(true)) => ImageGeneration::All,
            Some(Yaml::String(s)) => match s.trim().go_lower().as_str() {
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
        // A condition the port cannot read never holds, so its model entry never matches.
        .filter(|m| {
            let mut paths = (m.matches.iter().chain(&m.not_matches).flatten().map(|(p, _)| p))
                .chain(&m.exist)
                .chain(&m.not_exist);
            !paths.any(|p| unported(p))
        })
        .collect()
}

/// `part` split at each `|` that is not escaped or inside parentheses: gjson pipes.
fn pipe_parts(part: &str) -> Vec<&str> {
    let (mut out, mut start, mut depth, mut escaped) = (Vec::new(), 0, 0usize, false);
    for (i, c) in part.bytes().enumerate() {
        match c {
            _ if escaped => escaped = false,
            b'\\' => escaped = true,
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            b'|' if depth == 0 => {
                out.push(&part[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&part[start..]);
    out
}

/// A rule path outside what the port reads exactly like Go (docs/DIFFERENCES-FROM-GO.md);
/// such a rule is ignored. Allowed: dot-separated plain keys and indexes with `\\`
/// escapes and `*`/`?` wildcards, a bare `#` (count or `#.key`), `@this` without
/// arguments, `|` pipes between such parts, and whole `#(query)` or `#(query)#` parts
/// with a simple predicate (no `@`, `|` or nested query). Everything else (subselectors,
/// modifiers, static values, JSON lines, `#[...]` and queries after a pipe) is gjson
/// syntax `cpa_common::json` and the query resolver do not implement.
fn unported(path: &str) -> bool {
    let path = path.trim();
    path.is_empty()
        || !split_path(path).into_iter().all(|part| match query_part(part) {
            Some((query, _)) => simple_query(query),
            None => pipe_parts(part).into_iter().all(plain_read),
        })
}

/// A query predicate the resolver reads as gjson does: each `&&`/`||` term is one gjson
/// query that ends where the term ends, and its path is a plain read.
fn simple_query(query: &str) -> bool {
    !query.contains(['@', '|'])
        && !query.contains("#(")
        && !query.contains("#[")
        && split_logical(query, "||").into_iter().all(|or| {
            split_logical(or, "&&").into_iter().all(|term| {
                let term = format!("#({})", term.trim());
                match json::parse_query(term.as_bytes()) {
                    None => true,
                    Some((query, end)) => {
                        end == term.len()
                            && (query.path.is_empty()
                                || std::str::from_utf8(&query.path).is_ok_and(|path| !unported(path)))
                    }
                }
            })
        })
}

/// A part `cpa_common::json::get` reads as gjson does, between dots or pipes: gjson
/// reads `@` and `!` as syntax only at a part's start.
fn plain_read(part: &str) -> bool {
    if part == "#" || part == "@this" {
        return true;
    }
    let mut escaped = false;
    !part.is_empty()
        && !part.starts_with(['@', '!'])
        && part.bytes().all(|c| {
            if escaped {
                escaped = false;
                return true;
            }
            escaped = c == b'\\';
            !b"[]{}()#|\"'".contains(&c)
        })
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
            req.target_executor.trim().go_lower().as_str(),
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
    let needs_source = !(rules.default.is_empty() && rules.default_raw.is_empty());
    let entered = (needs_source && req.original.is_empty()).then(|| out.clone());
    let source: &[u8] = entered.as_deref().unwrap_or(req.original);
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
            let targets = resolve_targets(&out, &full);
            let in_source = in_source(source, &targets);
            let mut n = 0;
            each_target(&mut out, &targets, Mode::Any, |doc, path, resolved| {
                n += 1;
                if in_source[n - 1] || defaulted.contains(resolved) {
                    return None;
                }
                let next = sjson_set(doc, path, value)?;
                defaulted.insert(resolved.to_owned());
                mark(resolved);
                Some(next)
            });
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
            let targets = resolve_targets(&out, &full);
            let in_source = in_source(source, &targets);
            let mut n = 0;
            each_target(&mut out, &targets, Mode::Any, |doc, path, resolved| {
                n += 1;
                if in_source[n - 1] || defaulted.contains(resolved) {
                    return None;
                }
                let raw = raw_value(value)?;
                let next = set_raw_at(doc, path, &raw)?;
                defaulted.insert(resolved.to_owned());
                mark(resolved);
                Some(next)
            });
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
            let targets = resolve_targets(&out, &full);
            each_target(&mut out, &targets, Mode::Any, |doc, path, resolved| {
                let next = set_if_different(doc, path, value).ok()?;
                mark(resolved);
                next
            });
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
            let targets = resolve_targets(&out, &full);
            each_target(&mut out, &targets, Mode::Any, |doc, path, resolved| {
                let current = get_at(doc, path);
                let same = current.exists()
                    && current.indexes.as_ref().is_none_or(Vec::is_empty)
                    && current.raw() == raw.as_slice();
                if same {
                    mark(resolved);
                    return None;
                }
                let next = set_raw_at(doc, path, &raw)?;
                mark(resolved);
                Some(next)
            });
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
            let targets = resolve_targets(&out, &full);
            if let Some(next) = locals(&targets, Mode::Delete).and_then(|l| delete_elements(&out, &l)) {
                out = next;
                targets.iter().for_each(|t| mark(&t.path));
                continue;
            }
            // Go deletes in reverse, so earlier indexes still name the same elements. Edits
            // inside separate elements do not depend on their order.
            let mut targets = targets;
            if locals(&targets, Mode::Fields).is_none() {
                targets.reverse();
            }
            each_target(&mut out, &targets, Mode::Fields, |doc, path, resolved| {
                let next = json::try_delete(doc, path).ok()?;
                mark(resolved);
                Some(next)
            });
        }
    }
    out
}

/// `sjson.SetBytes(doc, path, value)` for a configured value: the new document, or
/// `None` when sjson returns an error.
fn sjson_set(doc: &[u8], path: &str, value: &Param) -> Option<Vec<u8>> {
    match value {
        Param::Str(s) if path.is_empty() => Some(json::stringify(s.as_bytes())),
        Param::Str(s) => json::try_set_str(doc, path, s).ok(),
        other => set_raw_at(doc, path, &other.sjson_raw()?),
    }
}

/// Go `setPayloadValueIfDifferentTracked`: an equal value counts as applied and leaves
/// the document as it is (`Ok(None)`); `Err` when sjson returns an error.
fn set_if_different(doc: &[u8], path: &str, value: &Param) -> Result<Option<Vec<u8>>, ()> {
    let current = get_at(doc, path);
    let same = match value {
        Param::Str(s) => current.kind == Kind::String && *current.str() == **s,
        Param::Bool(b) => current.kind == if *b { Kind::True } else { Kind::False },
        Param::Null => current.raw() == b"null",
        other => {
            let expected = other.sjson_raw().ok_or(())?;
            if current.indexes.as_ref().is_none_or(Vec::is_empty) && current.raw() == expected.as_slice() {
                return Ok(None);
            }
            return set_raw_at(doc, path, &expected).map(Some).ok_or(());
        }
    };
    if same {
        return Ok(None);
    }
    sjson_set(doc, path, value).map(Some).ok_or(())
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
            if !protocol.is_empty() && !req.protocol.is_empty() && !protocol.go_eq_fold(req.protocol) {
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
    any_target(payload, path, |r| {
        r.exists() && GoValue::parse_f64(r.raw().trim_ascii()).is_some_and(|actual| actual == expected)
    })
}

fn path_exists(payload: &[u8], path: &str) -> bool {
    any_target(payload, path, |r| r.exists() && r.kind != Kind::Null)
}

/// Whether any resolved path's value passes `test`, reading inside each target's element.
fn any_target(payload: &[u8], path: &str, test: impl Fn(&Res<'_>) -> bool) -> bool {
    let targets = resolve_targets(payload, path);
    match locals(&targets, Mode::Any) {
        Some(locals) => locals
            .iter()
            .any(|l| test(&get_at(&payload[l.elem.0..l.elem.1], &l.rest))),
        None => targets.iter().any(|t| test(&json::get(payload, &t.path))),
    }
}

/// `gjson.GetBytes(source, path).Exists()` for each target. Each source array a target
/// reads is parsed once, so the check stays linear in the source.
fn in_source(source: &[u8], targets: &[Target]) -> Vec<bool> {
    let Some(locals) = locals(targets, Mode::Any) else {
        return targets.iter().map(|t| json::get(source, &t.path).exists()).collect();
    };
    // ponytail: a nested query's targets each name a different inner array, so each is
    // located from the source's root (one get per outer element); caching the shared
    // ancestors would make nested defaults linear too.
    let mut bases: HashMap<&str, Base<'_>> = HashMap::new();
    targets
        .iter()
        .zip(locals)
        .map(|(t, l)| {
            let base = bases.entry(&l.base).or_insert_with(|| match route(source, &l.base) {
                Route::Missing => Base::Missing,
                Route::Ambiguous => Base::Ambiguous,
                Route::Found(bytes) => {
                    let value = json::parse(bytes);
                    match value.raw().first() {
                        Some(b'[') => Base::Array(value.array()),
                        // A numeric part reads an object key in gjson, which tries each
                        // member with that key in turn.
                        Some(b'{') => {
                            let mut keys: HashMap<Vec<u8>, Vec<Res<'_>>> = HashMap::new();
                            value.each(|k, v| {
                                keys.entry(k.bytes().into_owned()).or_default().push(v);
                                true
                            });
                            Base::Object(keys)
                        }
                        _ => Base::Missing,
                    }
                }
            });
            // A path through a scalar is missing, as in a whole-source get.
            let holds = |item: &Res<'_>| {
                l.rest.is_empty() || (matches!(item.raw().first(), Some(b'{' | b'[')) && item.get(&l.rest).exists())
            };
            match base {
                Base::Missing => false,
                Base::Array(items) => items.get(l.index).is_some_and(holds),
                Base::Object(keys) => keys
                    .get(l.index.to_string().as_bytes())
                    .is_some_and(|items| items.iter().any(holds)),
                // ponytail: O(N × source) when a key on the route is duplicated (Go has
                // the same shape); indexing the ordered duplicate candidates once would make
                // it linear.
                Base::Ambiguous => json::get(source, &t.path).exists(),
            }
        })
        .collect()
}

/// What the source holds at a target array's path, read once for all its targets.
enum Base<'s> {
    Missing,
    Array(Vec<Res<'s>>),
    Object(HashMap<Vec<u8>, Vec<Res<'s>>>),
    Ambiguous,
}

fn normalize_from_protocol(p: &str) -> String {
    match p.trim().go_lower().as_str() {
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
    !from.is_empty() && pattern.go_eq_fold(&from)
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
/// when it has a thinking suffix, the client's model as written, deduplicated by
/// `strings.ToLower` key (σ and ς stay distinct; `İ` and `i` collapse).
fn model_candidates(model: &str, requested: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    let mut add = |v: &str| {
        let v = v.trim();
        if !v.is_empty() && seen.insert(v.go_lower()) {
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

/// A resolved rule path. When the path went through a `#(...)` query, `elem` is the byte
/// range (in the body it was resolved against) of the array element the last query
/// selected, `rest` the path inside that element, `array` the range of the array holding
/// it, and `base` and `index` the array's path and the element's position in it.
struct Target {
    path: String,
    local: Option<Local>,
}

#[derive(Clone)]
struct Local {
    /// The route from the body's root to the element has one candidate at every level,
    /// so reading inside the element finds what gjson finds from the root.
    unique: bool,
    elem: (usize, usize),
    /// The element is an object or array; a path inside a scalar reads nothing.
    container: bool,
    rest: String,
    array: (usize, usize),
    base: String,
    index: usize,
}

/// A path part gjson and sjson read the same at any depth: a plain key or index. A
/// leading `:` is a key to gjson but forces an object key in sjson.
fn plain(part: &str) -> bool {
    !part.is_empty() && !part.bytes().any(|c| b"|@*?#\\!=<>%:".contains(&c))
}

/// Where a plain path leads in `doc`, when gjson cannot read it differently.
enum Route<'d> {
    /// No candidate at some level, so gjson finds nothing either.
    Missing,
    /// The value's bytes, a slice of `doc`.
    Found(&'d [u8]),
    /// A part that is not plain, a duplicate key (gjson tries each in turn), or a
    /// part an array does not read as a plain index: read the full path from the root.
    Ambiguous,
}

/// Walks `path` through `doc` one level at a time, visiting each container on the route
/// once. An empty path is the document.
fn route<'d>(doc: &'d [u8], path: &str) -> Route<'d> {
    let mut cur = doc;
    if path.is_empty() {
        return Route::Found(cur);
    }
    for part in path.split('.') {
        if !plain(part) {
            return Route::Ambiguous;
        }
        let value = json::parse(cur);
        let mut found = None;
        let mut ambiguous = false;
        match value.raw().first() {
            Some(b'{') => value.each(|k, v| {
                if *k.bytes() == *part.as_bytes() {
                    ambiguous |= found.is_some();
                    found = Some(v);
                }
                !ambiguous
            }),
            Some(b'[') => {
                if !part.bytes().all(|c| c.is_ascii_digit()) || (part.len() > 1 && part.starts_with('0')) {
                    return Route::Ambiguous;
                }
                // gjson's parseUint wraps past u64::MAX; the full-path read does too.
                let Ok(i) = part.parse::<usize>() else {
                    return Route::Ambiguous;
                };
                found = value.array().into_iter().nth(i);
            }
            _ => return Route::Missing,
        }
        if ambiguous {
            return Route::Ambiguous;
        }
        let Some(v) = found else { return Route::Missing };
        // Indexes count from the start of `cur`.
        match cur.get(v.index..v.index + v.raw().len()) {
            Some(bytes) if bytes == v.raw() => cur = bytes,
            _ => return Route::Ambiguous,
        }
    }
    Route::Found(cur)
}

/// Go `resolvePayloadRulePaths`: `#(query)` and `#(query)#` segments become the matching
/// array indexes; other paths are used as written. Arrays are read inside the element
/// the previous query selected, so a nested query reads each element once.
fn resolve_targets(payload: &[u8], path: &str) -> Vec<Target> {
    let path = path.trim();
    if path.is_empty() {
        return Vec::new();
    }
    if !path.contains("#(") {
        return vec![Target {
            path: path.to_owned(),
            local: None,
        }];
    }
    let mut targets = vec![Target {
        path: String::new(),
        local: None,
    }];
    for part in split_path(path) {
        let Some((query, all)) = query_part(part) else {
            for t in &mut targets {
                t.path = join(&t.path, part);
                if let Some(local) = &mut t.local {
                    local.rest = join(&local.rest, part);
                }
            }
            continue;
        };
        let mut next = Vec::new();
        for t in &targets {
            // Read the array inside the element the previous query selected only when
            // gjson's search from the root could not go anywhere else: every part is
            // plain and every level on the route has one candidate. Otherwise (a
            // wildcard or duplicate key before it) gjson moves on to the next candidate
            // when the rest is missing, so read the full path from the root.
            let routed = match &t.local {
                Some(l) if l.unique && (l.rest.is_empty() || l.container) => {
                    route(&payload[l.elem.0..l.elem.1], &l.rest)
                }
                Some(_) => Route::Ambiguous,
                None => route(payload, &t.path),
            };
            let (array, offset, unique) = match routed {
                Route::Missing => continue,
                Route::Found(bytes) => {
                    let offset = bytes.as_ptr() as usize - payload.as_ptr() as usize;
                    (json::parse(bytes), offset, true)
                }
                Route::Ambiguous if t.path.is_empty() => (json::parse(payload), 0, false),
                Route::Ambiguous => (json::get(payload, &t.path), 0, false),
            };
            if !array.is_array() {
                continue;
            }
            let located = matches!(array.raw, std::borrow::Cow::Borrowed(_))
                && payload.get(offset + array.index..offset + array.index + array.raw().len()) == Some(array.raw());
            for (index, item) in array.array().iter().enumerate() {
                if !query_matches(item, query) {
                    continue;
                }
                let start = offset + item.index;
                let path = join(&t.path, &index.to_string());
                let local =
                    (located && payload.get(start..start + item.raw().len()) == Some(item.raw())).then(|| Local {
                        unique,
                        elem: (start, start + item.raw().len()),
                        container: matches!(item.raw().first(), Some(b'{' | b'[')),
                        rest: String::new(),
                        array: (offset + array.index, offset + array.index + array.raw().len()),
                        base: t.path.clone(),
                        index,
                    });
                next.push(Target { path, local });
                if !all {
                    break;
                }
            }
        }
        targets = next;
        if targets.is_empty() {
            return targets;
        }
    }
    targets
}

/// Which element-local targets a batch takes.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// Whole elements only (filters that delete elements).
    Delete,
    /// Paths inside elements only (filters that delete fields).
    Fields,
    /// Whole elements and paths inside elements (setters and reads).
    Any,
}

/// Element-local targets, in body order and disjoint, when the edit of each touches only
/// its own element; `None` when the edits must run one by one on the whole body.
fn locals(targets: &[Target], mode: Mode) -> Option<Vec<&Local>> {
    if targets.len() < 2 {
        return None;
    }
    let mut out: Vec<&Local> = Vec::with_capacity(targets.len());
    for t in targets {
        let l = t.local.as_ref()?;
        let whole = l.rest.is_empty();
        let fits = match mode {
            Mode::Delete => whole,
            Mode::Fields => !whole,
            Mode::Any => true,
        };
        // The whole resolved path must be plain: sjson refuses a complex path (a
        // wildcard before the query included) on the whole body. A path inside a
        // scalar reads nothing in gjson, while the scalar's own bytes may parse.
        if !fits || !l.unique || !(whole || l.container) || !t.path.split('.').all(plain) {
            return None;
        }
        // sjson's delete reads an unforced `-1` key as the last element (`Get("#")`),
        // even in an object, so a delete under one may land outside this element.
        if mode != Mode::Any && l.base.split('.').any(|p| p == "-1") {
            return None;
        }
        if out.last().is_some_and(|prev| prev.elem.1 > l.elem.0) {
            return None;
        }
        out.push(l);
    }
    Some(out)
}

/// `gjson.GetBytes(doc, path)`, where an empty path (a whole element) is the document.
fn get_at<'d>(doc: &'d [u8], path: &str) -> Res<'d> {
    if path.is_empty() {
        json::parse(doc)
    } else {
        json::get(doc, path)
    }
}

/// `sjson.SetRawBytes(doc, path, raw)`; an existing element's whole path replaces it.
fn set_raw_at(doc: &[u8], path: &str, raw: &[u8]) -> Option<Vec<u8>> {
    if path.is_empty() {
        return Some(raw.to_vec());
    }
    json::try_set_raw(doc, path, raw).ok()
}

/// Runs `op(doc, path, full_path)` for each target in order, as Go's loop over the
/// resolved paths does. `op` returns the edited document. A target inside an array
/// element edits only that element (sjson recurses into the element's bytes), so `op`
/// runs on the element and the edited elements are joined in one pass over the body.
fn each_target(
    out: &mut Vec<u8>,
    targets: &[Target],
    mode: Mode,
    mut op: impl FnMut(&[u8], &str, &str) -> Option<Vec<u8>>,
) {
    let Some(locals) = locals(targets, mode) else {
        // ponytail: O(N × body) when a key on the rule's route is duplicated (Go has the
        // same shape); indexing the ordered duplicate candidates once, separately for
        // gjson reads and sjson edits, would make it linear.
        for t in targets {
            if let Some(next) = op(out, &t.path, &t.path) {
                *out = next;
            }
        }
        return;
    };
    let mut next = Vec::with_capacity(out.len());
    let mut at = 0;
    for (t, l) in targets.iter().zip(locals) {
        let elem = &out[l.elem.0..l.elem.1];
        next.extend_from_slice(&out[at..l.elem.0]);
        match op(elem, &l.rest, &t.path) {
            Some(edited) => next.extend_from_slice(&edited),
            None => next.extend_from_slice(elem),
        }
        at = l.elem.1;
    }
    next.extend_from_slice(&out[at..]);
    *out = next;
}

/// sjson's `DeleteBytes` of whole array elements, in Go's reverse order, in one pass.
/// Deleting element `k > 0` removes the comma before it, the whitespace after that
/// comma, and the element. Deleting element 0 (last, after the others) removes the
/// element and, when an element is left after it, everything up to and including the
/// next comma. `None` when the array's gaps are not plain whitespace and commas.
fn delete_elements(out: &[u8], locals: &[&Local]) -> Option<Vec<u8>> {
    let mut next = Vec::with_capacity(out.len());
    let mut at = 0;
    let mut i = 0;
    while i < locals.len() {
        let array = locals[i].array;
        let group = locals[i..].iter().take_while(|l| l.array == array).count();
        let deleted: HashSet<usize> = locals[i..i + group].iter().map(|l| l.index).collect();
        next.extend_from_slice(&out[at..array.0]);
        next.extend_from_slice(&delete_in_array(&out[array.0..array.1], &deleted)?);
        at = array.1;
        i += group;
    }
    next.extend_from_slice(&out[at..]);
    Some(next)
}

fn delete_in_array(raw: &[u8], deleted: &HashSet<usize>) -> Option<Vec<u8>> {
    let items = json::parse(raw).array();
    let spans: Vec<(usize, usize)> = items.iter().map(|it| (it.index, it.index + it.raw().len())).collect();
    if spans.is_empty() || raw.first() != Some(&b'[') || raw.last() != Some(&b']') {
        return None;
    }
    // gap(i): the bytes before element i (after '[' for i = 0), split at its comma.
    let gap = |i: usize| -> Option<(&[u8], &[u8])> {
        let from = if i == 0 { 1 } else { spans[i - 1].1 };
        let g = raw.get(from..spans[i].0)?;
        let commas = g.iter().filter(|&&c| c == b',').count();
        if g.iter().any(|&c| c > b' ' && c != b',') || commas != usize::from(i > 0) {
            return None;
        }
        Some(match g.iter().position(|&c| c == b',') {
            Some(c) => (&g[..c], &g[c + 1..]),
            None => (g, &[][..]),
        })
    };
    let tail = raw.get(spans[spans.len() - 1].1..raw.len() - 1)?;
    if tail.iter().any(|&c| c > b' ') {
        return None;
    }
    let mut out = Vec::with_capacity(raw.len());
    out.push(b'[');
    out.extend_from_slice(gap(0)?.0);
    let first_kept = (0..spans.len()).find(|i| !deleted.contains(i));
    let start = match first_kept {
        Some(j) if j > 0 && deleted.contains(&0) => {
            // Element 0's deletion ran past the leftover whitespace to j's comma.
            for k in 1..j {
                gap(k)?;
            }
            out.extend_from_slice(gap(j)?.1);
            j
        }
        _ => 0,
    };
    for i in start..spans.len() {
        let (before, after) = gap(i)?;
        if i > start {
            out.extend_from_slice(before);
        }
        if deleted.contains(&i) {
            continue;
        }
        if i > start {
            out.push(b',');
            out.extend_from_slice(after);
        }
        out.extend_from_slice(&raw[spans[i].0..spans[i].1]);
    }
    out.extend_from_slice(tail);
    out.push(b']');
    Some(out)
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
                // gjson.GetBytes("[item]", "#("+term+")").Exists(): the one element is
                // the item. An unbalanced term leaves gjson an empty query, which is
                // `Query::default()` (tests/query.rs checks both against Go).
                json::parse_query(format!("#({term})").as_bytes())
                    .map(|(query, _)| query)
                    .unwrap_or_default()
                    .matches_raw(item.raw())
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
        Kind::String => choice.str().trim().go_eq_fold(tool_type),
        Kind::Json => {
            let kind = choice.get("type").str().trim().to_owned();
            kind.go_eq_fold(tool_type)
                || (kind.go_eq_fold("tool") && choice.get("name").str().trim().go_eq_fold(tool_type))
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
        .is_some_and(|ua| ua.go_lower().contains("codex"))
}

/// `codexClientToolIntegerFields` and `matchCodexTargetTool`: the Codex client tools
/// whose `number` parameters Codex deserializes as integers. Fields are paths under
/// `parameters.properties`, not recursive names. A namespaced tool arrives as
/// `namespace__name`.
fn codex_integer_fields(tool: &str) -> &'static [&'static str] {
    let base = tool.trim();
    let base = base
        .strip_prefix("functions__")
        .or_else(|| base.strip_prefix("collab__"))
        .unwrap_or(base);
    let base = match base {
        "multi_agent_v1__wait_agent" | "collaboration__wait_agent" => "wait_agent",
        "collaboration__get_channels"
        | "collaboration__list_threads"
        | "collaboration__search_posts"
        | "collaboration__read_thread"
        | "collaboration__read_post" => &base["collaboration__".len()..],
        other => other,
    };
    match base {
        "exec_command" => &["yield_time_ms", "max_output_tokens", "timeout_ms"],
        "write_stdin" => &["session_id", "yield_time_ms", "max_output_tokens"],
        "sleep" => &["duration_ms"],
        "wait_agent" => &["timeout_ms"],
        "wait" => &["yield_time_ms", "max_tokens"],
        "tool_search" => &["limit"],
        "test_sync_tool" => &[
            "sleep_before_ms",
            "sleep_after_ms",
            "participants",
            "timeout_ms",
            "barrier.properties.participants",
            "barrier.properties.timeout_ms",
        ],
        "create_goal" => &["token_budget"],
        "get_channels" | "history__list_windows" | "history__search_contents" => &["limit"],
        "list_threads" | "search_posts" | "read_thread" => &["limit", "max_chars_per_post"],
        "read_post" | "history__read_item" => &["offset_chars", "limit_chars"],
        "memories__list" | "notes__list_files_by_prefix" => &["max_results"],
        "memories__read" => &["line_offset", "max_lines"],
        "memories__search" => &["context_lines", "max_results"],
        "history__list_items" => &["limit", "max_chars_per_item"],
        // Codex declares signed line numbers in the first nullable union branch.
        "notes__read_file" => &["start_line", "stop_line", "start_line.anyOf.0", "stop_line.anyOf.0"],
        "notes__search_contents" => &["max_matches_per_file", "max_files"],
        "image_gen__imagegen" => &["num_last_images_to_include"],
        "web__run" => &[
            "search_query.items.properties.recency",
            "image_query.items.properties.recency",
            "open.items.properties.lineno",
            "click.items.properties.id",
            "screenshot.items.properties.pageno",
            "weather.items.properties.duration",
            "sports.items.properties.num_games",
        ],
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
    if let Some(updated) = normalize_tool_array(&tools, "")
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
            if let Some(updated) = normalize_tool_array(&item.get("tools"), "")
                && let Ok(next) = json::try_set_raw(&body, &format!("input.{index}.tools"), updated)
            {
                body = next;
            }
        }
    }
    body
}

/// Rewrites changed elements of a tool array, copying the bytes between them verbatim.
fn normalize_tool_array(tools: &Res<'_>, namespace: &str) -> Option<Vec<u8>> {
    if !tools.is_array() {
        return None;
    }
    let raw = tools.raw();
    let mut out: Option<Vec<u8>> = None;
    let mut offset = 0;
    for tool in tools.array() {
        let Some(updated) = normalize_tool(&tool, namespace) else {
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

fn normalize_tool(tool: &Res<'_>, namespace: &str) -> Option<Vec<u8>> {
    let mut raw = tool.raw().to_vec();
    if *tool.get("type").str() == *"namespace" {
        // One namespace level; its name qualifies the nested tools.
        let name = tool.get("name").str().into_owned();
        if !namespace.is_empty() || name.is_empty() {
            return None;
        }
        let updated = normalize_tool_array(&tool.get("tools"), &name)?;
        return json::try_set_raw(&raw, "tools", updated).ok();
    }
    for key in ["function_declarations", "functionDeclarations"] {
        let decls = tool.get(key);
        if decls.is_array() {
            let updated = normalize_tool_array(&decls, namespace)?;
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
    if !namespace.is_empty() {
        name = format!("{namespace}__{name}");
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
        let kind = properties.get(field).get("type");
        let path = format!("properties.{field}.type");
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
