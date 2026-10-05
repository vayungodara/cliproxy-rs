# Patched third-party crates

Each directory here is a copy of a published crate with the changes listed below. The root `Cargo.toml` and `harness/rust/Cargo.toml` point `[patch.crates-io]` at them. They are not workspace members, so `cargo fmt`, clippy and `cargo test --workspace` leave them alone; CI runs each crate's own tests after the workspace tests, in the same network-denied step:

```sh
cargo test --locked --manifest-path vendor/gjson/Cargo.toml
```

## gjson 0.8.1

Source: the crates.io archive `gjson-0.8.1.crate`, SHA-256 `43503cc176394dd30a6525f5f36e838339b8b5619be33ed9a7783841580a97b6` (the checksum `Cargo.lock` recorded before the patch). License: MIT, `gjson/LICENSE`.

The Claude executor reads request bodies with gjson dozens of times per request, and almost every byte of a coding agent's prompt sits inside a JSON string, which gjson scanned one byte at a time.

[`gjson.patch`](gjson.patch) is the complete difference between the archive and `vendor/gjson`, apart from the files dropped below. Apply it with `patch -p2` inside an unpacked copy of the archive.

- `src/lib.rs`: `scan_string` finds the next `"` or `\` with `memchr2` instead of a table lookup per byte. The `test` module is declared again (see below), next to the new `scan_tests`.
- `src/valid.rs`: `valid_string` jumps to the next `"` or `\` the same way and checks the bytes it skipped for control characters in one pass. Results, including the index where validation fails, are unchanged.
- `src/scan_tests.rs` (new): both functions against verbatim copies of the originals, on every start offset of thousands of short adversarial buffers and of long ones with a special byte at every position.
- `Cargo.toml`: a `memchr = "2"` dependency. `Cargo.lock` (new) pins it to the version the workspace uses, so CI can run the tests with `--locked`.

Dropped from the archive: `.cargo_vcs_info.json`, `Cargo.toml.orig`, `.github/`, `.gitignore`, `extra/` (fuzzing and Go parity scripts) and `testfiles/` (`twitter.json`, 662 KB, and `twitterescaped.json`, 562 KB).

Upstream tests in `src/test.rs`: kept are `jsonlines`, `fuzz`, `array_value`, `escaped_query_string` and `bool_convert_query`, and every test module in the other source files. `fuzz` replays crash files from `extra/fuzz/out/default/crashes` and returns at once without that directory, as it does in the published crate. Left out, because each reads the 1.2 MB of fixtures in `testfiles/`: `various`, `modifiers`, `iterator`, `array`, `query`, `multipath` and `escaped`.
