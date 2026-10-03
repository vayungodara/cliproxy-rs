//! The raw C ABI against a Go-built plugin: calls racing shutdown must either complete or
//! report a closed client, never touch freed library state.

// Native plugins load only on Unix (the host's `native` module).
#![cfg(unix)]

mod support;

use std::sync::Arc;

use bytes::Bytes;
use cpa_plugin::client::{CallbackError, CallbackHandler, CallbackInstance};
use cpa_plugin::native::NativeClient;

struct NoCallbacks;

impl CallbackHandler for NoCallbacks {
    fn call_from_plugin(
        &self,
        _: &str,
        _: &Arc<CallbackInstance>,
        method: &str,
        _: Bytes,
    ) -> Result<Bytes, CallbackError> {
        Err(CallbackError::new(format!("unsupported host callback {method}")))
    }
}

#[test]
fn calls_racing_shutdown_are_safe() {
    let Some(built) = support::built_plugins_for("native_abi::calls_racing_shutdown_are_safe") else {
        return;
    };
    let client = Arc::new(
        NativeClient::open(
            &built.join("simple.so"),
            "simple",
            Arc::new(NoCallbacks),
            Arc::default(),
        )
        .unwrap(),
    );
    let first = client.call("auth.identifier", b"{}").unwrap();
    assert_eq!(&first[..], br#"{"ok":true,"result":{"identifier":"plugin-example"}}"#);
    // Each caller completes one call before shutdown starts (so `total` cannot be zero
    // on a slow machine), then keeps calling while shutdown races it.
    let started = Arc::new(std::sync::Barrier::new(5));
    let callers: Vec<_> = (0..4)
        .map(|_| {
            let client = client.clone();
            let started = started.clone();
            std::thread::spawn(move || {
                let first = client.call("model.static", b"{}").unwrap();
                assert!(first.starts_with(br#"{"ok":true"#));
                started.wait();
                let mut ok = 1;
                for _ in 0..500 {
                    match client.call("model.static", b"{}") {
                        Ok(resp) => {
                            assert!(resp.starts_with(br#"{"ok":true"#));
                            ok += 1;
                        }
                        Err(e) => {
                            assert_eq!(e, "plugin client is closed");
                            break;
                        }
                    }
                }
                ok
            })
        })
        .collect();
    started.wait();
    std::thread::sleep(std::time::Duration::from_millis(5));
    client.shutdown();
    let total: usize = callers.into_iter().map(|t| t.join().unwrap()).sum();
    assert!(total >= 4);
    assert_eq!(
        client.call("model.static", b"{}"),
        Err("plugin client is closed".into())
    );
}
