//! JSON Schema cleaning for Gemini and Antigravity tool and response schemas
//! (internal/util/gemini_schema.go).
//!
//! Like Go, this works on the schema text: it walks gjson paths, edits with sjson, and
//! re-marshals through Go's `any` model only where Go does (malformed-object repair and
//! local `$ref` inlining). Pass a single schema, never a whole request document.

use crate::json::{self as gj, AnyValue, GoValue, Kind, Res};
use std::collections::{BTreeMap, HashMap};

const PLACEHOLDER_REASON_DESCRIPTION: &[u8] = b"Brief explanation of why you are calling this tool";

#[derive(Clone, Copy, Default)]
struct Options {
    add_placeholder: bool,
    add_missing_array_items: bool,
    antigravity_semantics: bool,
    remove_tool_title: bool,
    remove_gemini_metadata: bool,
    flatten_unions: bool,
    /// Flatten multi-branch anyOf/oneOf unions to one branch instead of preserving them.
    flatten_multi_branch_unions: bool,
    force_enum_string_type: bool,
    drop_all_enums: bool,
    drop_boolean_enums: bool,
    preserve_additional_properties_false: bool,
    preserve_all_additional_properties: bool,
    preserve_standard_constraints: bool,
}

/// CleanJSONSchemaForAntigravity.
pub fn for_antigravity(schema: &[u8]) -> Vec<u8> {
    for_antigravity_tool(schema, true)
}

/// CleanJSONSchemaForAntigravityTool.
pub fn for_antigravity_tool(schema: &[u8], require_placeholder: bool) -> Vec<u8> {
    antigravity_tool(schema, require_placeholder, false)
}

/// CleanJSONSchemaForAntigravityClaudeTool: the Antigravity tool cleaner for Claude models,
/// which flattens multi-branch unions because that backend rejects them.
pub fn for_antigravity_claude_tool(schema: &[u8], require_placeholder: bool) -> Vec<u8> {
    antigravity_tool(schema, require_placeholder, true)
}

fn antigravity_tool(schema: &[u8], require_placeholder: bool, flatten_multi_branch_unions: bool) -> Vec<u8> {
    clean(
        schema,
        Options {
            add_placeholder: require_placeholder,
            add_missing_array_items: true,
            antigravity_semantics: true,
            remove_tool_title: !require_placeholder,
            flatten_unions: true,
            flatten_multi_branch_unions,
            drop_all_enums: true,
            ..Options::default()
        },
    )
}

/// CleanJSONSchemaForAntigravityResponse.
pub fn for_antigravity_response(schema: &[u8]) -> Vec<u8> {
    clean(
        schema,
        Options {
            antigravity_semantics: true,
            flatten_unions: true,
            drop_boolean_enums: true,
            preserve_additional_properties_false: true,
            ..Options::default()
        },
    )
}

/// CleanJSONSchemaForGemini.
pub fn for_gemini(schema: &[u8]) -> Vec<u8> {
    clean(
        schema,
        Options {
            add_missing_array_items: true,
            remove_gemini_metadata: true,
            flatten_unions: true,
            force_enum_string_type: true,
            ..Options::default()
        },
    )
}

/// CleanJSONSchemaForGeminiJSONSchema: for the `parametersJsonSchema` carrier, keeping
/// standard constraints and every `additionalProperties`.
pub fn for_gemini_json_schema(schema: &[u8]) -> Vec<u8> {
    clean(
        schema,
        Options {
            add_missing_array_items: true,
            remove_gemini_metadata: true,
            flatten_unions: true,
            force_enum_string_type: true,
            preserve_all_additional_properties: true,
            preserve_standard_constraints: true,
            ..Options::default()
        },
    )
}

/// InlineLocalRefs.
pub fn inline_local_refs(schema: &[u8]) -> Vec<u8> {
    on_ref_stack(schema, || inline_refs(schema.to_vec()))
}

/// Runs `f` on a scoped thread with Go's maximum goroutine stack (1 GiB, reserved, not
/// committed) when `schema` holds a `"$ref"`. Inlining nests each expansion inside its
/// reference, so a chain of definitions in a small input expands to thousands of levels
/// (about 6,000 within the budget), and the passes recurse per level like Go's, whose
/// stacks grow. Input-depth guards cannot see that. Cost: one thread spawn per schema with
/// a reference, on the request path of every Gemini, Vertex, AI Studio, Antigravity, xAI
/// and Kimi request whose tool schemas carry one.
fn on_ref_stack<T: Send>(schema: &[u8], f: impl FnOnce() -> T + Send) -> T {
    if !contains_seq(schema, br#""$ref""#) {
        return f();
    }
    std::thread::scope(|scope| {
        let handle = std::thread::Builder::new()
            .name("schema-refs".into())
            .stack_size(1 << 30)
            .spawn_scoped(scope, f)
            .expect("spawn the schema inlining thread");
        handle.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    })
}

/// cleanJSONSchema.
///
/// ponytail: the passes keep Go's algorithmic shape, which is superlinear: each pass walks
/// the schema and re-reads it by path, and a budget-filling schema takes about 10 s per
/// tool schema in Go (the uncharged type and nullable fallback copies grow roughly
/// cubically). The ceiling is Go's own; the upgrade path is a linear rewrite of the
/// cleaner, with the work moved off the tokio workers.
fn clean(schema: &[u8], options: Options) -> Vec<u8> {
    on_ref_stack(schema, || clean_inner(schema, options))
}

fn clean_inner(schema: &[u8], options: Options) -> Vec<u8> {
    let mut s = normalize_malformed_schema_objects(schema.to_vec(), options.add_missing_array_items);
    // Every cleaner removes $defs/definitions later, so local references expand on every
    // route. Per request this costs one decode and re-encode of each schema with a
    // `"$ref"` (none without one), and up to the 256 KiB budget of copies.
    s = inline_refs_for_clean(s);
    s = convert_refs_to_hints(s, options.antigravity_semantics);
    s = convert_const_to_enum(s);
    s = convert_enum_values_to_strings(s, options.force_enum_string_type);
    s = add_enum_hints(s);
    s = drop_ignored_enums_to_hints(s, options);
    if !options.preserve_additional_properties_false && !options.preserve_all_additional_properties {
        s = add_additional_properties_hints(s);
    }
    s = move_constraints_to_description(s, options);
    if options.antigravity_semantics {
        s = move_not_to_description(s);
    }
    s = merge_conditionals(s);
    s = merge_all_of(s);
    if options.flatten_unions {
        s = flatten_any_of_one_of(
            s,
            options.preserve_standard_constraints,
            options.flatten_multi_branch_unions,
        );
    }
    s = flatten_type_arrays(s, options.antigravity_semantics);
    s = remove_unsupported_keywords(s, options);
    if options.remove_gemini_metadata {
        s = remove_keywords(s, &[b"nullable", b"title"]);
        s = remove_placeholder_fields(s);
    } else if options.remove_tool_title {
        s = remove_keywords(s, &[b"title"]);
    }
    s = cleanup_required_fields(s);
    s = sanitize_object_properties(s);
    s = sanitize_array_items(s, options.add_missing_array_items);
    if options.add_placeholder {
        s = add_empty_schema_placeholder(s);
    }
    s
}

// ---------------------------------------------------------------------------------------
// Path helpers

fn escape_key(key: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(key.len());
    for &c in key {
        if matches!(c, b'.' | b'*' | b'?') {
            out.push(b'\\');
        }
        out.push(c);
    }
    out
}

fn unescape_key(key: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(key.len());
    let mut i = 0;
    while i < key.len() {
        if key[i] == b'\\' && i + 1 < key.len() {
            i += 1;
        }
        out.push(key[i]);
        i += 1;
    }
    out
}

/// splitGJSONPath: segments keep their escapes.
fn split_path(path: &[u8]) -> Vec<&[u8]> {
    if path.is_empty() {
        return vec![];
    }
    let mut parts = vec![];
    let mut start = 0;
    let mut i = 0;
    while i < path.len() {
        if path[i] == b'\\' && i + 1 < path.len() {
            i += 2;
            continue;
        }
        if path[i] == b'.' {
            parts.push(&path[start..i]);
            start = i + 1;
        }
        i += 1;
    }
    parts.push(&path[start..]);
    parts
}

fn join_path(base: &[u8], suffix: &[u8]) -> Vec<u8> {
    if base.is_empty() {
        return suffix.to_vec();
    }
    [base, b".", suffix].concat()
}

fn trim_suffix<'a>(path: &'a [u8], suffix: &[u8]) -> &'a [u8] {
    if path == suffix.strip_prefix(b".").unwrap_or(suffix) {
        return b"";
    }
    path.strip_suffix(suffix).unwrap_or(path)
}

fn depth_sorted(mut paths: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    paths.sort_by_key(|p| std::cmp::Reverse(split_path(p).len()));
    paths
}

/// util.WalkSchema: every path (keys escaped) whose last key equals `field`, in document
/// order, without descending into literal values.
fn find_paths(json: &[u8], field: &[u8]) -> Vec<Vec<u8>> {
    let mut paths = vec![];
    walk_fields(&gj::parse(json), b"", true, &mut |key, path| {
        if key == field {
            paths.push(path.to_vec());
        }
    });
    paths
}

fn find_paths_by_fields(json: &[u8], fields: &[&[u8]]) -> HashMap<Vec<u8>, Vec<Vec<u8>>> {
    let mut paths: HashMap<Vec<u8>, Vec<Vec<u8>>> = HashMap::new();
    walk_fields(&gj::parse(json), b"", true, &mut |key, path| {
        if fields.contains(&key) {
            paths.entry(key.to_vec()).or_default().push(path.to_vec());
        }
    });
    paths
}

/// util.Walk: the escaped gjson paths of every `field` key inside `value`, depth first
/// in document order.
pub fn walk(value: &Res<'_>, field: &[u8]) -> Vec<Vec<u8>> {
    let mut paths = vec![];
    walk_fields(value, b"", false, &mut |key, path| {
        if key == field {
            paths.push(path.to_vec());
        }
    });
    paths
}

/// `schema_aware` (WalkSchema) records a literal keyword but does not descend into its value,
/// except under a name map, where the key is a property name.
fn walk_fields(value: &Res<'_>, path: &[u8], schema_aware: bool, visit: &mut dyn FnMut(&[u8], &[u8])) {
    if value.kind != Kind::Json {
        return;
    }
    value.each(|key, child| {
        let key = key.bytes();
        let child_path = join_path(path, &escape_key(&key));
        visit(&key, &child_path);
        if !(schema_aware && is_literal_value_keyword(&key) && !is_property_definition(path)) {
            walk_fields(&child, &child_path, schema_aware, visit);
        }
        true
    });
}

/// literalValueKeywords: their values are client data, never subschemas.
fn is_literal_value_keyword(key: &[u8]) -> bool {
    matches!(key, b"default" | b"example" | b"examples" | b"const" | b"enum")
}

/// isPropertyDefinition: an odd run of trailing name-map keywords means `path` is a map
/// of author-chosen names, not a schema.
fn is_property_definition(path: &[u8]) -> bool {
    let trailing = split_path(path)
        .iter()
        .rev()
        .take_while(|seg| {
            matches!(
                unescape_key(seg).as_slice(),
                b"properties"
                    | b"patternProperties"
                    | b"dependentSchemas"
                    | b"dependencies"
                    | b"$defs"
                    | b"definitions"
            )
        })
        .count();
    trailing % 2 == 1
}

fn description_path(parent: &[u8]) -> Vec<u8> {
    if parent.is_empty() || parent == b"@this" {
        return b"description".to_vec();
    }
    [parent, b".description"].concat()
}

fn contains_seq(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty() || haystack.windows(needle.len()).any(|w| w == needle)
}

/// mergeHint: an existing copy of the hint is kept as is.
fn merge_hint(existing: &[u8], hint: &[u8]) -> Vec<u8> {
    if existing.is_empty() {
        return hint.to_vec();
    }
    if existing == hint
        || existing.starts_with(&[hint, b" ("].concat())
        || contains_seq(existing, &[&b"("[..], hint, b")"].concat())
    {
        return existing.to_vec();
    }
    [existing, b" (", hint, b")"].concat()
}

fn append_hint(mut json: Vec<u8>, parent: &[u8], hint: &[u8]) -> Vec<u8> {
    let path = description_path(parent);
    let merged = merge_hint(&gj::get(&json, &path).bytes(), hint);
    gj::set_str(&mut json, &path, merged);
    json
}

fn get_strings(json: &[u8], path: &[u8]) -> Vec<Vec<u8>> {
    let arr = gj::get(json, path);
    if !arr.is_array() {
        return vec![];
    }
    arr.array().iter().map(|r| r.bytes().into_owned()).collect()
}

/// `sjson.SetBytes(json, path, []string)`: a nil (empty) slice marshals as `null`.
fn set_strings(json: &mut Vec<u8>, path: &[u8], items: &[Vec<u8>]) {
    if items.is_empty() {
        gj::set_raw(json, path, b"null");
    } else {
        gj::set_strs(json, path, items);
    }
}

fn set_raw_at(json: Vec<u8>, path: &[u8], value: &[u8]) -> Vec<u8> {
    if path.is_empty() {
        return value.to_vec();
    }
    let mut json = json;
    gj::set_raw(&mut json, path, value);
    json
}

fn delete(mut json: Vec<u8>, path: &[u8]) -> Vec<u8> {
    gj::delete(&mut json, path);
    json
}

fn ref_name(reference: &[u8]) -> Vec<u8> {
    match reference.iter().rposition(|&c| c == b'/') {
        Some(i) if i + 1 < reference.len() => pointer_unescape(&reference[i + 1..]),
        _ => reference.to_vec(),
    }
}

/// `~1` -> `/`, then `~0` -> `~` (two strings.ReplaceAll passes).
fn pointer_unescape(part: &[u8]) -> Vec<u8> {
    let replace = |s: &[u8], from: &[u8], to: u8| {
        let mut out = Vec::with_capacity(s.len());
        let mut i = 0;
        while i < s.len() {
            if s[i..].starts_with(from) {
                out.push(to);
                i += from.len();
            } else {
                out.push(s[i]);
                i += 1;
            }
        }
        out
    };
    replace(&replace(part, b"~1", b'/'), b"~0", b'~')
}

// ---------------------------------------------------------------------------------------
// Phase 0: malformed object repair (Go's `any` model)

type Map = BTreeMap<String, GoValue>;

/// json.Decoder.Decode: only the first value is decoded; the decoder reports bytes after
/// it on the next call, so they never fail this one.
fn decode_first(text: &[u8]) -> Option<GoValue> {
    let start = text.iter().position(|c| !matches!(c, b' ' | b'\t' | b'\n' | b'\r'))?;
    let rest = &text[start..];
    let end = match rest[0] {
        b't' | b'n' => 4,
        b'f' => 5,
        _ => gj::parse(rest).raw.len(),
    };
    GoValue::parse(rest.get(..end)?)
}

fn is_known_keyword_or_extension(key: &str) -> bool {
    key.starts_with("x-")
        || matches!(
            key,
            "properties"
                | "patternProperties"
                | "additionalProperties"
                | "items"
                | "prefixItems"
                | "$defs"
                | "definitions"
                | "dependentSchemas"
                | "dependentRequired"
                | "dependencies"
                | "if"
                | "then"
                | "else"
                | "not"
                | "contains"
                | "propertyNames"
                | "unevaluatedProperties"
                | "unevaluatedItems"
                | "contentSchema"
                | "additionalItems"
                | "default"
                | "const"
                | "example"
                | "examples"
                | "discriminator"
                | "xml"
                | "externalDocs"
                | "enumDescriptions"
                | "enumTitles"
        )
}

// strings.EqualFold against an ASCII word with no special Unicode folds ("object" and
// "array" contain neither k nor s) is an ASCII case-insensitive comparison.
fn is_non_object_type(t: Option<&GoValue>) -> bool {
    match t {
        Some(GoValue::String(s)) => !s.is_empty() && !s.eq_ignore_ascii_case("object"),
        Some(GoValue::Array(items)) => {
            !items
                .iter()
                .any(|i| matches!(i, GoValue::String(s) if s.eq_ignore_ascii_case("object")))
                && !items.is_empty()
        }
        _ => false,
    }
}

fn is_array_type(t: Option<&GoValue>) -> bool {
    match t {
        Some(GoValue::String(s)) => s.eq_ignore_ascii_case("array"),
        Some(GoValue::Array(items)) => items
            .iter()
            .any(|i| matches!(i, GoValue::String(s) if s.eq_ignore_ascii_case("array"))),
        _ => false,
    }
}

/// `clone["type"] == nil || clone["type"] == ""`.
fn type_unset(t: Option<&GoValue>) -> bool {
    matches!(t, None | Some(GoValue::Null)) || matches!(t, Some(GoValue::String(s)) if s.is_empty())
}

fn is_api_request_document(m: &Map) -> bool {
    for key in [
        "tools",
        "contents",
        "messages",
        "functionDeclarations",
        "function_declarations",
    ] {
        if matches!(m.get(key), Some(GoValue::Array(_))) {
            return true;
        }
    }
    matches!(m.get("request"), Some(GoValue::Object(r)) if is_api_request_document(r))
}

fn normalize_malformed_schema_objects(json: Vec<u8>, add_missing_array_items: bool) -> Vec<u8> {
    if json.is_empty() {
        return json;
    }
    let Some(root) = decode_first(&json) else {
        return json;
    };
    if root == GoValue::Bool(true) {
        return b"{}".to_vec();
    }
    let root = match root.into_object().ok() {
        Some(map) if !is_api_request_document(&map) => map,
        _ => return json,
    };
    if root.len() == 1 {
        match root.get("schema") {
            Some(GoValue::Object(inner)) => {
                let (repaired, modified) = repair_node(inner, add_missing_array_items);
                if !modified {
                    return json;
                }
                let mut wrapped = Map::new();
                wrapped.insert("schema".into(), GoValue::Object(repaired));
                return GoValue::Object(wrapped).marshal_no_html();
            }
            Some(GoValue::Bool(true)) => return br#"{"schema":{}}"#.to_vec(),
            _ => {}
        }
    }
    let (repaired, modified) = repair_node(&root, add_missing_array_items);
    if !modified {
        return json;
    }
    GoValue::Object(repaired).marshal_no_html()
}

fn string_array(value: Option<&GoValue>) -> Vec<String> {
    match value {
        Some(GoValue::Array(items)) => items
            .iter()
            .filter_map(|i| match i {
                GoValue::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        _ => vec![],
    }
}

fn merge_string_slices(existing: Vec<String>, promoted: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    existing
        .into_iter()
        .chain(promoted.iter().cloned())
        .filter(|s| !s.is_empty() && seen.insert(s.clone()))
        .collect()
}

fn promote_required(clone: &mut Map, promoted: &[String]) {
    let merged = merge_string_slices(string_array(clone.get("required")), promoted);
    // A nil []string marshals as null.
    let value = if merged.is_empty() {
        GoValue::Null
    } else {
        GoValue::Array(merged.into_iter().map(GoValue::String).collect())
    };
    clone.insert("required".into(), value);
}

fn empty_object() -> GoValue {
    GoValue::Object(Map::new())
}

fn repair_node(node: &Map, add_items: bool) -> (Map, bool) {
    let mut modified = false;
    let mut clone = node.clone();

    if !is_non_object_type(clone.get("type")) {
        let bare: Map = clone
            .iter()
            .filter(|(k, v)| matches!(v, GoValue::Object(_)) && !is_known_keyword_or_extension(k))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if !bare.is_empty() {
            let (repaired, promoted, _) = repair_property_map(&bare, add_items);
            for k in bare.keys() {
                clone.remove(k);
            }
            match clone.get_mut("properties") {
                Some(GoValue::Object(existing)) => existing.extend(repaired),
                _ => {
                    clone.insert("properties".into(), GoValue::Object(repaired));
                    clone
                        .entry("type".into())
                        .or_insert_with(|| GoValue::String("object".into()));
                }
            }
            if !promoted.is_empty() {
                promote_required(&mut clone, &promoted);
            }
            modified = true;
        }
    }

    if let Some(GoValue::Object(props)) = clone.get("properties") {
        let props = props.clone();
        if !clone.contains_key("type") {
            clone.insert("type".into(), GoValue::String("object".into()));
            modified = true;
        }
        let (repaired, promoted, props_modified) = repair_property_map(&props, add_items);
        if props_modified {
            clone.insert("properties".into(), GoValue::Object(repaired));
            modified = true;
        }
        if !promoted.is_empty() {
            promote_required(&mut clone, &promoted);
            modified = true;
        }
    }

    // A node still holding a $ref gets neither: both would clobber the target during inlining.
    if add_items && matches!(clone.get("$ref"), None | Some(GoValue::Null)) {
        if is_array_type(clone.get("type")) {
            if !clone.contains_key("items") {
                let mut items = Map::new();
                items.insert("type".into(), GoValue::String("string".into()));
                clone.insert("items".into(), GoValue::Object(items));
                modified = true;
            }
        } else if clone.contains_key("items") && type_unset(clone.get("type")) {
            clone.insert("type".into(), GoValue::String("array".into()));
            modified = true;
        }
    }

    match clone.get("items") {
        Some(GoValue::Object(items)) => {
            let (repaired, m) = repair_node(items, add_items);
            if m {
                clone.insert("items".into(), GoValue::Object(repaired));
                modified = true;
            }
        }
        Some(GoValue::Array(list)) => {
            if let Some(repaired) = repair_list(list, add_items) {
                clone.insert("items".into(), GoValue::Array(repaired));
                modified = true;
            }
        }
        Some(GoValue::Bool(true)) => {
            clone.insert("items".into(), empty_object());
            modified = true;
        }
        _ => {}
    }

    if let Some(GoValue::Object(add)) = clone.get("additionalProperties") {
        let (repaired, m) = repair_node(add, add_items);
        if m {
            clone.insert("additionalProperties".into(), GoValue::Object(repaired));
            modified = true;
        }
    }
    if let Some(GoValue::Object(pattern)) = clone.get("patternProperties") {
        let (repaired, _, m) = repair_property_map(pattern, add_items);
        if m {
            clone.insert("patternProperties".into(), GoValue::Object(repaired));
            modified = true;
        }
    }
    for key in [
        "if",
        "then",
        "else",
        "not",
        "contains",
        "propertyNames",
        "unevaluatedProperties",
        "unevaluatedItems",
        "contentSchema",
        "additionalItems",
    ] {
        match clone.get(key) {
            Some(GoValue::Object(sub)) => {
                let (repaired, m) = repair_node(sub, add_items);
                if m {
                    clone.insert(key.into(), GoValue::Object(repaired));
                    modified = true;
                }
            }
            Some(GoValue::Bool(true)) => {
                clone.insert(key.into(), empty_object());
                modified = true;
            }
            _ => {}
        }
    }
    for key in ["anyOf", "oneOf", "allOf", "prefixItems"] {
        if let Some(GoValue::Array(list)) = clone.get(key)
            && let Some(repaired) = repair_list(list, add_items)
        {
            clone.insert(key.into(), GoValue::Array(repaired));
            modified = true;
        }
    }
    for key in ["$defs", "definitions", "dependentSchemas", "dependencies"] {
        if let Some(GoValue::Object(defs)) = clone.get(key) {
            let mut repaired = Map::new();
            let mut defs_modified = false;
            for (k, v) in defs {
                let value = match v {
                    GoValue::Object(def) => {
                        let (r, m) = repair_node(def, add_items);
                        defs_modified |= m;
                        GoValue::Object(r)
                    }
                    GoValue::Bool(true) => {
                        defs_modified = true;
                        empty_object()
                    }
                    other => other.clone(),
                };
                repaired.insert(k.clone(), value);
            }
            if defs_modified {
                clone.insert(key.into(), GoValue::Object(repaired));
                modified = true;
            }
        }
    }
    (clone, modified)
}

/// repairSchemaList: `Some` only when an entry changed.
fn repair_list(list: &[GoValue], add_items: bool) -> Option<Vec<GoValue>> {
    let mut modified = false;
    let repaired = list
        .iter()
        .map(|item| match item {
            GoValue::Object(m) => {
                let (r, changed) = repair_node(m, add_items);
                modified |= changed;
                GoValue::Object(r)
            }
            GoValue::Bool(true) => {
                modified = true;
                empty_object()
            }
            other => other.clone(),
        })
        .collect();
    modified.then_some(repaired)
}

fn repair_property_map(props: &Map, add_items: bool) -> (Map, Vec<String>, bool) {
    let mut out = Map::new();
    let mut promoted = vec![];
    let mut modified = false;
    for (k, v) in props {
        match v {
            GoValue::Bool(true) => {
                out.insert(k.clone(), empty_object());
                modified = true;
            }
            GoValue::Object(child) => {
                let mut child = child.clone();
                if let Some(GoValue::Bool(required)) = child.get("required").cloned() {
                    child.remove("required");
                    modified = true;
                    if required {
                        promoted.push(k.clone());
                    }
                }
                let (repaired, m) = repair_node(&child, add_items);
                modified |= m;
                out.insert(k.clone(), GoValue::Object(repaired));
            }
            other => {
                out.insert(k.clone(), other.clone());
            }
        }
    }
    promoted.sort();
    (out, promoted, modified)
}

// ---------------------------------------------------------------------------------------
// Phase 1: references, enums and hints

/// maxInlinedSchemaBytes: shared definitions fan out once per reference, so expansions past
/// this budget degrade to a typed hint like cycles do.
const MAX_INLINED_SCHEMA_BYTES: usize = 256 * 1024;
/// maxFallbackDescriptionBytes: the longest target description a degraded reference copies.
const MAX_FALLBACK_DESCRIPTION_BYTES: usize = 512;

type Active = std::collections::HashSet<String>;

/// The inlining budget of one call, and each reference's target size. Targets never
/// change within a call, so a reference used many times is sized once (Go sizes it on
/// every use, which is 14 s for 8,000 references to one 300 KB definition).
struct Budget {
    left: usize,
    sizes: HashMap<String, usize>,
}

impl Budget {
    fn new() -> Self {
        Self {
            left: MAX_INLINED_SCHEMA_BYTES,
            sizes: HashMap::new(),
        }
    }

    /// targetExpansionSize of `reference`'s `target`, memoized.
    fn target_size(&mut self, reference: &str, target: &GoValue, for_clean: bool) -> usize {
        if let Some(size) = self.sizes.get(reference) {
            return *size;
        }
        let size = target_expansion_size(target, for_clean);
        self.sizes.insert(reference.to_owned(), size);
        size
    }
}

fn is_name_map_keyword(key: &str) -> bool {
    matches!(
        key,
        "properties" | "patternProperties" | "dependentSchemas" | "dependencies" | "$defs" | "definitions"
    )
}

fn is_definitions(key: &str) -> bool {
    key == "$defs" || key == "definitions"
}

/// inlineLocalRefs: every reference gets its own copy while the budget lasts, sibling
/// keywords override the target, and cycles end as a typed hint.
fn inline_refs(json: Vec<u8>) -> Vec<u8> {
    if !contains_seq(&json, br#""$ref""#) {
        return json;
    }
    let Some(root) = decode_first(&json) else {
        return json;
    };
    let mut budget = Budget::new();
    resolve_schema_root(&root, &mut budget, false).marshal()
}

/// inlineLocalRefsForClean: inside the single-key `{"schema": ...}` carrier, references
/// resolve against the inner schema.
fn inline_refs_for_clean(json: Vec<u8>) -> Vec<u8> {
    if !contains_seq(&json, br#""$ref""#) {
        return json;
    }
    let Some(root) = decode_first(&json) else {
        return json;
    };
    let mut budget = Budget::new();
    if let GoValue::Object(map) = &root
        && map.len() == 1
        && let Some(inner @ GoValue::Object(_)) = map.get("schema")
    {
        let resolved = resolve_schema_root(inner, &mut budget, true);
        let mut out = Map::new();
        out.insert("schema".into(), resolved);
        return GoValue::Object(out).marshal();
    }
    resolve_schema_root(&root, &mut budget, true).marshal()
}

/// resolveSchemaRoot: the schema body resolves before the definition containers, which
/// cleaning drops altogether, so the budget is spent on what is kept.
fn resolve_schema_root(root: &GoValue, budget: &mut Budget, for_clean: bool) -> GoValue {
    let GoValue::Object(root_map) = root else {
        return resolve_local_refs(root, root, &mut Active::new(), budget, for_clean);
    };
    let mut out = Map::new();
    for (key, item) in root_map {
        match key.as_str() {
            "$defs" | "definitions" => {
                if !for_clean {
                    out.insert(key.clone(), GoValue::Null); // resolved after the body
                }
            }
            "$ref" => {
                out.insert(key.clone(), item.clone());
            }
            _ => {
                let resolved = resolve_schema_entry(root, key, item, &mut Active::new(), budget, for_clean);
                out.insert(key.clone(), resolved);
            }
        }
    }
    if let Some(GoValue::String(reference)) = root_map.get("$ref")
        && reference.starts_with("#/")
        && let Some(target) = resolve_pointer(root, reference)
    {
        let size = budget.target_size(reference, target, for_clean);
        if size <= budget.left {
            budget.left -= size;
            let mut active = Active::from([reference.clone()]);
            match resolve_local_refs(root, target, &mut active, budget, for_clean).into_object() {
                Ok(mut merged) => {
                    for (key, value) in std::mem::take(&mut out) {
                        if key != "$ref" {
                            merged.insert(key, value);
                        }
                    }
                    out = merged;
                }
                Err(_) => {
                    out = resolve_ref_fallback(root, &out, target, reference, &mut Active::new(), budget, for_clean)
                }
            }
        } else {
            out = resolve_ref_fallback(root, &out, target, reference, &mut Active::new(), budget, for_clean);
        }
        out.remove("$ref");
    }
    if !for_clean {
        for key in ["$defs", "definitions"] {
            if let Some(item) = root_map.get(key) {
                let resolved = resolve_schema_entry(root, key, item, &mut Active::new(), budget, false);
                out.insert(key.into(), resolved);
            }
        }
    }
    GoValue::Object(out)
}

/// targetExpansionSize: when cleaning, definitions inside the target are deleted later, so
/// they cost nothing.
fn target_expansion_size(value: &GoValue, for_clean: bool) -> usize {
    if for_clean {
        clean_expansion_size(value, false)
    } else {
        json_value_size(value)
    }
}

fn clean_expansion_size(value: &GoValue, in_name_map: bool) -> usize {
    const CAP: usize = MAX_INLINED_SCHEMA_BYTES + 1;
    match value {
        GoValue::Array(items) => {
            let mut size = 2;
            for item in items {
                size += clean_expansion_size(item, false) + 1;
                if size > CAP {
                    return CAP;
                }
            }
            size
        }
        GoValue::Object(map) => {
            let mut size = 2;
            for (key, item) in map {
                if !in_name_map && is_definitions(key) {
                    continue;
                }
                let child = if !in_name_map && is_literal_value_keyword(key.as_bytes()) {
                    json_value_size(item)
                } else {
                    clean_expansion_size(item, !in_name_map && is_name_map_keyword(key))
                };
                size += json_string_encoded_size(key) + 1 + child + 1;
                if size > CAP {
                    return CAP;
                }
            }
            size
        }
        scalar => json_value_size(scalar),
    }
}

/// jsonValueSize: the bytes encoding/json writes for a decoded value.
fn json_value_size(value: &GoValue) -> usize {
    match value {
        GoValue::Null => 4,
        GoValue::Bool(_) => 5,
        GoValue::Number(n) => n.len(),
        GoValue::String(s) => json_string_encoded_size(s),
        GoValue::Array(items) => 2 + items.iter().map(|i| json_value_size(i) + 1).sum::<usize>(),
        GoValue::Object(map) => {
            2 + map
                .iter()
                .map(|(k, v)| json_string_encoded_size(k) + 1 + json_value_size(v) + 1)
                .sum::<usize>()
        }
    }
}

/// jsonStringEncodedSize: with encoding/json's HTML escaping.
fn json_string_encoded_size(s: &str) -> usize {
    let b = s.as_bytes();
    let mut size = 2;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        size += match c {
            b'\n' | b'\r' | b'\t' => 2,
            0..0x20 => 6,
            b'"' | b'\\' => 2,
            b'<' | b'>' | b'&' => 6,
            0xe2 if i + 2 < b.len() && b[i + 1] == 0x80 && matches!(b[i + 2], 0xa8 | 0xa9) => {
                i += 2;
                6
            }
            _ => 1,
        };
        i += 1;
    }
    size
}

fn resolve_local_refs(
    root: &GoValue,
    value: &GoValue,
    active: &mut Active,
    budget: &mut Budget,
    for_clean: bool,
) -> GoValue {
    match value {
        GoValue::Array(items) => GoValue::Array(
            items
                .iter()
                .map(|i| resolve_local_refs(root, i, active, budget, for_clean))
                .collect(),
        ),
        GoValue::Object(node) => {
            if let Some(GoValue::String(reference)) = node.get("$ref")
                && reference.starts_with("#/")
                && let Some(target) = resolve_pointer(root, reference)
            {
                if active.contains(reference) {
                    return GoValue::Object(resolve_ref_fallback(
                        root, node, target, reference, active, budget, for_clean,
                    ));
                }
                // Each reference charges its stored target; nested references charge their own.
                let size = budget.target_size(reference, target, for_clean);
                if size > budget.left {
                    return GoValue::Object(resolve_ref_fallback(
                        root, node, target, reference, active, budget, for_clean,
                    ));
                }
                budget.left -= size;
                active.insert(reference.clone());
                let resolved = resolve_local_refs(root, target, active, budget, for_clean);
                active.remove(reference);
                if let Ok(target_map) = resolved.into_object() {
                    let mut out = Map::new();
                    for (key, item) in target_map {
                        if !(for_clean && is_definitions(&key)) {
                            out.insert(key, item);
                        }
                    }
                    for (key, item) in node {
                        if key == "$ref" || (for_clean && is_definitions(key)) {
                            continue;
                        }
                        let resolved = resolve_schema_entry(root, key, item, active, budget, for_clean);
                        out.insert(key.clone(), resolved);
                    }
                    return GoValue::Object(out);
                }
            }
            let mut out = Map::new();
            for (key, item) in node {
                if for_clean && is_definitions(key) {
                    continue;
                }
                let resolved = resolve_schema_entry(root, key, item, active, budget, for_clean);
                out.insert(key.clone(), resolved);
            }
            GoValue::Object(out)
        }
        other => other.clone(),
    }
}

/// resolveRefFallback: a reference that cannot expand (cycle or spent budget) becomes a
/// typed hint, and its sibling subschemas still resolve.
fn resolve_ref_fallback(
    root: &GoValue,
    node: &Map,
    target: &GoValue,
    reference: &str,
    active: &mut Active,
    budget: &mut Budget,
    for_clean: bool,
) -> Map {
    let mut out = cyclic_ref_fallback(node, target, reference);
    if let GoValue::Object(target_map) = target
        && let Some(GoValue::String(description)) = target_map.get("description")
        && !description.is_empty()
        && description.len() <= MAX_FALLBACK_DESCRIPTION_BYTES
        && !node.contains_key("description")
    {
        let cost = json_string_encoded_size(description);
        if cost > budget.left {
            let hint = [&b"See: "[..], &ref_name(reference.as_bytes())].concat();
            out.insert(
                "description".into(),
                GoValue::String(String::from_utf8_lossy(&hint).into_owned()),
            );
        } else {
            budget.left -= cost;
        }
    }
    for (key, item) in node {
        if key == "$ref" || key == "description" {
            continue;
        }
        if for_clean && is_definitions(key) {
            out.remove(key);
            continue;
        }
        let resolved = resolve_schema_entry(root, key, item, active, budget, for_clean);
        out.insert(key.clone(), resolved);
    }
    out
}

/// resolveSchemaEntry: literal values are copied verbatim; name-map entries are subschemas.
fn resolve_schema_entry(
    root: &GoValue,
    key: &str,
    item: &GoValue,
    active: &mut Active,
    budget: &mut Budget,
    for_clean: bool,
) -> GoValue {
    if is_literal_value_keyword(key.as_bytes()) {
        return item.clone();
    }
    if is_name_map_keyword(key)
        && let GoValue::Object(names) = item
    {
        return GoValue::Object(
            names
                .iter()
                .map(|(name, v)| (name.clone(), resolve_local_refs(root, v, active, budget, for_clean)))
                .collect(),
        );
    }
    resolve_local_refs(root, item, active, budget, for_clean)
}

fn resolve_pointer<'a>(root: &'a GoValue, reference: &str) -> Option<&'a GoValue> {
    let mut current = root;
    for raw in reference["#/".len()..].split('/') {
        let part = String::from_utf8(pointer_unescape(raw.as_bytes())).unwrap_or_default();
        current = match current {
            GoValue::Object(map) => map.get(&part)?,
            GoValue::Array(items) => {
                let index: i64 = part.parse().ok()?;
                items.get(usize::try_from(index).ok()?)?
            }
            _ => return None,
        };
    }
    Some(current)
}

fn cyclic_ref_fallback(node: &Map, target: &GoValue, reference: &str) -> Map {
    let mut out = Map::new();
    if let GoValue::Object(target) = target {
        for key in ["type", "nullable", "description"] {
            if let Some(v) = target.get(key) {
                if key == "description" && matches!(v, GoValue::String(d) if d.len() > MAX_FALLBACK_DESCRIPTION_BYTES) {
                    continue;
                }
                out.insert(key.into(), v.clone());
            }
        }
    }
    for (key, value) in node {
        if key != "$ref" {
            out.insert(key.clone(), value.clone());
        }
    }
    let hint = [&b"See: "[..], &ref_name(reference.as_bytes())].concat();
    let description = match out.get("description") {
        Some(GoValue::String(d)) if !d.is_empty() => merge_hint(d.as_bytes(), &hint),
        _ => hint,
    };
    out.insert(
        "description".into(),
        GoValue::String(String::from_utf8_lossy(&description).into_owned()),
    );
    out
}

fn convert_refs_to_hints(mut json: Vec<u8>, preserve_siblings: bool) -> Vec<u8> {
    for p in depth_sorted(find_paths(&json, b"$ref")) {
        let parent = trim_suffix(&p, b".$ref").to_vec();
        if is_property_definition(&parent) {
            continue; // a property literally named "$ref"
        }
        let name = ref_name(&gj::get(&json, &p).bytes());
        let mut hint = [&b"See: "[..], &name].concat();
        if !preserve_siblings {
            let existing = gj::get(&json, &description_path(&parent)).bytes().into_owned();
            if !existing.is_empty() {
                hint = [&existing[..], b" (", &hint, b")"].concat();
            }
            let mut replacement = br#"{"type":"object","description":""}"#.to_vec();
            gj::set_str(&mut replacement, "description", &hint);
            json = set_raw_at(json, &parent, &replacement);
            continue;
        }
        json = delete(json, &p);
        json = append_hint(json, &parent, &hint);
    }
    json
}

fn convert_const_to_enum(mut json: Vec<u8>) -> Vec<u8> {
    for p in find_paths(&json, b"const") {
        let value = gj::get(&json, &p).into_owned();
        if !value.exists() {
            continue;
        }
        let enum_path = [trim_suffix(&p, b".const"), b".enum"].concat();
        if !gj::get(&json, &enum_path).exists() {
            AnyValue::Array(vec![AnyValue::from_res(&value)]).set(&mut json, &enum_path);
        }
    }
    json
}

fn convert_enum_values_to_strings(mut json: Vec<u8>, force_string_type: bool) -> Vec<u8> {
    for p in find_paths(&json, b"enum") {
        let arr = gj::get(&json, &p);
        if !arr.is_array() {
            continue;
        }
        let values: Vec<Vec<u8>> = arr.array().iter().map(|i| i.bytes().into_owned()).collect();
        set_strings(&mut json, &p, &values);
        if force_string_type {
            let parent = trim_suffix(&p, b".enum").to_vec();
            gj::set_str(&mut json, &join_path(&parent, b"type"), "string");
        }
    }
    json
}

fn add_enum_hints(mut json: Vec<u8>) -> Vec<u8> {
    for p in find_paths(&json, b"enum") {
        let arr = gj::get(&json, &p);
        if !arr.is_array() {
            continue;
        }
        let items = arr.array();
        if items.len() <= 1 || items.len() > 10 {
            continue;
        }
        let values: Vec<Vec<u8>> = items.iter().map(|i| i.bytes().into_owned()).collect();
        let hint = [&b"Allowed: "[..], &values.join(&b", "[..])].concat();
        json = append_hint(json, trim_suffix(&p, b".enum"), &hint);
    }
    json
}

fn drop_ignored_enums_to_hints(mut json: Vec<u8>, options: Options) -> Vec<u8> {
    for p in find_paths(&json, b"enum") {
        let parent = trim_suffix(&p, b".enum").to_vec();
        let drop = options.drop_all_enums
            || (options.drop_boolean_enums
                && gj::get(&json, &join_path(&parent, b"type")).bytes().as_ref() == b"boolean");
        if !drop {
            continue;
        }
        let values = gj::get(&json, &p);
        if values.is_array() {
            let items = values.array();
            if items.len() == 1 {
                let hint = [&b"Allowed: "[..], &items[0].bytes()].concat();
                json = append_hint(json, &parent, &hint);
            }
        }
        json = delete(json, &p);
    }
    json
}

fn add_additional_properties_hints(mut json: Vec<u8>) -> Vec<u8> {
    for p in find_paths(&json, b"additionalProperties") {
        if gj::get(&json, &p).kind == Kind::False {
            json = append_hint(
                json,
                trim_suffix(&p, b".additionalProperties"),
                b"No extra properties allowed",
            );
        }
    }
    json
}

const UNSUPPORTED_CONSTRAINTS: [&[u8]; 12] = [
    b"minLength",
    b"maxLength",
    b"exclusiveMinimum",
    b"exclusiveMaximum",
    b"pattern",
    b"minItems",
    b"maxItems",
    b"uniqueItems",
    b"contains",
    b"format",
    b"default",
    b"examples",
];

fn constraint_keywords(options: Options) -> Vec<&'static [u8]> {
    if options.preserve_standard_constraints {
        return vec![];
    }
    let mut keywords = UNSUPPORTED_CONSTRAINTS.to_vec();
    if options.antigravity_semantics {
        keywords.extend([&b"minimum"[..], b"maximum", b"multipleOf"]);
    }
    keywords
}

fn move_constraints_to_description(mut json: Vec<u8>, options: Options) -> Vec<u8> {
    let constraints = constraint_keywords(options);
    if constraints.is_empty() {
        return json;
    }
    let by_field = find_paths_by_fields(&json, &constraints);
    for key in &constraints {
        for p in by_field.get(*key).into_iter().flatten() {
            let value = gj::get(&json, p).into_owned();
            if !value.exists() {
                continue;
            }
            let parent = trim_suffix(p, &[b".", *key].concat()).to_vec();
            if is_property_definition(&parent) {
                continue;
            }
            let shown = if value.is_object() || value.is_array() {
                value.raw.to_vec()
            } else {
                value.bytes().into_owned()
            };
            json = append_hint(json, &parent, &[*key, b": ", &shown].concat());
        }
    }
    json
}

fn move_not_to_description(mut json: Vec<u8>) -> Vec<u8> {
    for p in find_paths(&json, b"not") {
        let value = gj::get(&json, &p).into_owned();
        let parent = trim_suffix(&p, b".not").to_vec();
        if !value.exists() || is_property_definition(&parent) {
            continue;
        }
        json = append_hint(json, &parent, &[&b"not: "[..], &value.raw].concat());
    }
    json
}

// ---------------------------------------------------------------------------------------
// Phase 2: flattening

fn merge_conditionals(mut json: Vec<u8>) -> Vec<u8> {
    let by_field = find_paths_by_fields(&json, &[b"then", b"else"]);
    let mut paths = vec![];
    for key in [&b"then"[..], b"else"] {
        for p in by_field.get(key).into_iter().flatten() {
            if !is_property_definition(trim_suffix(p, &[b".", key].concat())) {
                paths.push(p.clone());
            }
        }
    }
    for p in depth_sorted(paths) {
        let props = gj::get(&json, &join_path(&p, b"properties")).into_owned();
        if !props.is_object() {
            continue;
        }
        let parent: Vec<u8> = if let Some(parent) = p.strip_suffix(b".then").or_else(|| p.strip_suffix(b".else")) {
            parent.to_vec()
        } else if p == b"then" || p == b"else" {
            vec![]
        } else {
            continue;
        };
        props.each(|key, value| {
            let dest = join_path(&parent, &[&b"properties."[..], &escape_key(&key.bytes())].concat());
            if !gj::get(&json, &dest).exists() {
                gj::set_raw(&mut json, &dest, &value.raw);
            }
            true
        });
    }
    json
}

fn merge_all_of(mut json: Vec<u8>) -> Vec<u8> {
    for p in depth_sorted(find_paths(&json, b"allOf")) {
        let all_of = gj::get(&json, &p).into_owned();
        if !all_of.is_array() {
            continue;
        }
        let parent = trim_suffix(&p, b".allOf").to_vec();
        for item in all_of.array() {
            if !item.is_object() {
                continue;
            }
            item.each(|key, value| {
                let field = key.bytes();
                match field.as_ref() {
                    b"required" => {
                        if !value.is_array() {
                            return true;
                        }
                        let path = join_path(&parent, b"required");
                        let mut current = get_strings(&json, &path);
                        for required in value.array() {
                            let name = required.bytes().into_owned();
                            if !current.contains(&name) {
                                current.push(name);
                            }
                        }
                        set_strings(&mut json, &path, &current);
                    }
                    b"if" | b"then" | b"else" | b"allOf" => {}
                    _ => {
                        let dest = join_path(&parent, &escape_key(&field));
                        json = merge_missing_schema(std::mem::take(&mut json), &dest, &value);
                    }
                }
                true
            });
        }
        json = delete(json, &p);
    }
    json
}

/// mergeMissingSchemaAtPath: fills absent fields, never replacing an existing one.
fn merge_missing_schema(mut json: Vec<u8>, dest: &[u8], incoming: &Res<'_>) -> Vec<u8> {
    let existing = gj::get(&json, dest);
    if !existing.exists() {
        gj::set_raw(&mut json, dest, &incoming.raw);
        return json;
    }
    if !existing.is_object() || !incoming.is_object() {
        return json;
    }
    incoming.each(|key, value| {
        let child = join_path(dest, &escape_key(&key.bytes()));
        json = merge_missing_schema(std::mem::take(&mut json), &child, &value);
        true
    });
    json
}

fn merge_description_raw(schema: &[u8], parent_desc: &[u8]) -> Vec<u8> {
    let child = gj::get(schema, "description").bytes().into_owned();
    let mut out = schema.to_vec();
    if child.is_empty() {
        gj::set_str(&mut out, "description", parent_desc);
    } else if child != parent_desc {
        gj::set_str(&mut out, "description", [parent_desc, b" (", &child, b")"].concat());
    }
    out
}

fn type_of(item: &Res<'_>) -> Vec<u8> {
    item.get("type").bytes().into_owned()
}

/// selectBest: objects beat arrays beat other types beat null; the first best wins.
fn select_best(items: &[Res<'_>]) -> (usize, Vec<Vec<u8>>) {
    let mut best = (0, -1);
    let mut types = vec![];
    for (i, item) in items.iter().enumerate() {
        let mut t = type_of(item);
        let score = if t == b"object" || item.get("properties").exists() {
            if t.is_empty() {
                t = b"object".to_vec();
            }
            3
        } else if t == b"array" || item.get("items").exists() {
            if t.is_empty() {
                t = b"array".to_vec();
            }
            2
        } else if !t.is_empty() && t != b"null" {
            1
        } else {
            0
        };
        if !t.is_empty() {
            types.push(t);
        }
        if score > best.1 {
            best = (i, score);
        }
    }
    (best.0, types)
}

fn is_required_only_union_branch(item: &Res<'_>) -> bool {
    item.is_object()
        && item.get("required").exists()
        && ["type", "properties", "items", "enum", "anyOf", "oneOf", "allOf"]
            .iter()
            .all(|k| !item.get(k).exists())
}

/// materializeRequiredOnlyBranch: a `{"required": [...]}` branch becomes an object branch
/// with the parent's definitions of those properties.
fn materialize_required_only_branch(item: &Res<'_>, parent_props: &Res<'_>) -> Option<Vec<u8>> {
    if !parent_props.is_object() {
        return None;
    }
    let required = get_strings(&item.raw, b"required");
    if required.is_empty() {
        return None;
    }
    let mut raw = item.raw.to_vec();
    for name in &required {
        let escaped = escape_key(name);
        let prop = parent_props.get(&escaped);
        if !prop.exists() {
            return None;
        }
        gj::set_raw(&mut raw, &[&b"properties."[..], &escaped].concat(), &prop.raw);
    }
    gj::set_str(&mut raw, "type", "object");
    Some(gj::parse(&raw).raw.to_vec())
}

fn is_null_only_branch(item: &Res<'_>) -> bool {
    if item.get("properties").exists() || item.get("items").exists() {
        return false;
    }
    let t = item.get("type");
    if t.kind == Kind::String {
        return t.bytes().as_ref() == b"null";
    }
    t.is_array() && !t.array().is_empty() && t.array().iter().all(|e| e.bytes().as_ref() == b"null")
}

/// partitionUnionBranches: the non-null branches (raw), whether a null branch was seen,
/// and whether any branch was dropped or rewritten.
fn partition_union_branches(items: &[Res<'_>], parent_props: &Res<'_>) -> (Vec<Vec<u8>>, bool, bool) {
    let (mut non_null, mut has_null, mut filtered) = (vec![], false, false);
    for item in items {
        if item.kind == Kind::True {
            non_null.push(b"{}".to_vec());
            filtered = true;
        } else if !item.is_object() {
            filtered = true;
        } else if is_null_only_branch(item) {
            has_null = true;
        } else if is_required_only_union_branch(item) {
            if let Some(materialized) = materialize_required_only_branch(item, parent_props) {
                non_null.push(materialized);
            }
            filtered = true;
        } else {
            non_null.push(item.raw.to_vec());
        }
    }
    (non_null, has_null, filtered)
}

/// flattenAnyOfOneOf: multi-branch unions are kept (oneOf becomes anyOf on native Schema
/// carriers) unless `flatten_multi_branch`; other unions project onto the strongest branch.
fn flatten_any_of_one_of(mut json: Vec<u8>, preserve_one_of: bool, flatten_multi_branch: bool) -> Vec<u8> {
    let mut by_field = find_paths_by_fields(&json, &[b"oneOf", b"anyOf"]);
    let mut paths = by_field.remove(&b"oneOf"[..]).unwrap_or_default();
    paths.extend(by_field.remove(&b"anyOf"[..]).unwrap_or_default());
    for p in depth_sorted(paths) {
        let key: &[u8] = if p == b"oneOf" || p.ends_with(b".oneOf") {
            b"oneOf"
        } else {
            b"anyOf"
        };
        let arr = gj::get(&json, &p).into_owned();
        if !arr.is_array() || arr.array().is_empty() {
            continue;
        }
        let parent_path = trim_suffix(&p, &[b".", key].concat()).to_vec();
        if is_property_definition(&parent_path) {
            continue;
        }
        let parent = if parent_path.is_empty() {
            gj::parse(&json).into_owned()
        } else {
            gj::get(&json, &parent_path).into_owned()
        };
        // A native Schema has a single anyOf: a sibling oneOf moves to a hint.
        if key == b"oneOf" && !flatten_multi_branch && !preserve_one_of && parent.get("anyOf").is_array() {
            json = append_hint(json, &parent_path, &[&b"oneOf: "[..], &arr.raw].concat());
            json = delete(json, &p);
            continue;
        }
        let items = arr.array();
        let parent_props = parent.get("properties");
        let (non_null, has_null, filtered) = partition_union_branches(&items, &parent_props);
        // An earlier pass may already have recorded the null branch as nullable:true.
        let nullable = has_null || parent.get("nullable").bool();

        if non_null.len() >= 2 && !flatten_multi_branch {
            let target_key: &[u8] = if key == b"oneOf" && !preserve_one_of {
                b"anyOf"
            } else {
                key
            };
            if target_key != key || has_null || filtered {
                // sjson.SetBytes with []json.RawMessage: Marshal compacts each branch.
                let mut list = b"[".to_vec();
                for (i, raw) in non_null.iter().enumerate() {
                    if i > 0 {
                        list.push(b',');
                    }
                    list.extend(gj::compact(raw, true));
                }
                list.push(b']');
                gj::set_raw(&mut json, &join_path(&parent_path, target_key), &list);
                if target_key != key {
                    json = delete(json, &p);
                }
            }
            if has_null {
                gj::set_bool(&mut json, &join_path(&parent_path, b"nullable"), true);
                json = append_hint(json, &parent_path, b"(nullable)");
            }
            continue;
        }

        if parent_props.is_object() {
            for raw in &non_null {
                let branch = gj::parse(raw);
                let branch_props = branch.get("properties");
                if branch_props.is_object() {
                    branch_props.each(|prop_key, prop_value| {
                        let dest = join_path(
                            &parent_path,
                            &[&b"properties."[..], &escape_key(&prop_key.bytes())].concat(),
                        );
                        json = merge_missing_schema(std::mem::take(&mut json), &dest, &prop_value);
                        true
                    });
                }
            }
            if nullable {
                gj::set_bool(&mut json, &join_path(&parent_path, b"nullable"), true);
            }
            if parent.get("type").bytes().is_empty() {
                gj::set_str(&mut json, &join_path(&parent_path, b"type"), "object");
            }
            json = delete(json, &p);
            continue;
        }

        if non_null.is_empty() {
            if !has_null {
                json = delete(json, &p);
                continue;
            }
            let mut selected = br#"{"type":"null"}"#.to_vec();
            let parent_desc = gj::get(&json, &description_path(&parent_path)).bytes().into_owned();
            if !parent_desc.is_empty() {
                selected = merge_description_raw(&selected, &parent_desc);
            }
            json = set_raw_at(json, &parent_path, &selected);
            continue;
        }

        let parsed: Vec<Res<'_>> = non_null.iter().map(|raw| gj::parse(raw)).collect();
        let null_branch = gj::parse(br#"{"type":"null"}"#);
        let all_types = if filtered || non_null.len() >= 2 {
            let mut candidates = parsed.clone();
            if has_null {
                candidates.push(null_branch);
            }
            select_best(&candidates).1
        } else {
            select_best(&items).1
        };
        let parent_desc = gj::get(&json, &description_path(&parent_path)).bytes().into_owned();
        let mut selected = non_null[select_best(&parsed).0].clone();
        if nullable {
            gj::set_bool(&mut selected, "nullable", true);
        }
        if !parent_desc.is_empty() {
            selected = merge_description_raw(&selected, &parent_desc);
        }
        let hint = if all_types.len() > 1 {
            Some([&b"Accepts: "[..], &all_types.join(&b" | "[..])].concat())
        } else if nullable {
            Some(b"(nullable)".to_vec())
        } else {
            None
        };
        if let Some(hint) = hint {
            let merged = merge_hint(&gj::get(&selected, "description").bytes(), &hint);
            gj::set_str(&mut selected, "description", merged);
        }
        json = set_raw_at(json, &parent_path, &selected);
    }
    json
}

fn flatten_type_arrays(mut json: Vec<u8>, preserve_native_nullable: bool) -> Vec<u8> {
    let mut nullable_fields: Vec<(Vec<u8>, Vec<Vec<u8>>)> = vec![];
    for p in depth_sorted(find_paths(&json, b"type")) {
        let res = gj::get(&json, &p);
        if !res.is_array() {
            continue;
        }
        let values = res.array();
        if values.is_empty() {
            continue;
        }
        let mut has_null = false;
        let mut non_null: Vec<Vec<u8>> = vec![];
        for item in &values {
            let s = item.bytes();
            if s.as_ref() == b"null" {
                has_null = true;
            } else if !s.is_empty() {
                non_null.push(s.into_owned());
            }
        }
        let parent = trim_suffix(&p, b".type").to_vec();
        let items_path = join_path(&parent, b"items");
        let first = if non_null.is_empty() {
            b"string".to_vec()
        } else if gj::get(&json, &items_path).exists() && non_null.iter().any(|t| t == b"array") {
            b"array".to_vec()
        } else if gj::get(&json, &join_path(&parent, b"properties")).exists() && non_null.iter().any(|t| t == b"object")
        {
            b"object".to_vec()
        } else {
            non_null[0].clone()
        };
        gj::set_str(&mut json, &p, &first);
        if first != b"array" && gj::get(&json, &items_path).exists() {
            gj::delete(&mut json, &items_path);
        }
        if non_null.len() > 1 {
            json = append_hint(
                json,
                &parent,
                &[&b"Accepts: "[..], &non_null.join(&b" | "[..])].concat(),
            );
        }
        if has_null {
            if preserve_native_nullable {
                gj::set_bool(&mut json, &join_path(&parent, b"nullable"), true);
                json = append_hint(json, &parent, b"(nullable)");
                continue;
            }
            let parts = split_path(&p);
            if parts.len() >= 3 && parts[parts.len() - 3] == b"properties" {
                let escaped = parts[parts.len() - 2].to_vec();
                let object_path = parts[..parts.len() - 3].join(&b'.');
                let field = unescape_key(&escaped);
                match nullable_fields.iter_mut().find(|(o, _)| *o == object_path) {
                    Some((_, fields)) => fields.push(field),
                    None => nullable_fields.push((object_path.clone(), vec![field])),
                }
                let target = join_path(&object_path, &[&b"properties."[..], &escaped].concat());
                json = append_hint(json, &target, b"(nullable)");
            }
        }
    }
    for (object_path, fields) in nullable_fields {
        let path = join_path(&object_path, b"required");
        let required = gj::get(&json, &path);
        if !required.is_array() {
            continue;
        }
        let kept: Vec<Vec<u8>> = required
            .array()
            .iter()
            .map(|r| r.bytes().into_owned())
            .filter(|r| !fields.contains(r))
            .collect();
        if kept.is_empty() {
            gj::delete(&mut json, &path);
        } else {
            gj::set_strs(&mut json, &path, &kept);
        }
    }
    json
}

// ---------------------------------------------------------------------------------------
// Phase 3: cleanup

fn delete_keyword_paths(mut json: Vec<u8>, keywords: &[&[u8]], keep: impl Fn(&[u8], &[u8], &[u8]) -> bool) -> Vec<u8> {
    let by_field = find_paths_by_fields(&json, keywords);
    let mut paths = vec![];
    for key in keywords {
        for p in by_field.get(*key).into_iter().flatten() {
            if is_property_definition(trim_suffix(p, &[b".", *key].concat())) || keep(key, p, &json) {
                continue;
            }
            paths.push(p.clone());
        }
    }
    for p in depth_sorted(paths) {
        gj::delete(&mut json, &p);
    }
    json
}

fn remove_unsupported_keywords(json: Vec<u8>, options: Options) -> Vec<u8> {
    let mut keywords = constraint_keywords(options);
    keywords.extend([
        &b"$schema"[..],
        b"$defs",
        b"definitions",
        b"const",
        b"$ref",
        b"$id",
        b"id",
        b"additionalProperties",
        b"$anchor",
        b"$vocabulary",
        b"$dynamicRef",
        b"$dynamicAnchor",
        b"propertyNames",
        b"patternProperties",
        b"dependentRequired",
        b"dependentSchemas",
        b"dependencies",
        b"if",
        b"then",
        b"else",
        b"$comment",
        b"enumDescriptions",
        b"enumTitles",
        b"prefill",
        b"deprecated",
        b"encrypted",
        b"discriminator",
        b"readOnly",
        b"writeOnly",
        b"additionalItems",
        b"unevaluatedProperties",
        b"unevaluatedItems",
        b"contentSchema",
        b"contentMediaType",
        b"contentEncoding",
    ]);
    if options.antigravity_semantics {
        keywords.push(b"not");
    }
    let json = delete_keyword_paths(json, &keywords, |key, p, json| {
        key == b"additionalProperties"
            && (options.preserve_all_additional_properties
                || (options.preserve_additional_properties_false && gj::get(json, p).kind == Kind::False))
    });
    remove_extension_fields(json)
}

fn remove_extension_fields(mut json: Vec<u8>) -> Vec<u8> {
    let mut paths = vec![];
    walk_extensions(&gj::parse(&json), b"", &mut paths);
    for p in paths {
        gj::delete(&mut json, &p);
    }
    json
}

fn walk_extensions(value: &Res<'_>, path: &[u8], paths: &mut Vec<Vec<u8>>) {
    if value.is_array() {
        let items = value.array();
        for (i, item) in items.iter().enumerate().rev() {
            walk_extensions(item, &join_path(path, i.to_string().as_bytes()), paths);
        }
        return;
    }
    if value.is_object() {
        value.each(|key, child| {
            let key = key.bytes();
            let child_path = join_path(path, &escape_key(&key));
            if key.starts_with(b"x-") && !is_property_definition(path) {
                paths.push(child_path);
            } else if is_literal_value_keyword(&key) && !is_property_definition(path) {
                // literal values are client data
            } else {
                walk_extensions(&child, &child_path, paths);
            }
            true
        });
    }
}

fn remove_keywords(json: Vec<u8>, keywords: &[&[u8]]) -> Vec<u8> {
    delete_keyword_paths(json, keywords, |_, _, _| false)
}

fn filter_required(json: &mut Vec<u8>, parent: &[u8], name: &[u8]) {
    let path = join_path(parent, b"required");
    let required = gj::get(json, &path);
    if !required.is_array() {
        return;
    }
    let kept: Vec<Vec<u8>> = required
        .array()
        .iter()
        .map(|r| r.bytes().into_owned())
        .filter(|r| r != name)
        .collect();
    if kept.is_empty() {
        gj::delete(json, &path);
    } else {
        gj::set_strs(json, &path, &kept);
    }
}

fn remove_placeholder_fields(mut json: Vec<u8>) -> Vec<u8> {
    for p in depth_sorted(find_paths(&json, b"_")) {
        let Some(parent) = p.strip_suffix(b".properties._") else {
            continue;
        };
        gj::delete(&mut json, &p);
        filter_required(&mut json, parent, b"_");
    }
    for p in depth_sorted(find_paths(&json, b"reason")) {
        let Some(parent) = p.strip_suffix(b".properties.reason") else {
            continue;
        };
        let props = gj::get(&json, &join_path(parent, b"properties"));
        let distinct: std::collections::HashSet<Vec<u8>> = props.map().into_iter().map(|(k, _)| k).collect();
        if !props.is_object() || distinct.len() != 1 {
            continue;
        }
        if gj::get(&json, &[&p[..], b".description"].concat()).bytes().as_ref() != PLACEHOLDER_REASON_DESCRIPTION {
            continue;
        }
        gj::delete(&mut json, &p);
        filter_required(&mut json, parent, b"reason");
    }
    json
}

fn cleanup_required_fields(mut json: Vec<u8>) -> Vec<u8> {
    for p in find_paths(&json, b"required") {
        let parent = trim_suffix(&p, b".required").to_vec();
        let required = gj::get(&json, &p).into_owned();
        let props = gj::get(&json, &join_path(&parent, b"properties")).into_owned();
        if !required.is_array() {
            continue;
        }
        if !props.is_object() {
            gj::delete(&mut json, &p);
            continue;
        }
        let names = required.array();
        let valid: Vec<Vec<u8>> = names
            .iter()
            .map(|r| r.bytes().into_owned())
            .filter(|key| props.get(&escape_key(key)).exists())
            .collect();
        if valid.len() != names.len() {
            if valid.is_empty() {
                gj::delete(&mut json, &p);
            } else {
                gj::set_strs(&mut json, &p, &valid);
            }
        }
    }
    json
}

/// sanitizeObjectProperties: a node declaring `properties` is typed `object`.
fn sanitize_object_properties(mut json: Vec<u8>) -> Vec<u8> {
    for p in depth_sorted(find_paths(&json, b"properties")) {
        let parent = trim_suffix(&p, b".properties").to_vec();
        if is_property_definition(&parent) || !gj::get(&json, &p).is_object() {
            continue;
        }
        let type_path = join_path(&parent, b"type");
        if !gj::get(&json, &type_path).bytes().eq_ignore_ascii_case(b"object") {
            gj::set_str(&mut json, &type_path, "object");
        }
    }
    json
}

/// sanitizeArrayItems; with `add_missing`, an array node still lacking `items` after
/// reference inlining gets Phase 0's placeholder.
fn sanitize_array_items(mut json: Vec<u8>, add_missing: bool) -> Vec<u8> {
    for p in depth_sorted(find_paths(&json, b"items")) {
        let parent = trim_suffix(&p, b".items").to_vec();
        if is_property_definition(&parent) {
            continue;
        }
        let type_path = join_path(&parent, b"type");
        let t = gj::get(&json, &type_path).bytes().into_owned();
        if t.is_empty() {
            gj::set_str(&mut json, &type_path, "array");
        } else if !t.eq_ignore_ascii_case(b"array") {
            gj::delete(&mut json, &p);
        }
    }
    if !add_missing {
        return json;
    }
    for p in depth_sorted(find_paths(&json, b"type")) {
        let parent = trim_suffix(&p, b".type").to_vec();
        if is_property_definition(&parent) || !is_array_declared(&gj::get(&json, &p)) {
            continue;
        }
        if gj::get(&json, &join_path(&parent, b"items")).exists()
            || gj::get(&json, &join_path(&parent, b"$ref")).exists()
        {
            continue;
        }
        gj::set_raw(&mut json, &join_path(&parent, b"items"), br#"{"type":"string"}"#);
    }
    json
}

/// isArrayDeclaredType on a gjson value.
fn is_array_declared(t: &Res<'_>) -> bool {
    match t.kind {
        Kind::String => t.bytes().eq_ignore_ascii_case(b"array"),
        _ if t.is_array() => t
            .array()
            .iter()
            .any(|i| i.kind == Kind::String && i.bytes().eq_ignore_ascii_case(b"array")),
        _ => false,
    }
}

fn add_empty_schema_placeholder(mut json: Vec<u8>) -> Vec<u8> {
    for p in depth_sorted(find_paths(&json, b"type")) {
        if gj::get(&json, &p).bytes().as_ref() != b"object" {
            continue;
        }
        let parent = trim_suffix(&p, b".type").to_vec();
        let props_path = join_path(&parent, b"properties");
        let props = gj::get(&json, &props_path).into_owned();
        let required_path = join_path(&parent, b"required");
        let required = gj::get(&json, &required_path);
        let has_required = required.is_array() && !required.array().is_empty();
        if gj::get(&json, &join_path(&parent, b"anyOf")).is_array()
            || gj::get(&json, &join_path(&parent, b"oneOf")).is_array()
        {
            continue;
        }
        if !props.exists() || (props.is_object() && props.map().is_empty()) {
            let reason = join_path(&props_path, b"reason");
            gj::set_str(&mut json, &[&reason[..], b".type"].concat(), "string");
            gj::set_str(
                &mut json,
                &[&reason[..], b".description"].concat(),
                PLACEHOLDER_REASON_DESCRIPTION,
            );
            gj::set_strs(&mut json, &required_path, &["reason"]);
            continue;
        }
        if props.is_object() && !has_required {
            if parent.is_empty() {
                continue;
            }
            let placeholder = join_path(&props_path, b"_");
            if !gj::get(&json, &placeholder).exists() {
                gj::set_str(&mut json, &[&placeholder[..], b".type"].concat(), "boolean");
            }
            gj::set_strs(&mut json, &required_path, &["_"]);
        }
    }
    json
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Go's TestIssue6732_CleanInliningStripsDefinitionsFromIntermediate: the cleaning
    /// inliner drops definition containers, so a large unused definition is never copied
    /// into the intermediate document, and both references still expand.
    #[test]
    fn clean_inlining_strips_definitions_from_intermediate() {
        let person = format!(
            r#"{{"type":"object","properties":{{"name":{{"type":"string"}}}},"$defs":{{"Unused":{{"type":"string","description":"{}"}}}}}}"#,
            "x".repeat(280 * 1024)
        );
        let input = format!(
            r##"{{"type":"object","properties":{{"p0":{{"$ref":"#/$defs/Person"}},"p1":{{"$ref":"#/$defs/Person"}}}},"$defs":{{"Person":{person}}}}}"##
        );
        let got = inline_refs_for_clean(input.into_bytes());
        assert!(got.len() < 64 * 1024, "intermediate is {} bytes", got.len());
        assert!(!gj::get(&got, "$defs").exists());
        assert!(!gj::get(&got, "properties.p0.$defs").exists());
        assert_eq!(
            gj::get(&got, "properties.p1.properties.name.type").bytes().as_ref(),
            b"string"
        );
    }
}
