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
    clean(
        schema,
        Options {
            add_placeholder: require_placeholder,
            add_missing_array_items: true,
            antigravity_semantics: true,
            remove_tool_title: !require_placeholder,
            flatten_unions: true,
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
    inline_refs(schema.to_vec())
}

fn clean(schema: &[u8], options: Options) -> Vec<u8> {
    let mut s = normalize_malformed_schema_objects(schema.to_vec(), options.add_missing_array_items);
    if options.antigravity_semantics {
        s = inline_refs(s);
    }
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
        s = flatten_any_of_one_of(s);
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
    s = sanitize_array_items(s);
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

/// util.Walk: every path (keys escaped) whose last key equals `field`, in document order.
fn find_paths(json: &[u8], field: &[u8]) -> Vec<Vec<u8>> {
    let mut paths = vec![];
    walk_fields(&gj::parse(json), b"", &mut |key, path| {
        if key == field {
            paths.push(path.to_vec());
        }
    });
    paths
}

fn find_paths_by_fields(json: &[u8], fields: &[&[u8]]) -> HashMap<Vec<u8>, Vec<Vec<u8>>> {
    let mut paths: HashMap<Vec<u8>, Vec<Vec<u8>>> = HashMap::new();
    walk_fields(&gj::parse(json), b"", &mut |key, path| {
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
    walk_fields(value, b"", &mut |key, path| {
        if key == field {
            paths.push(path.to_vec());
        }
    });
    paths
}

fn walk_fields(value: &Res<'_>, path: &[u8], visit: &mut dyn FnMut(&[u8], &[u8])) {
    if value.kind != Kind::Json {
        return;
    }
    value.each(|key, child| {
        let key = key.bytes();
        let child_path = join_path(path, &escape_key(&key));
        visit(&key, &child_path);
        walk_fields(&child, &child_path, visit);
        true
    });
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
                b"properties" | b"patternProperties" | b"dependentSchemas" | b"$defs" | b"definitions"
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
    let root = match root {
        GoValue::Bool(true) => return b"{}".to_vec(),
        GoValue::Object(map) if !is_api_request_document(&map) => map,
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
        let (repaired, promoted, props_modified) = repair_property_map(props, add_items);
        if props_modified {
            clone.insert("properties".into(), GoValue::Object(repaired));
            modified = true;
        }
        if !promoted.is_empty() {
            promote_required(&mut clone, &promoted);
            modified = true;
        }
    }

    if add_items {
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

fn inline_refs(json: Vec<u8>) -> Vec<u8> {
    if !contains_seq(&json, br#""$ref""#) {
        return json;
    }
    let Some(root) = decode_first(&json) else {
        return json;
    };
    let mut active = std::collections::HashSet::new();
    resolve_refs(&root, &root, &mut active).marshal()
}

fn resolve_refs(root: &GoValue, value: &GoValue, active: &mut std::collections::HashSet<String>) -> GoValue {
    match value {
        GoValue::Array(items) => GoValue::Array(items.iter().map(|i| resolve_refs(root, i, active)).collect()),
        GoValue::Object(node) => {
            if let Some(GoValue::String(reference)) = node.get("$ref")
                && reference.starts_with("#/")
                && let Some(target) = resolve_pointer(root, reference)
            {
                if active.contains(reference) {
                    return GoValue::Object(cyclic_ref_fallback(node, target, reference));
                }
                active.insert(reference.clone());
                let resolved = resolve_refs(root, target, active);
                active.remove(reference);
                if let GoValue::Object(target_map) = resolved {
                    let mut out = target_map;
                    for (key, item) in node {
                        if key != "$ref" {
                            out.insert(key.clone(), resolve_refs(root, item, active));
                        }
                    }
                    return GoValue::Object(out);
                }
            }
            GoValue::Object(
                node.iter()
                    .map(|(k, v)| (k.clone(), resolve_refs(root, v, active)))
                    .collect(),
            )
        }
        other => other.clone(),
    }
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
        let name = ref_name(&gj::get(&json, &p).bytes());
        let parent = trim_suffix(&p, b".$ref").to_vec();
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

fn flatten_any_of_one_of(mut json: Vec<u8>) -> Vec<u8> {
    for key in [&b"anyOf"[..], b"oneOf"] {
        for p in depth_sorted(find_paths(&json, key)) {
            let arr = gj::get(&json, &p).into_owned();
            if !arr.is_array() {
                continue;
            }
            let items = arr.array();
            if items.is_empty() {
                continue;
            }
            let parent_path = trim_suffix(&p, &[b".", key].concat()).to_vec();
            let parent = if parent_path.is_empty() {
                gj::parse(&json).into_owned()
            } else {
                gj::get(&json, &parent_path).into_owned()
            };
            let has_null = items.iter().any(|i| type_of(i) == b"null");
            if parent.get("properties").is_object() {
                for item in &items {
                    let branch = item.get("properties");
                    if branch.is_object() {
                        branch.each(|prop_key, prop_value| {
                            let dest = join_path(
                                &parent_path,
                                &[&b"properties."[..], &escape_key(&prop_key.bytes())].concat(),
                            );
                            json = merge_missing_schema(std::mem::take(&mut json), &dest, &prop_value);
                            true
                        });
                    }
                }
                if has_null {
                    gj::set_bool(&mut json, &join_path(&parent_path, b"nullable"), true);
                }
                json = delete(json, &p);
                continue;
            }
            let parent_desc = gj::get(&json, &description_path(&parent_path)).bytes().into_owned();
            let (best, types) = select_best(&items);
            let mut selected = items[best].raw.to_vec();
            if has_null && type_of(&items[best]) != b"null" {
                gj::set_bool(&mut selected, "nullable", true);
            }
            if !parent_desc.is_empty() {
                selected = merge_description_raw(&selected, &parent_desc);
            }
            if types.len() > 1 {
                let hint = [&b"Accepts: "[..], &types.join(&b" | "[..])].concat();
                let merged = merge_hint(&gj::get(&selected, "description").bytes(), &hint);
                gj::set_str(&mut selected, "description", merged);
            }
            json = set_raw_at(json, &parent_path, &selected);
        }
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
        b"if",
        b"then",
        b"else",
        b"$comment",
        b"enumDescriptions",
        b"enumTitles",
        b"prefill",
        b"deprecated",
        b"encrypted",
        b"additionalItems",
        b"unevaluatedProperties",
        b"unevaluatedItems",
        b"contentSchema",
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

fn sanitize_array_items(mut json: Vec<u8>) -> Vec<u8> {
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
    json
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
