# Multipart form goldens

`../../fixtures/multipart_form_go.json` holds what Go's `mime/multipart`
`Reader.ReadForm(32 << 20)` returns over `bytes.NewReader(body)`, as CLIProxyAPI
v8.0.35's image executors call it. The boundary is `b` unless a case records one. The
cases cover MIME header continuation lines, textproto's SP/HTAB trimming, a bare CR at
the end of the body, header values outside UTF-8, the canonical key in value
diagnostics, the 80-byte initial-line diagnostic, the 4096-byte line buffer (including
an unterminated header line whose last `ReadLine` fragment ends the body), boundaries
of 4089 to 4092 bytes, where a part's delimiter no longer fits that buffer, and the
`NextPart` diagnostics. Bodies, values, header values and file bodies
are base64. `openai_compat_multipart::tests::reads_forms_as_go_does` replays them.

The program uses the standard library only. Record it with Go 1.26.4, the toolchain of
the v8.0.35 release build, from which `textproto` quotes MIME header diagnostics with
`%q`:

```sh
crate=/absolute/path/to/cliproxy-rs/crates/cpa-exec
tmp=$(mktemp -d)
cp "$crate/tests/reference/multipart_form/main.go" "$tmp/"
(
  cd "$tmp"
  go mod init multipartform
  TZ=UTC GOTOOLCHAIN=local /path/to/go1.26.4/bin/go run . "$crate/tests/fixtures/multipart_form_go.json"
)
rm -rf "$tmp"
```
