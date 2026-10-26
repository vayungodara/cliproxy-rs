//! Replays Go-generated goldens (tests/reference) for every pair in tests/fixtures/pairs.
use cpa_common::json as gj;
use cpa_core::format::Format;
use cpa_translate::{
    RequestCtx, ResponseCtx, go_stream, pair, sse::Framer, token_count, translate_request, translate_token_count,
};
use serde_json::Value;
use std::{
    collections::HashMap,
    time::{SystemTime, UNIX_EPOCH},
};

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

fn field(v: &Value, bytes: bool) -> Vec<u8> {
    let s = v.as_str().unwrap_or_default();
    if bytes {
        s.chars().map(|c| c as u32 as u8).collect()
    } else {
        s.as_bytes().to_vec()
    }
}

/// Byte range of the JSON document `data` inside one output (-1: the whole output).
fn doc_range(out: &[u8], data: i64) -> Option<(usize, usize)> {
    if data < 0 {
        return Some((0, out.len()));
    }
    let mut start = 0;
    for (i, line) in out.split(|&c| c == b'\n').enumerate() {
        if i as i64 == data {
            let rest = line.strip_prefix(b"data:")?;
            let lead = rest.len() - rest.trim_ascii_start().len();
            let trimmed = rest.trim_ascii();
            let s = start + 5 + lead;
            return Some((s, s + trimmed.len()));
        }
        start += line.len() + 1;
    }
    None
}

/// Unix seconds of a `YYYY-MM-DDThh:mm:ss[.frac](Z|±hh:mm)` stamp.
fn parse_rfc3339(s: &[u8]) -> Option<i64> {
    let s = std::str::from_utf8(s).ok()?;
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, se) = (
        num(0..4)?,
        num(5..7)?,
        num(8..10)?,
        num(11..13)?,
        num(14..16)?,
        num(17..19)?,
    );
    let rest = s.get(19..)?;
    let rest = rest.trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    let offset = match rest {
        "Z" => 0,
        _ => {
            let sign = if rest.starts_with('-') { -1 } else { 1 };
            sign * (rest.get(1..3)?.parse::<i64>().ok()? * 3600 + rest.get(4..6)?.parse::<i64>().ok()? * 60)
        }
    };
    let (y, mo) = if mo <= 2 { (y - 1, mo + 12) } else { (y, mo) };
    let days = 365 * y + y / 4 - y / 100 + y / 400 + (153 * (mo - 3) + 2) / 5 + d - 719_469;
    Some(days * 86_400 + h * 3600 + mi * 60 + se - offset)
}

struct Dyn {
    out: usize,
    data: i64,
    path: String,
    prefix: String,
    time: bool,
}

/// Replaces each dynamic value with `"<dynN>"`, numbering distinct values in order of
/// appearance so repeated IDs must stay consistent. Returns shape errors.
fn normalize(outputs: &mut [Vec<u8>], dynamics: &[Dyn], check: Option<(i64, i64)>) -> Vec<String> {
    let mut errors = vec![];
    let mut ids: HashMap<Vec<u8>, usize> = HashMap::new();
    for d in dynamics {
        let Some(out) = outputs.get_mut(d.out) else {
            errors.push(format!("missing output {}", d.out));
            continue;
        };
        let Some((s, e)) = doc_range(out, d.data) else {
            errors.push(format!("missing data line {} in output {}", d.data, d.out));
            continue;
        };
        let mut doc = out[s..e].to_vec();
        let value = gj::get(&doc, &d.path);
        if !value.exists() {
            errors.push(format!("dynamic path {} missing in output {}", d.path, d.out));
            continue;
        }
        if let Some((start, end)) = check {
            if d.time {
                let n = match value.kind {
                    gj::Kind::String => parse_rfc3339(&value.s).unwrap_or(0),
                    _ => value.int(),
                };
                if !((start - 2..=end + 2).contains(&n) || ((start - 2) * 1000..=(end + 2) * 1000).contains(&n)) {
                    errors.push(format!("{} = {n} is not a current timestamp", d.path));
                }
            } else if !d.prefix.is_empty() && !value.s.starts_with(d.prefix.as_bytes()) {
                errors.push(format!("{} = {} lacks prefix {}", d.path, value.str(), d.prefix));
            }
        }
        let next = ids.len();
        let n = *ids.entry(value.raw.to_vec()).or_insert(next);
        gj::set_raw(&mut doc, &d.path, format!("\"<dyn{n}>\""));
        out.splice(s..e, doc);
    }
    errors
}

/// A request translation's output, or its refusal as `REFUSED:<message>`: Go records the
/// refusal as the fixture's `request_error`.
fn request_output(result: Result<Vec<u8>, cpa_translate::Error>) -> Vec<u8> {
    result.unwrap_or_else(|e| format!("REFUSED:{}", e.0).into_bytes())
}

/// Fixtures regenerated at v8.0.35 whose port is still open: pair, fixture name (unique
/// within a pair; see `pending` for numbered repeats) and the work that closes them. Every fixture an entry matches must still fail, so the list
/// shrinks as ports land and a listed fixture cannot hide a passing neighbour's regression.
const PENDING: &[(&str, &str, &str)] = &[
    (
        "claude->codex",
        "TestConvertClaudeRequestToCodex_AssistantMetaSignatureToReasoningItem:731",
        "signature replay: Meta Muse envelopes (7a0b7563, cc5682c5, 674dbe8f, a68c7d8e)",
    ),
    (
        "claude->codex",
        "TestConvertClaudeRequestToCodex_AssistantRoutedMetaSignatureToReasoningItem:764",
        "signature replay: Meta Muse envelopes (7a0b7563, cc5682c5, 674dbe8f, a68c7d8e)",
    ),
    (
        "claude->codex",
        "TestConvertClaudeRequestToCodex_AssistantRoutedMetaSignatureToReasoningItem:781",
        "signature replay: Meta Muse envelopes (7a0b7563, cc5682c5, 674dbe8f, a68c7d8e)",
    ),
    (
        "claude->codex",
        "TestConvertClaudeRequestToCodex_AssistantRoutedMetaSignatureToReasoningItem:788",
        "signature replay: Meta Muse envelopes (7a0b7563, cc5682c5, 674dbe8f, a68c7d8e)",
    ),
    (
        "openai-response->openai",
        "TestLocalShellDeclarationSurvivesChatTranslation:16",
        "Responses local_shell tool (91c934f6)",
    ),
    (
        "openai-response->openai",
        "TestLocalShellHistoryWithoutDeclaration:171#*",
        "Responses local_shell tool (91c934f6)",
    ),
    (
        "openai-response->openai",
        "TestLocalShellInvalidActions:37#*",
        "Responses local_shell tool (91c934f6)",
    ),
    (
        "openai-response->openai",
        "TestLocalShellInvalidActions:44:stream#*",
        "Responses local_shell tool (91c934f6)",
    ),
    (
        "openai-response->openai",
        "TestLocalShellOnlyExplicitLocalEnvironment:157#*",
        "Responses local_shell tool (91c934f6)",
    ),
    (
        "openai-response->openai",
        "TestLocalShellRoundTrip:37",
        "Responses local_shell tool (91c934f6)",
    ),
    (
        "openai-response->openai",
        "TestLocalShellRoundTrip:44:stream",
        "Responses local_shell tool (91c934f6)",
    ),
    (
        "openai-response->openai",
        "TestLocalShellRoundTrip:69",
        "Responses local_shell tool (91c934f6)",
    ),
    (
        "openai-response->openai",
        "TestLocalShellRoundTrip:103",
        "Responses local_shell tool (91c934f6)",
    ),
];

/// A key names one fixture; a key ending in `#*` names that fixture and its numbered repeats
/// (`name#2`, `name#3`, ...).
fn pending(pair: &str, fixture: &Value) -> Option<usize> {
    let name = fixture["name"].as_str().unwrap_or_default();
    PENDING.iter().position(|(p, key, _)| {
        *p == pair
            && match key.strip_suffix("#*") {
                Some(base) => name
                    .strip_prefix(base)
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with('#')),
                None => name == *key,
            }
    })
}

fn run(client: Format, upstream: Format, f: &Value, bytes: bool, tool_search: bool) -> Vec<Vec<u8>> {
    let model = f["model"].as_str().unwrap();
    let pair = pair(client, upstream).unwrap();
    let original = field(&f["original"], bytes);
    let translated = field(&f["translated"], bytes);
    let rctx = ResponseCtx {
        model,
        original_request: &original,
        translated_request: &translated,
        tool_search,
    };
    let input = field(&f["input"], bytes);
    let ctx = RequestCtx {
        model,
        stream: f["stream"].as_bool().unwrap_or(false),
        tool_search,
    };
    match f["path"].as_str().unwrap() {
        "request" => vec![request_output(translate_request(client, upstream, &ctx, &input))],
        "request_compat" => vec![request_output(match (client, upstream) {
            (Format::OpenAI, Format::Claude) => cpa_translate::openai_to_claude_with_compat(&ctx, &input),
            (Format::OpenAIResponse, Format::Claude) => cpa_translate::responses_to_claude_with_compat(&ctx, &input),
            (Format::Claude, Format::OpenAI) => cpa_translate::claude_to_openai_with_compat(&ctx, &input),
            (Format::Claude, Format::Gemini) => cpa_translate::claude_to_gemini_with_compat(&ctx, &input),
            (Format::Claude, Format::Codex) => cpa_translate::claude_to_codex_with_compat(&ctx, &input),
            (Format::Claude, Format::Interactions) => cpa_translate::claude_to_interactions_with_compat(&ctx, &input),
            other => panic!("no compat request for {other:?}"),
        })],
        "request_envelope" => {
            // The executor's ResolvedModelInfo with native web search on.
            let info = cpa_core::registry::ModelInfo::from_raw(
                serde_json::json!({"id": model, "native_capabilities": {"web_search": true}})
                    .as_object()
                    .unwrap()
                    .clone(),
            )
            .unwrap();
            vec![request_output(cpa_translate::translate_request_envelope(
                client,
                upstream,
                &ctx,
                &input,
                Some(&info),
            ))]
        }
        // Go returns nil (written as "") when the apply_patch bridge rejects the body.
        "non_stream" => vec![match (pair.non_stream)(&rctx, &input) {
            Ok(out) => out,
            Err(e) if e.0 == cpa_translate::APPLY_PATCH_UPSTREAM_ERROR => vec![],
            Err(e) => panic!("non_stream failed: {e}"),
        }],
        "token_count" => vec![translate_token_count(
            client,
            upstream,
            f["count"].as_i64().unwrap_or(0),
            &input,
        )],
        "stream" => {
            let mut s = go_stream(client, upstream).unwrap()(&rctx);
            let mut out = vec![];
            for line in f["lines"].as_array().unwrap() {
                out.extend(s.line(&field(line, bytes)).unwrap());
            }
            out
        }
        other => panic!("unknown fixture path {other}"),
    }
}

/// JSON with object keys sorted recursively (for outputs whose Go key order varies).
fn canonical(raw: &[u8]) -> Vec<u8> {
    fn sorted(value: Value) -> Value {
        match value {
            Value::Object(map) => {
                let mut entries: Vec<(String, Value)> = map.into_iter().map(|(k, v)| (k, sorted(v))).collect();
                entries.sort_by(|a, b| a.0.cmp(&b.0));
                Value::Object(entries.into_iter().collect())
            }
            Value::Array(items) => Value::Array(items.into_iter().map(sorted).collect()),
            other => other,
        }
    }
    match serde_json::from_slice::<Value>(raw) {
        Ok(value) => serde_json::to_vec(&sorted(value)).unwrap(),
        Err(_) => raw.to_vec(),
    }
}

/// Replays one fixture; returns its mismatches.
fn check(client: Format, upstream: Format, f: &Value, bytes: bool, tool_search: bool) -> Vec<String> {
    let mut expected: Vec<Vec<u8>> = f["outputs"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|o| o.as_array().unwrap().iter().map(|c| field(c, bytes)))
        .collect();
    if let Some(refusal) = f["request_error"].as_str() {
        expected = vec![format!("REFUSED:{refusal}").into_bytes()];
    }
    let dynamics: Vec<Dyn> = f["dynamic"]
        .as_array()
        .map(|d| {
            d.iter()
                .map(|d| Dyn {
                    out: d["out"].as_u64().unwrap() as usize,
                    data: d["data"].as_i64().unwrap(),
                    path: d["path"].as_str().unwrap().into(),
                    prefix: d["prefix"].as_str().unwrap_or_default().into(),
                    time: d["time"].as_bool().unwrap_or(false),
                })
                .collect()
        })
        .unwrap_or_default();
    // Mismatches found while running (chunk counts, error states).
    let mut early: Vec<String> = vec![];
    let start = now();
    let mut actual = if f["path"] == "stream" {
        // Compare per input line: chunk boundaries matter.
        let model = f["model"].as_str().unwrap();
        let original = field(&f["original"], bytes);
        let translated = field(&f["translated"], bytes);
        let rctx = ResponseCtx {
            model,
            original_request: &original,
            translated_request: &translated,
            tool_search,
        };
        let mut s = go_stream(client, upstream).unwrap()(&rctx);
        let mut out = vec![];
        let lines = f["lines"].as_array().unwrap();
        for (i, line) in lines.iter().enumerate() {
            let chunks = s.line(&field(line, bytes)).unwrap();
            let want = f["outputs"][i].as_array().unwrap().len();
            if chunks.len() != want {
                early.push(format!("line {i} produced {} chunks, Go {want}", chunks.len()));
            }
            out.extend(chunks);
        }
        if f["finalize"].as_bool().unwrap_or(false) {
            let chunks = s.finalize_tool_input();
            let want = f["outputs"][lines.len()].as_array().unwrap().len();
            if chunks.len() != want {
                early.push(format!("finalize produced {} chunks, Go {want}", chunks.len()));
            }
            out.extend(chunks);
        }
        if s.tool_input_failed() != f["tool_error"].as_bool().unwrap_or(false) {
            early.push("tool input error state differs from Go".to_owned());
        }
        out
    } else {
        let out = run(client, upstream, f, bytes, tool_search);
        if f["path"] == "non_stream"
            && out[0].is_empty() != f["tool_error"].as_bool().unwrap_or(false)
            && f["tool_error"].as_bool().unwrap_or(false)
        {
            early.push("Go rejected the apply_patch body, Rust did not".to_owned());
        }
        if f["path"] == "non_stream" && f["tool_error"].as_bool().unwrap_or(false) && out[0].is_empty() {
            // Go's executors answer 502 and drop whatever body the translator
            // returned with ToolInputError set; Rust returns the error instead.
            expected.clone()
        } else {
            out
        }
    };
    if let Some(variants) = f["variants"].as_array()
        && let Some(out) = actual.first()
        && variants.iter().any(|v| canonical(&field(v, bytes)) == canonical(out))
    {
        // Go's own output order varies here (map iteration); any order it
        // produced is accepted.
        actual = expected.clone();
    }
    let end = now();
    if let Some(variants) = f["stream_variants"].as_array() {
        // Go's output order varies (map iteration); any order it produced is
        // accepted.
        let produced = variants.iter().any(|v| {
            let lists: Vec<Vec<String>> = serde_json::from_str(v.as_str().unwrap()).unwrap();
            let flat: Vec<Vec<u8>> = lists.into_iter().flatten().map(String::into_bytes).collect();
            flat == actual
        });
        if produced {
            actual = expected.clone();
        }
    }
    let mut errors = early;
    errors.extend(normalize(&mut actual, &dynamics, Some((start, end))));
    normalize(&mut expected, &dynamics, None);
    if actual != expected {
        let show = |v: &Vec<Vec<u8>>| {
            v.iter()
                .map(|o| String::from_utf8_lossy(o).into_owned())
                .collect::<Vec<_>>()
        };
        errors.push(format!("\n  go:   {:?}\n  rust: {:?}", show(&expected), show(&actual)));
    }
    errors
}

/// Pair fixtures (recorded at 6fecc6e) whose behaviour a later Go commit changed, each
/// replayed at v8.0.35 by the named golden instead.
const SUPERSEDED: &[(&str, &str)] = &[(
    "claude->antigravity tools/schema/11",
    "the v8.0.35 schema cleaner (a0fd8448, c558d320): cpa-common gemini_schema.json.gz; \
         the same fixture recorded at v8.0.35 matches",
)];

#[test]
fn reference_goldens() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/pairs");
    let mut files: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).collect();
    files.sort();
    assert!(!files.is_empty());
    let mut failures = vec![];
    let mut total = 0;
    // Per pending entry: fixtures matched, and how many of them failed.
    let mut pending_matched = vec![0; PENDING.len()];
    let mut pending_failed = vec![0; PENDING.len()];
    for file in files {
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        let client = Format::parse(doc["client"].as_str().unwrap()).unwrap();
        let upstream = Format::parse(doc["upstream"].as_str().unwrap()).unwrap();
        if pair(client, upstream).is_none() {
            failures.push(format!("{}: pair not registered", file.display()));
            continue;
        }
        assert_eq!(
            token_count(client, upstream).is_some(),
            doc["token_count"].as_bool().unwrap(),
            "{client:?}->{upstream:?}"
        );
        let fixtures = doc["fixtures"].as_array().unwrap();
        assert!(
            fixtures.len() > 20,
            "fixture generation lost cases for {}",
            file.display()
        );
        // PENDING keys are exact names, so names must be unique within a pair.
        let mut names = std::collections::HashSet::new();
        for f in fixtures {
            total += 1;
            if !names.insert(f["name"].as_str().unwrap()) {
                failures.push(format!("{}: duplicate fixture name {}", file.display(), f["name"]));
            }
            let bytes = f["bytes"].as_bool().unwrap_or(false);
            let name = format!(
                "{}->{} {}",
                client.as_str(),
                upstream.as_str(),
                f["name"].as_str().unwrap()
            );
            if SUPERSEDED.iter().any(|(n, _)| *n == name) {
                continue;
            }
            let errors = check(client, upstream, f, bytes, false);
            let pending = pending(&format!("{}->{}", client.as_str(), upstream.as_str()), f);
            if let Some(i) = pending {
                pending_matched[i] += 1;
            }
            if !errors.is_empty() {
                match pending {
                    Some(i) => pending_failed[i] += 1,
                    None => failures.push(format!("{name}: {}", errors.join("; "))),
                }
            }
        }
    }
    let keys = PENDING.iter().map(|(pair, key, _)| format!("{pair} {key}"));
    for ((key, matched), failed) in keys.zip(&pending_matched).zip(&pending_failed) {
        if *matched == 0 {
            failures.push(format!("{key}: pending entry matches no fixture"));
        } else if failed != matched {
            failures.push(format!(
                "{key}: pending entry matches {matched} fixtures but only {failed} fail; narrow or remove it"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {total} fixtures failed:\n{}",
        failures.len(),
        failures[..failures.len().min(400)].join("\n")
    );
}

/// upstream.claude.tool-search on: every openai-response->claude fixture, replaced by its
/// re-recording in claude_tool_search.json where Go's output changes with the switch (the
/// generator's `tool_search` mode). The rest Go produces unchanged.
#[test]
fn tool_search_goldens() {
    let read = |path: &str| -> Value { serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap() };
    let on = read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/claude_tool_search.json"
    ));
    let off = read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/pairs/openai-response-claude.json"
    ));
    let overrides: HashMap<&str, &Value> = on["fixtures"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| (f["name"].as_str().unwrap(), f))
        .collect();
    assert!(overrides.len() > 50, "fixture generation lost tool_search cases");
    let mut failures = vec![];
    let mut used = 0;
    for f in off["fixtures"].as_array().unwrap() {
        let f = overrides
            .get(f["name"].as_str().unwrap())
            .copied()
            .inspect(|_| used += 1)
            .unwrap_or(f);
        let errors = check(
            Format::OpenAIResponse,
            Format::Claude,
            f,
            f["bytes"].as_bool().unwrap_or(false),
            true,
        );
        if !errors.is_empty() {
            failures.push(format!("{}: {}", f["name"].as_str().unwrap(), errors.join("; ")));
        }
    }
    assert_eq!(used, overrides.len(), "a tool_search fixture names no pair fixture");
    assert!(
        failures.is_empty(),
        "{} fixtures failed:\n{}",
        failures.len(),
        failures[..failures.len().min(400)].join("\n")
    );
}

#[test]
fn fragmented_sse_keeps_tool_state_request_local() {
    let pair = pair(Format::OpenAI, Format::Claude).unwrap();
    let ctx = ResponseCtx {
        model: "m",
        original_request: b"{}",
        translated_request: b"{}",
        tool_search: false,
    };
    let input = b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":3,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call-x\",\"name\":\"f\"}}\n\ndata: {\"type\":\"content_block_delta\",\"index\":3,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"x\\\":1}\"}}\r\n\r\ndata: {\"type\":\"content_block_stop\",\"index\":3}\n\n";
    for width in [1, 7, 23, input.len()] {
        let mut framer = Framer::default();
        let mut stream = (pair.stream)(&ctx);
        let mut output = Vec::new();
        for chunk in input.chunks(width) {
            for event in framer.push(chunk).unwrap() {
                output.extend(stream.event(&event).unwrap());
            }
        }
        assert_eq!(output.len(), 1);
        let raw = std::str::from_utf8(&output[0]).unwrap();
        assert!(
            raw.starts_with("data: ") && raw.ends_with("\n\n"),
            "OpenAI clients get data frames"
        );
        let value: Value = serde_json::from_str(raw.trim().strip_prefix("data: ").unwrap()).unwrap();
        assert_eq!(
            value["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            "{\"x\":1}"
        );
        assert!(
            (pair.stream)(&ctx)
                .event(b"data: {\"type\":\"content_block_stop\",\"index\":3}\n\n")
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn openai_passthrough_is_byte_oriented() {
    // Invalid UTF-8 must neither error nor be repaired (oracle P2).
    let pair = pair(Format::OpenAI, Format::OpenAI).unwrap();
    let ctx = ResponseCtx {
        model: "m",
        original_request: b"",
        translated_request: b"",
        tool_search: false,
    };
    let mut s = (pair.stream)(&ctx);
    let out = s.event(b"data: {\"x\":\"\xff\"}\n\n").unwrap();
    assert_eq!(out, [bytes::Bytes::from_static(b"data: {\"x\":\"\xff\"}\n\n")]);
    assert!(s.event(b"data: [DONE]\n\n").unwrap().is_empty());
    assert!(s.event(b"data: {}\n\n").unwrap().is_empty(), "nothing after [DONE]");
    let body = b"{\"model\":\"m\",\"x\":\"\xff\"}";
    let req = RequestCtx {
        model: "m",
        stream: true,
        tool_search: false,
    };
    assert_eq!(
        translate_request(Format::OpenAI, Format::OpenAI, &req, body).unwrap(),
        body
    );
}

#[test]
fn registration_matches_go() {
    for upstream in [Format::Claude, Format::OpenAI] {
        let p = pair(Format::OpenAI, upstream).unwrap();
        assert!(p.count_tokens.is_none() && token_count(Format::OpenAI, upstream).is_none());
    }
    assert!(pair(Format::Claude, Format::Claude).is_none());
    // Unregistered pairs fall back to a model rewrite only.
    let ctx = RequestCtx {
        model: "new",
        stream: false,
        tool_search: false,
    };
    assert_eq!(
        translate_request(Format::Claude, Format::Claude, &ctx, br#"{"model":"old","x":"<"}"#).unwrap(),
        br#"{"model":"new","x":"<"}"#
    );
    assert_eq!(
        translate_request(Format::Claude, Format::Claude, &ctx, br#"{"model":"new"}"#).unwrap(),
        br#"{"model":"new"}"#
    );
    assert_eq!(translate_token_count(Format::Claude, Format::Claude, 5, b"raw"), b"raw");
}

#[test]
fn deeply_nested_client_bodies_translate_without_overflowing() {
    let depth = 50_000;
    let schema = [vec![b'['; depth], vec![b']'; depth]].concat();
    let body = [
        &br#"{"tools":[{"name":"t","input_schema":{"type":"object","properties":{"x":"#[..],
        &schema,
        b"}}}],\"messages\":[{\"role\":\"user\",\"content\":\"x\"}]}",
    ]
    .concat();
    let ctx = RequestCtx {
        model: "gpt-test",
        stream: false,
        tool_search: false,
    };
    let out = translate_request(Format::Claude, Format::OpenAI, &ctx, &body).unwrap();
    assert!(out.starts_with(br#"{"model":"gpt-test","messages":"#));
    assert!(out.windows(depth).any(|w| w.iter().all(|&c| c == b'[')));
}

#[test]
fn deeply_nested_upstream_bodies_translate_without_overflowing() {
    let depth = 50_000;
    let nested = [vec![b'['; depth], vec![b']'; depth]].concat();
    let rctx = ResponseCtx {
        model: "m",
        original_request: b"{}",
        translated_request: b"{}",
        tool_search: false,
    };
    // Non-stream: a Gemini body whose function-call arguments nest deeply.
    let body = [
        &br#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"f","args":{"x":"#[..],
        &nested,
        b"}}}]}}]}",
    ]
    .concat();
    let out = (pair(Format::Claude, Format::Gemini).unwrap().non_stream)(&rctx, &body).unwrap();
    assert!(out.starts_with(br#"{"id":"","type":"message""#));
    // Stream: the same payload as one upstream event.
    let mut stream = (pair(Format::OpenAI, Format::Gemini).unwrap().stream)(&rctx);
    let chunks = stream.event(&[&b"data: "[..], &body, b"\n\n"].concat()).unwrap();
    assert_eq!(chunks.len(), 1);
}

#[test]
fn deeply_nested_retained_state_translates_without_overflowing() {
    let depth = 50_000;
    let nested = [vec![b'['; depth], vec![b']'; depth]].concat();
    let deep_run = |out: &[u8]| out.windows(depth).any(|w| w.iter().all(|&c| c == b'['));
    // Responses completions echo the request's metadata (Go's Value() re-marshal) while
    // the upstream events themselves are shallow.
    let original = [&br#"{"model":"m","input":"q","metadata":{"x":"#[..], &nested, b"}}"].concat();
    let ctx = ResponseCtx {
        model: "gemini-2.5-pro",
        original_request: &original,
        translated_request: b"{}",
        tool_search: false,
    };
    let gemini = pair(Format::OpenAIResponse, Format::Gemini).unwrap();
    let mut stream = (gemini.stream)(&ctx);
    let mut out = vec![];
    for event in [
        &b"data: {\"responseId\":\"r\",\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"x\"}]}}]}\n\n"[..],
        b"data: [DONE]\n\n",
    ] {
        out.extend(stream.event(event).unwrap());
    }
    out.extend(stream.finish().unwrap());
    let completed = out
        .iter()
        .find(|f| f.starts_with(b"event: response.completed"))
        .unwrap();
    assert!(deep_run(completed));
    let body = br#"{"responseId":"r","candidates":[{"content":{"parts":[{"text":"x"}]},"finishReason":"STOP"}]}"#;
    assert!(deep_run(&(gemini.non_stream)(&ctx, body).unwrap()));

    // Tool arguments that only nest once the stream's string deltas are joined.
    let ctx = ResponseCtx {
        model: "claude-sonnet-4-5",
        original_request: br#"{"model":"m","input":"q"}"#,
        translated_request: b"{}",
        tool_search: false,
    };
    let mut stream = (pair(Format::OpenAIResponse, Format::Claude).unwrap().stream)(&ctx);
    let ev = |json: &str| format!("data: {json}\n\n").into_bytes();
    let mut out = vec![];
    out.extend(
        stream
            .event(&ev(r#"{"type":"message_start","message":{"id":"m1","model":"c"}}"#))
            .unwrap(),
    );
    out.extend(
        stream
            .event(&ev(
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"f","input":{}}}"#,
            ))
            .unwrap(),
    );
    for part in [&b"{\"x\":"[..], &vec![b'['; depth], &vec![b']'; depth], b"}"] {
        for piece in part.chunks(5_000) {
            let delta = serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":String::from_utf8(piece.to_vec()).unwrap()}});
            out.extend(stream.event(&ev(&delta.to_string())).unwrap());
        }
    }
    for json in [
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":1}}"#,
        r#"{"type":"message_stop"}"#,
    ] {
        out.extend(stream.event(&ev(json)).unwrap());
    }
    out.extend(stream.finish().unwrap());
    let completed = out
        .iter()
        .find(|f| f.starts_with(b"event: response.completed"))
        .unwrap();
    assert!(deep_run(completed));
}

#[test]
fn apply_patch_failures_reach_the_stream_contract() {
    let original = br#"{"model":"m","input":"q","tools":[{"type":"custom","name":"apply_patch"}]}"#;
    let ctx = ResponseCtx {
        model: "gemini-2.5-pro",
        original_request: original,
        translated_request: b"{}",
        tool_search: false,
    };
    let pair = pair(Format::OpenAIResponse, Format::Gemini).unwrap();
    let failed = |frame: &bytes::Bytes| frame.starts_with(b"event: response.failed\ndata: ");

    // Invalid patch input: the event's frames end in response.failed, then the executor
    // stops with the 502 message.
    let mut stream = (pair.stream)(&ctx);
    let frames = stream
        .event(b"data: {\"responseId\":\"r\",\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hi\"},{\"functionCall\":{\"name\":\"apply_patch\",\"args\":{\"input\":5}}}]}}]}\n\n")
        .unwrap();
    assert!(stream.tool_input_failed());
    assert!(failed(frames.last().unwrap()), "{frames:?}");
    assert!(frames.iter().filter(|f| failed(f)).count() == 1);

    // A patch-enabled stream that ends without its terminator fails at EOF.
    let mut stream = (pair.stream)(&ctx);
    stream
        .event(b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"x\"}]}}]}\n\n")
        .unwrap();
    assert!(!stream.tool_input_failed());
    let frames = stream.finalize_tool_input();
    assert_eq!(frames.len(), 1);
    assert!(failed(&frames[0]));
    assert!(stream.tool_input_failed());
    assert!(stream.finalize_tool_input().is_empty(), "the failure is reported once");

    // A completed stream, or one without apply_patch, has nothing to finalize.
    let mut stream = (pair.stream)(&ctx);
    stream
        .event(b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"x\"}]},\"finishReason\":\"STOP\"}]}\n\n")
        .unwrap();
    assert!(stream.finalize_tool_input().is_empty());
    let plain = ResponseCtx {
        original_request: br#"{"input":"q"}"#,
        ..ctx
    };
    let mut stream = (pair.stream)(&plain);
    stream
        .event(b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"x\"}]}}]}\n\n")
        .unwrap();
    assert!(stream.finalize_tool_input().is_empty());
    assert!(!stream.tool_input_failed());

    // Buffered responses report the same failure as an error carrying Go's message.
    let err = (pair.non_stream)(
        &ctx,
        br#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"apply_patch","args":{"input":5}}}]}}]}"#,
    )
    .unwrap_err();
    assert_eq!(err.0, cpa_translate::APPLY_PATCH_UPSTREAM_ERROR);
}

#[test]
fn golden_rfc3339_parser_matches_unix_seconds() {
    assert_eq!(parse_rfc3339(b"1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(parse_rfc3339(b"2023-11-14T22:13:20Z"), Some(1_700_000_000));
    assert_eq!(parse_rfc3339(b"2000-02-29T01:00:00+01:00"), Some(951_782_400));
    assert_eq!(parse_rfc3339(b"2025-08-15T02:52:03.884209Z"), Some(1_755_226_323));
}
