# Contributing

Thanks for helping. Bug reports, fixes and new tests are all welcome.

## Before you start

- cliproxy-rs follows CLIProxyAPI at commit `6fecc6e`. When the two disagree, CLIProxyAPI's code is the reference, unless the difference is listed in [docs/DIFFERENCES-FROM-GO.md](docs/DIFFERENCES-FROM-GO.md). [docs/PARITY.md](docs/PARITY.md) shows what is ported and what is not.
- For a larger change, open an issue first so we can agree on the approach.

## Building and testing

You need Rust (stable), `cmake`, `clang` and `perl` for BoringSSL. Go 1.26 and PostgreSQL let the plugin and storage tests run instead of skipping.

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Tests use local mock upstreams only. Never point a test at a real provider or use a real account or key, and run the tests with external network access blocked if you can; CI runs them in a network namespace with only a loopback interface. The BoringSSL build and the release link use a lot of memory, so on a small machine set `CARGO_BUILD_JOBS=2`.

The dashboard lives in `ui/` (Svelte 5). `npm ci`, `npm run check`, `npm test` and `npm run build` there; the build also checks the bundle size budget. See [ui/README.md](ui/README.md).

## Pull requests

- Keep each pull request to one change, with a test that fails without it.
- Request bodies are forwarded byte for byte unless a ported rule rewrites them; do not re-serialize JSON just to pass it through.
- Never log or forward credentials or client keys to the wrong side.
- Mark a deliberate simplification with a `ponytail:` comment that names its limit and how to lift it.
- CI must pass: format, clippy, tests, the harness check and the Windows build check.

By contributing you agree that your work is released under the [MIT license](LICENSE).
