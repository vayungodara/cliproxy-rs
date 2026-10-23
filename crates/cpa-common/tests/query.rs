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
    assert_eq!(terms.len(), 644);
    assert_eq!(items.len(), 37);
    let mut failures = Vec::new();
    let mut unbalanced = 0;
    for (item, row) in items.iter().zip(results) {
        let item = item.as_str().unwrap();
        let wrapped = format!("[{item}]");
        let elem = json::parse(wrapped.as_bytes()).array().remove(0);
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
            if query.matches(&elem) != (want == b'1') {
                failures.push(format!("{term} on {item}: Go {}", want as char));
            }
        }
    }
    assert_eq!(unbalanced, 3 * items.len());
    assert!(
        failures.is_empty(),
        "{} mismatches:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
