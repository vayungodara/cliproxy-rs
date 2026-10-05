// Copyright 2021 Joshua J Baker. All rights reserved.
// Use of this source code is governed by an MIT-style
// license that can be found in the LICENSE file.

// Bit flags passed to the "info" parameter of the iter function which
// provides additional information about the data

#[cfg(test)]
use super::*;

#[test]
fn jsonlines() {
    let json = r#"
        {"a": 1 }
        {"a": 2 }
        true
        false
        4
    "#;
    assert_eq!(get(json, "..#").i32(), 5);
    assert_eq!(get(json, "..0.a").i32(), 1);
    assert_eq!(get(json, "..1.a").i32(), 2);
    assert_eq!(
        get(json, "..#.@this|@ugly").json(),
        r#"[{"a":1},{"a":2},true,false,4]"#
    );
    assert_eq!(get(json, "..#.@this|@join|@ugly").json(), r#"{"a":2}"#);
}


#[cfg(test)]
const EXAMPLE: &str = r#"
{
  "name": {"first": "Tom", "last": "Anderson"},
  "age":37,
  "children": ["Sara","Alex","Jack"],
  "fav.movie": "Deer Hunter",
  "friends": [
    {"first": "Dale", "last": "Murphy", "age": 44, "nets": ["ig", "fb", "tw"]},
    {"first": "Roger", "last": "Craig", "age": 68, "nets": ["fb", "tw"]},
    {"first": "Jane", "last": "Murphy", "age": 47, "nets": ["ig", "tw"]}
  ]
}
"#;

#[cfg(test)]
fn exec_simple_fuzz(data: &[u8]) {
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = std::str::from_utf8(get(s, s).json().as_bytes()).unwrap();
        let _ = std::str::from_utf8(get(EXAMPLE, s).json().as_bytes()).unwrap();
    }
}

#[test]
fn fuzz() {
    // This only runs on crash files in the fuzz directory.
    let crash_dir = "extra/fuzz/out/default/crashes";
    if !std::path::Path::new(crash_dir).exists() {
        return;
    }
    let mut files = std::fs::read_dir(crash_dir)
        .unwrap()
        .map(|res| res.map(|e| e.path()))
        .collect::<Result<Vec<_>, std::io::Error>>()
        .unwrap();
    files.sort();
    for file in files {
        let fname = file.as_path().to_str().unwrap().to_owned();
        eprintln!("{}", fname);
        let data = std::fs::read(file).unwrap();
        exec_simple_fuzz(&data);
    }
}

#[test]
fn array_value() {
    const PROGRAMMERS: &str = r#"
    {
        "programmers": [
          {
            "firstName": "Janet", 
            "lastName": "McLaughlin", 
          }, {
            "firstName": "Elliotte", 
            "lastName": "Hunter", 
          }, {
            "firstName": "Jason", 
            "lastName": "Harold", 
          }
        ]
      }
    "#;
    let mut res = String::new();
    let value = get(PROGRAMMERS, "programmers.#.lastName");
    for name in value.array() {
        res.extend(format!("{}\n", name).chars());
    }
    assert_eq!(res, "McLaughlin\nHunter\nHarold\n");
}

#[test]
fn escaped_query_string() {
    const JSON: &str = r#"
    {
        "name": {"first": "Tom", "last": "Anderson"},
        "age":37,
        "children": ["Sara","Alex","Jack"],
        "fav.movie": "Deer Hunter",
        "friends": [
          {"first": "Dale", "last": "Mur\"phy", "age": 44, "nets": ["ig", "fb", "tw"]},
          {"first": "Roger", "last": "Craig", "age": 68, "nets": ["fb", "tw"]},
          {"first": "Jane", "last": "Murphy", "age": 47, "nets": ["ig", "tw"]}
        ]
      }
    }
    "#;
    assert_eq!(get(JSON, r#"friends.#(last="Mur\"phy").age"#).i32(), 44);
    assert_eq!(get(JSON, r#"friends.#(last="Murphy").age"#).i32(), 47);
}


#[test]
fn bool_convert_query() {
    const JSON: &str = r#"
    {
		"vals": [
			{ "a": 1, "b": true },
			{ "a": 2, "b": true },
			{ "a": 3, "b": false },
			{ "a": 4, "b": "0" },
			{ "a": 5, "b": 0 },
			{ "a": 6, "b": "1" },
			{ "a": 7, "b": 1 },
			{ "a": 8, "b": "true" },
			{ "a": 9, "b": false },
			{ "a": 10, "b": null },
			{ "a": 11 }
		]
	}
    "#;

    assert_eq!(get(JSON, r#"vals.#(b==~true)#.a"#).json(), "[1,2,6,7,8]");
    // assert_eq!(get(JSON, r#"vals.#(b==~false)#.a"#).json(), "[3,4,5,9,10,11]");
}
