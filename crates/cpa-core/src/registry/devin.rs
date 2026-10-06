//! Devin model catalog (internal/registry/devin_models.go, devin_models_updater.go's
//! store, and `staticDevinModels` in model_definitions.go).
//!
//! Three layers, as in Go:
//! - the active catalog: `devin_models.json` (embedded verbatim from CLIProxyAPI
//!   6fecc6e, replaceable at runtime by the remote updater through [`Store::load`]),
//!   validated, namespaced under `devin/` and aggregated so effort variants
//!   (`swe-2-high`, `glm-5-2-max-1m`) collapse into one base model with thinking levels;
//! - [`static_models`]: the hard-coded fallback list, which Go's `LookupStaticModelInfo`
//!   searches between xAI and Meta;
//! - the built-in `devin/swe-1-6-slow`, upserted into every catalog.
//!
//! Models are rendered as Go's `json.Marshal(ModelInfo)` would publish them, so
//! [`ModelInfo::raw`] keeps Go's field order and omitempty rules.

use std::sync::{LazyLock, RwLock};

use serde_json::{Map, Value};

use super::ModelInfo;

const EMBEDDED: &[u8] = include_bytes!("devin_models.json");
const BUILTIN_SWE16_SLOW_ID: &str = "devin/swe-1-6-slow";

/// One Go `registry.ModelInfo` as the Devin catalog decodes and publishes it.
#[derive(Debug, Clone, Default, PartialEq)]
struct Model {
    id: String,
    object: String,
    created: i64,
    owned_by: String,
    kind: String,
    display_name: String,
    name: String,
    version: String,
    description: String,
    input_token_limit: i64,
    output_token_limit: i64,
    supported_generation_methods: Vec<String>,
    context_length: i64,
    max_completion_tokens: i64,
    supported_parameters: Vec<String>,
    supported_input_modalities: Vec<String>,
    supported_output_modalities: Vec<String>,
    supports_web_search: bool,
    thinking: Option<Thinking>,
    /// `config.override_header`, or `Some(empty)` for a present `config` object.
    config: Option<Vec<(String, String)>>,
}

#[derive(Debug, Clone, Default, PartialEq)]
struct Thinking {
    min: i64,
    max: i64,
    zero_allowed: bool,
    dynamic_allowed: bool,
    levels: Vec<String>,
}

/// Go `encoding/json` field matching: an exact key wins, otherwise a case-insensitive
/// one; with duplicates the last matching key in the object wins.
// ponytail: ASCII case folding; Go also folds the Kelvin sign and long s.
fn field<'a>(obj: &'a Map<String, Value>, name: &str) -> Option<&'a Value> {
    let mut found = None;
    for (key, value) in obj {
        if key.eq_ignore_ascii_case(name) {
            found = Some(value);
        }
    }
    found
}

/// A decode failure (Go's `UnmarshalTypeError`): the whole payload is rejected.
#[derive(Debug)]
struct TypeError;

fn de_string(v: Option<&Value>, out: &mut String) -> Result<(), TypeError> {
    match v {
        None | Some(Value::Null) => Ok(()),
        Some(Value::String(s)) => {
            *out = s.clone();
            Ok(())
        }
        Some(_) => Err(TypeError),
    }
}

fn de_int(v: Option<&Value>, out: &mut i64) -> Result<(), TypeError> {
    match v {
        None | Some(Value::Null) => Ok(()),
        Some(Value::Number(n)) => {
            // Go decodes integers only from integer literals that fit.
            let literal = n.to_string();
            *out = literal.parse::<i64>().map_err(|_| TypeError)?;
            Ok(())
        }
        Some(_) => Err(TypeError),
    }
}

fn de_bool(v: Option<&Value>, out: &mut bool) -> Result<(), TypeError> {
    match v {
        None | Some(Value::Null) => Ok(()),
        Some(Value::Bool(b)) => {
            *out = *b;
            Ok(())
        }
        Some(_) => Err(TypeError),
    }
}

fn de_strings(v: Option<&Value>, out: &mut Vec<String>) -> Result<(), TypeError> {
    match v {
        None => Ok(()),
        Some(Value::Null) => {
            out.clear();
            Ok(())
        }
        Some(Value::Array(items)) => {
            let mut list = Vec::with_capacity(items.len());
            for item in items {
                let mut s = String::new();
                de_string(Some(item), &mut s)?;
                list.push(s);
            }
            *out = list;
            Ok(())
        }
        Some(_) => Err(TypeError),
    }
}

impl Model {
    /// `json.Unmarshal` into `*ModelInfo`: `None` for `null`.
    fn decode(value: &Value) -> Result<Option<Self>, TypeError> {
        let obj = match value {
            Value::Null => return Ok(None),
            Value::Object(obj) => obj,
            _ => return Err(TypeError),
        };
        let mut m = Model::default();
        de_string(field(obj, "id"), &mut m.id)?;
        de_string(field(obj, "object"), &mut m.object)?;
        de_int(field(obj, "created"), &mut m.created)?;
        de_string(field(obj, "owned_by"), &mut m.owned_by)?;
        de_string(field(obj, "type"), &mut m.kind)?;
        de_string(field(obj, "display_name"), &mut m.display_name)?;
        de_string(field(obj, "name"), &mut m.name)?;
        de_string(field(obj, "version"), &mut m.version)?;
        de_string(field(obj, "description"), &mut m.description)?;
        de_int(field(obj, "inputTokenLimit"), &mut m.input_token_limit)?;
        de_int(field(obj, "outputTokenLimit"), &mut m.output_token_limit)?;
        de_strings(
            field(obj, "supportedGenerationMethods"),
            &mut m.supported_generation_methods,
        )?;
        de_int(field(obj, "context_length"), &mut m.context_length)?;
        de_int(field(obj, "max_completion_tokens"), &mut m.max_completion_tokens)?;
        de_strings(field(obj, "supported_parameters"), &mut m.supported_parameters)?;
        de_strings(
            field(obj, "supportedInputModalities"),
            &mut m.supported_input_modalities,
        )?;
        de_strings(
            field(obj, "supportedOutputModalities"),
            &mut m.supported_output_modalities,
        )?;
        de_bool(field(obj, "supports_web_search"), &mut m.supports_web_search)?;
        // Go's UnmarshalJSON also reads these two internal fields.
        let mut support_update = false;
        de_bool(field(obj, "support_configuration_update"), &mut support_update)?;
        // ponytail: native_capabilities is type-checked as an object only; the Devin
        // catalog never carries it and nothing reads it for Devin.
        match field(obj, "native_capabilities") {
            None | Some(Value::Null) | Some(Value::Object(_)) => {}
            Some(_) => return Err(TypeError),
        }
        match field(obj, "thinking") {
            None | Some(Value::Null) => {}
            Some(Value::Object(t)) => {
                let mut thinking = Thinking::default();
                de_int(field(t, "min"), &mut thinking.min)?;
                de_int(field(t, "max"), &mut thinking.max)?;
                de_bool(field(t, "zero_allowed"), &mut thinking.zero_allowed)?;
                de_bool(field(t, "dynamic_allowed"), &mut thinking.dynamic_allowed)?;
                de_strings(field(t, "levels"), &mut thinking.levels)?;
                m.thinking = Some(thinking);
            }
            Some(_) => return Err(TypeError),
        }
        match field(obj, "config") {
            None | Some(Value::Null) => {}
            Some(Value::Object(c)) => {
                let mut headers = Vec::new();
                match field(c, "override_header") {
                    None | Some(Value::Null) => {}
                    Some(Value::Object(h)) => {
                        for (k, v) in h {
                            let mut s = String::new();
                            de_string(Some(v), &mut s)?;
                            headers.retain(|(n, _): &(String, String)| n != k);
                            headers.push((k.clone(), s));
                        }
                    }
                    Some(_) => return Err(TypeError),
                }
                headers.sort();
                m.config = Some(headers);
            }
            Some(_) => return Err(TypeError),
        }
        Ok(Some(m))
    }

    /// `json.Marshal(ModelInfo)`: Go's field order and omitempty rules.
    fn marshal(&self) -> Map<String, Value> {
        let mut out = Map::new();
        out.insert("id".into(), self.id.clone().into());
        out.insert("object".into(), self.object.clone().into());
        out.insert("created".into(), self.created.into());
        out.insert("owned_by".into(), self.owned_by.clone().into());
        out.insert("type".into(), self.kind.clone().into());
        let strings = |out: &mut Map<String, Value>, key: &str, v: &[String]| {
            if !v.is_empty() {
                out.insert(key.into(), v.to_vec().into());
            }
        };
        for (key, value) in [
            ("display_name", &self.display_name),
            ("name", &self.name),
            ("version", &self.version),
            ("description", &self.description),
        ] {
            if !value.is_empty() {
                out.insert(key.into(), value.clone().into());
            }
        }
        for (key, value) in [
            ("inputTokenLimit", self.input_token_limit),
            ("outputTokenLimit", self.output_token_limit),
        ] {
            if value != 0 {
                out.insert(key.into(), value.into());
            }
        }
        strings(
            &mut out,
            "supportedGenerationMethods",
            &self.supported_generation_methods,
        );
        for (key, value) in [
            ("context_length", self.context_length),
            ("max_completion_tokens", self.max_completion_tokens),
        ] {
            if value != 0 {
                out.insert(key.into(), value.into());
            }
        }
        strings(&mut out, "supported_parameters", &self.supported_parameters);
        strings(&mut out, "supportedInputModalities", &self.supported_input_modalities);
        strings(&mut out, "supportedOutputModalities", &self.supported_output_modalities);
        if self.supports_web_search {
            out.insert("supports_web_search".into(), true.into());
        }
        if let Some(t) = &self.thinking {
            let mut thinking = Map::new();
            if t.min != 0 {
                thinking.insert("min".into(), t.min.into());
            }
            if t.max != 0 {
                thinking.insert("max".into(), t.max.into());
            }
            if t.zero_allowed {
                thinking.insert("zero_allowed".into(), true.into());
            }
            if t.dynamic_allowed {
                thinking.insert("dynamic_allowed".into(), true.into());
            }
            strings(&mut thinking, "levels", &t.levels);
            out.insert("thinking".into(), Value::Object(thinking));
        }
        if let Some(headers) = &self.config {
            let mut config = Map::new();
            if !headers.is_empty() {
                let h: Map<String, Value> = headers.iter().map(|(k, v)| (k.clone(), v.clone().into())).collect();
                config.insert("override_header".into(), Value::Object(h));
            }
            out.insert("config".into(), Value::Object(config));
        }
        out
    }

    fn info(&self) -> ModelInfo {
        ModelInfo::from_raw(self.marshal()).expect("catalog model renders a valid ModelInfo")
    }
}

fn builtin_swe16_slow() -> Model {
    Model {
        id: BUILTIN_SWE16_SLOW_ID.into(),
        object: "model".into(),
        kind: "devin".into(),
        owned_by: "cognition".into(),
        display_name: "SWE-1.6 Slow".into(),
        context_length: 200_000,
        max_completion_tokens: 64_000,
        input_token_limit: 200_000,
        output_token_limit: 64_000,
        supported_input_modalities: vec!["text".into(), "image".into()],
        supported_output_modalities: vec!["text".into()],
        supported_generation_methods: vec!["generateContent".into(), "countTokens".into()],
        ..Model::default()
    }
}

/// `WithDevinBuiltins` (`upsertModelInfos`): built-ins replace models with the same ID
/// (case-insensitive) and are appended at the end.
fn with_builtins(models: Vec<Model>) -> Vec<Model> {
    let builtin = builtin_swe16_slow();
    let key = builtin.id.to_lowercase();
    let mut out: Vec<Model> = models
        .into_iter()
        .filter(|m| {
            let id = m.id.trim();
            !id.is_empty() && id.to_lowercase() != key
        })
        .collect();
    out.push(builtin);
    out
}

fn static_model(id: &str, owned_by: &str, display: &str, context: i64, max: i64, levels: &[&str]) -> Model {
    Model {
        id: id.into(),
        kind: "devin".into(),
        owned_by: owned_by.into(),
        display_name: display.into(),
        context_length: context,
        max_completion_tokens: max,
        thinking: Some(Thinking {
            levels: levels.iter().map(|l| (*l).to_owned()).collect(),
            ..Thinking::default()
        }),
        ..Model::default()
    }
}

/// `staticDevinModels`.
fn static_list() -> Vec<Model> {
    vec![
        builtin_swe16_slow(),
        static_model(
            "devin/swe-2",
            "cognition",
            "SWE-2",
            262_000,
            128_000,
            &["medium", "high", "max"],
        ),
        static_model(
            "devin/claude-fable-5-1",
            "anthropic",
            "Claude Fable 5.1",
            1_000_000,
            64_000,
            &["low", "medium", "high", "xhigh", "max"],
        ),
        static_model(
            "devin/gpt-6-astra",
            "openai",
            "GPT-6 Astra",
            1_000_000,
            64_000,
            &["low", "medium", "high", "xhigh", "max"],
        ),
        static_model("devin/glm-5-2", "zhipu", "GLM-5.2", 200_000, 64_000, &["none", "high"]),
        static_model(
            "devin/glm-5-3",
            "zhipu",
            "GLM-5.3",
            1_048_576,
            128_000,
            &["low", "high", "max"],
        ),
        static_model(
            "devin/glm-5-3-flash",
            "zhipu",
            "GLM-5.3 Flash",
            1_000_000,
            128_000,
            &["low", "high", "max"],
        ),
        static_model(
            "devin/gpt-5-6-sol",
            "openai",
            "GPT-5.6 Sol",
            1_000_000,
            128_000,
            &["none", "low", "medium", "high", "xhigh", "max"],
        ),
        static_model(
            "devin/gemini-3-8-flash",
            "google",
            "Gemini 3.8 Flash",
            1_048_576,
            65_536,
            &["low", "medium", "high"],
        ),
        static_model(
            "devin/grok-4-6",
            "xai",
            "Grok 4.6",
            500_000,
            131_072,
            &["low", "medium", "high", "xhigh"],
        ),
        static_model(
            "devin/deepseek-v4-flash",
            "deepseek",
            "DeepSeek V4 Flash",
            1_048_576,
            64_000,
            &["high", "max"],
        ),
        static_model(
            "devin/deepseek-v4-1-flash",
            "deepseek",
            "DeepSeek V4.1 Flash",
            1_048_576,
            64_000,
            &["high", "max"],
        ),
    ]
}

/// `staticDevinModels`, which Go's `LookupStaticModelInfo` searches after the
/// models.json `devin` section.
pub fn static_models() -> &'static [ModelInfo] {
    static STATIC: LazyLock<Vec<ModelInfo>> = LazyLock::new(|| static_list().iter().map(Model::info).collect());
    &STATIC
}

const COMPOUND_SUFFIXES: [(&str, &str, &str); 16] = [
    ("-low-fast", "low", ""),
    ("-medium-fast", "medium", ""),
    ("-high-fast", "high", ""),
    ("-xhigh-fast", "xhigh", ""),
    ("-max-fast", "max", ""),
    ("-none-fast", "none", ""),
    ("-low-priority", "low", ""),
    ("-medium-priority", "medium", ""),
    ("-high-priority", "high", ""),
    ("-xhigh-priority", "xhigh", ""),
    ("-max-priority", "max", ""),
    ("-none-priority", "none", ""),
    ("-thinking-1m", "", "-1m"),
    ("-thinking", "", ""),
    ("-max-1m", "max", "-1m"),
    ("-none-1m", "none", "-1m"),
];

const SIMPLE_SUFFIXES: [(&str, &str); 7] = [
    ("-none", "none"),
    ("-minimal", "minimal"),
    ("-low", "low"),
    ("-medium", "medium"),
    ("-high", "high"),
    ("-xhigh", "xhigh"),
    ("-max", "max"),
];

const UPPER_SUFFIXES: [(&str, &str); 8] = [
    ("_NONE", "none"),
    ("_MINIMAL", "minimal"),
    ("_LOW", "low"),
    ("_MEDIUM", "medium"),
    ("_HIGH", "high"),
    ("_XHIGH", "xhigh"),
    ("_MAX", "max"),
    ("_THINKING", "high"),
];

const DISPLAY_SUFFIXES: [&str; 26] = [
    " Low Fast",
    " Medium Fast",
    " High Fast",
    " XHigh Fast",
    " Max Fast",
    " Low Thinking Fast",
    " Medium Thinking Fast",
    " High Thinking Fast",
    " XHigh Thinking Fast",
    " Max Thinking Fast",
    " No Thinking Fast",
    " Low Thinking",
    " Medium Thinking",
    " High Thinking",
    " XHigh Thinking",
    " Max Thinking",
    " No Thinking",
    " Low",
    " Medium",
    " High",
    " XHigh",
    " Max",
    " None",
    " Minimal",
    " Thinking",
    " Fast",
];

/// `splitDevinModelID`: the base model of an effort variant and that effort.
// ponytail: ASCII case mapping; Go's ToUpper/ToLower differ only for non-ASCII IDs.
pub fn split_model_id(clean: &str) -> (String, String) {
    if clean == "swe-1-6-slow" {
        return (clean.to_owned(), String::new());
    }
    if clean == "swe-1-6-fast" {
        return ("swe-1-6".into(), String::new());
    }
    let upper = clean.to_ascii_uppercase();
    for (suffix, effort) in UPPER_SUFFIXES {
        if upper.ends_with(suffix) {
            return (clean[..clean.len() - suffix.len()].to_owned(), effort.into());
        }
    }
    for (suffix, effort, readd) in COMPOUND_SUFFIXES {
        if let Some(base) = clean.strip_suffix(suffix) {
            return (format!("{base}{readd}"), effort.into());
        }
    }
    for (suffix, effort) in SIMPLE_SUFFIXES {
        if let Some(base) = clean.strip_suffix(suffix) {
            return (base.to_owned(), effort.into());
        }
    }
    (clean.to_owned(), String::new())
}

/// `cleanDevinDisplayName`: strips effort words from the end, repeatedly.
fn clean_display_name(name: &str) -> String {
    let mut trimmed = name.trim().to_owned();
    loop {
        let lower = trimmed.to_ascii_lowercase();
        let Some(suffix) = DISPLAY_SUFFIXES
            .iter()
            .find(|s| lower.ends_with(&s.to_ascii_lowercase()))
        else {
            return trimmed;
        };
        trimmed = trimmed[..trimmed.len() - suffix.len()].trim().to_owned();
    }
}

fn level_rank(level: &str) -> u8 {
    match level {
        "none" => 0,
        "minimal" => 1,
        "low" => 2,
        "medium" => 3,
        "high" => 4,
        "xhigh" => 5,
        "max" => 6,
        "fast" => 7,
        "priority" => 8,
        _ => 99,
    }
}

fn push_unique(target: &mut Vec<String>, items: &[String]) {
    for item in items {
        if !target.contains(item) {
            target.push(item.clone());
        }
    }
}

/// `aggregateDevinModels`.
fn aggregate(models: Vec<Model>) -> Vec<Model> {
    let mut order: Vec<(Model, Vec<String>)> = Vec::new();
    for m in models {
        let clean = m.id.trim();
        let clean = clean.strip_prefix("devin/").unwrap_or(clean).to_lowercase();
        let (mut base, effort) = split_model_id(&clean);
        if base.is_empty() {
            base.clone_from(&clean);
        }
        let namespaced = format!("devin/{base}");
        let is_base = base == clean;
        let index = match order.iter().position(|(e, _)| e.id == namespaced) {
            Some(i) => i,
            None => {
                let mut entry = m.clone();
                entry.id.clone_from(&namespaced);
                entry.display_name = clean_display_name(&m.display_name);
                if entry.display_name.is_empty() {
                    entry.display_name.clone_from(&m.display_name);
                }
                order.push((entry, Vec::new()));
                order.len() - 1
            }
        };
        let (entry, levels) = &mut order[index];
        if is_base {
            if !m.display_name.is_empty() {
                entry.display_name = clean_display_name(&m.display_name);
            }
            if !m.owned_by.is_empty() {
                entry.owned_by.clone_from(&m.owned_by);
            }
        }
        entry.context_length = entry.context_length.max(m.context_length);
        entry.max_completion_tokens = entry.max_completion_tokens.max(m.max_completion_tokens);
        entry.input_token_limit = entry.input_token_limit.max(m.input_token_limit);
        entry.output_token_limit = entry.output_token_limit.max(m.output_token_limit);
        push_unique(&mut entry.supported_input_modalities, &m.supported_input_modalities);
        push_unique(&mut entry.supported_output_modalities, &m.supported_output_modalities);
        push_unique(&mut entry.supported_generation_methods, &m.supported_generation_methods);
        if let Some(thinking) = &m.thinking {
            for l in &thinking.levels {
                if !l.is_empty() && l != "priority" && !levels.contains(l) {
                    levels.push(l.clone());
                }
            }
        }
        if !effort.is_empty() && effort != "priority" && !levels.contains(&effort) {
            levels.push(effort);
        }
    }
    order
        .into_iter()
        .map(|(mut m, mut levels)| {
            if !levels.is_empty() {
                levels.sort_by(|a, b| level_rank(a).cmp(&level_rank(b)).then_with(|| a.cmp(b)));
                m.thinking = Some(Thinking {
                    levels,
                    ..Thinking::default()
                });
            }
            if m.kind.is_empty() {
                m.kind = "devin".into();
            }
            if m.object.is_empty() {
                m.object = "model".into();
            }
            if m.supported_input_modalities.is_empty() {
                m.supported_input_modalities = vec!["text".into()];
            }
            if m.supported_output_modalities.is_empty() {
                m.supported_output_modalities = vec!["text".into()];
            }
            if m.input_token_limit == 0 && m.context_length > 0 {
                m.input_token_limit = m.context_length;
            }
            if m.output_token_limit == 0 && m.max_completion_tokens > 0 {
                m.output_token_limit = m.max_completion_tokens;
            }
            if m.supported_generation_methods.is_empty() {
                m.supported_generation_methods = vec!["generateContent".into(), "countTokens".into()];
            }
            m
        })
        .collect()
}

/// `sanitizeAndValidateDevinModels`.
fn sanitize(models: Vec<Option<Model>>) -> Result<Vec<Model>, String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(models.len());
    for (i, m) in models.into_iter().enumerate() {
        let Some(mut m) = m else {
            return Err(format!("model at index {i} is null"));
        };
        let mut id = m.id.trim().to_owned();
        if id.is_empty() {
            return Err(format!("model at index {i} has empty id"));
        }
        if !id.to_lowercase().starts_with("devin/") {
            id = format!("devin/{id}");
        }
        let id = id.to_lowercase();
        if !seen.insert(id.clone()) {
            return Err(format!("duplicate model id: {}", go_quote(&id)));
        }
        m.id = id;
        out.push(m);
    }
    Ok(aggregate(out))
}

/// Go's `%q` for the ASCII IDs the catalog holds.
fn go_quote(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

fn decode_list(value: &Value) -> Result<Vec<Option<Model>>, TypeError> {
    match value {
        Value::Null => Ok(Vec::new()),
        Value::Array(items) => items.iter().map(Model::decode).collect(),
        _ => Err(TypeError),
    }
}

/// `ValidateDevinModelsJSON`: `{"devin": [...]}`, `{"models": [...]}` or a bare array,
/// sanitized and aggregated.
fn validate_models(data: &[u8]) -> Result<Vec<Model>, String> {
    if data.iter().all(u8::is_ascii_whitespace) {
        return Err("empty Devin models payload".into());
    }
    let parsed: Option<Value> = serde_json::from_slice(data).ok();
    if let Some(value) = &parsed {
        // The envelope: a JSON object (or null) whose `devin`/`models` decode.
        let envelope = match value {
            Value::Null => Some((Vec::new(), Vec::new())),
            Value::Object(obj) => {
                let devin = field(obj, "devin").map(decode_list).unwrap_or(Ok(Vec::new()));
                let models = field(obj, "models").map(decode_list).unwrap_or(Ok(Vec::new()));
                match (devin, models) {
                    (Ok(d), Ok(m)) => Some((d, m)),
                    _ => None,
                }
            }
            _ => None,
        };
        if let Some((devin, models)) = envelope {
            let candidates = if devin.is_empty() { models } else { devin };
            if !candidates.is_empty() {
                return sanitize(candidates);
            }
        }
        if let Ok(list) = decode_list(value)
            && !list.is_empty()
        {
            return sanitize(list);
        }
    }
    Err("invalid Devin models JSON: expected non-empty 'devin'/'models' array or model list".into())
}

/// `ValidateDevinModelsJSON` for callers that only need the verdict and the models.
pub fn validate(data: &[u8]) -> Result<Vec<ModelInfo>, String> {
    validate_models(data).map(|models| models.iter().map(Model::info).collect())
}

/// The active catalog (`devinModelsStore`). [`global`] is the process-wide one Go keeps;
/// tests use their own.
pub struct Store {
    inner: RwLock<Inner>,
}

#[derive(Default)]
struct Inner {
    models: Vec<ModelInfo>,
    raw: Vec<u8>,
    revision: u64,
}

impl Store {
    /// An empty store (no catalog loaded).
    pub fn empty() -> Self {
        Self {
            inner: RwLock::new(Inner::default()),
        }
    }

    /// `loadDevinModelsFromBytes`: validates and installs a catalog. `Ok(false)` when
    /// the bytes equal the current catalog; errors carry `source` like Go's.
    pub fn load(&self, data: &[u8], source: &str) -> Result<bool, String> {
        let models = validate_models(data).map_err(|e| format!("{source}: {e}"))?;
        let models: Vec<ModelInfo> = with_builtins(models).iter().map(Model::info).collect();
        let mut inner = self.inner.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        if inner.raw == data {
            return Ok(false);
        }
        inner.models = models;
        inner.raw = data.to_vec();
        inner.revision += 1;
        // Registries built from the previous Devin models are stale now.
        super::GENERATION.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Ok(true)
    }

    fn current(&self) -> Vec<ModelInfo> {
        self.inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .models
            .clone()
    }

    /// `GetDevinModels`: the loaded catalog, else models.json's `devin` section, else
    /// the static list; built-ins always included.
    pub fn models(&self) -> Vec<ModelInfo> {
        let current = self.current();
        if !current.is_empty() {
            return upsert_builtins(current);
        }
        let section = super::pinned().channel("devin");
        if !section.is_empty() {
            return upsert_builtins(section.to_vec());
        }
        upsert_builtins(static_models().to_vec())
    }

    /// `LookupDevinModel`: by namespaced or bare ID, case-insensitive; an effort variant
    /// falls back to its base model.
    pub fn lookup(&self, id: &str) -> Option<ModelInfo> {
        let clean = id.trim().to_lowercase();
        let clean = clean.strip_prefix("devin/").unwrap_or(&clean).to_owned();
        if clean.is_empty() {
            return None;
        }
        let mut models = self.current();
        if models.is_empty() {
            models = self.models();
        }
        let bare = |m: &ModelInfo| m.id.strip_prefix("devin/").unwrap_or(&m.id).to_lowercase();
        if let Some(m) = models.iter().find(|m| bare(m) == clean) {
            return Some(m.clone());
        }
        if let Some(m) = upsert_builtins(Vec::new()).into_iter().find(|m| bare(m) == clean) {
            return Some(m);
        }
        let (base, _) = split_model_id(&clean);
        if base != clean && !base.is_empty() {
            return models.into_iter().find(|m| bare(m) == base);
        }
        None
    }

    /// `GetDevinModelsSnapshot`: the raw catalog bytes and their revision.
    pub fn snapshot(&self) -> (Vec<u8>, u64) {
        let inner = self.inner.read().unwrap_or_else(std::sync::PoisonError::into_inner);
        (inner.raw.clone(), inner.revision)
    }
}

/// `WithDevinBuiltins` over already rendered models.
fn upsert_builtins(models: Vec<ModelInfo>) -> Vec<ModelInfo> {
    let builtin = builtin_swe16_slow().info();
    let key = builtin.id.to_lowercase();
    let mut out: Vec<ModelInfo> = models
        .into_iter()
        .filter(|m| {
            let id = m.id.trim();
            !id.is_empty() && id.to_lowercase() != key
        })
        .collect();
    out.push(builtin);
    out
}

/// The process-wide catalog, loaded from the embedded `devin_models.json` on first use.
pub fn global() -> &'static Store {
    static GLOBAL: LazyLock<Store> = LazyLock::new(|| {
        let store = Store::empty();
        if let Err(e) = store.load(EMBEDDED, "embed") {
            tracing::warn!(
                "registry: failed to parse embedded devin_models.json (will rely on static fallback and remote refresh): {e}"
            );
        }
        store
    });
    &GLOBAL
}

/// `GetDevinModels` on the process-wide catalog.
pub fn models() -> Vec<ModelInfo> {
    global().models()
}

/// `LookupDevinModel` on the process-wide catalog.
pub fn lookup(id: &str) -> Option<ModelInfo> {
    global().lookup(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_names_lose_effort_words_repeatedly() {
        assert_eq!(clean_display_name(" GLM 5 High Fast "), "GLM 5");
        assert_eq!(clean_display_name("M Medium Thinking Fast"), "M");
        assert_eq!(clean_display_name("Fast"), "Fast", "needs the leading space");
        assert_eq!(clean_display_name("X Thinking Low"), "X");
    }

    #[test]
    fn split_follows_go_suffix_precedence() {
        assert_eq!(split_model_id("n_low"), ("n".into(), "low".into()));
        assert_eq!(split_model_id("q-thinking-1m"), ("q-1m".into(), String::new()));
        assert_eq!(split_model_id("glm-5-2-max-1m"), ("glm-5-2-1m".into(), "max".into()));
        assert_eq!(split_model_id("a-low-priority"), ("a".into(), "low".into()));
        assert_eq!(split_model_id("swe-1-6-fast"), ("swe-1-6".into(), String::new()));
        assert_eq!(split_model_id("plain"), ("plain".into(), String::new()));
    }
}
