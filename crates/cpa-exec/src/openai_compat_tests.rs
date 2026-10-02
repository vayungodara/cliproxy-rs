//! Differential tests against `tests/fixtures/openai_compat_go.json`, produced by the real
//! Go executor (tests/reference/openai_compat). Each scenario replays the same scripted
//! upstream answer to the Rust executor and compares the raw upstream request and the
//! executor's output. Expected values come only from Go.

use std::sync::{Arc, Mutex};

use cpa_core::exec::Caller;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use super::*;

const FIXTURE: &str = include_str!("../tests/fixtures/openai_compat_go.json");

/// Shared helpers whose real port has not landed: scenarios that need them are skipped
/// and listed, so integration can remove an entry and see the scenario run.
const PENDING: &[&str] = &["thinking", "signature", "translator"];

/// One-shot raw HTTP/1.1 capture server answering with the scripted response.
struct Mock {
    addr: String,
    raw: Arc<Mutex<Option<Vec<u8>>>>,
}

impl Mock {
    async fn start(up: &Value) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let raw: Arc<Mutex<Option<Vec<u8>>>> = Arc::default();
        let sink = raw.clone();
        let status = up["status"].as_u64().unwrap() as u16;
        let headers: Vec<(String, String)> = up["headers"]
            .as_array()
            .map(|hs| {
                hs.iter()
                    .map(|h| (h[0].as_str().unwrap().to_owned(), h[1].as_str().unwrap().to_owned()))
                    .collect()
            })
            .unwrap_or_default();
        let mut body = up["body"].as_str().unwrap().as_bytes().to_vec();
        if up["gzip"].as_bool() == Some(true) {
            use async_compression::tokio::bufread::GzipEncoder;
            let mut encoder = GzipEncoder::new(std::io::Cursor::new(body));
            let mut out = Vec::new();
            encoder.read_to_end(&mut out).await.unwrap();
            body = out;
        }
        let no_length = up["no_length"].as_bool() == Some(true);
        tokio::spawn(async move {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            let (read, mut write) = socket.into_split();
            let mut reader = BufReader::new(read);
            let mut captured = Vec::new();
            let mut length = 0;
            loop {
                let mut line = Vec::new();
                if reader.read_until(b'\n', &mut line).await.unwrap_or(0) == 0 {
                    break;
                }
                captured.extend_from_slice(&line);
                let text = String::from_utf8_lossy(&line).to_ascii_lowercase();
                if let Some(v) = text.strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap_or(0);
                }
                if line == b"\r\n" {
                    break;
                }
            }
            let mut payload = vec![0; length];
            reader.read_exact(&mut payload).await.unwrap();
            captured.extend_from_slice(&payload);
            *sink.lock().unwrap() = Some(captured);
            let reason = http::StatusCode::from_u16(status)
                .ok()
                .and_then(|s| s.canonical_reason())
                .unwrap_or("");
            let mut out = format!("HTTP/1.1 {status} {reason}\r\n").into_bytes();
            for (n, v) in &headers {
                out.extend_from_slice(format!("{n}: {v}\r\n").as_bytes());
            }
            if !no_length {
                out.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
            }
            out.extend_from_slice(b"Connection: close\r\n\r\n");
            out.extend_from_slice(&body);
            let _ = write.write_all(&out).await;
            let _ = write.shutdown().await;
        });
        Self { addr, raw }
    }

    fn request(&self) -> Option<Vec<u8>> {
        let raw = self.raw.lock().unwrap().clone()?;
        let encoding = crate::openai_compat_go::GoText::new(&raw).unwrap();
        let mut text = encoding.text.replace(&self.addr, "UPSTREAM");
        // Go's multipart writer picks a random 60-hex-digit boundary; so does Rust.
        let marker = "boundary=";
        if let Some(i) = text.find(marker)
            && let Some(boundary) = text.get(i + marker.len()..i + marker.len() + 60)
            && boundary.bytes().all(|b| b.is_ascii_hexdigit())
        {
            text = text.replace(&boundary.to_owned(), "BOUNDARY");
        }
        Some(encoding.bytes(&text))
    }
}

fn credential(s: &Value, cfg: &Config, addr: &str) -> Credential {
    let index = s["config_auth"].as_i64().unwrap();
    if index >= 0 {
        let compat: Vec<Credential> = cpa_core::config::credentials::from_config(cfg)
            .into_iter()
            .filter(|c| {
                c.attributes.get("compat_name").is_some_and(|n| !n.is_empty()) || c.provider == "openai-compatibility"
            })
            .collect();
        return compat[index as usize].clone();
    }
    let metadata = s["metadata"].as_object().cloned().unwrap_or_default();
    let mut c = Credential::from_file(
        std::path::Path::new("/fixture"),
        std::path::Path::new("/fixture/compat.json"),
        {
            let mut m = metadata;
            m.insert("type".into(), s["provider"].clone());
            m
        },
    )
    .unwrap();
    for (k, v) in s["attributes"].as_object().into_iter().flatten() {
        c.attributes
            .insert(k.clone(), v.as_str().unwrap().replace("UPSTREAM", addr));
    }
    c
}

fn request(s: &Value) -> (ExecRequest, String) {
    let op = s["op"].as_str().unwrap();
    let source = Format::parse(s["source"].as_str().unwrap()).unwrap();
    let response = s["response"].as_str().and_then(Format::parse).unwrap_or(source);
    let model = s["model"].as_str().unwrap().to_owned();
    let payload = match s["payload_b64"].as_str() {
        Some(b64) => {
            Bytes::from(base64::engine::Engine::decode(&base64::engine::general_purpose::STANDARD, b64).unwrap())
        }
        None => Bytes::from(s["payload"].as_str().unwrap().to_owned()),
    };
    let original = s["original"]
        .as_str()
        .map(|o| Bytes::from(o.to_owned()))
        .unwrap_or_else(|| payload.clone());
    let mut headers = http::HeaderMap::new();
    for (k, v) in s["headers"].as_object().into_iter().flatten() {
        headers.insert(
            http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
            v.as_str().unwrap().parse().unwrap(),
        );
    }
    if let Some(ct) = s["content_type"].as_str() {
        headers.insert(http::header::CONTENT_TYPE, ct.parse().unwrap());
    }
    let req = ExecRequest {
        operation: if op == "count" {
            Operation::CountTokens
        } else {
            Operation::Generate
        },
        source_format: source,
        response_format: response,
        requested_model: s["requested_model"].as_str().unwrap_or(&model).to_owned(),
        model,
        original_body: original,
        body: payload,
        stream: op.ends_with("stream") || s["stream"].as_bool() == Some(true),
        alt: s["alt"].as_str().map(str::to_owned),
        session: None,
        headers,
        caller: Caller {
            principal: "fake-client-key".into(),
            source: "authorization",
        },
    };
    (req, op.to_owned())
}

fn error_json(e: &ExecError) -> Value {
    serde_json::json!({
        "status": e.status,
        "message": String::from_utf8_lossy(&e.body),
        "retry_after_ms": e.retry_after.map_or(-1, |d| d.as_millis() as i64),
    })
}

fn want_bytes(s: &Value) -> Option<Vec<u8>> {
    match s["request_b64"].as_str() {
        Some(b64) => Some(base64::engine::Engine::decode(&base64::engine::general_purpose::STANDARD, b64).unwrap()),
        None => s["request"].as_str().map(|r| r.as_bytes().to_vec()),
    }
}

#[tokio::test]
async fn go_reference_scenarios() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    let scenarios = fixture["scenarios"].as_array().unwrap();
    assert!(scenarios.len() >= 60, "fixture lost scenarios");
    let executor = OpenAICompatExecutor::default();
    let mut skipped = Vec::new();
    for s in scenarios {
        let name = s["name"].as_str().unwrap();
        let needs: Vec<&str> = s["needs"]
            .as_array()
            .map(|n| n.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        if needs.iter().any(|n| PENDING.contains(n)) {
            skipped.push(name);
            continue;
        }
        let mock = match s.get("upstream").filter(|u| !u.is_null()) {
            Some(up) => Some(Mock::start(up).await),
            None => None,
        };
        let addr = mock.as_ref().map_or("127.0.0.1:9".to_owned(), |m| m.addr.clone());
        let cfg = Config::parse(&s["config"].as_str().unwrap_or_default().replace("UPSTREAM", &addr)).unwrap();
        let cred = credential(s, &cfg, &addr);
        let (req, op) = request(s);
        let result = if op.starts_with("images") {
            let path = s["request_path"].as_str().unwrap_or_default().to_owned();
            executor.images(&cred, req, &path, &cfg).await
        } else {
            executor.execute(&cred, req, &cfg).await
        };
        let mut output = None;
        let mut chunks = Vec::new();
        let mut error = None;
        match result {
            Err(e) => error = Some(error_json(&e)),
            Ok(response) => match response.body {
                ResponseBody::Buffered(bytes) => output = Some(String::from_utf8_lossy(&bytes).into_owned()),
                ResponseBody::Stream(mut stream) => {
                    let mut joined = Vec::new();
                    while let Some(item) = stream.next().await {
                        match item {
                            Ok(bytes) => {
                                joined.extend_from_slice(&bytes);
                                chunks.push(String::from_utf8_lossy(&bytes).into_owned());
                            }
                            Err(e) => error = Some(error_json(&e)),
                        }
                    }
                    if op == "images_stream" {
                        output = Some(String::from_utf8_lossy(&joined).into_owned());
                        chunks.clear();
                    }
                }
            },
        }
        if let Some(mock) = &mock {
            let want = match s["request_b64"].as_str() {
                Some(b64) => {
                    Some(base64::engine::Engine::decode(&base64::engine::general_purpose::STANDARD, b64).unwrap())
                }
                None => s["request"].as_str().map(|r| r.as_bytes().to_vec()),
            };
            assert_eq!(
                mock.request().map(|r| String::from_utf8_lossy(&r).into_owned()),
                want.map(|r| String::from_utf8_lossy(&r).into_owned()),
                "{name}: upstream request"
            );
            assert_eq!(mock.request(), want_bytes(s), "{name}: upstream request bytes");
        }
        assert_eq!(output.as_deref(), s["output"].as_str(), "{name}: output");
        // Go's chunks are payloads the route frames as `data: <chunk>\n\n`; the Rust
        // translator contract emits the framed event.
        let want_chunks: Vec<String> = s["chunks"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(|c| format!("data: {c}\n\n"))
            .collect();
        assert_eq!(chunks, want_chunks, "{name}: stream chunks");
        let mut want_error = s["error"].clone();
        if want_error["status"] == 0 {
            // A plain Go error: the handler answers 500.
            want_error["status"] = 500.into();
        }
        assert_eq!(error.unwrap_or(Value::Null), want_error, "{name}: error");
    }
    assert_eq!(
        skipped,
        [
            "stream_multiline_data",
            "compact_invalid_encrypted_content",
            "needs_thinking_suffix_level",
            "needs_thinking_body_level_clamped",
            "needs_translator_responses_source",
            "needs_translator_responses_eof_without_done",
            "needs_translator_claude_code_prompt_cache",
        ]
    );
}

#[test]
fn provider_keys() {
    assert!(handles("openai-compatibility"));
    assert!(handles("openai-compatible-acme"));
    assert!(!handles("openai"));
    assert!(!handles("codex"));
}

fn b64(s: &Value) -> Vec<u8> {
    base64::engine::Engine::decode(&base64::engine::general_purpose::STANDARD, s.as_str().unwrap()).unwrap()
}

/// Go standard-library answers recorded by the generator (`vectors`).
#[test]
fn go_primitive_vectors() {
    use crate::openai_compat_go as go;
    use crate::openai_compat_multipart::{MediaType, parse_media_type};
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    let v = &fixture["vectors"];
    for case in v["http_time"].as_array().unwrap() {
        let raw = case[0].as_str().unwrap();
        let got = go::parse_http_time(raw).map(|t| match t.duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => d.as_nanos() as i64,
            Err(e) => -(e.duration().as_nanos() as i64),
        });
        assert_eq!(got, case[1].as_i64(), "http.ParseTime({raw:?})");
    }
    for case in v["json_valid"].as_array().unwrap() {
        let input = b64(&case[0]);
        assert_eq!(
            go::json_valid(&input),
            case[1].as_bool().unwrap(),
            "json.Valid({:?})",
            String::from_utf8_lossy(&input[..input.len().min(40)])
        );
    }
    for case in v["gjson_int"].as_array().unwrap() {
        let raw = case[0].as_str().unwrap();
        assert_eq!(Some(go::int(&gjson::parse(raw))), case[1].as_i64(), "gjson Int({raw})");
    }
    for case in v["trim_space"].as_array().unwrap() {
        assert_eq!(go::trim_space(&b64(&case[0])), b64(&case[1]).as_slice());
    }
    for case in v["media_type"].as_array().unwrap() {
        let input = case["input"].as_str().unwrap();
        let (media, error) = (case["media"].as_str().unwrap(), case["error"].as_str().unwrap());
        let want = if error.is_empty() {
            let params = case["params"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
                .collect();
            MediaType::Ok(media.to_owned(), params)
        } else if media.is_empty() {
            MediaType::Invalid
        } else {
            MediaType::BadParams(media.to_owned())
        };
        assert_eq!(parse_media_type(input), want, "mime.ParseMediaType({input:?})");
    }
}
