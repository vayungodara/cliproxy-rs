//! Differential check of the gjson/sjson port against fixtures produced by the pinned Go
//! libraries (tests/reference/json). Byte fields map each byte to the rune of equal value.
use cpa_common::json::{self as gj, Kind, Res};
use serde_json::Value;

fn b(v: &Value) -> Vec<u8> {
    v.as_str().unwrap().chars().map(|c| c as u32 as u8).collect()
}

fn kind(k: Kind) -> i64 {
    match k {
        Kind::Null => 0,
        Kind::False => 1,
        Kind::Number => 2,
        Kind::String => 3,
        Kind::True => 4,
        Kind::Json => 5,
    }
}

fn h(bytes: &[u8]) -> String {
    bytes.iter().map(|&c| c as char).collect()
}

fn describe(r: &Res<'_>) -> Vec<String> {
    vec![
        kind(r.kind).to_string(),
        h(&r.raw),
        h(&r.s),
        h(&r.bytes()),
        r.index.to_string(),
    ]
}

fn fixtures() -> Value {
    serde_json::from_str(include_str!("fixtures/json.json")).unwrap()
}

#[test]
fn get_matches_gjson() {
    let all = fixtures();
    let mut failures = vec![];
    for case in all["get"].as_array().unwrap() {
        let json = b(&case["json"]);
        let path = case["path"].as_str().unwrap();
        let r = if path == "\0parse" {
            gj::parse(&json)
        } else {
            gj::get(&json, path)
        };
        let mut array = vec![];
        for item in r.array() {
            array.extend(describe(&item));
        }
        let mut each = vec![];
        r.each(|k, v| {
            each.extend(describe(&k));
            each.extend(describe(&v));
            true
        });
        let mut map: Vec<String> = if path == "\0parse" {
            vec![]
        } else {
            r.map()
                .iter()
                .map(|(k, v)| format!("{}={}@{}", h(k), h(&v.raw), v.index))
                .collect()
        };
        map.sort();
        let actual = serde_json::json!({
            "exists": r.exists(),
            "type": kind(r.kind),
            "raw": h(&r.raw),
            "str": h(&r.s),
            "string": h(&r.bytes()),
            "int": r.int(),
            "uint": r.uint(),
            "float": if r.float().is_nan() { case["float"].as_str().unwrap().to_owned() } else { r.float().to_bits().to_string() },
            "bool": r.bool(),
            "index": r.index,
            "indexes": r.indexes,
            "array": array,
            "each": each,
            "map": map,
        });
        let mut expected = case.clone();
        let fields = expected.as_object_mut().unwrap();
        fields.remove("json");
        fields.remove("path");
        if actual != expected {
            failures.push(format!(
                "json={:?} path={path:?}\n  go:   {expected}\n  rust: {actual}",
                String::from_utf8_lossy(&json)
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} mismatches:\n{}",
        failures.len(),
        failures[..failures.len().min(15)].join("\n")
    );
}

#[test]
fn set_matches_sjson() {
    let all = fixtures();
    let mut failures = vec![];
    for case in all["set"].as_array().unwrap() {
        let json = b(&case["json"]);
        let path = case["path"].as_str().unwrap();
        let value = b(&case["value"]);
        let mut out = json.clone();
        match case["kind"].as_str().unwrap() {
            "str" => gj::set_str(&mut out, path, &value),
            "raw" => gj::set_raw(&mut out, path, &value),
            _ => gj::delete(&mut out, path),
        };
        // sjson returns the input unchanged alongside every error it reports.
        let expected = b(&case["output"]);
        if out != expected {
            failures.push(format!(
                "json={:?} path={path:?} kind={} value={:?}\n  go:   {:?}\n  rust: {:?}",
                String::from_utf8_lossy(&json),
                case["kind"],
                String::from_utf8_lossy(&value),
                String::from_utf8_lossy(&expected),
                String::from_utf8_lossy(&out)
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} mismatches:\n{}",
        failures.len(),
        failures[..failures.len().min(15)].join("\n")
    );
}

#[test]
fn encoding_matches_encoding_json() {
    let all = fixtures();
    for case in all["encode"].as_array().unwrap() {
        let input = b(&case["input"]);
        assert_eq!(gj::quote(&input), b(&case["html"]), "{input:?}");
        let mut plain = vec![];
        gj::marshal_str(&mut plain, &input, false);
        assert_eq!(plain, b(&case["plain"]), "{input:?}");
    }
    for case in all["float"].as_array().unwrap() {
        let f = f64::from_bits(case["bits"].as_u64().unwrap());
        assert_eq!(gj::fmt_float(f), case["f"].as_str().unwrap(), "{f:e}");
        assert_eq!(gj::json_float(f).unwrap(), case["json"].as_str().unwrap(), "{f:e}");
    }
}

/// The sjson/gjson vectors the Kimi thread generated from Go (cpa-exec device fixtures).
#[test]
fn kimi_device_vectors_match() {
    let vectors: Vec<Value> =
        serde_json::from_str(include_str!("../../cpa-exec/tests/device_fixtures/kimi/vectors.json")).unwrap();
    let mut seen = 0;
    for v in vectors {
        let input = v["in"].as_str().unwrap_or_default().as_bytes();
        let path = v["path"].as_str().unwrap_or_default();
        let value = v["value"].as_str().unwrap_or_default();
        let got = match v["fn"].as_str().unwrap() {
            "sjson_delete" => gj::try_delete(input, path),
            "sjson_set_str" => gj::try_set_str(input, path, value),
            "sjson_set_raw" => gj::try_set_raw(input, path, value),
            "gjson_string" => Ok(gj::get(input, "n").bytes().into_owned()),
            _ => continue,
        };
        seen += 1;
        let got = got.map(|b| String::from_utf8(b).unwrap());
        match v["err"].as_str() {
            Some(message) => assert_eq!(got, Err(message.to_owned()), "{v}"),
            None => assert_eq!(got.as_deref(), Ok(v["out"].as_str().unwrap()), "{v}"),
        }
    }
    assert!(seen >= 25, "vector extraction lost cases: {seen}");
}

/// json.Unmarshal into `any` (float64 numbers) and Decoder.UseNumber, each re-marshaled.
#[test]
fn decode_then_marshal_matches_encoding_json() {
    let all = fixtures();
    for case in all["decode"].as_array().unwrap() {
        let input = b(&case["input"]);
        let float = gj::GoValue::parse_f64(&input).map(|v| v.marshal()).unwrap_or_default();
        let number = gj::GoValue::parse(&input).map(|v| v.marshal()).unwrap_or_default();
        assert_eq!(
            float,
            b(&case["float"]),
            "float mode: {:?}",
            String::from_utf8_lossy(&input)
        );
        assert_eq!(
            number,
            b(&case["number"]),
            "number mode: {:?}",
            String::from_utf8_lossy(&input)
        );
    }
}

#[test]
fn std_valid_applies_encoding_json_nesting_limit() {
    let nested = |n: usize| [vec![b'['; n], vec![b']'; n]].concat();
    assert!(cpa_common::json::std_valid(&nested(10_000)));
    assert!(!cpa_common::json::std_valid(&nested(10_001)));
    assert!(cpa_common::json::std_valid(br#"{"a":"[[[\"]]]"}"#));
    assert!(!cpa_common::json::std_valid(b"{"));
}
