//! Responses web search over Gemini grounding
//! (internal/translator/gemini/openai/responses/gemini_openai-responses_web_search.go).

use std::collections::{BTreeMap, HashMap, HashSet};

use cpa_common::json::{self as gj, Kind, Res};
use cpa_core::registry::{self, ModelInfo};

use crate::common::{go_lower, go_runes, trim_space};

fn native_web_search(info: &Option<ModelInfo>) -> Option<bool> {
    info.as_ref()?
        .raw
        .get("native_capabilities")?
        .get("web_search")?
        .as_bool()
}

fn supports_web_search_flag(info: &Option<ModelInfo>) -> bool {
    info.as_ref()
        .and_then(|i| i.raw.get("supports_web_search"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// normalizeAntigravityCapabilityModelID.
fn normalize_antigravity_model(id: &str) -> String {
    let mut id = go_lower(id.trim().as_bytes());
    if let Some(open) = id.iter().rposition(|&c| c == b'(')
        && id.ends_with(b")")
    {
        id = trim_space(&id[..open]).to_vec();
    }
    String::from_utf8_lossy(&id).into_owned()
}

/// registry.AntigravityWebSearchModelFor(model) != "": the first available Antigravity
/// model with this normalized ID decides.
pub(crate) fn antigravity_web_search(model: &str) -> bool {
    let wanted = normalize_antigravity_model(model);
    if wanted.is_empty() {
        return false;
    }
    for info in registry::available_models_by_provider("antigravity") {
        let id = normalize_antigravity_model(&info.id);
        if !id.is_empty() && id == wanted {
            return supports_web_search_flag(&Some(info));
        }
    }
    false
}

/// ModelSupportsWebSearch: catalog `native_capabilities.web_search` (an explicit false
/// vetoes), then Antigravity's dynamic capability.
pub(crate) fn model_supports_web_search(model: &str) -> bool {
    let info = registry::lookup_model(model, None);
    let info_ag = registry::lookup_model(model, Some("antigravity"));
    let (native, native_ag) = (native_web_search(&info), native_web_search(&info_ag));
    if native == Some(false) || native_ag == Some(false) {
        return false;
    }
    if native == Some(true) || native_ag == Some(true) {
        return true;
    }
    antigravity_web_search(model) || supports_web_search_flag(&info) || supports_web_search_flag(&info_ag)
}

/// isResponsesWebSearchToolType.
pub(crate) fn is_web_search_tool_type(kind: &[u8]) -> bool {
    matches!(
        kind,
        b"web_search" | b"web_search_2025_08_26" | b"web_search_preview" | b"web_search_preview_2025_03_11"
    )
}

/// HasResponsesWebSearchTool.
pub(crate) fn has_web_search_tool(root: &Res<'_>) -> bool {
    let tools = root.get("tools");
    tools.is_array()
        && tools
            .array()
            .iter()
            .any(|t| is_web_search_tool_type(&t.get("type").bytes()))
}

/// HasOnlyResponsesWebSearchTools: at least one tool, and every tool a web search.
pub(crate) fn has_only_web_search_tools(root: &Res<'_>) -> bool {
    let tools = root.get("tools");
    if !tools.is_array() {
        return false;
    }
    let tools = tools.array();
    !tools.is_empty() && tools.iter().all(|t| is_web_search_tool_type(&t.get("type").bytes()))
}

/// AllowsResponsesWebSearchToolChoice.
pub(crate) fn allows_web_search_tool_choice(root: &Res<'_>) -> bool {
    let choice = root.get("tool_choice");
    if !choice.exists() {
        return true;
    }
    if choice.kind == Kind::String {
        return matches!(choice.s.as_ref(), b"" | b"auto" | b"required");
    }
    if !choice.is_object() {
        return false;
    }
    let kind = choice.get("type").bytes();
    match kind.as_ref() {
        b"" | b"auto" | b"required" => true,
        k if is_web_search_tool_type(k) => true,
        b"allowed_tools" => {
            let tools = choice.get("tools");
            tools.is_array()
                && tools
                    .array()
                    .iter()
                    .any(|t| is_web_search_tool_type(&t.get("type").bytes()))
        }
        _ => false,
    }
}

/// ExtractResponsesWebSearchQuery: string input, flat input_text parts, the last user
/// message's text, then the instructions.
pub(crate) fn extract_query(root: &Res<'_>) -> Vec<u8> {
    let input = root.get("input");
    if input.kind == Kind::String {
        return trim_space(&input.s).to_vec();
    }
    if input.is_array() {
        let items = input.array();
        let mut flat: Vec<Vec<u8>> = vec![];
        let mut is_flat = true;
        for item in &items {
            if item.get("type").bytes().as_ref() == b"input_text" {
                let text = trim_space(&item.get("text").bytes()).to_vec();
                if !text.is_empty() {
                    flat.push(text);
                }
            } else if item.get("role").exists() {
                is_flat = false;
                break;
            }
        }
        if is_flat && !flat.is_empty() {
            return flat.join(&b'\n');
        }
        for item in items.iter().rev() {
            let role = item.get("role").bytes();
            if !role.is_empty() && role.as_ref() != b"user" {
                continue;
            }
            let content = item.get("content");
            if content.kind == Kind::String && !trim_space(&content.s).is_empty() {
                return trim_space(&content.s).to_vec();
            }
            if content.is_array() {
                let texts: Vec<Vec<u8>> = content
                    .array()
                    .iter()
                    .map(|p| trim_space(&p.get("text").bytes()).to_vec())
                    .filter(|t| !t.is_empty())
                    .collect();
                if !texts.is_empty() {
                    return texts.join(&b'\n');
                }
            }
            let text = trim_space(&item.get("text").bytes()).to_vec();
            if !text.is_empty() {
                return text;
            }
        }
    }
    trim_space(&root.get("instructions").bytes()).to_vec()
}

/// ExtractResponsesWebSearchAllowedDomains: the first web search tool with a
/// `filters.allowed_domains` array decides.
pub(crate) fn allowed_domains(root: &Res<'_>) -> Vec<Vec<u8>> {
    let tools = root.get("tools");
    if !tools.is_array() {
        return vec![];
    }
    for tool in tools.array() {
        if !is_web_search_tool_type(&tool.get("type").bytes()) {
            continue;
        }
        let domains = tool.get("filters.allowed_domains");
        if !domains.is_array() {
            continue;
        }
        return domains
            .array()
            .iter()
            .map(|d| trim_space(&d.bytes()).to_vec())
            .filter(|d| !d.is_empty())
            .collect();
    }
    vec![]
}

/// ExtractGroundingMetadata: direct or `response`-wrapped.
pub(crate) fn grounding_metadata<'a>(root: &Res<'a>) -> Res<'a> {
    let gm = root.get("candidates.0.groundingMetadata");
    if gm.exists() {
        return gm;
    }
    let gm = root.get("response.candidates.0.groundingMetadata");
    if gm.exists() {
        return gm;
    }
    Res::default()
}

/// ExtractGroundingQueries.
pub(crate) fn grounding_queries(gm: &Res<'_>) -> Vec<Vec<u8>> {
    let queries = gm.get("webSearchQueries");
    if !queries.is_array() {
        return vec![];
    }
    queries
        .array()
        .iter()
        .map(|q| trim_space(&q.bytes()).to_vec())
        .filter(|q| !q.is_empty())
        .collect()
}

/// ExtractGroundingSources: one `url` source per distinct chunk URI.
pub(crate) fn grounding_sources(gm: &Res<'_>) -> Vec<Vec<u8>> {
    let mut seen = HashSet::new();
    let mut sources = vec![];
    for chunk in gm.get("groundingChunks").array() {
        let uri = trim_space(&chunk.get("web.uri").bytes()).to_vec();
        if uri.is_empty() || !seen.insert(uri.clone()) {
            continue;
        }
        let mut src = br#"{"type":"url","url":""}"#.to_vec();
        gj::set_str(&mut src, "url", &uri);
        sources.push(src);
    }
    sources
}

/// BuildResponsesWebSearchCallItem.
pub(crate) fn web_search_call_item(id: &[u8], query: &[u8], queries: &[Vec<u8>], sources: &[Vec<u8>]) -> Vec<u8> {
    let mut item =
        br#"{"id":"","type":"web_search_call","status":"completed","action":{"type":"search","query":""}}"#.to_vec();
    gj::set_str(&mut item, "id", id);
    gj::set_str(&mut item, "action.query", query);
    if !queries.is_empty() {
        gj::set_strs(&mut item, "action.queries", queries);
    }
    if !sources.is_empty() {
        gj::set_raw(&mut item, "action.sources", gj::join(sources));
    }
    item
}

/// HasValidWebGrounding: a non-blank query or a chunk with a non-blank URI.
pub(crate) fn has_valid_web_grounding(gm: &Res<'_>) -> bool {
    if !gm.exists() {
        return false;
    }
    let queries = gm.get("webSearchQueries");
    if queries.is_array() && queries.array().iter().any(|q| !trim_space(&q.bytes()).is_empty()) {
        return true;
    }
    let chunks = gm.get("groundingChunks");
    chunks.is_array()
        && chunks
            .array()
            .iter()
            .any(|c| !trim_space(&c.get("web.uri").bytes()).is_empty())
}

fn blank(r: &Res<'_>) -> bool {
    !r.exists() || trim_space(&r.raw).is_empty()
}

/// MergeGroundingMetadata: merges one frame's grounding into the accumulated metadata,
/// deduplicating queries, chunks (by URI, else raw) and supports, and remapping
/// stream-wide chunk indices. Returns the merged document (`None`: nothing yet).
pub(crate) fn merge_grounding_metadata(existing: Option<&[u8]>, new: &Res<'_>) -> Option<Vec<u8>> {
    let existing_res = existing.map(gj::parse).unwrap_or_default();
    if blank(&existing_res) && blank(new) {
        return existing.map(<[u8]>::to_vec);
    }
    let existing_raw: Vec<u8> = if blank(&existing_res) {
        b"{}".to_vec()
    } else {
        existing_res.raw.to_vec()
    };
    let existing = gj::parse(&existing_raw);
    if blank(new) {
        return Some(existing_raw.clone());
    }
    let mut merged = existing_raw.clone();

    // 1. Queries.
    let new_queries = grounding_queries(new);
    if !new_queries.is_empty() {
        let mut seen = HashSet::new();
        let queries: Vec<Vec<u8>> = grounding_queries(&existing)
            .into_iter()
            .chain(new_queries)
            .filter(|q| seen.insert(q.clone()))
            .collect();
        gj::set_strs(&mut merged, "webSearchQueries", &queries);
    }

    // 2. Chunks with index remapping.
    let existing_chunks = existing.get("groundingChunks").array();
    let new_chunks = new.get("groundingChunks").array();
    let mut remap: BTreeMap<i64, i64> = BTreeMap::new();
    let saved = existing.get("_chunkIndexRemap");
    if saved.is_object() {
        for (key, value) in saved.map() {
            if let Some(old) = std::str::from_utf8(&key).ok().and_then(|k| k.parse::<i64>().ok()) {
                remap.insert(old, value.int());
            }
        }
    }
    let raw_count = existing.get("_rawChunkCount");
    let prev_raw_count = if raw_count.exists() {
        raw_count.int()
    } else {
        existing_chunks.len() as i64
    };
    if remap.is_empty() && !existing_chunks.is_empty() {
        for i in 0..existing_chunks.len() as i64 {
            remap.insert(i, i);
        }
    }
    let mut chunks_raw: Vec<Vec<u8>> = vec![];
    let mut by_uri: HashMap<Vec<u8>, i64> = HashMap::new();
    let mut by_raw: HashMap<Vec<u8>, i64> = HashMap::new();
    for (i, chunk) in existing_chunks.iter().enumerate() {
        chunks_raw.push(chunk.raw.to_vec());
        let uri = trim_space(&chunk.get("web.uri").bytes()).to_vec();
        if !uri.is_empty() {
            by_uri.entry(uri).or_insert(i as i64);
        }
        by_raw.insert(chunk.raw.to_vec(), i as i64);
    }
    for (i, chunk) in new_chunks.iter().enumerate() {
        let i = i as i64;
        let uri = trim_space(&chunk.get("web.uri").bytes()).to_vec();
        let title = trim_space(&chunk.get("web.title").bytes()).to_vec();
        let new_raw_idx = prev_raw_count + i;
        let known = if !uri.is_empty() {
            by_uri.get(&uri).copied()
        } else {
            by_raw.get(chunk.raw.as_ref()).copied()
        };
        if let Some(idx) = known {
            remap.insert(new_raw_idx, idx);
            if prev_raw_count == 0 {
                remap.insert(i, idx);
            }
            if !uri.is_empty() && !title.is_empty() {
                let slot = &mut chunks_raw[idx as usize];
                if trim_space(&gj::get(slot, "web.title").bytes()).is_empty() {
                    gj::set_str(slot, "web.title", &title);
                }
            }
            continue;
        }
        let idx = chunks_raw.len() as i64;
        chunks_raw.push(chunk.raw.to_vec());
        if !uri.is_empty() {
            by_uri.insert(uri, idx);
        }
        by_raw.insert(chunk.raw.to_vec(), idx);
        remap.insert(new_raw_idx, idx);
        if prev_raw_count == 0 {
            remap.insert(i, idx);
        }
    }
    if !chunks_raw.is_empty() {
        gj::set_raw(&mut merged, "groundingChunks", raw_array(&chunks_raw));
    }
    let total_raw_count = prev_raw_count + new_chunks.len() as i64;
    if !remap.is_empty() {
        let pairs: Vec<String> = remap.iter().map(|(k, v)| format!("\"{k}\":{v}")).collect();
        gj::set_raw(&mut merged, "_chunkIndexRemap", format!("{{{}}}", pairs.join(",")));
        gj::set_int(&mut merged, "_rawChunkCount", total_raw_count);
    }

    // 3. Supports with remapped chunk indices.
    let existing_count = existing_chunks.len() as i64;
    let mut seen_supports = HashSet::new();
    let mut supports_raw: Vec<Vec<u8>> = vec![];
    let mut add_supports = |supports: Vec<Res<'_>>, is_existing: bool| {
        for s in supports {
            let part_index = s.get("segment.partIndex").int();
            let start = s.get("segment.startIndex").int();
            let end = s.get("segment.endIndex").int();
            let original = s.get("groundingChunkIndices").array();
            let mut indices: Vec<i64> = vec![];
            let mut rewrite = false;
            for idx in &original {
                let old = idx.int();
                let mut target = old;
                if (!is_existing || existing_count == 0 || old >= existing_count)
                    && let Some(&t) = remap.get(&old)
                {
                    target = t;
                    rewrite |= t != old;
                }
                if !indices.contains(&target) {
                    indices.push(target);
                }
            }
            rewrite |= indices.len() != original.len();
            let mut sorted = indices.clone();
            sorted.sort_unstable();
            if !seen_supports.insert((part_index, start, end, sorted)) {
                continue;
            }
            let mut raw = s.raw.to_vec();
            if rewrite {
                let list: Vec<String> = indices.iter().map(i64::to_string).collect();
                gj::set_raw(&mut raw, "groundingChunkIndices", format!("[{}]", list.join(",")));
            }
            supports_raw.push(raw);
        }
    };
    add_supports(existing.get("groundingSupports").array(), true);
    add_supports(new.get("groundingSupports").array(), false);
    if !supports_raw.is_empty() {
        gj::set_raw(&mut merged, "groundingSupports", raw_array(&supports_raw));
    }

    // 4-5. Entry point and retrieval queries.
    let entry = new.get("searchEntryPoint");
    if entry.exists() {
        gj::set_raw(&mut merged, "searchEntryPoint", &entry.raw);
    }
    let retrieval = new.get("retrievalQueries");
    if retrieval.exists() && !existing.get("retrievalQueries").exists() {
        gj::set_raw(&mut merged, "retrievalQueries", &retrieval.raw);
    }
    Some(merged)
}

/// `"[" + strings.Join(raws, ",") + "]"`.
fn raw_array(items: &[Vec<u8>]) -> Vec<u8> {
    let mut out = vec![b'['];
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        out.extend_from_slice(item);
    }
    out.push(b']');
    out
}

/// MergeCitationAnnotations: deduplicated by URL and offsets; a late citation replaces an
/// untitled one.
pub(crate) fn merge_citations(existing: &[Vec<u8>], late: &[Vec<u8>]) -> Vec<Vec<u8>> {
    if existing.is_empty() {
        return late.to_vec();
    }
    if late.is_empty() {
        return existing.to_vec();
    }
    let key = |a: &[u8]| {
        (
            gj::get(a, "url").bytes().into_owned(),
            gj::get(a, "start_index").int(),
            gj::get(a, "end_index").int(),
        )
    };
    let mut result: Vec<Vec<u8>> = vec![];
    let mut index: HashMap<(Vec<u8>, i64, i64), usize> = HashMap::new();
    for a in existing {
        index.entry(key(a)).or_insert_with(|| {
            result.push(a.clone());
            result.len() - 1
        });
    }
    for a in late {
        match index.get(&key(a)) {
            Some(&i) => {
                if gj::get(&result[i], "title").bytes().is_empty() && !gj::get(a, "title").bytes().is_empty() {
                    result[i] = a.clone();
                }
            }
            None => {
                index.insert(key(a), result.len());
                result.push(a.clone());
            }
        }
    }
    result
}

/// utf8.RuneCount of `text[..byte_offset]`, clamped.
fn rune_offset(text: &[u8], byte_offset: i64) -> i64 {
    if byte_offset <= 0 {
        return 0;
    }
    let end = (byte_offset as u64).min(text.len() as u64) as usize;
    go_runes(&text[..end]).count() as i64
}

/// GeminiPartMapping: where one Gemini part's text sits inside a Responses message.
#[derive(Clone, Debug, Default)]
pub(crate) struct PartMapping {
    pub part_index: i64,
    pub message_index: i64,
    pub start_rune: i64,
    pub text: Vec<u8>,
}

struct RuneRange {
    message: i64,
    start: i64,
    end: i64,
}

/// mapByteOffsetsToRuneRanges.
fn rune_ranges(mappings: &[PartMapping], start: i64, end: i64) -> Vec<RuneRange> {
    let start = start.max(0);
    if mappings.is_empty() || start >= end {
        return vec![];
    }
    let total: i64 = mappings.iter().map(|m| m.text.len() as i64).sum();
    if start >= total {
        return vec![];
    }
    let end = end.min(total);
    let mut ranges: Vec<RuneRange> = vec![];
    let mut cum = 0i64;
    for m in mappings {
        let (span_start, span_end) = (cum, cum + m.text.len() as i64);
        cum = span_end;
        let (lo, hi) = (start.max(span_start), end.min(span_end));
        if lo >= hi {
            continue;
        }
        let part_start = rune_offset(&m.text, lo - span_start);
        let part_end = rune_offset(&m.text, hi - span_start);
        if part_end <= part_start || part_start < 0 {
            continue;
        }
        let (s, e) = (m.start_rune + part_start, m.start_rune + part_end);
        match ranges.last_mut() {
            Some(last) if last.message == m.message_index && last.end == s => last.end = e,
            _ => ranges.push(RuneRange {
                message: m.message_index,
                start: s,
                end: e,
            }),
        }
    }
    ranges
}

/// BuildResponsesURLCitationsForMessages: url_citation annotations per message index,
/// with Gemini part byte offsets turned into message rune offsets. `None` without
/// supports or chunks (Go's nil map).
pub(crate) fn url_citations(
    gm: &Res<'_>,
    mappings: &[PartMapping],
    message_texts: &[Vec<u8>],
) -> Option<HashMap<i64, Vec<Vec<u8>>>> {
    let chunks = gm.get("groundingChunks").array();
    let supports = gm.get("groundingSupports").array();
    if supports.is_empty() || chunks.is_empty() {
        return None;
    }
    let mut coalesced: Vec<PartMapping> = vec![];
    for m in mappings {
        match coalesced.last_mut() {
            Some(last) if last.part_index == m.part_index && last.message_index == m.message_index => {
                last.text.extend_from_slice(&m.text)
            }
            _ => coalesced.push(m.clone()),
        }
    }
    let mut result: HashMap<i64, Vec<Vec<u8>>> = HashMap::new();
    let mut seen = HashSet::new();
    for support in supports {
        let segment = support.get("segment");
        let part = segment.get("partIndex");
        let start = segment.get("startIndex").int();
        let end = segment.get("endIndex").int();
        let ranges = if part.exists() {
            let part_index = part.int();
            let mut matching: Vec<PartMapping> = coalesced
                .iter()
                .filter(|m| m.part_index == part_index)
                .cloned()
                .collect();
            if matching.is_empty() && part_index == 0 && coalesced.len() == 1 {
                matching = coalesced.clone();
            }
            rune_ranges(&matching, start, end)
        } else if !coalesced.is_empty() {
            rune_ranges(&coalesced, start, end)
        } else if let Some(text) = message_texts.first() {
            let (s, e) = (rune_offset(text, start), rune_offset(text, end));
            if e > s && s >= 0 {
                vec![RuneRange {
                    message: 0,
                    start: s,
                    end: e,
                }]
            } else {
                vec![]
            }
        } else {
            vec![]
        };
        if ranges.is_empty() {
            continue;
        }
        for idx in support.get("groundingChunkIndices").array() {
            let idx = idx.int();
            if idx < 0 || idx >= chunks.len() as i64 {
                continue;
            }
            let chunk = &chunks[idx as usize];
            let uri = trim_space(&chunk.get("web.uri").bytes()).to_vec();
            let title = trim_space(&chunk.get("web.title").bytes()).to_vec();
            if uri.is_empty() {
                continue;
            }
            for r in &ranges {
                if !seen.insert((r.message, uri.clone(), r.start, r.end)) {
                    continue;
                }
                let mut cite = br#"{"type":"url_citation","url":"","title":"","start_index":0,"end_index":0}"#.to_vec();
                gj::set_str(&mut cite, "url", &uri);
                gj::set_str(&mut cite, "title", &title);
                gj::set_int(&mut cite, "start_index", r.start);
                gj::set_int(&mut cite, "end_index", r.end);
                result.entry(r.message).or_default().push(cite);
            }
        }
    }
    Some(result)
}
