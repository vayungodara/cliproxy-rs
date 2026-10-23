//! Pinned static model catalog: `internal/registry/models/models.json` from CLIProxyAPI
//! v8.0.35, byte for byte.
//!
//! This is the static layer only. Go overlays a dynamic registry (per-credential
//! registrations, config model definitions, remote catalog refresh) and consults it before
//! these definitions (internal/registry/model_registry.go); that overlay belongs to the
//! server runtime.

use std::sync::LazyLock;

pub mod devin;
pub mod dynamic;

use serde::Deserialize;
use serde_json::{Map, Value};

/// The models.json built into the binary.
pub static MODELS_JSON: &str = include_str!("registry/models.json");

/// Channels searched by [`Catalog::lookup`], in Go's `LookupStaticModelInfo` order
/// (internal/registry/model_definitions.go). `gemini-cli`, `codex-free`, `codex-team` and
/// `codex-plus` are not searched; Go reaches them only through per-channel getters.
/// After the `devin` section Go searches `staticDevinModels` ([`devin::static_models`]),
/// not the `devin_models.json` catalog.
const LOOKUP_ORDER: [&str; 10] = [
    "claude",
    "gemini",
    "vertex",
    "aistudio",
    "codex-pro",
    "kimi",
    "antigravity",
    "xai",
    "devin",
    "meta",
];

/// Thinking budget and level support (`ThinkingSupport` in model_registry.go).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ThinkingSupport {
    pub min: i64,
    pub max: i64,
    /// `zero_allowed` in models.json, `zero-allowed` in config YAML (Go's yaml tag).
    #[serde(alias = "zero-allowed")]
    pub zero_allowed: bool,
    #[serde(alias = "dynamic-allowed")]
    pub dynamic_allowed: bool,
    /// The Claude `thinking.type` that turns thinking off (`disable-type` in config
    /// YAML). Empty means `disabled`; claude-sonnet-5-5 needs `between_tools`.
    #[serde(alias = "disable-type")]
    pub disable_type: String,
    /// Discrete effort levels; empty means budget-based thinking.
    pub levels: Vec<String>,
}

/// One model definition. Typed fields cover what routing and translation read; `raw` keeps
/// the full published object in file key order for handlers that render Go's shapes.
#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub id: String,
    /// Go's `type` field: the provider family that serves the model.
    pub kind: String,
    pub thinking: Option<ThinkingSupport>,
    pub raw: Map<String, Value>,
}

impl ModelInfo {
    /// Whether the resolved model is a config entry marked `is-compat` (Go
    /// `ModelInfo.IsCompat`). Only registry-resolved models carry it.
    pub fn is_compat(&self) -> bool {
        self.raw.get("is_compat").and_then(Value::as_bool).unwrap_or(false)
    }

    pub fn from_raw(raw: Map<String, Value>) -> Result<Self, serde_json::Error> {
        #[derive(Deserialize)]
        struct Typed {
            id: String,
            #[serde(default, rename = "type")]
            kind: String,
            #[serde(default)]
            thinking: Option<ThinkingSupport>,
        }
        let typed: Typed = serde_json::from_value(Value::Object(go_integers(&raw)))?;
        Ok(Self {
            id: typed.id,
            kind: typed.kind,
            thinking: typed.thinking,
            raw,
        })
    }
}

pub struct Catalog {
    /// Channels in file order.
    channels: Vec<(String, Vec<ModelInfo>)>,
}

impl Catalog {
    pub fn parse(json: &str) -> Result<Self, serde_json::Error> {
        Self::from_root(serde_json::from_str(json)?)
    }

    fn from_root(root: Map<String, Value>) -> Result<Self, serde_json::Error> {
        let mut channels = Vec::with_capacity(root.len());
        for (name, models) in root {
            if models.is_null() {
                continue;
            }
            let models: Vec<Map<String, Value>> = serde_json::from_value(models)?;
            let models = models.into_iter().map(ModelInfo::from_raw).collect::<Result<_, _>>()?;
            channels.push((name, models));
        }
        Ok(Self { channels })
    }

    /// Definitions for one channel (`claude`, `codex-plus`, ...), empty when unknown.
    pub fn channel(&self, name: &str) -> &[ModelInfo] {
        self.channels
            .iter()
            .find(|(n, _)| n == name)
            .map_or(&[], |(_, models)| models.as_slice())
    }

    /// First definition with this exact ID across channels, in Go's search order. The same
    /// ID can mean different things per channel (`claude-sonnet-4-6` is also an
    /// Antigravity model), so callers that know the provider should use [`Self::channel`].
    pub fn lookup(&self, id: &str) -> Option<&ModelInfo> {
        if id.is_empty() {
            return None;
        }
        LOOKUP_ORDER
            .iter()
            .flat_map(|channel| {
                let statics: &[ModelInfo] = if *channel == "devin" {
                    devin::static_models()
                } else {
                    &[]
                };
                self.channel(channel).iter().chain(statics)
            })
            .find(|m| m.id == id)
            .or_else(|| {
                let claude = self.channel("claude");
                claude_family_ids(id)
                    .into_iter()
                    .find_map(|family| claude.iter().find(|m| m.id == family))
            })
    }
}

/// How Go's `ModelInfo` and `ThinkingSupport` fields decode.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Text,
    Int,
    Flag,
    List,
    Thinking,
    Config,
    Native,
}
use Kind::*;
/// `ModelInfo`'s JSON fields: name, kind, `omitempty`.
const MODEL: [(&str, Kind, bool); 22] = [
    ("id", Text, false),
    ("object", Text, false),
    ("created", Int, false),
    ("owned_by", Text, false),
    ("type", Text, false),
    ("display_name", Text, true),
    ("name", Text, true),
    ("version", Text, true),
    ("description", Text, true),
    ("inputTokenLimit", Int, true),
    ("outputTokenLimit", Int, true),
    ("supportedGenerationMethods", List, true),
    ("context_length", Int, true),
    ("max_completion_tokens", Int, true),
    ("supported_parameters", List, true),
    ("supportedInputModalities", List, true),
    ("supportedOutputModalities", List, true),
    ("supports_web_search", Flag, true),
    ("thinking", Thinking, true),
    ("config", Config, true),
    // Decoded (and type-checked) only.
    ("native_capabilities", Native, true),
    ("support_configuration_update", Flag, true),
];
/// `ThinkingSupport`'s JSON fields.
const THINKING: [(&str, Kind, bool); 6] = [
    ("min", Int, true),
    ("max", Int, true),
    ("zero_allowed", Flag, true),
    ("dynamic_allowed", Flag, true),
    ("disable_type", Text, true),
    ("levels", List, true),
];

/// `raw` for typed deserialisation, with `-0` as 0 in its integer fields and `thinking`'s:
/// Go reads that integer literal as 0, serde as the float `-0.0`. `raw` itself keeps it.
/// A refresh refuses `-0.0` itself by its text (`check_catalog_members`).
pub(crate) fn go_integers(raw: &Map<String, Value>) -> Map<String, Value> {
    fn zero(obj: &mut Map<String, Value>, fields: &[(&'static str, Kind, bool)]) {
        for (name, kind, _) in fields {
            let negative_zero = |v: &Value| v.as_f64().is_some_and(|f| f == 0.0 && f.is_sign_negative());
            if *kind == Int && obj.get(*name).is_some_and(negative_zero) {
                obj.insert((*name).into(), Value::from(0));
            }
        }
    }
    let mut out = raw.clone();
    zero(&mut out, &MODEL);
    if let Some(Value::Object(thinking)) = out.get_mut("thinking") {
        zero(thinking, &THINKING);
    }
    out
}

/// A catalog model as Go's typed decode reads it into `registry.ModelInfo` and
/// `json.Marshal` writes it back (model_registry.go): members match fields exactly, else
/// case-insensitively; null leaves a field at its zero value (and is `""` inside string
/// lists and header maps); integer fields take integer literals only; unknown members
/// are skipped; `json:"-"` fields are left out of the result. `Err` is the decode error
/// that makes Go refuse the whole catalog. `raw` itself is left as it is.
pub fn go_model_json(raw: &Map<String, Value>) -> Result<Value, String> {
    use crate::gostr::GoStr;
    fn field(fields: &[(&'static str, Kind, bool)], key: &str) -> Option<usize> {
        fields
            .iter()
            .position(|f| f.0 == key)
            .or_else(|| fields.iter().position(|f| f.0.go_eq_fold(key)))
    }
    fn err(key: &str, v: &Value) -> String {
        format!("cannot unmarshal {v} into field {key}")
    }
    fn string(key: &str, v: &Value) -> Result<String, String> {
        match v {
            Value::Null => Ok(String::new()),
            Value::String(s) => Ok(s.clone()),
            _ => Err(err(key, v)),
        }
    }
    /// Decodes `obj` into `out` (Go's struct, held as the fields set so far).
    fn decode(
        fields: &[(&'static str, Kind, bool)],
        obj: &Map<String, Value>,
        out: &mut [Option<Value>],
    ) -> Result<(), String> {
        for (key, v) in obj {
            let Some(i) = field(fields, key) else { continue };
            let (name, kind, _) = fields[i];
            if v.is_null() {
                // Null clears a pointer, slice or map; other fields keep their value.
                if matches!(kind, List | Thinking | Config | Native) {
                    out[i] = None;
                }
                continue;
            }
            out[i] = Some(match kind {
                Text => Value::String(string(name, v)?),
                Int => match v {
                    Value::Number(n) if n.is_i64() => Value::from(n.as_i64().unwrap_or_default()),
                    // `-0`, an integer literal, which serde reads as a float. A refresh
                    // refuses `-0.0` and other fractions by their text before this runs
                    // (`check_catalog_members`).
                    Value::Number(n) if n.as_f64().is_some_and(|f| f == 0.0 && f.is_sign_negative()) => Value::from(0),
                    _ => return Err(err(name, v)),
                },
                Flag => match v {
                    Value::Bool(b) => Value::Bool(*b),
                    _ => return Err(err(name, v)),
                },
                List => match v {
                    Value::Array(items) => Value::Array(
                        items
                            .iter()
                            .map(|item| string(name, item).map(Value::String))
                            .collect::<Result<_, _>>()?,
                    ),
                    _ => return Err(err(name, v)),
                },
                Thinking => {
                    let Value::Object(t) = v else { return Err(err(name, v)) };
                    // Decoding into a set pointer updates the struct it points to.
                    let mut inner = match out[i].take() {
                        Some(Value::Array(prev)) => prev.into_iter().map(|v| (!v.is_null()).then_some(v)).collect(),
                        _ => vec![None; THINKING.len()],
                    };
                    decode(&THINKING, t, &mut inner)?;
                    Value::Array(inner.into_iter().map(|v| v.unwrap_or(Value::Null)).collect())
                }
                Config => {
                    let Value::Object(c) = v else { return Err(err(name, v)) };
                    // The header map; nil and empty both marshal as absent (`omitempty`).
                    let mut headers = match out[i].take() {
                        Some(Value::Object(prev)) => prev,
                        _ => Map::new(),
                    };
                    for (k, h) in c {
                        if !k.as_str().go_eq_fold("override_header") {
                            continue;
                        }
                        match h {
                            Value::Null => headers.clear(),
                            // A map decode adds to the map already there.
                            Value::Object(m) => {
                                for (hk, hv) in m {
                                    headers.insert(hk.clone(), Value::String(string("override_header", hv)?));
                                }
                            }
                            _ => return Err(err("override_header", h)),
                        }
                    }
                    Value::Object(headers)
                }
                Native => {
                    let Value::Object(n) = v else { return Err(err(name, v)) };
                    for (k, b) in n {
                        let known = ["web_search", "tool_search"].iter().any(|f| *f == k || f.go_eq_fold(k));
                        if known && !matches!(b, Value::Null | Value::Bool(_)) {
                            return Err(err(k, b));
                        }
                    }
                    Value::Null
                }
            });
        }
        Ok(())
    }
    fn encode(fields: &[(&'static str, Kind, bool)], vals: Vec<Option<Value>>) -> Map<String, Value> {
        let mut out = Map::new();
        for ((name, kind, omit), v) in fields.iter().zip(vals) {
            let v = match (kind, v) {
                (Native, _) => continue,
                (_, None) if matches!(kind, List | Thinking | Config) => continue,
                (Text, None) => Value::from(""),
                (Int, None) => Value::from(0),
                (Flag, None) => Value::from(false),
                (Thinking, Some(Value::Array(t))) => {
                    let t = t.into_iter().map(|v| (!v.is_null()).then_some(v)).collect();
                    out.insert((*name).into(), Value::Object(encode(&THINKING, t)));
                    continue;
                }
                (Config, Some(Value::Object(h))) => {
                    let mut config = Map::new();
                    if !h.is_empty() {
                        // A Go map: keys sorted.
                        let sorted: std::collections::BTreeMap<String, Value> = h.into_iter().collect();
                        config.insert("override_header".into(), Value::Object(sorted.into_iter().collect()));
                    }
                    out.insert((*name).into(), Value::Object(config));
                    continue;
                }
                (_, Some(v)) => v,
                (_, None) => continue,
            };
            let empty = match &v {
                Value::String(s) => s.is_empty(),
                Value::Number(n) => n.as_i64() == Some(0),
                Value::Bool(b) => !b,
                Value::Array(a) => a.is_empty(),
                _ => false,
            };
            if !(*omit && empty) {
                out.insert((*name).into(), v);
            }
        }
        out
    }
    let mut vals = vec![None; MODEL.len()];
    decode(&MODEL, raw, &mut vals)?;
    // `support_configuration_update` is `json:"-"`.
    vals.truncate(MODEL.len() - 1);
    Ok(Value::Object(encode(&MODEL[..MODEL.len() - 1], vals)))
}

/// `staticModelsJSON`'s sections.
const SECTIONS: [&str; 13] = [
    "claude",
    "gemini",
    "vertex",
    "aistudio",
    "codex-free",
    "codex-team",
    "codex-plus",
    "codex-pro",
    "kimi",
    "antigravity",
    "xai",
    "devin",
    "meta",
];

/// An object's members in order, repeats included, each value as its JSON text.
struct Members<'a>(Vec<(String, &'a serde_json::value::RawValue)>);

impl<'de> Deserialize<'de> for Members<'de> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Visit;
        impl<'de> serde::de::Visitor<'de> for Visit {
            type Value = Members<'de>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an object")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut out = Vec::new();
                while let Some(member) = map.next_entry()? {
                    out.push(member);
                }
                Ok(Members(out))
            }
        }
        d.deserialize_map(Visit)
    }
}

/// Where Go's typed decode of a catalog would differ from the decoded `Map` the refresh
/// keeps (docs/DIFFERENCES-FROM-GO.md, remote model catalogs): a known section or field
/// named in another case (Go matches it case-insensitively) or repeated (Go decodes each
/// occurrence in turn into the same field), and an integer field holding anything but an
/// integer literal (serde reads `-0.0` and `1e-400` as zero).
fn check_catalog_members(json: &str) -> Result<(), String> {
    use crate::gostr::GoStr;
    use serde_json::value::RawValue;
    fn known<'a>(names: &[&'static str], obj: &Members<'a>, at: &str) -> Result<Vec<(usize, &'a RawValue)>, String> {
        let mut seen = vec![false; names.len()];
        let mut out = Vec::new();
        for (key, v) in &obj.0 {
            let Some(i) = names.iter().position(|n| n.go_eq_fold(key)) else {
                continue;
            };
            if key != names[i] {
                return Err(format!("{at}: member {key:?} is {:?} in another case", names[i]));
            }
            if std::mem::replace(&mut seen[i], true) {
                return Err(format!("{at}: member {key:?} is repeated"));
            }
            out.push((i, *v));
        }
        Ok(out)
    }
    fn object(v: &RawValue) -> Option<Result<Members<'_>, String>> {
        v.get()
            .starts_with('{')
            .then(|| serde_json::from_str(v.get()).map_err(|e| e.to_string()))
    }
    fn integer(v: &RawValue) -> bool {
        let text = v.get();
        let digits = text.strip_prefix('-').unwrap_or(text);
        text == "null"
            || (!digits.is_empty()
                && digits.bytes().all(|b| b.is_ascii_digit())
                && (digits == "0" || !digits.starts_with('0')))
    }
    fn fields(table: &[(&'static str, Kind, bool)], obj: &Members, at: &str) -> Result<(), String> {
        let names: Vec<&'static str> = table.iter().map(|f| f.0).collect();
        for (i, v) in known(&names, obj, at)? {
            let (name, kind, _) = table[i];
            let at = format!("{at}.{name}");
            match kind {
                Int if !integer(v) => return Err(format!("{at}: {} is not an integer", v.get())),
                Thinking => {
                    if let Some(t) = object(v) {
                        fields(&THINKING, &t?, &at)?;
                    }
                }
                Config => {
                    if let Some(c) = object(v) {
                        for (_, headers) in known(&["override_header"], &c?, &at)? {
                            // Go records a type error even when a repeated key's later value
                            // would replace it; header names are matched as written.
                            let Some(headers) = object(headers) else { continue };
                            for (name, value) in headers?.0 {
                                if !(value.get().starts_with('"') || value.get() == "null") {
                                    return Err(format!(
                                        "{at}.override_header.{name}: {} is not a string",
                                        value.get()
                                    ));
                                }
                            }
                        }
                    }
                }
                Native => {
                    if let Some(n) = object(v) {
                        known(&["web_search", "tool_search"], &n?, &at)?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
    let root: Members = serde_json::from_str(json).map_err(|e| e.to_string())?;
    for (i, models) in known(&SECTIONS, &root, "catalog")? {
        let section = SECTIONS[i];
        let Ok(models) = serde_json::from_str::<Vec<&RawValue>>(models.get()) else {
            continue;
        };
        for (n, model) in models.into_iter().enumerate() {
            if let Some(model) = object(model) {
                fields(&MODEL, &model?, &format!("{section}[{n}]"))?;
            }
        }
    }
    Ok(())
}

/// Go `json.Unmarshal` into `staticModelsJSON`: every model of its sections decodes.
fn decode_catalog(root: &Map<String, Value>) -> Result<(), String> {
    for section in SECTIONS {
        match root.get(section) {
            None | Some(Value::Null) => {}
            Some(Value::Array(models)) => {
                for model in models {
                    match model {
                        Value::Null => {}
                        Value::Object(m) => {
                            go_model_json(m).map_err(|e| format!("decode models catalog: {section}: {e}"))?;
                        }
                        other => return Err(format!("decode models catalog: {section}: cannot unmarshal {other}")),
                    }
                }
            }
            Some(other) => return Err(format!("decode models catalog: {section}: cannot unmarshal {other}")),
        }
    }
    Ok(())
}

/// Dated and bracketed Claude variants (`claude-sonnet-5-5-20261001`,
/// `claude-opus-5-5[1m]`) share their catalog entry's capabilities (Go
/// `claudeFamilyModelIDs`): the ID without a `[...]` suffix, then also without a
/// `-YYYYMMDD` date.
pub(crate) fn claude_family_ids(id: &str) -> Vec<&str> {
    let mut candidates = Vec::new();
    if !id.starts_with("claude-") {
        return candidates;
    }
    let mut base = id;
    if let Some(i) = base.find('[').filter(|&i| i > 0 && base.ends_with(']')) {
        base = &base[..i];
        candidates.push(base);
    }
    let n = base.len();
    if n > 9 && base.as_bytes()[n - 9] == b'-' && base.as_bytes()[n - 8..].iter().all(u8::is_ascii_digit) {
        candidates.push(&base[..n - 9]);
    }
    candidates
}

/// The static catalog in effect (Go `getModels()`): the embedded models.json until a
/// remote refresh ([`refresh_catalog`]) replaces it.
pub fn pinned() -> &'static Catalog {
    static EMBEDDED: LazyLock<Catalog> =
        LazyLock::new(|| Catalog::parse(MODELS_JSON).expect("embedded models.json is valid"));
    CURRENT
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .unwrap_or(&EMBEDDED)
}

/// The refreshed catalog, once one replaced the embedded copy.
// ponytail: `pinned()` hands out `'static` borrows, so a replaced catalog is kept alive
// (leaked) rather than freed: about half a megabyte per upstream catalog change, which
// happens a few times a month. Switch callers to an `Arc` snapshot to reclaim it.
static CURRENT: std::sync::RwLock<Option<&'static Catalog>> = std::sync::RwLock::new(None);
static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Bumped whenever [`refresh_catalog`] swaps in a different catalog; caches derived from
/// [`pinned`] key on it.
pub fn catalog_generation() -> u64 {
    GENERATION.load(std::sync::atomic::Ordering::Acquire)
}

/// Go `validateModelsCatalog`: in the required sections every model is an object with a
/// non-empty, unique (trimmed) `id`. Empty or missing sections are allowed.
fn validate_catalog(root: &Map<String, Value>) -> Result<(), String> {
    const REQUIRED: [&str; 12] = [
        "claude",
        "gemini",
        "vertex",
        "aistudio",
        "codex-free",
        "codex-team",
        "codex-plus",
        "codex-pro",
        "kimi",
        "antigravity",
        "xai",
        "meta",
    ];
    for section in REQUIRED {
        let models = match root.get(section) {
            None | Some(Value::Null) => continue,
            Some(Value::Array(models)) => models,
            Some(_) => return Err(format!("{section} is not a list")),
        };
        let mut seen = std::collections::HashSet::new();
        for (i, model) in models.iter().enumerate() {
            let Some(model) = model.as_object() else {
                return Err(format!("{section}[{i}] is null"));
            };
            // The ID Go's typed decode reads.
            let decoded = go_model_json(model).map_err(|e| format!("{section}[{i}]: {e}"))?;
            let id = decoded.get("id").and_then(Value::as_str).unwrap_or_default().trim();
            if id.is_empty() {
                return Err(format!("{section}[{i}] has empty id"));
            }
            if !seen.insert(id.to_owned()) {
                return Err(format!("{section} contains duplicate model id {id:?}"));
            }
        }
    }
    Ok(())
}

/// Go `detectChangedProviders`: the providers whose section differs. Gemini covers both
/// Gemini protocols and the Codex tiers are one provider.
fn changed_providers(old: &Catalog, new: &Catalog) -> Vec<String> {
    const SECTIONS: [(&str, &str); 17] = [
        ("claude", "claude"),
        ("gemini", "gemini"),
        ("gemini-interactions", "gemini"),
        ("vertex", "vertex"),
        ("aistudio", "aistudio"),
        ("codex", "codex-free"),
        ("codex", "codex-team"),
        ("codex", "codex-plus"),
        ("codex", "codex-pro"),
        ("kimi", "kimi"),
        ("kimi-ai", "kimi"),
        ("kimi.ai", "kimi"),
        ("kimi.com", "kimi"),
        ("antigravity", "antigravity"),
        ("xai", "xai"),
        ("devin", "devin"),
        ("meta", "meta"),
    ];
    let mut changed: Vec<String> = Vec::new();
    for (provider, section) in SECTIONS {
        if changed.iter().any(|p| p == provider) {
            continue;
        }
        let raws = |c: &Catalog| c.channel(section).iter().map(|m| m.raw.clone()).collect::<Vec<_>>();
        if raws(old) != raws(new) {
            changed.push(provider.to_owned());
        }
    }
    changed
}

/// Go `validateCatalogBytes`: whether [`refresh_catalog`] would accept `json`.
pub fn validate_catalog_json(json: &str) -> Result<(), String> {
    checked_catalog(json).map(drop)
}

fn checked_catalog(json: &str) -> Result<Map<String, Value>, String> {
    let root: Map<String, Value> = serde_json::from_str(json).map_err(|e| e.to_string())?;
    check_catalog_members(json)?;
    decode_catalog(&root)?;
    validate_catalog(&root)?;
    Ok(root)
}

/// Go `tryRefreshModels` after a fetch: parses and validates a remote models.json,
/// keeps the current `meta` section when the remote one is empty, and swaps it in when
/// it differs. Returns the changed providers (Go's refresh callback argument).
pub fn refresh_catalog(json: &str) -> Result<Vec<String>, String> {
    let mut root = checked_catalog(json)?;
    let old = pinned();
    if root.get("meta").and_then(Value::as_array).is_none_or(Vec::is_empty) && !old.channel("meta").is_empty() {
        let meta = old
            .channel("meta")
            .iter()
            .map(|m| Value::Object(m.raw.clone()))
            .collect();
        root.insert("meta".into(), Value::Array(meta));
    }
    let new = Catalog::from_root(root).map_err(|e| e.to_string())?;
    let changed = changed_providers(old, &new);
    let same =
        old.channels.len() == new.channels.len()
            && old.channels.iter().zip(&new.channels).all(|((a, am), (b, bm))| {
                a == b && am.len() == bm.len() && am.iter().zip(bm).all(|(x, y)| x.raw == y.raw)
            });
    if !same {
        *CURRENT.write().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Box::leak(Box::new(new)));
        GENERATION.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }
    Ok(changed)
}

/// The server's dynamic registry, consulted before the pinned catalog.
pub trait Overlay: Send + Sync {
    /// A registered model by public ID, preferring `provider`'s registration.
    fn lookup(&self, id: &str, provider: Option<&str>) -> Option<ModelInfo>;
    /// The model one credential registered under `model` (config model entries carry
    /// display names, context limits, thinking and `is_compat`).
    fn for_credential(&self, credential_id: &str, model: &str) -> Option<ModelInfo>;
    /// Go `GetAvailableModelsByProvider`: the models `provider`'s credentials currently
    /// serve (translators read Antigravity's `supports_web_search` from them). Defaults
    /// to none.
    fn available_by_provider(&self, _provider: &str) -> Vec<ModelInfo> {
        Vec::new()
    }
    /// Go `GetAvailableModels("openai")`: every model some credential can serve now, in
    /// registry order (Codex `spawn_agent` descriptions list them). Defaults to none.
    fn available(&self) -> Vec<ModelInfo> {
        Vec::new()
    }
    /// Go `GetModelProviders`: the providers that registered exactly `model`, most
    /// registrations first. Defaults to none.
    fn model_providers(&self, _model: &str) -> Vec<String> {
        Vec::new()
    }
}

static OVERLAY: std::sync::RwLock<Option<std::sync::Arc<dyn Overlay>>> = std::sync::RwLock::new(None);

/// Installs (or removes) the dynamic registry. Wired once at startup.
pub fn install_overlay(overlay: Option<std::sync::Arc<dyn Overlay>>) {
    *OVERLAY.write().unwrap_or_else(std::sync::PoisonError::into_inner) = overlay;
}

fn overlay() -> Option<std::sync::Arc<dyn Overlay>> {
    OVERLAY
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Go `registry.LookupModelInfo`: registered models first, then the pinned catalog.
pub fn lookup_model(id: &str, provider: Option<&str>) -> Option<ModelInfo> {
    let id = id.trim();
    if id.is_empty() {
        return None;
    }
    overlay()
        .and_then(|o| o.lookup(id, provider))
        .or_else(|| pinned().lookup(id).cloned())
}

/// Go `GetGlobalRegistry().GetModelProviders(id)`; empty without an installed registry.
pub fn model_providers(id: &str) -> Vec<String> {
    overlay().map(|o| o.model_providers(id)).unwrap_or_default()
}

/// Go `GetGlobalRegistry().GetModelInfo(id, provider)`: registered models only, without
/// the pinned catalog fallback of [`lookup_model`].
pub fn registered_model(id: &str, provider: Option<&str>) -> Option<ModelInfo> {
    overlay().and_then(|o| o.lookup(id, provider))
}

/// Go `GetGlobalRegistry().GetAvailableModelsByProvider(provider)`; empty without an
/// installed registry.
pub fn available_models_by_provider(provider: &str) -> Vec<ModelInfo> {
    overlay().map(|o| o.available_by_provider(provider)).unwrap_or_default()
}

/// Go `GetGlobalRegistry().GetAvailableModels("openai")`; empty without an installed
/// registry.
pub fn available_models() -> Vec<ModelInfo> {
    overlay().map(|o| o.available()).unwrap_or_default()
}

/// The model info resolved for one credential and model (Go attaches this to the
/// execution request), when the registry is installed and the credential registered it.
pub fn credential_model(credential_id: &str, model: &str) -> Option<ModelInfo> {
    overlay().and_then(|o| o.for_credential(credential_id, model))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog(json: &str) -> Catalog {
        Catalog::parse(json).unwrap()
    }

    /// Go TestLookupStaticModelInfoClaudeVariantsUseCatalogEntry (claude_family_lookup_test.go).
    #[test]
    fn claude_variants_use_their_catalog_entry() {
        let c = catalog(MODELS_JSON);
        for (id, family) in [
            ("claude-opus-5-5[1m]", "claude-opus-5-5"),
            ("claude-opus-5-5-20261001", "claude-opus-5-5"),
            ("claude-opus-5-5-20261001[1m]", "claude-opus-5-5"),
            ("claude-sonnet-5-5[1m]", "claude-sonnet-5-5"),
            ("claude-sonnet-5-5-20261001", "claude-sonnet-5-5"),
            ("claude-fable-5[1m]", "claude-fable-5"),
            ("claude-fable-5-1[1m]", "claude-fable-5-1"),
            ("claude-haiku-4-5-20251001[1m]", "claude-haiku-4-5-20251001"),
        ] {
            assert_eq!(c.lookup(id).map(|m| m.id.as_str()), Some(family), "{id}");
        }
        assert_eq!(
            c.lookup("claude-haiku-4-5-20251001").map(|m| m.id.as_str()),
            Some("claude-haiku-4-5-20251001")
        );
        for id in [
            "claude-unknown-20261001",
            "claude-opus-5-5-2026",
            "claude-opus-5-5[1m",
            "gpt-5[1m]",
        ] {
            assert!(c.lookup(id).is_none(), "{id}");
        }
        assert_eq!(
            c.lookup("claude-sonnet-5-5")
                .and_then(|m| m.thinking.as_ref())
                .map(|t| t.disable_type.as_str()),
            Some("between_tools")
        );
    }

    /// Go TestDetectChangedProviders_CodexConfigurationUpdate: an internal-field-only
    /// change in a Codex tier reports `codex`.
    #[test]
    fn detect_changed_providers_codex_configuration_update() {
        let old = catalog(r#"{"codex-free":[{"id":"gpt-6-luna"}]}"#);
        let new = catalog(r#"{"codex-free":[{"id":"gpt-6-luna","support_configuration_update":true}]}"#);
        assert_eq!(changed_providers(&old, &new), ["codex"]);
    }

    /// Go TestDetectChangedProviders_KimiAliases: a Kimi change reports every Kimi alias.
    #[test]
    fn detect_changed_providers_kimi_aliases() {
        let old = catalog(r#"{"kimi":[{"id":"kimi-k2"}]}"#);
        let new = catalog(r#"{"kimi":[{"id":"kimi-k2"},{"id":"kimi-k3"}]}"#);
        assert_eq!(
            changed_providers(&old, &new),
            ["kimi", "kimi-ai", "kimi.ai", "kimi.com"]
        );
        // Gemini changes cover both Gemini protocols; unchanged sections report nothing.
        let old = catalog(r#"{"gemini":[{"id":"g"}],"claude":[{"id":"c"}]}"#);
        let new = catalog(r#"{"gemini":[{"id":"g","display_name":"G"}],"claude":[{"id":"c"}]}"#);
        assert_eq!(changed_providers(&old, &new), ["gemini", "gemini-interactions"]);
    }

    /// Go validateModelsCatalog: null entries, empty ids and duplicates in required
    /// sections reject the catalog; empty sections and unvalidated ones do not.
    #[test]
    fn validate_catalog_follows_go() {
        let check = |json: &str| validate_catalog(&serde_json::from_str(json).unwrap());
        assert!(check(r#"{"claude":[]}"#).is_ok());
        assert!(check(r#"{"gemini-cli":[{"id":"x"},{"id":"x"}]}"#).is_ok());
        assert_eq!(check(r#"{"claude":[null]}"#).unwrap_err(), "claude[0] is null");
        assert_eq!(
            check(r#"{"xai":[{"id":"a"},{"id":" "}]}"#).unwrap_err(),
            "xai[1] has empty id"
        );
        assert_eq!(
            check(r#"{"meta":[{"id":"m"},{"id":" m "}]}"#).unwrap_err(),
            "meta contains duplicate model id \"m\""
        );
    }

    /// Go 1.26.4 `json.Unmarshal` into `registry.ModelInfo`, then `json.Marshal`
    /// (recorded from the v8.0.35 reference); `None` where Go's decode fails.
    #[test]
    fn models_decode_like_go_model_info() {
        let cases: [(&str, Option<&str>); 11] = [
            (
                r#"{"id":"gemini-typed","InputTokenLimit":123,"supportedInputModalities":[null,"text"],"config":{"override_header":{"X-Test":null,"b":"<v>"}},"THINKING":{"Min":1,"levels":[null],"DISABLE_TYPE":"x"},"Display_Name":"D","created":-0,"native_capabilities":{"web_search":true},"unknown":{"a":[1.5]}}"#,
                Some(
                    r#"{"id":"gemini-typed","object":"","created":0,"owned_by":"","type":"","display_name":"D","inputTokenLimit":123,"supportedInputModalities":["","text"],"thinking":{"min":1,"disable_type":"x","levels":[""]},"config":{"override_header":{"X-Test":"","b":"<v>"}}}"#,
                ),
            ),
            (
                r#"{"id":"n","display_name":null,"thinking":null,"config":{},"supportedOutputModalities":[],"supports_web_search":true}"#,
                Some(
                    r#"{"id":"n","object":"","created":0,"owned_by":"","type":"","supports_web_search":true,"config":{}}"#,
                ),
            ),
            (r#"{"id":"bad","inputTokenLimit":1.5}"#, None),
            (r#"{"id":"bad","inputTokenLimit":1e2}"#, None),
            (r#"{"id":"bad","inputTokenLimit":"5"}"#, None),
            (r#"{"id":"bad","created":9223372036854775808}"#, None),
            (r#"{"id":"bad","supportedInputModalities":[1]}"#, None),
            (r#"{"id":"bad","config":{"override_header":{"a":1}}}"#, None),
            (r#"{"id":"bad","native_capabilities":{"web_search":"yes"}}"#, None),
            (r#"{"id":"bad","support_configuration_update":1}"#, None),
            (r#"{"id":"bad","thinking":[]}"#, None),
        ];
        for (input, want) in cases {
            let got = go_model_json(&serde_json::from_str(input).unwrap()).ok();
            // Go escapes `<` and `>`; the management writer does too, serde does not.
            let want: Option<Value> = want.map(|w| serde_json::from_str(w).unwrap());
            assert_eq!(
                got.as_ref().map(Value::to_string),
                want.as_ref().map(Value::to_string),
                "{input}"
            );
        }
        // The embedded catalog decodes as Go's does.
        decode_catalog(&serde_json::from_str(MODELS_JSON).unwrap()).unwrap();
        check_catalog_members(MODELS_JSON).unwrap();
    }

    #[test]
    fn embedded_catalog_matches_go_channel_sizes() {
        let catalog = pinned();
        let sizes: Vec<(&str, usize)> = catalog.channels.iter().map(|(n, m)| (n.as_str(), m.len())).collect();
        assert_eq!(
            sizes,
            [
                ("claude", 19),
                ("gemini", 14),
                ("vertex", 21),
                ("gemini-cli", 7),
                ("aistudio", 15),
                ("codex-free", 5),
                ("codex-team", 9),
                ("codex-plus", 9),
                ("codex-pro", 9),
                ("kimi", 10),
                ("antigravity", 11),
                ("xai", 12),
                ("meta", 9),
            ]
        );
    }

    #[test]
    fn lookup_follows_go_search_order_across_providers() {
        let catalog = pinned();
        // Present in claude and antigravity: the claude definition wins.
        assert_eq!(catalog.lookup("claude-sonnet-4-6").unwrap().kind, "claude");
        // Only in vertex and antigravity: vertex is searched first.
        assert_eq!(catalog.lookup("gemini-3-flash").unwrap().kind, "gemini");
        // Non-Claude capabilities are visible to Claude-bound translation (Go does the same).
        let kimi = catalog.lookup("kimi-k2.5").unwrap().thinking.as_ref().unwrap();
        assert_eq!(kimi.levels, ["low", "high"]);
        assert!(kimi.zero_allowed);
        assert!(catalog.lookup("").is_none());
        assert!(catalog.lookup("no-such-model").is_none());
    }

    #[test]
    fn raw_definition_keeps_published_key_order() {
        let haiku = &pinned().channel("claude")[0];
        let keys: Vec<&str> = haiku.raw.keys().map(String::as_str).collect();
        assert_eq!(&keys[..4], ["id", "object", "created", "owned_by"]);
        assert_eq!(haiku.thinking.as_ref().unwrap().min, 1024);
    }
}
