//! An in-memory Home KV behind the fake Home, for the Home-mode cache tests.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// An in-memory KV behind the fake Home, honouring NX and XX.
pub(crate) fn kv_home(values: Arc<Mutex<HashMap<String, String>>>) -> impl Fn(&[String]) -> cpa_home::fake::Reply {
    use cpa_home::fake::{bulk, raw};
    move |args| {
        let mut values = values.lock().unwrap();
        match args[0].to_ascii_lowercase().as_str() {
            "get" => values.get(&args[1]).map_or_else(|| raw("$-1\r\n"), bulk),
            "set" => {
                let exists = values.contains_key(&args[1]);
                let flag = |f: &str| args.iter().any(|a| a == f);
                if (flag("NX") && exists) || (flag("XX") && !exists) {
                    return raw("$-1\r\n");
                }
                values.insert(args[1].clone(), args[2].clone());
                raw("+OK\r\n")
            }
            // Home's `CAS <key> <0|1> <expected> <new> [PX ms]`.
            "cas" => {
                let current = values.get(&args[1]);
                let matches = match args[2].as_str() {
                    "0" => current.is_none(),
                    _ => current == Some(&args[3]),
                };
                if !matches {
                    return raw(":0\r\n");
                }
                values.insert(args[1].clone(), args[4].clone());
                raw(":1\r\n")
            }
            "del" => {
                let deleted = args[1..].iter().filter(|k| values.remove(*k).is_some()).count();
                raw(&format!(":{deleted}\r\n"))
            }
            "expire" => raw(if values.contains_key(&args[1]) {
                ":1\r\n"
            } else {
                ":0\r\n"
            }),
            _ => raw("+PONG\r\n"),
        }
    }
}

/// One RESP command in the shape the Go goldens record KV client calls: `SET .. EX ..
/// NX` is `KVSetNX`, other SETs are `KVSet` with their NX/XX flags; TTLs in
/// milliseconds.
pub(crate) fn as_go_call(args: &[String]) -> Option<serde_json::Value> {
    use serde_json::json;
    let seconds_ms = |s: &str| s.parse::<i64>().unwrap() * 1000;
    match args[0].to_ascii_lowercase().as_str() {
        "get" => Some(json!(["get", args[1]])),
        "set" => {
            let ex = args
                .iter()
                .position(|a| a == "EX")
                .map_or(0, |i| seconds_ms(&args[i + 1]));
            let flags: Vec<&String> = args[3..].iter().filter(|a| *a == "NX" || *a == "XX").collect();
            if ex > 0 && flags.iter().any(|f| *f == "NX") {
                return Some(json!(["setnx", args[1], args[2], ex]));
            }
            let mut call = vec![json!("set"), json!(args[1]), json!(args[2]), json!(ex)];
            call.extend(flags.into_iter().map(|f| json!(f)));
            Some(serde_json::Value::Array(call))
        }
        "expire" => Some(json!(["expire", args[1], seconds_ms(&args[2])])),
        "del" => Some(json!(["del", args[1]])),
        "cas" => {
            let ttl = args
                .iter()
                .position(|a| a == "PX")
                .map_or(0, |i| args[i + 1].parse::<i64>().unwrap());
            Some(json!(["cas", args[1], args[3], args[2] == "1", args[4], ttl]))
        }
        _ => None,
    }
}
