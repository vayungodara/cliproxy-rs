//! Claude tool names and arguments on the way back to a Claude client
//! (internal/util/translator.go, 202b8b8c): exact declared names win over canonical
//! restoration, and the `reason` / `_` placeholders a Gemini-family schema cleaner
//! injected for empty object schemas are removed from the returned arguments, judged
//! against the client's original schema and, when present, the schema actually sent.
//!
//! Schemas the analysis does not model (unknown keywords, multi-branch unions it
//! cannot resolve, unresolvable `$ref`) leave the arguments untouched.

use std::collections::{HashMap, HashSet};

use cpa_common::json::{self as gj, Kind, Res};

use crate::common::{self, canonical_tool_name, trim_space};

/// util.placeholderReasonDescription.
const PLACEHOLDER_REASON_DESCRIPTION: &[u8] = b"Brief explanation of why you are calling this tool";

/// A tool's declared name: `name`, else `function.name`, trimmed.
fn declared_name(tool: &Res<'_>) -> Vec<u8> {
    let name = trim_space(&tool.get("name").bytes()).to_vec();
    if name.is_empty() {
        trim_space(&tool.get("function.name").bytes()).to_vec()
    } else {
        name
    }
}

/// util.MapClaudeToolName: an exact declared name is kept, and a name several distinct
/// declarations share canonically stays as the upstream returned it; otherwise
/// canonical restoration ([`common::map_tool_name`]).
pub fn map_claude_tool_name(map: Option<&HashMap<Vec<u8>, Vec<u8>>>, original: &[u8], name: &[u8]) -> Vec<u8> {
    if name.is_empty() {
        return vec![];
    }
    if !original.is_empty() && gj::valid(original) {
        let tools = gj::get(original, "tools");
        if tools.is_array() {
            let canonical = canonical_tool_name(name);
            let mut canonical_names = HashSet::new();
            for tool in tools.array() {
                let declared = declared_name(&tool);
                if declared == name {
                    return name.to_vec();
                }
                if !declared.is_empty() && canonical_tool_name(&declared) == canonical {
                    canonical_names.insert(declared);
                }
            }
            if canonical_names.len() > 1 {
                return name.to_vec();
            }
        }
    }
    common::map_tool_name(map, name)
}

/// claudeObjectSchemaInfo.
#[derive(Default, Clone)]
struct Info<'a> {
    properties: HashMap<Vec<u8>, Vec<Res<'a>>>,
    required: HashSet<Vec<u8>>,
    has_properties: bool,
    allows_dynamic: bool,
    forbids_dynamic: bool,
    has_pattern_properties: bool,
    complex: bool,
    unsafe_complex: bool,
    declares_object: bool,
    branch_infos: Vec<Info<'a>>,
}

fn has_key(object: &Res<'_>, key: &[u8]) -> bool {
    object.map().iter().any(|(k, _)| k == key)
}

fn has_pattern_properties(schema: &Res<'_>) -> bool {
    let patterns = schema.get("patternProperties");
    patterns.is_object() && !patterns.map().is_empty()
}

/// claudeSchemaAllowsDynamic: pattern properties, or `additionalProperties` other
/// than `false`.
fn allows_dynamic(schema: &Res<'_>) -> bool {
    let additional = schema.get("additionalProperties");
    has_pattern_properties(schema) || (additional.exists() && additional.kind != Kind::False)
}

fn forbids_dynamic(schema: &Res<'_>) -> bool {
    let additional = schema.get("additionalProperties");
    additional.exists() && additional.kind == Kind::False
}

/// claudeResolveLocalRef: a `#/...` JSON pointer within `root`.
fn resolve_local_ref<'a>(root: &Res<'a>, reference: &[u8]) -> Option<Res<'a>> {
    let pointer = reference.strip_prefix(b"#/")?;
    let mut current = root.clone();
    for segment in pointer.split(|&c| c == b'/') {
        let segment = String::from_utf8_lossy(segment).replace("~1", "/").replace("~0", "~");
        let next = current
            .map()
            .into_iter()
            .find(|(k, _)| k == segment.as_bytes())
            .map(|(_, v)| v);
        current = match next {
            Some(next) => next,
            None if current.is_array() => {
                let items = current.array();
                let index: usize = segment.parse().ok()?;
                items.get(index)?.clone()
            }
            None => return None,
        };
    }
    Some(current)
}

fn declares_object(schema: &Res<'_>) -> bool {
    let kind = schema.get("type");
    &*kind.bytes() == b"object" || (kind.is_array() && kind.array().iter().any(|t| &*t.bytes() == b"object"))
}

/// claudeUnionBranches: the non-null `anyOf`/`oneOf` branches, deduplicated; `None`
/// when a union keyword is not an array.
fn union_branches<'a>(schema: &Res<'a>) -> Option<Vec<Res<'a>>> {
    let mut branches = vec![];
    let mut seen = HashSet::new();
    for keyword in ["anyOf", "oneOf"] {
        let union = schema.get(keyword);
        if !union.exists() {
            continue;
        }
        if !union.is_array() {
            return None;
        }
        for branch in union.array() {
            let kind = branch.get("type");
            if &*kind.bytes() == b"null" {
                continue;
            }
            if kind.is_array() {
                let kinds = kind.array();
                if kinds.len() == 1 && &*kinds[0].bytes() == b"null" {
                    continue;
                }
            }
            if seen.insert(branch.raw.to_vec()) {
                branches.push(branch);
            }
        }
    }
    Some(branches)
}

/// claudeAnalyzedSchemaKeywords.
const ANALYZED_KEYWORDS: &[&str] = &[
    "type",
    "description",
    "title",
    "properties",
    "required",
    "items",
    "additionalProperties",
    "patternProperties",
    "anyOf",
    "oneOf",
    "allOf",
    "$ref",
    "$defs",
    "definitions",
    "nullable",
    "minLength",
    "maxLength",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "multipleOf",
    "pattern",
    "format",
    "default",
    "examples",
    "minItems",
    "maxItems",
    "maxProperties",
    "$schema",
    "$id",
    "id",
    "$comment",
    "deprecated",
    "readOnly",
    "writeOnly",
    "example",
    "contentEncoding",
    "contentMediaType",
];

fn has_unanalyzed_keyword(schema: &Res<'_>) -> bool {
    schema
        .map()
        .iter()
        .any(|(key, _)| !key.starts_with(b"x-") && !ANALYZED_KEYWORDS.iter().any(|k| k.as_bytes() == key))
}

fn push_unique<'a>(schemas: &mut Vec<Res<'a>>, value: Res<'a>) {
    if !schemas.iter().any(|s| s.raw == value.raw) {
        schemas.push(value);
    }
}

/// mergeClaudeObjectSchemaInfo.
fn merge<'a>(dst: &mut Info<'a>, src: Info<'a>) {
    if src.has_properties {
        dst.has_properties = true;
        for (key, values) in src.properties {
            let entry = dst.properties.entry(key).or_default();
            for value in values {
                push_unique(entry, value);
            }
        }
    }
    dst.required.extend(src.required);
    dst.branch_infos.extend(src.branch_infos);
    dst.allows_dynamic |= src.allows_dynamic;
    dst.forbids_dynamic |= src.forbids_dynamic;
    dst.has_pattern_properties |= src.has_pattern_properties;
    dst.complex |= src.complex;
    dst.unsafe_complex |= src.unsafe_complex;
    dst.declares_object |= src.declares_object;
}

fn collect_properties<'a>(info: &mut Info<'a>, props: &Res<'a>) {
    info.has_properties = true;
    info.declares_object = true;
    for (key, value) in props.map() {
        push_unique(info.properties.entry(key).or_default(), value);
    }
}

fn collect_required(info: &mut Info<'_>, schema: &Res<'_>) {
    for name in schema.get("required").array() {
        info.required.insert(name.bytes().into_owned());
    }
}

/// analyzeClaudeObjectSchema.
fn analyze<'a>(schema: &Res<'a>, root: &Res<'a>) -> Info<'a> {
    analyze_depth(schema, root, 0, &mut HashSet::new())
}

/// Resolves `$ref` against `root` and merges the target's analysis; `false` when the
/// reference cycles or does not resolve.
fn merge_ref<'a>(
    info: &mut Info<'a>,
    reference: &Res<'a>,
    root: &Res<'a>,
    depth: usize,
    visited: &mut HashSet<Vec<u8>>,
) -> bool {
    let value = reference.bytes().into_owned();
    if visited.contains(&value) {
        return false;
    }
    let Some(target) = resolve_local_ref(root, &value) else {
        return false;
    };
    visited.insert(value.clone());
    let resolved = analyze_depth(&target, root, depth + 1, visited);
    visited.remove(&value);
    merge(info, resolved);
    true
}

/// analyzeClaudeObjectSchemaDepth.
fn analyze_depth<'a>(schema: &Res<'a>, root: &Res<'a>, depth: usize, visited: &mut HashSet<Vec<u8>>) -> Info<'a> {
    let mut info = Info::default();
    if depth > 16 || !schema.exists() || !schema.is_object() {
        info.complex = true;
        return info;
    }
    info.declares_object = declares_object(schema);
    if has_unanalyzed_keyword(schema) {
        // Keywords outside the analyzed set (enum, const, conditionals, ...) may constrain
        // legal values in ways this analysis does not model: never clean such a schema.
        info.complex = true;
        info.unsafe_complex = true;
        return info;
    }
    if schema.get("anyOf").exists() || schema.get("oneOf").exists() {
        let branches = match union_branches(schema) {
            Some(branches) if !branches.is_empty() => branches,
            _ => {
                info.complex = true;
                return info;
            }
        };
        if branches.len() > 1 {
            // Multi-branch unions stay complex, but every branch's declared properties
            // are collected; the branch infos are kept apart because branches are
            // alternatives, not simultaneous constraints.
            for branch in &branches {
                let branch_info = analyze_depth(branch, root, depth + 1, visited);
                info.branch_infos.push(branch_info.clone());
                merge(&mut info, branch_info);
            }
            info.allows_dynamic |= allows_dynamic(schema);
            info.forbids_dynamic |= forbids_dynamic(schema);
            info.has_pattern_properties |= has_pattern_properties(schema);
            collect_required(&mut info, schema);
            let props = schema.get("properties");
            if props.is_object() {
                collect_properties(&mut info, &props);
            }
            let reference = schema.get("$ref");
            if reference.exists() {
                merge_ref(&mut info, &reference, root, depth, visited);
            }
            let all_of = schema.get("allOf");
            if all_of.is_array() {
                for branch in all_of.array() {
                    let branch_info = analyze_depth(&branch, root, depth + 1, visited);
                    merge(&mut info, branch_info);
                }
            }
            info.complex = true;
            return info;
        }
        let branch_info = analyze_depth(&branches[0], root, depth + 1, visited);
        merge(&mut info, branch_info);
    }
    let reference = schema.get("$ref");
    if reference.exists() && !merge_ref(&mut info, &reference, root, depth, visited) {
        info.complex = true;
        return info;
    }
    let all_of = schema.get("allOf");
    if all_of.is_array() {
        for branch in all_of.array() {
            let branch_info = analyze_depth(&branch, root, depth + 1, visited);
            merge(&mut info, branch_info);
        }
    } else if all_of.exists() {
        info.complex = true;
    }
    info.allows_dynamic |= allows_dynamic(schema);
    info.forbids_dynamic |= forbids_dynamic(schema);
    info.has_pattern_properties |= has_pattern_properties(schema);
    let props = schema.get("properties");
    if props.exists() {
        if props.is_object() {
            collect_properties(&mut info, &props);
        } else {
            info.complex = true;
        }
    }
    collect_required(&mut info, schema);
    info
}

/// findClaudeToolSchema: the input schema of the original tool named `name` (exactly,
/// else its only canonical match), when unambiguous. `Some(None)` means the tool
/// declares no schema.
fn find_tool_schema<'a>(original: &'a [u8], name: &[u8]) -> Option<Option<Res<'a>>> {
    if name.is_empty() || original.is_empty() || !gj::valid(original) {
        return None;
    }
    let tools = gj::parse(original).get("tools");
    if !tools.is_array() {
        return None;
    }
    let canonical = canonical_tool_name(name);
    let mut exact: Vec<Res<'a>> = vec![];
    let mut canonical_matches: Vec<(Vec<u8>, Vec<Res<'a>>)> = vec![];
    for tool in tools.array() {
        let declared = declared_name(&tool);
        if declared.is_empty() {
            continue;
        }
        let mut schema = tool.get("input_schema");
        if !schema.exists() {
            schema = tool.get("parameters");
        }
        if !schema.exists() {
            schema = tool.get("function.parameters");
        }
        if declared == name {
            exact.push(schema.clone());
        }
        if canonical_tool_name(&declared) == canonical {
            match canonical_matches.iter_mut().find(|(n, _)| *n == declared) {
                Some((_, list)) => list.push(schema),
                None => canonical_matches.push((declared, vec![schema])),
            }
        }
    }
    let identical = |list: &[Res<'a>]| list.iter().all(|s| s.raw == list[0].raw);
    let found = |schema: &Res<'a>| Some(schema.exists().then(|| schema.clone()));
    if !exact.is_empty() {
        return if identical(&exact) { found(&exact[0]) } else { None };
    }
    match canonical_matches.as_slice() {
        [(_, list)] if identical(list) => found(&list[0]),
        _ => None,
    }
}

/// util.ClaudeToolIsParameterless: the original tool declared an empty or missing
/// object schema, so the arguments Claude receives must be `{}`.
pub fn claude_tool_is_parameterless(name: &[u8], original: &[u8]) -> bool {
    let Some(schema) = find_tool_schema(original, name) else {
        return false;
    };
    let Some(schema) = schema else {
        return true;
    };
    let info = analyze(&schema, &schema);
    if info.complex || any_branch_allows_dynamic(&info) || !info.declares_object {
        return false;
    }
    info.required.is_empty() && (!info.has_properties || info.properties.is_empty())
}

/// resolveClaudeArrayItemsSchema: the item schemas of an array schema, and whether
/// they could be resolved deterministically.
fn array_items<'a>(
    schema: &Res<'a>,
    root: &Res<'a>,
    depth: usize,
    visited: &mut HashSet<Vec<u8>>,
) -> Option<(Vec<Res<'a>>, bool)> {
    if depth > 16 || !schema.exists() || !schema.is_object() {
        return None;
    }
    let mut items = vec![];
    let mut deterministic = true;
    let reference = schema.get("$ref");
    if reference.exists() {
        let value = reference.bytes().into_owned();
        if visited.contains(&value) {
            return None;
        }
        let target = resolve_local_ref(root, &value)?;
        visited.insert(value.clone());
        let resolved = array_items(&target, root, depth + 1, visited);
        visited.remove(&value);
        match resolved {
            Some((found, true)) => found.into_iter().for_each(|i| push_unique(&mut items, i)),
            _ => deterministic = false,
        }
    }
    if schema.get("anyOf").exists() || schema.get("oneOf").exists() {
        match union_branches(schema) {
            Some(branches) if branches.len() == 1 => match array_items(&branches[0], root, depth + 1, visited) {
                Some((found, true)) => found.into_iter().for_each(|i| push_unique(&mut items, i)),
                _ => deterministic = false,
            },
            _ => deterministic = false,
        }
    }
    let all_of = schema.get("allOf");
    if all_of.is_array() {
        for branch in all_of.array() {
            match array_items(&branch, root, depth + 1, visited) {
                Some((found, true)) => found.into_iter().for_each(|i| push_unique(&mut items, i)),
                _ => deterministic = false,
            }
        }
    } else if all_of.exists() {
        deterministic = false;
    }
    let direct = schema.get("items");
    if direct.exists() {
        push_unique(&mut items, direct);
    }
    Some((items, deterministic))
}

/// Go's `([]gjson.Result, bool)` from resolveClaudeArrayItemsSchema.
fn resolve_items<'a>(schema: &Res<'a>, root: &Res<'a>) -> (Vec<Res<'a>>, bool) {
    array_items(schema, root, 0, &mut HashSet::new()).unwrap_or((vec![], false))
}

/// joinPath.
fn join_path(base: &[u8], suffix: &[u8]) -> Vec<u8> {
    if base.is_empty() {
        suffix.to_vec()
    } else {
        [base, b".", suffix].concat()
    }
}

/// escapeGJSONPathKey.
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

/// The value at `path`, or the whole document for the empty path.
fn at<'j>(json: &'j [u8], path: &[u8]) -> Res<'j> {
    if path.is_empty() {
        gj::parse(json)
    } else {
        gj::get(json, path)
    }
}

fn set_raw(json: &mut Vec<u8>, path: &[u8], raw: &[u8]) {
    gj::set_raw(json, path, raw);
}

fn sorted_keys<'a, 'b>(info: &'b Info<'a>) -> Vec<&'b Vec<u8>> {
    let mut keys: Vec<_> = info.properties.keys().collect();
    keys.sort();
    keys
}

/// claudeInfoAnyBranchAllowsDynamic.
fn any_branch_allows_dynamic(info: &Info<'_>) -> bool {
    info.has_pattern_properties
        || info.branch_infos.iter().any(any_branch_allows_dynamic)
        || (info.allows_dynamic && !info.forbids_dynamic)
}

fn carries_required(info: &Info<'_>, current: &Res<'_>) -> bool {
    info.required.iter().all(|name| has_key(current, name))
}

/// claudeBranchMayAccept.
fn branch_may_accept(info: &Info<'_>, key: &[u8], current: &Res<'_>) -> bool {
    if info.has_pattern_properties {
        return true;
    }
    // An open branch accepts the key by default, but only counts when the current
    // object carries every property the branch requires.
    if !info.forbids_dynamic && carries_required(info, current) {
        return true;
    }
    if info.allows_dynamic && !info.forbids_dynamic {
        return true;
    }
    info.branch_infos
        .iter()
        .any(|branch| !branch.properties.contains_key(key) && branch_may_accept(branch, key, current))
}

/// claudeParentBranchAcceptsChild.
fn parent_branch_accepts_child(info: &Info<'_>, key: &[u8], current: &Res<'_>) -> bool {
    info.branch_infos
        .iter()
        .any(|branch| !branch.properties.contains_key(key) && branch_may_accept(branch, key, current))
}

/// claudeCandidateMayPermitChild.
fn candidate_may_permit_child(info: &Info<'_>, key: &[u8], current: &Res<'_>) -> bool {
    if info.properties.contains_key(key) {
        return false;
    }
    if info.has_pattern_properties {
        return true;
    }
    !info.forbids_dynamic && carries_required(info, current)
}

/// claudeOriginalsMayPermitChild.
fn originals_may_permit_child(originals: &[Res<'_>], root: &Res<'_>, key: &[u8], current: &Res<'_>) -> bool {
    originals.len() > 1
        && originals.iter().any(|original| {
            let info = analyze(original, root);
            !info.complex && candidate_may_permit_child(&info, key, current)
        })
}

/// claudeOriginalsAnyCandidateAcceptsImplicitly.
fn originals_accept_implicitly(originals: &[Res<'_>], root: &Res<'_>, current: &Res<'_>) -> bool {
    originals.len() > 1
        && originals.iter().any(|original| {
            let info = analyze(original, root);
            !info.complex && !info.forbids_dynamic && carries_required(&info, current)
        })
}

/// claudeOriginalsAnyBranchAllowsDynamic.
fn originals_allow_dynamic(originals: &[Res<'_>], root: &Res<'_>) -> bool {
    originals
        .iter()
        .any(|original| any_branch_allows_dynamic(&analyze(original, root)))
}

fn merged_info<'a>(schemas: &[Res<'a>], root: &Res<'a>) -> Info<'a> {
    let mut merged = Info::default();
    for schema in schemas {
        merge(&mut merged, analyze(schema, root));
    }
    merged
}

fn all_declare_object(schemas: &[Res<'_>], root: &Res<'_>) -> bool {
    schemas.iter().all(|schema| analyze(schema, root).declares_object)
}

fn is_empty_schema(info: &Info<'_>) -> bool {
    info.required.is_empty() && (!info.has_properties || info.properties.is_empty())
}

/// Replaces the object at `path` with `{}` (the whole document at the top level).
fn normalize_empty(json: Vec<u8>, path: &[u8], top_level: bool) -> Vec<u8> {
    if top_level {
        return b"{}".to_vec();
    }
    let mut json = json;
    set_raw(&mut json, path, b"{}");
    json
}

/// The top-level object left empty becomes the canonical `{}`.
fn finish(json: Vec<u8>, path: &[u8]) -> Vec<u8> {
    if path.is_empty() {
        let parsed = gj::parse(&json);
        if parsed.is_object() && parsed.map().is_empty() {
            return b"{}".to_vec();
        }
    }
    json
}

/// cleanClaudeToolArray.
fn clean_array(json: Vec<u8>, schemas: &[Res<'_>], root: &Res<'_>, path: &[u8]) -> Vec<u8> {
    if schemas.iter().any(|schema| analyze(schema, root).unsafe_complex) {
        return json;
    }
    let mut item_schemas = vec![];
    let mut deterministic = true;
    for schema in schemas {
        let (items, ok) = resolve_items(schema, root);
        if !ok {
            deterministic = false;
            continue;
        }
        if items.is_empty() {
            // A candidate without items accepts arbitrary elements; keep it so element
            // cleaning sees the free alternative.
            push_unique(&mut item_schemas, schema.clone());
            continue;
        }
        items.into_iter().for_each(|i| push_unique(&mut item_schemas, i));
    }
    if !deterministic || item_schemas.is_empty() {
        return json;
    }
    let count = gj::get(&json, path).array().len();
    let mut cleaned = json;
    for index in 0..count {
        let element_path = join_path(path, index.to_string().as_bytes());
        let element = gj::get(&cleaned, &element_path);
        if element.is_object() {
            cleaned = clean_object(cleaned, &item_schemas, root, &element_path, false);
        } else if element.is_array() {
            cleaned = clean_array(cleaned, &item_schemas, root, &element_path);
        }
    }
    cleaned
}

/// cleanClaudeToolObject: placeholder removal judged from the original schemas only.
fn clean_object(json: Vec<u8>, schemas: &[Res<'_>], root: &Res<'_>, path: &[u8], top_level: bool) -> Vec<u8> {
    let args = at(&json, path);
    if !args.is_object() || schemas.is_empty() {
        return json;
    }
    let merged = merged_info(schemas, root);
    if merged.complex || !merged.declares_object {
        return json;
    }
    if is_empty_schema(&merged)
        && all_declare_object(schemas, root)
        && !any_branch_allows_dynamic(&merged)
        && !originals_allow_dynamic(schemas, root)
        && !originals_accept_implicitly(schemas, root, &args)
    {
        if args.map().is_empty() {
            return json;
        }
        return normalize_empty(json, path, top_level);
    }
    let mut cleaned = json;
    for key in sorted_keys(&merged) {
        let child_path = join_path(path, &escape_key(key));
        let child = gj::get(&cleaned, &child_path);
        if !child.exists() {
            continue;
        }
        let current = at(&cleaned, path);
        if parent_branch_accepts_child(&merged, key, &current)
            || originals_may_permit_child(schemas, root, key, &current)
        {
            continue;
        }
        let (is_object, is_array) = (child.is_object(), child.is_array());
        let property_schemas = &merged.properties[key];
        if is_object {
            cleaned = clean_object(cleaned, property_schemas, root, &child_path, false);
        } else if is_array {
            cleaned = clean_array(cleaned, property_schemas, root, &child_path);
        }
    }
    if any_branch_allows_dynamic(&merged) {
        return cleaned;
    }
    if !at(&cleaned, path).is_object() {
        return cleaned;
    }
    for placeholder in [&b"reason"[..], b"_"] {
        let current = at(&cleaned, path);
        if merged.properties.contains_key(placeholder)
            || merged.required.contains(placeholder)
            || originals_allow_dynamic(schemas, root)
            || parent_branch_accepts_child(&merged, placeholder, &current)
            || originals_accept_implicitly(schemas, root, &current)
            || !has_key(&current, placeholder)
        {
            continue;
        }
        let placeholder_path = join_path(path, placeholder);
        gj::delete(&mut cleaned, &placeholder_path);
    }
    finish(cleaned, path)
}

/// findTranslatedClaudeToolSchemas: the declarations the translated (sent) request
/// gave this tool, matched by the name actually sent when it can be derived.
fn find_translated_schemas<'a>(translated: &'a [u8], name: &[u8], original: &[u8]) -> Vec<Res<'a>> {
    if name.is_empty() || translated.is_empty() || !gj::valid(translated) {
        return vec![];
    }
    let root = gj::parse(translated);
    let mut named: Vec<(Vec<u8>, Res<'a>)> = vec![];
    for tools in [root.get("tools"), root.get("request.tools")] {
        if !tools.is_array() {
            continue;
        }
        for group in tools.array() {
            let declarations = group.get("functionDeclarations");
            if !declarations.is_array() {
                continue;
            }
            for declaration in declarations.array() {
                let declared = trim_space(&declaration.get("name").bytes()).to_vec();
                if declared.is_empty() {
                    continue;
                }
                let mut schema = declaration.get("parametersJsonSchema");
                if !schema.exists() {
                    schema = declaration.get("parameters");
                }
                if schema.exists() && schema.is_object() {
                    named.push((declared, schema));
                }
            }
        }
    }
    if named.is_empty() {
        return vec![];
    }
    let canonical = canonical_tool_name(name);
    let sanitized = common::sanitize_function_name(name);
    let sent = crate::responses_tools::sanitized_function_name_map(original)
        .remove(name)
        .unwrap_or_default();
    let mut exact = vec![];
    let mut by_name: Vec<(Vec<u8>, Vec<Res<'a>>)> = vec![];
    for (declared, schema) in named {
        if declared == name || (!sent.is_empty() && declared == sent) {
            exact.push(schema.clone());
        }
        if canonical_tool_name(&declared) == canonical
            || declared == sanitized
            || common::sanitize_function_name(&declared) == sanitized
        {
            match by_name.iter_mut().find(|(n, _)| *n == declared) {
                Some((_, list)) => list.push(schema),
                None => by_name.push((declared, vec![schema])),
            }
        }
    }
    if !exact.is_empty() {
        return exact;
    }
    match by_name.len() {
        1 => by_name.pop().map(|(_, list)| list).unwrap_or_default(),
        _ => vec![],
    }
}

/// claudeOriginalDeclaresPropertyDeep.
fn declares_property_deep(
    schema: &Res<'_>,
    root: &Res<'_>,
    property: &[u8],
    depth: usize,
    visited: &mut HashSet<Vec<u8>>,
) -> bool {
    if depth > 16 || !schema.exists() || !schema.is_object() {
        return false;
    }
    let reference = schema.get("$ref");
    if reference.exists() {
        let value = reference.bytes().into_owned();
        if !visited.contains(&value)
            && let Some(target) = resolve_local_ref(root, &value)
        {
            visited.insert(value.clone());
            let declared = declares_property_deep(&target, root, property, depth + 1, visited);
            visited.remove(&value);
            if declared {
                return true;
            }
        }
    }
    if has_key(&schema.get("properties"), property)
        || schema.get("required").array().iter().any(|r| &*r.bytes() == property)
    {
        return true;
    }
    ["allOf", "anyOf", "oneOf"].iter().any(|keyword| {
        schema
            .get(*keyword)
            .array()
            .iter()
            .any(|branch| declares_property_deep(branch, root, property, depth + 1, visited))
    })
}

fn requires(schema: &Res<'_>, name: &[u8]) -> bool {
    schema.get("required").array().iter().any(|r| &*r.bytes() == name)
}

/// Whether the translated schemas show that `placeholder` was injected for this object.
fn placeholder_injected(placeholder: &[u8], translated: &[Res<'_>], top_level: bool, forbids: bool) -> bool {
    for schema in translated {
        let info = analyze(schema, schema);
        let Some(placeholder_schemas) = info.properties.get(placeholder) else {
            continue;
        };
        if placeholder == b"reason" {
            if requires(schema, b"reason")
                || placeholder_schemas
                    .iter()
                    .any(|p| &*p.get("description").bytes() == PLACEHOLDER_REASON_DESCRIPTION)
            {
                return true;
            }
        } else {
            if requires(schema, b"_") {
                return true;
            }
            // Without `required` (dropped by branch flattening), a boolean `_` marks the
            // injection, but only for nested objects.
            if !top_level
                && placeholder_schemas
                    .iter()
                    .any(|p| &*p.get("type").bytes() == b"boolean")
            {
                return true;
            }
        }
    }
    // Union branches: the original declares the field nowhere, so a required
    // occurrence in a sent branch is the injected one.
    for schema in translated {
        for keyword in ["anyOf", "oneOf"] {
            for branch in schema.get(keyword).array() {
                if analyze(&branch, &branch).properties.contains_key(placeholder) && requires(&branch, placeholder) {
                    return true;
                }
            }
        }
    }
    // The Antigravity executor re-cleans wrapped tool schemas after translation and can
    // inject placeholders the translated request does not show yet; simulated only for
    // closed objects, where the key cannot be legitimate user data.
    if top_level && forbids {
        for schema in translated {
            let mut wrapped = b"{}".to_vec();
            if !gj::set_raw(&mut wrapped, "schema", &schema.raw) {
                continue;
            }
            let recleaned_doc = cpa_common::gemini_schema::for_antigravity(&wrapped);
            let recleaned = gj::get(&recleaned_doc, "schema");
            if !recleaned.exists() {
                continue;
            }
            if analyze(&recleaned, &recleaned).properties.contains_key(placeholder) && requires(&recleaned, placeholder)
            {
                return true;
            }
        }
    }
    false
}

/// cleanClaudeToolArgumentsByTranslatedSchema.
fn clean_by_translated(
    json: Vec<u8>,
    translated: &[Res<'_>],
    originals: &[Res<'_>],
    root: &Res<'_>,
    path: &[u8],
    top_level: bool,
) -> Vec<u8> {
    if !at(&json, path).is_object() || translated.is_empty() || originals.is_empty() {
        return json;
    }
    if originals.iter().any(|original| !original.is_object()) {
        return json;
    }
    let original_info = merged_info(originals, root);
    if original_info.unsafe_complex {
        return json;
    }
    if original_info.complex
        && !originals
            .iter()
            .any(|o| o.get("anyOf").is_array() || o.get("oneOf").is_array())
    {
        // Only multi-branch unions fall through, relying on the translated schema.
        return json;
    }
    let mut cleaned = json;
    if original_info.has_properties {
        for key in sorted_keys(&original_info) {
            let child_path = join_path(path, &escape_key(key));
            let child = gj::get(&cleaned, &child_path);
            if !child.exists() {
                continue;
            }
            let mut translated_children = vec![];
            for schema in translated {
                if let Some(children) = analyze(schema, schema).properties.get(key) {
                    children
                        .iter()
                        .for_each(|c| push_unique(&mut translated_children, c.clone()));
                }
            }
            if translated_children.is_empty() {
                continue;
            }
            let current = at(&cleaned, path);
            if parent_branch_accepts_child(&original_info, key, &current)
                || originals_may_permit_child(originals, root, key, &current)
            {
                // Another branch or candidate may accept this key without an explicit
                // schema, so the explicit candidates do not constrain it.
                continue;
            }
            let original_children = &original_info.properties[key];
            let (is_object, is_array) = (child.is_object(), child.is_array());
            if is_object && !original_children.is_empty() {
                cleaned = clean_by_translated(
                    cleaned,
                    &translated_children,
                    original_children,
                    root,
                    &child_path,
                    false,
                );
            } else if is_array && !original_children.is_empty() {
                cleaned = clean_translated_array(cleaned, &translated_children, original_children, root, &child_path);
            }
        }
    }
    if !at(&cleaned, path).is_object() {
        return cleaned;
    }
    for placeholder in [&b"reason"[..], b"_"] {
        let current = at(&cleaned, path);
        if original_info.properties.contains_key(placeholder)
            || original_info.required.contains(placeholder)
            || any_branch_allows_dynamic(&original_info)
            || originals_allow_dynamic(originals, root)
            || originals_accept_implicitly(originals, root, &current)
            || parent_branch_accepts_child(&original_info, placeholder, &current)
            || originals
                .iter()
                .any(|o| declares_property_deep(o, root, placeholder, 0, &mut HashSet::new()))
            || !has_key(&current, placeholder)
        {
            continue;
        }
        if placeholder_injected(placeholder, translated, top_level, original_info.forbids_dynamic) {
            gj::delete(&mut cleaned, &join_path(path, placeholder));
        }
    }
    // A parameterless declaration (no candidate allowing dynamic or free-form data):
    // any remaining keys can only be injected placeholders.
    let current = at(&cleaned, path);
    if !original_info.complex
        && all_declare_object(originals, root)
        && !any_branch_allows_dynamic(&original_info)
        && !originals_allow_dynamic(originals, root)
        && !originals_accept_implicitly(originals, root, &current)
        && original_info.declares_object
        && is_empty_schema(&original_info)
    {
        return normalize_empty(cleaned, path, top_level);
    }
    finish(cleaned, path)
}

/// cleanClaudeTranslatedArray.
fn clean_translated_array(
    json: Vec<u8>,
    translated: &[Res<'_>],
    originals: &[Res<'_>],
    root: &Res<'_>,
    path: &[u8],
) -> Vec<u8> {
    if originals.iter().any(|schema| analyze(schema, root).unsafe_complex) {
        return json;
    }
    let mut translated_items = vec![];
    let mut original_items = vec![];
    let mut deterministic = true;
    for schema in translated {
        let (items, ok) = resolve_items(schema, schema);
        deterministic &= ok;
        items.into_iter().for_each(|i| push_unique(&mut translated_items, i));
    }
    for schema in originals {
        let (items, ok) = resolve_items(schema, root);
        deterministic &= ok;
        if ok && items.is_empty() {
            push_unique(&mut original_items, schema.clone());
            continue;
        }
        items.into_iter().for_each(|i| push_unique(&mut original_items, i));
    }
    if !deterministic || translated_items.is_empty() || original_items.is_empty() {
        return json;
    }
    let count = gj::get(&json, path).array().len();
    let mut cleaned = json;
    for index in 0..count {
        let element_path = join_path(path, index.to_string().as_bytes());
        let element = gj::get(&cleaned, &element_path);
        if element.is_object() {
            cleaned = clean_by_translated(cleaned, &translated_items, &original_items, root, &element_path, false);
        } else if element.is_array() {
            cleaned = clean_translated_array(cleaned, &translated_items, &original_items, root, &element_path);
        }
    }
    cleaned
}

/// The open streamed tool_use block (CurrentToolName, CurrentToolIsParameterless,
/// CurrentToolInputEmitted in Go's Gemini and Antigravity Claude response params).
#[derive(Default)]
pub(crate) struct OpenTool {
    pub name: Vec<u8>,
    parameterless: bool,
    emitted: bool,
}

impl OpenTool {
    pub fn open(&mut self, name: &[u8], original: &[u8]) {
        self.name = name.to_vec();
        self.parameterless = claude_tool_is_parameterless(name, original);
        self.emitted = false;
    }

    /// The cleaned `partial_json` for streamed arguments: a parameterless tool emits its
    /// input once, and later chunks emit an empty string.
    pub fn input(&mut self, args: &[u8], original: &[u8], translated: &[u8]) -> Vec<u8> {
        let mut partial = clean_claude_tool_arguments_with_translated(args, &self.name, original, translated);
        if self.parameterless {
            if self.emitted {
                partial.clear();
            }
            self.emitted = true;
        } else if !partial.is_empty() {
            self.emitted = true;
        }
        partial
    }
}

/// util.CleanClaudeToolArgumentsWithTranslated: placeholders are removed only where
/// the translated (sent) schema shows they were injected; without a translated schema,
/// [`clean_claude_tool_arguments`]. Parameterless tools always get `{}`.
pub fn clean_claude_tool_arguments_with_translated(
    raw: &[u8],
    name: &[u8],
    original: &[u8],
    translated: &[u8],
) -> Vec<u8> {
    let schemas = find_translated_schemas(translated, name, original);
    if schemas.is_empty() {
        return clean_claude_tool_arguments(raw, name, original);
    }
    let raw = trim_space(raw);
    if raw.is_empty() || raw == b"{}" {
        return b"{}".to_vec();
    }
    if !gj::valid(raw) || !gj::parse(raw).is_object() {
        return raw.to_vec();
    }
    match find_tool_schema(original, name) {
        None => raw.to_vec(),
        Some(None) => b"{}".to_vec(),
        Some(Some(schema)) => clean_by_translated(
            raw.to_vec(),
            &schemas,
            std::slice::from_ref(&schema),
            &schema,
            b"",
            true,
        ),
    }
}

/// util.CleanClaudeToolArguments: placeholders injected for empty object schemas are
/// removed, judged from the original declaration; parameterless tools get `{}`.
pub fn clean_claude_tool_arguments(raw: &[u8], name: &[u8], original: &[u8]) -> Vec<u8> {
    let raw = trim_space(raw);
    if raw.is_empty() || raw == b"{}" {
        return b"{}".to_vec();
    }
    if !gj::valid(raw) {
        return raw.to_vec();
    }
    let schema = match find_tool_schema(original, name) {
        None => return raw.to_vec(),
        Some(None) => return b"{}".to_vec(),
        Some(Some(schema)) => schema,
    };
    if !gj::parse(raw).is_object() {
        return raw.to_vec();
    }
    let info = analyze(&schema, &schema);
    if info.complex || !info.declares_object {
        return raw.to_vec();
    }
    if is_empty_schema(&info) && !any_branch_allows_dynamic(&info) {
        return b"{}".to_vec();
    }
    clean_object(raw.to_vec(), std::slice::from_ref(&schema), &schema, b"", true)
}
