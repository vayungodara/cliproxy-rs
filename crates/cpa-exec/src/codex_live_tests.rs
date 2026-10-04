//! Expected values come from `tests/fixtures/codex_live_go.json`, produced by
//! `tests/reference/codex_live/zz_rsfix_live_vectors_test.go` running the Go functions.

use base64::Engine;
use serde_json::Value;

use super::*;

fn vectors() -> Vec<Value> {
    serde_json::from_str(include_str!("../tests/fixtures/codex_live_go.json")).expect("fixture JSON")
}

/// Go strings in the fixture: plain when valid UTF-8, else `b64:` + standard base64.
fn bytes(v: &Value) -> Vec<u8> {
    let s = v.as_str().unwrap_or_default();
    match s.strip_prefix("b64:") {
        Some(b64) => base64::engine::general_purpose::STANDARD.decode(b64).expect("base64"),
        None => s.as_bytes().to_vec(),
    }
}

fn text(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_owned()
}

fn err(v: &Value) -> Option<String> {
    v.as_str().map(str::to_owned)
}

fn show(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// Go returns `(nil, "", "", err)` on failure; the fixture records those zero values.
fn call_out(result: Result<Call, ShapeError>) -> Value {
    match result {
        Ok(call) => serde_json::json!({
            "body": show(&call.body), "content_type": call.content_type, "model": call.model, "err": null,
        }),
        Err(ShapeError::Invalid(e)) => serde_json::json!({"body": "", "content_type": "", "model": "", "err": e}),
        Err(ShapeError::NilMap) => serde_json::json!({"panic": true}),
    }
}

fn expect_call(out: &Value) -> Value {
    if out.get("panic").is_some() {
        return serde_json::json!({"panic": true});
    }
    serde_json::json!({
        "body": show(&bytes(&out["body"])),
        "content_type": out["content_type"],
        "model": out["model"],
        "err": out["err"],
    })
}

#[test]
fn shaping_matches_go() {
    let mut checked = 0;
    let mut failures = Vec::new();
    for v in vectors() {
        let (fn_name, input, out) = (v["fn"].as_str().unwrap(), &v["in"], &v["out"]);
        let body = bytes(&input["body"]);
        let ct = text(&input["content_type"]);
        let (got, want) = match fn_name {
            "prepare" => (call_out(prepare(&body, &ct)), expect_call(out)),
            "pipeline" => {
                let session = bytes(&input["session"]);
                let got = prepare(&body, &ct)
                    .and_then(|c| apply_client_secret(c, &session))
                    .and_then(rewrite_model);
                // The harness keeps Go's stale content type on a rewrite error; the
                // handler only uses the error then.
                let mut want = expect_call(out);
                if want["err"].is_string() {
                    want["body"] = "".into();
                    want["content_type"] = "".into();
                    want["model"] = "".into();
                }
                (call_out(got), want)
            }
            "rewrite_model" => {
                let call = Call {
                    body: body.clone(),
                    content_type: ct.clone(),
                    model: text(&input["model"]),
                };
                let got = match rewrite_model(call) {
                    Ok(c) => serde_json::json!({"body": show(&c.body), "model": c.model, "err": null}),
                    Err(ShapeError::Invalid(e)) => serde_json::json!({"body": "", "model": "", "err": e}),
                    Err(ShapeError::NilMap) => serde_json::json!({"panic": true}),
                };
                let want = if out.get("panic").is_some() {
                    out.clone()
                } else {
                    serde_json::json!({"body": show(&bytes(&out["body"])), "model": out["model"], "err": out["err"]})
                };
                (got, want)
            }
            "model_from_json" => (Value::String(model_from_json(&body)), out["model"].clone()),
            "call_request_sdp" => {
                let got = match request_sdp(&body, &ct) {
                    Ok(s) => serde_json::json!({"sdp": s, "err": null}),
                    Err(e) => serde_json::json!({"sdp": "", "err": e}),
                };
                (got, serde_json::json!({"sdp": out["sdp"], "err": out["err"]}))
            }
            "response_sdp" => {
                let got = match response_sdp(&body, &ct) {
                    Ok(s) => serde_json::json!({"sdp": s, "err": null}),
                    Err(e) => serde_json::json!({"sdp": "", "err": e}),
                };
                (got, serde_json::json!({"sdp": out["sdp"], "err": out["err"]}))
            }
            "replace_sdp" => {
                let got = match replace_sdp(&body, &ct, &text(&input["sdp"])) {
                    Ok((b, c)) => serde_json::json!({"body": show(&b), "content_type": c, "err": null}),
                    Err(ShapeError::Invalid(e)) => serde_json::json!({"body": "", "content_type": "", "err": e}),
                    Err(ShapeError::NilMap) => serde_json::json!({"panic": true}),
                };
                let want = if out.get("panic").is_some() {
                    out.clone()
                } else {
                    serde_json::json!({"body": show(&bytes(&out["body"])), "content_type": out["content_type"], "err": out["err"]})
                };
                (got, want)
            }
            "codex_model" => (Value::String(codex_model(&text(&input["model"]))), out["model"].clone()),
            "call_id_from_location" => (
                Value::String(call_id_from_location(&text(&input["location"]))),
                out["call_id"].clone(),
            ),
            "call_id_valid" => (
                Value::Bool(valid_call_id(&text(&input["call_id"]))),
                out["valid"].clone(),
            ),
            "normalize_secret" => {
                let got = match normalize_client_secret_session(&bytes(&input["session"])) {
                    Ok((c, u)) => serde_json::json!({"client": show(&c), "upstream": show(&u), "err": null}),
                    Err(SessionError::Invalid(e) | SessionError::Unsupported(e)) => {
                        serde_json::json!({"client": "", "upstream": "", "err": e})
                    }
                };
                let want = serde_json::json!({
                    "client": show(&bytes(&out["client"])), "upstream": show(&bytes(&out["upstream"])), "err": out["err"],
                });
                (got, want)
            }
            "session_response" => {
                if out.get("skip").is_some() {
                    continue;
                }
                let (client, _) = normalize_client_secret_session(&bytes(&input["session"])).expect("normalized");
                let got = session_response(&client, "sess_fixed", 1_700_000_000).map(|v| show(&v.marshal()));
                (serde_json::json!(got), serde_json::json!(show(&bytes(&out["body"]))))
            }
            "session_update" => {
                let got = match session_update(&bytes(&input["session"])) {
                    Ok(b) => serde_json::json!({"body": show(&b), "err": null}),
                    Err(e) => serde_json::json!({"body": "", "err": e}),
                };
                (
                    got,
                    serde_json::json!({"body": show(&bytes(&out["body"])), "err": out["err"]}),
                )
            }
            "sideband_url" => {
                let style = match input["style"].as_i64().unwrap() {
                    0 => Sideband::Frameless,
                    1 => Sideband::Calls,
                    _ => Sideband::Query,
                };
                let url = sideband_url(&text(&input["base"]), style, &text(&input["call_id"]));
                let http = http_url(&url);
                (
                    serde_json::json!([url, http]),
                    serde_json::json!([out["url"], out["http_url"]]),
                )
            }
            "direct_url" => (
                serde_json::json!([direct_url(API_BASE, &text(&input["model"])), http_base(API_BASE)]),
                serde_json::json!([out["url"], out["hangup_base"]]),
            ),
            "bearer" => (
                Value::String(bearer_token(&text(&input["authorization"])).to_owned()),
                out["token"].clone(),
            ),
            "lifetime" => {
                let expires = input["set"]
                    .as_bool()
                    .unwrap()
                    .then(|| (text(&input["anchor"]), input["seconds"].as_i64().unwrap()));
                let got = match client_secret_lifetime(expires.as_ref()) {
                    Ok(d) => serde_json::json!({"seconds": d.as_secs(), "err": null}),
                    Err(e) => serde_json::json!({"seconds": 0, "err": e}),
                };
                (got, serde_json::json!({"seconds": out["seconds"], "err": out["err"]}))
            }
            "secret_request" => {
                let got = match decode_secret_request(text(&input["body"]).as_bytes()) {
                    Some(r) => {
                        let mut v = serde_json::json!({"ok": true, "session": show(&r.session)});
                        if let Some((anchor, seconds)) = r.expires_after {
                            v["anchor"] = anchor.into();
                            v["seconds"] = seconds.into();
                        }
                        v
                    }
                    None => serde_json::json!({"ok": false}),
                };
                (got, out.clone())
            }
            "quoted_printable" => {
                let got = match form::quoted_printable(&body, None) {
                    Ok(b) => serde_json::json!({"body": show(&b), "err": null}),
                    Err(e) => serde_json::json!({"err": e}),
                };
                let want = match out["err"].as_str() {
                    Some(e) => serde_json::json!({"err": e}),
                    None => serde_json::json!({"body": show(&bytes(&out["body"])), "err": null}),
                };
                (got, want)
            }
            other => panic!("unknown vector kind {other}"),
        };
        checked += 1;
        if got != want {
            failures.push(format!("{fn_name} {input}\n   got {got}\n  want {want}"));
        }
    }
    assert!(checked > 600, "only {checked} vectors checked");
    assert!(
        failures.is_empty(),
        "{} mismatches:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Errors carry Go's texts verbatim (`err` above compares them); spot-check one so a
/// fixture regeneration that drops error cases cannot pass silently.
#[test]
fn fixture_covers_error_and_panic_paths() {
    let all = vectors();
    let errors = all.iter().filter(|v| err(&v["out"]["err"]).is_some()).count();
    let panics = all.iter().filter(|v| v["out"].get("panic").is_some()).count();
    assert!(errors > 50 && panics >= 3, "errors={errors} panics={panics}");
}

#[test]
fn protocol_headers_keep_values_and_canonical_names() {
    let mut client = HeaderMap::new();
    client.append("openai-alpha", "quicksilver=v2".parse().unwrap());
    client.append("x-session-id", "a".parse().unwrap());
    client.append("x-session-id", "b".parse().unwrap());
    client.append("authorization", "Bearer client-key".parse().unwrap());
    let headers = protocol_headers(&client);
    assert_eq!(
        headers,
        vec![
            ("Openai-Alpha".to_owned(), "quicksilver=v2".to_owned()),
            ("X-Session-Id".to_owned(), "a".to_owned()),
            ("X-Session-Id".to_owned(), "b".to_owned()),
        ],
        "only the live protocol headers, never the client key"
    );
    let direct = direct_headers(&client);
    assert!(direct.iter().all(|(n, _)| n != "Openai-Alpha"));
    assert!(direct.contains(&("Originator".to_owned(), "Codex Desktop".to_owned())));
    client.insert("originator", "Codex CLI".parse().unwrap());
    assert!(direct_headers(&client).contains(&("Originator".to_owned(), "Codex CLI".to_owned())));
}

#[test]
fn redact_sdp_hides_ice_credentials_in_every_wrapping() {
    let raw = b"v=0\r\na=ice-ufrag:Ufr4g\r\na=ice-pwd:s3cret+pass/word\r\na=mid:0\r\n";
    assert_eq!(
        &*redact_sdp(raw),
        b"v=0\r\na=ice-ufrag:[REDACTED]\r\na=ice-pwd:[REDACTED]\r\na=mid:0\r\n"
    );
    // Bare LF line ends and a value at the end of the body.
    assert_eq!(
        &*redact_sdp(b"a=ice-pwd:p1\na=ice-pwd:p2"),
        b"a=ice-pwd:[REDACTED]\na=ice-pwd:[REDACTED]"
    );
    // JSON strings: the value ends at an escape or the closing quote; `\/` is part of it.
    let json = br#"{"sdp":"v=0\r\na=ice-pwd:ab\/cd\r\na=ice-ufrag:uf","session":{}}"#;
    assert_eq!(
        &*redact_sdp(json),
        br#"{"sdp":"v=0\r\na=ice-pwd:[REDACTED]\r\na=ice-ufrag:[REDACTED]","session":{}}"#
    );
    // A multipart field keeps its boundary lines.
    let multipart = b"--b\r\nContent-Disposition: form-data; name=\"sdp\"\r\n\r\na=ice-pwd:mp\r\n--b--\r\n";
    assert_eq!(
        &*redact_sdp(multipart),
        b"--b\r\nContent-Disposition: form-data; name=\"sdp\"\r\n\r\na=ice-pwd:[REDACTED]\r\n--b--\r\n"
    );
    // Other bodies, and an empty value, stay as they are, without a copy.
    assert!(matches!(
        redact_sdp(b"v=0\r\na=mid:0\r\n"),
        std::borrow::Cow::Borrowed(_)
    ));
    assert_eq!(&*redact_sdp(b"a=ice-pwd:\r\n"), b"a=ice-pwd:\r\n");
}

#[test]
fn redact_sdp_follows_json_escapes_to_the_end_of_the_credential() {
    // An escaped character is part of the credential; the escaped line end is not.
    let json = br#"{"sdp":"a=ice-pwd:\u0073ecret\"x\\y\r\na=ice-ufrag:u\u0066rag\u000d\u000Aa=mid:0"}"#;
    assert_eq!(
        &*redact_sdp(json),
        br#"{"sdp":"a=ice-pwd:[REDACTED]\r\na=ice-ufrag:[REDACTED]\u000d\u000Aa=mid:0"}"#
    );
    // A trailing lone backslash or a cut-off escape ends with the body.
    assert_eq!(&*redact_sdp(br"a=ice-pwd:ab\"), b"a=ice-pwd:[REDACTED]");
    assert_eq!(&*redact_sdp(br"a=ice-pwd:ab\u00"), b"a=ice-pwd:[REDACTED]");
}

#[test]
fn redact_sdp_scans_many_markers_in_one_pass() {
    let markers = 100_000;
    let body = "a=ice-pwd:secret\r\na=ice-ufrag:frag\r\n".repeat(markers / 2);
    let started = std::time::Instant::now();
    let redacted = redact_sdp(body.as_bytes());
    // A rescan per marker would take minutes here.
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        &*redacted,
        "a=ice-pwd:[REDACTED]\r\na=ice-ufrag:[REDACTED]\r\n"
            .repeat(markers / 2)
            .as_bytes()
    );
}

#[test]
fn redact_sdp_recognises_an_escaped_equals_sign() {
    let json = br#"{"sdp":"a\u003dice-pwd:secret\r\na\u003Dice-ufrag:frag\r\na\u003dmid:0","note":"a\u003dice"}"#;
    assert_eq!(
        &*redact_sdp(json),
        br#"{"sdp":"a\u003dice-pwd:[REDACTED]\r\na\u003Dice-ufrag:[REDACTED]\r\na\u003dmid:0","note":"a\u003dice"}"#
    );
}
