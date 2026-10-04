# TUI goldens

`../../fixtures/tui_go.json` holds what Go's TUI models (internal/tui at CLIProxyAPI
`6fecc6e`) render for the inputs in `cases.json`: `render_fixture_test.go` feeds each
case's messages and keys straight into Go's models (no command runs, nothing is
fetched) and records the text with lipgloss set to the ASCII profile, so the fixture
is the layout without colour. `src/tui/golden.rs` drives the Rust tabs the same way and
compares line by line, trailing spaces aside.

Values are fake. Run the generator with external network denied (for example inside
`unshare -rn` with `GOPROXY=off GOTOOLCHAIN=local`, after `go mod download`):

```sh
reference=/absolute/path/to/CLIProxyAPI   # at 6fecc6e, Go 1.26
here=$PWD                                 # this directory
crate=$(cd ../../.. && pwd)               # crates/cliproxy
echo "{\"Replace\": {\"$reference/internal/tui/zz_render_fixture_test.go\": \"$here/render_fixture_test.go\"}}" > /tmp/tui-overlay.json
(cd "$reference" && CPA_FIXTURE_IN="$here/cases.json" CPA_FIXTURE_OUT="$crate/tests/fixtures/tui_go.json" \
  go test -count=1 -overlay /tmp/tui-overlay.json -run '^TestZZRenderFixture$' ./internal/tui)
```

Two deliberate layout differences are handled in the test: the dashboard cards fit the
screen in the TUI (`Dashboard::fit_cards`, off in the test), and a tab bar wider than
the screen is cut instead of wrapped onto a second row.
