//! Go's `helps/payload_media.go` (868e137): payload rules for image request bodies. A
//! multipart form is shown to the rules as JSON business values, with file parts as
//! `{filename, content_type, data}` (data in base64); the multipart framing stays opaque.

use std::collections::BTreeMap;

use cpa_common::json as gj;

use crate::openai_compat_multipart::{self as multipart, MediaType};

/// `ApplyMediaPayloadConfig`: the body and content type to send. `finalize` applies the
/// rules to the view, given the client's original request in the same view. A form is
/// rebuilt only when the rules changed its view.
pub(crate) fn apply(
    body: &[u8],
    content_type: &str,
    original: &[u8],
    original_content_type: &str,
    finalize: impl FnOnce(Vec<u8>, &[u8]) -> Vec<u8>,
) -> Result<(Vec<u8>, String), String> {
    let (view, form) = form_view(body, content_type)?;
    let (original, _) = form_view(original, original_content_type)?;
    let out = finalize(view.clone(), &original);
    if !form {
        return Ok((out, content_type.to_owned()));
    }
    if out == view {
        return Ok((body.to_vec(), content_type.to_owned()));
    }
    // Go ranges over a map here, so its part order is random; document order is one of
    // the orders it produces.
    let mut writer = multipart::Writer::new();
    for (key, value) in gj::parse(&out).map() {
        let values = if value.is_array() { value.array() } else { vec![value] };
        for item in values {
            if item.is_object() && item.get("filename").exists() {
                let data = cpa_common::signature::std_base64_decode(&item.get("data").bytes())
                    .map_err(|e| format!("multipart payload {}: {e}", String::from_utf8_lossy(&key)))?;
                let filename = item.get("filename").bytes();
                let params = BTreeMap::from([("filename", &filename[..]), ("name", &key[..])]);
                let disposition = multipart::format_media_type("form-data", &params).unwrap_or_default();
                let headers = [
                    ("Content-Disposition".to_owned(), disposition.into_bytes()),
                    ("Content-Type".to_owned(), item.get("content_type").bytes().into_owned()),
                ];
                writer.part(&headers, &data);
            } else {
                writer.field(&String::from_utf8_lossy(&key), &item.bytes());
            }
        }
    }
    Ok(writer.finish())
}

/// `mediaPayloadJSON`: JSON as it is, a `multipart/form-data` body as one JSON object
/// (`true`), anything else unchanged.
fn form_view(body: &[u8], content_type: &str) -> Result<(Vec<u8>, bool), String> {
    if gj::std_valid(body) {
        return Ok((body.to_vec(), false));
    }
    let MediaType::Ok(media, params) = multipart::parse_media_type(content_type) else {
        return Ok((body.to_vec(), false));
    };
    if media != "multipart/form-data" {
        return Ok((body.to_vec(), false));
    }
    let parts = multipart::parts(body, params.get("boundary").map_or("", String::as_str))?;
    // json.Marshal of a map: keys sorted, a repeated name collects its values in order.
    let mut fields: BTreeMap<&[u8], Vec<Vec<u8>>> = BTreeMap::new();
    for part in &parts {
        let mut value = Vec::new();
        if part.filename.is_empty() {
            gj::marshal_str(&mut value, &part.body, true);
        } else {
            let data = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &part.body);
            value.extend_from_slice(b"{\"content_type\":");
            gj::marshal_str(&mut value, part.header("Content-Type").unwrap_or_default(), true);
            value.extend_from_slice(b",\"data\":");
            gj::marshal_str(&mut value, data.as_bytes(), true);
            value.extend_from_slice(b",\"filename\":");
            gj::marshal_str(&mut value, part.filename.as_bytes(), true);
            value.push(b'}');
        }
        fields.entry(part.name.as_bytes()).or_default().push(value);
    }
    let mut out = vec![b'{'];
    for (i, (key, values)) in fields.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        gj::marshal_str(&mut out, key, true);
        out.push(b':');
        match values.as_slice() {
            [one] => out.extend_from_slice(one),
            many => {
                out.push(b'[');
                out.extend_from_slice(&many.join(&b','));
                out.push(b']');
            }
        }
    }
    out.push(b'}');
    Ok((out, true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde_json::Value;

    /// A form as Go's recorder lists it: each part's headers sorted, then its body in
    /// base64, the parts sorted (Go writes them in random order).
    fn parts(body: &[u8], content_type: &str) -> Vec<String> {
        let MediaType::Ok(media, params) = multipart::parse_media_type(content_type) else {
            panic!("{content_type}");
        };
        assert_eq!(media, "multipart/form-data");
        let mut out: Vec<String> = multipart::parts(body, &params["boundary"])
            .unwrap()
            .iter()
            .map(|part| {
                let mut headers: Vec<String> = part
                    .headers
                    .iter()
                    .map(|(k, v)| format!("{k}: {}\n", String::from_utf8_lossy(v)))
                    .collect();
                headers.sort();
                format!("{}\n{}", headers.concat(), STANDARD.encode(&part.body))
            })
            .collect();
        out.sort();
        out
    }

    /// Each repeated name's part bodies (base64) in the order they were written.
    fn order(body: &[u8], content_type: &str) -> BTreeMap<String, Vec<String>> {
        let MediaType::Ok(_, params) = multipart::parse_media_type(content_type) else {
            panic!("{content_type}");
        };
        let mut order: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for part in multipart::parts(body, &params["boundary"]).unwrap() {
            order
                .entry(part.name.clone())
                .or_default()
                .push(STANDARD.encode(&part.body));
        }
        order.retain(|_, bodies| bodies.len() > 1);
        order
    }

    #[test]
    fn media_payload_rules_match_go() {
        let go: Value = serde_json::from_str(include_str!("../tests/fixtures/payload_media_go.json")).unwrap();
        for case in go["cases"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let b64 = |k: &str| STANDARD.decode(case[k].as_str().unwrap()).unwrap();
            let (body, original) = (b64("body_b64"), b64("original_b64"));
            let content_type = case["content_type"].as_str().unwrap();
            if let Some(view) = case["view"].as_str() {
                assert_eq!(
                    String::from_utf8_lossy(&form_view(&body, content_type).unwrap().0),
                    view,
                    "{name}: view"
                );
            }
            let cfg = cpa_core::config::Config::parse(case["config"].as_str().unwrap()).unwrap();
            let rules = cpa_common::payload::Rules::of(&cfg);
            let mut headers = http::HeaderMap::new();
            headers.insert(http::header::CONTENT_TYPE, content_type.parse().unwrap());
            let got = apply(&body, content_type, &original, content_type, |view, original| {
                let request = cpa_common::payload::Request {
                    target_executor: "openai",
                    model: "gpt-image-2",
                    requested_model: "gpt-image-2",
                    protocol: "openai",
                    from_protocol: "",
                    root: "",
                    original,
                    request_path: "",
                    headers: Some(&headers),
                };
                cpa_common::payload::apply(&rules, &request, view)
            });
            match (got, case["error"].as_str()) {
                (Err(e), Some(want)) => assert_eq!(e, want, "{name}"),
                (Err(e), None) => panic!("{name}: {e}"),
                (Ok(_), Some(want)) => panic!("{name}: expected {want}"),
                (Ok((out, out_type)), None) => {
                    if case["unchanged"] == true {
                        assert_eq!(out, body, "{name}");
                        assert_eq!(out_type, case["out_content_type"].as_str().unwrap(), "{name}");
                    } else if let Some(want) = case["out_b64"].as_str() {
                        assert_eq!(STANDARD.encode(&out), want, "{name}");
                        assert_eq!(out_type, case["out_content_type"].as_str().unwrap(), "{name}");
                    } else {
                        let want: Vec<String> = serde_json::from_value(case["out_parts"].clone()).unwrap();
                        assert_eq!(parts(&out, &out_type), want, "{name}");
                        let want: BTreeMap<String, Vec<String>> =
                            serde_json::from_value(case["out_order"].clone()).unwrap();
                        assert_eq!(order(&out, &out_type), want, "{name}: repeated order");
                    }
                }
            }
        }
    }
}
