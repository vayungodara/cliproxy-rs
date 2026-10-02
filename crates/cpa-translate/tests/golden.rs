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
                let n = value.int();
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

fn run(client: Format, upstream: Format, f: &Value, bytes: bool) -> Vec<Vec<u8>> {
    let model = f["model"].as_str().unwrap();
    let pair = pair(client, upstream).unwrap();
    let original = field(&f["original"], bytes);
    let translated = field(&f["translated"], bytes);
    let rctx = ResponseCtx {
        model,
        original_request: &original,
        translated_request: &translated,
    };
    let input = field(&f["input"], bytes);
    let ctx = RequestCtx {
        model,
        stream: f["stream"].as_bool().unwrap_or(false),
    };
    match f["path"].as_str().unwrap() {
        "request" => vec![translate_request(client, upstream, &ctx, &input).unwrap()],
        "request_compat" => vec![
            match (client, upstream) {
                (Format::OpenAI, Format::Claude) => cpa_translate::openai_to_claude_with_compat(&ctx, &input),
                (Format::Claude, Format::OpenAI) => cpa_translate::claude_to_openai_with_compat(&ctx, &input),
                other => panic!("no compat request for {other:?}"),
            }
            .unwrap(),
        ],
        "non_stream" => vec![(pair.non_stream)(&rctx, &input).unwrap()],
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

#[test]
fn reference_goldens() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/pairs");
    let mut files: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).collect();
    files.sort();
    assert!(!files.is_empty());
    let mut failures = vec![];
    let mut total = 0;
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
        for f in fixtures {
            total += 1;
            let bytes = f["bytes"].as_bool().unwrap_or(false);
            let name = format!(
                "{}->{} {}",
                client.as_str(),
                upstream.as_str(),
                f["name"].as_str().unwrap()
            );
            let mut expected: Vec<Vec<u8>> = f["outputs"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|o| o.as_array().unwrap().iter().map(|c| field(c, bytes)))
                .collect();
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
                };
                let mut s = go_stream(client, upstream).unwrap()(&rctx);
                let mut out = vec![];
                for (i, line) in f["lines"].as_array().unwrap().iter().enumerate() {
                    let chunks = s.line(&field(line, bytes)).unwrap();
                    let want = f["outputs"][i].as_array().unwrap().len();
                    if chunks.len() != want {
                        failures.push(format!("{name}: line {i} produced {} chunks, Go {want}", chunks.len()));
                    }
                    out.extend(chunks);
                }
                out
            } else {
                run(client, upstream, f, bytes)
            };
            let end = now();
            let mut errors = normalize(&mut actual, &dynamics, Some((start, end)));
            normalize(&mut expected, &dynamics, None);
            if actual != expected {
                let show = |v: &Vec<Vec<u8>>| {
                    v.iter()
                        .map(|o| String::from_utf8_lossy(o).into_owned())
                        .collect::<Vec<_>>()
                };
                errors.push(format!("\n  go:   {:?}\n  rust: {:?}", show(&expected), show(&actual)));
            }
            if !errors.is_empty() {
                failures.push(format!("{name}: {}", errors.join("; ")));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {total} fixtures failed:\n{}",
        failures.len(),
        failures[..failures.len().min(12)].join("\n")
    );
}

#[test]
fn fragmented_sse_keeps_tool_state_request_local() {
    let pair = pair(Format::OpenAI, Format::Claude).unwrap();
    let ctx = ResponseCtx {
        model: "m",
        original_request: b"{}",
        translated_request: b"{}",
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
