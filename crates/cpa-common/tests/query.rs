//! gjson v1.18.0 array-query predicates (`#(path op value)`) against Go goldens
//! recorded by `tests/reference/query/main.go`.

use cpa_common::json;

#[test]
fn query_predicates_match_go_gjson() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!("fixtures/query_go.json")).unwrap();
    let terms: Vec<&str> = fixture["terms"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t.as_str().unwrap())
        .collect();
    let items = fixture["items"].as_array().unwrap();
    let results = fixture["results"].as_array().unwrap();
    assert_eq!(terms.len(), 1255);
    assert_eq!(items.len(), 72);
    let mut failures = Vec::new();
    let mut unbalanced = 0;
    for (item, row) in items.iter().zip(results) {
        let item = item.as_str().unwrap();
        for (term, want) in terms.iter().zip(row.as_str().unwrap().bytes()) {
            let path = format!("#({term})");
            let query = match json::parse_query(path.as_bytes()) {
                Some((query, end)) => {
                    assert_eq!(end, path.len(), "{term}");
                    query
                }
                None => {
                    unbalanced += 1;
                    json::Query::default()
                }
            };
            if query.matches_raw(item.as_bytes()) != (want == b'1') {
                failures.push(format!("{term} on {item}: Go {}", want as char));
            }
        }
    }
    assert_eq!(unbalanced, 3 * items.len());
    let hex = |h: &str| -> Vec<u8> {
        (0..h.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap())
            .collect()
    };
    let pairs = fixture["pairs"].as_array().unwrap();
    assert_eq!(pairs.len(), 15);
    for pair in pairs {
        let (item, term) = (hex(pair["item"].as_str().unwrap()), hex(pair["term"].as_str().unwrap()));
        let path = [&b"#("[..], &term, b")"].concat();
        let (query, _) = json::parse_query(&path).unwrap();
        if query.matches_raw(&item) != pair["match"].as_bool().unwrap() {
            failures.push(format!(
                "{} on {}: Go {}",
                String::from_utf8_lossy(&term),
                String::from_utf8_lossy(&item),
                pair["match"]
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} mismatches:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
