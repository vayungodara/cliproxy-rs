# Patched third-party crates

Each directory here is an unmodified copy of a published crate except for the changes listed below. The root `Cargo.toml` points `[patch.crates-io]` at them, and they are not workspace members, so `cargo fmt`, clippy and the workspace tests leave them alone. Run a crate's own tests with `cargo test --manifest-path vendor/<crate>/Cargo.toml`.

## gjson 0.8.1

Source: crates.io `gjson` 0.8.1 (MIT, `LICENSE`). The Claude executor reads request bodies with gjson dozens of times per request, and almost every byte of a coding agent's prompt sits inside a JSON string, which gjson scanned one byte at a time.

- `scan_string` (`src/lib.rs`) finds the next `"` or `\` with `memchr2` instead of a table lookup per byte.
- `valid_string` (`src/valid.rs`) jumps to the next `"` or `\` the same way and checks the bytes it skipped for control characters in one pass.
- `src/scan_tests.rs` replaces the upstream `src/test.rs` (it needs fixture files the published crate does not ship) and compares both functions with verbatim copies of the originals on adversarial inputs.

Results, including the index where validation fails, are unchanged.
