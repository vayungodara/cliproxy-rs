use cpa_core::format::Format;
use cpa_translate::{RequestCtx, ResponseCtx, pair, sse::Framer};
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

fn has_message_start(raw: &str) -> bool {
    raw.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .any(|payload| {
            let message = gjson::parse(payload.trim());
            message.get("type").str() == "message_start" && message.get("message").exists()
        })
}

fn normalized(raw: &[u8], start: u64, end: u64, timestamp_required: bool) -> String {
    let mut raw = std::str::from_utf8(raw).unwrap().to_owned();
    let created = gjson::get(&raw, "created");
    if created.exists() {
        let time = created.u64();
        assert_eq!(time != 0, timestamp_required, "incorrect timestamp presence");
        assert!(
            time == 0 || (start..=end).contains(&time),
            "timestamp {time} outside {start}..={end}"
        );
        let offset = created.json().as_ptr() as usize - raw.as_ptr() as usize;
        let len = created.json().len();
        raw.replace_range(offset..offset + len, "0");
    }
    raw
}

#[test]
fn reference_goldens() {
    let fixtures: Vec<Value> = serde_json::from_str(include_str!("fixtures/go.json")).unwrap();
    assert!(fixtures.len() >= 160, "fixture generation lost cases");
    for fixture in fixtures {
        let name = fixture["name"].as_str().unwrap();
        let model = fixture["model"].as_str().unwrap();
        let client = Format::parse(fixture["client"].as_str().unwrap()).unwrap();
        let upstream = Format::parse(fixture["upstream"].as_str().unwrap()).unwrap();
        let pair = pair(client, upstream).unwrap();
        let response = ResponseCtx {
            model,
            original_request: b"{}",
            translated_request: b"{}",
        };
        let start = now();
        match fixture["path"].as_str().unwrap() {
            "request" | "request_compat" => {
                let ctx = RequestCtx {
                    model,
                    stream: fixture["stream"].as_bool().unwrap(),
                };
                let transform = if fixture["path"] == "request_compat" {
                    cpa_translate::openai_to_claude_with_compat
                } else {
                    pair.request
                };
                let output = transform(&ctx, fixture["input"].as_str().unwrap().as_bytes()).unwrap();
                let mut output = std::str::from_utf8(&output).unwrap().to_owned();
                if let Some(paths) = fixture["generated_ids"].as_array() {
                    for path in paths {
                        let path = path.as_str().unwrap();
                        let value = gjson::get(&output, path);
                        let id = value.str().strip_prefix("toolu_").expect("Claude tool ID prefix");
                        if path.ends_with("tool_use_id") {
                            let (time, count) = id.split_once('_').unwrap();
                            assert!((start..=now()).contains(&(time.parse::<u64>().unwrap() / 1_000_000_000)));
                            assert!(count.parse::<u64>().unwrap() > 0);
                        } else {
                            assert_eq!(id.len(), 24);
                            assert!(id.bytes().all(|c| c.is_ascii_alphanumeric()));
                        }
                        let offset = value.json().as_ptr() as usize - output.as_ptr() as usize;
                        let len = value.json().len();
                        output.replace_range(offset..offset + len, "\"generated\"");
                    }
                }
                assert_eq!(output, fixture["output"].as_str().unwrap(), "{name}");
            }
            "non_stream" => {
                let input = fixture["input"].as_str().unwrap();
                let output = (pair.non_stream)(&response, input.as_bytes()).unwrap();
                let actual = if upstream == Format::Claude {
                    normalized(&output, start, now(), has_message_start(input))
                } else {
                    String::from_utf8(output).unwrap()
                };
                assert_eq!(actual, fixture["output"].as_str().unwrap(), "{name}");
            }
            "stream" => {
                let mut stream = (pair.stream)(&response);
                let mut timestamp_required = false;
                for (i, event) in fixture["events"].as_array().unwrap().iter().enumerate() {
                    let input = if event.as_str().unwrap().starts_with("data:") {
                        format!("{}\r\n\r\n", event.as_str().unwrap())
                    } else {
                        event.as_str().unwrap().to_owned()
                    };
                    timestamp_required |= has_message_start(&input);
                    let output = stream.event(input.as_bytes()).unwrap();
                    let actual: Vec<_> = output
                        .iter()
                        .map(|chunk| {
                            let raw = std::str::from_utf8(chunk).unwrap();
                            assert!(
                                raw.starts_with("data: ") && raw.ends_with("\n\n"),
                                "{name}: unframed output"
                            );
                            let payload = &raw.as_bytes()[6..raw.len() - 2];
                            if upstream == Format::Claude {
                                normalized(payload, start, now(), timestamp_required)
                            } else {
                                std::str::from_utf8(payload).unwrap().to_owned()
                            }
                        })
                        .collect();
                    let expected: Vec<_> = fixture["chunks"][i]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|v| v.as_str().unwrap())
                        .collect();
                    assert_eq!(actual, expected, "{name}, event {i}");
                }
                assert!(stream.finish().unwrap().is_empty(), "{name}: unexpected EOF synthesis");
            }
            _ => panic!("unknown fixture path"),
        }
    }
}

#[test]
fn fragmented_sse_keeps_tool_state_request_local() {
    let pair = pair(Format::OpenAI, Format::Claude).unwrap();
    let ctx = ResponseCtx {
        model: "m",
        original_request: b"{}",
        translated_request: b"{}",
    };
    let input = b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":3,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call-x\",\"name\":\"f\"}}\n\ndata: {\"type\":\"content_block_delta\",\"index\":3,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"x\\\":1}\"}}\n\ndata: {\"type\":\"content_block_stop\",\"index\":3}\n\n";
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
        let raw = std::str::from_utf8(&output[0])
            .unwrap()
            .trim()
            .strip_prefix("data: ")
            .unwrap();
        let value: Value = serde_json::from_str(raw).unwrap();
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
fn count_registration_matches_go() {
    for upstream in [Format::Claude, Format::OpenAI] {
        assert!(pair(Format::OpenAI, upstream).unwrap().count_tokens.is_none());
    }
    assert!(pair(Format::Claude, Format::Claude).is_none());
}
