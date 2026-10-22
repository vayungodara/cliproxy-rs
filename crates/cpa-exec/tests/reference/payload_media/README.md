# Media payload rule goldens

`../../fixtures/payload_media_go.json` holds what CLIProxyAPI v8.0.35's
`helps.ApplyMediaPayloadConfig` (868e137) returns for JSON and multipart image
requests: the rules' JSON view of each body (`mediaPayloadJSON`), and the body sent
upstream. Go writes a rebuilt form's parts in random order under a random boundary, so a
rebuilt form is recorded as its parts (headers sorted, body in base64), sorted.
`payload_media::tests::media_payload_rules_match_go` replays them.

Record with Go 1.26.4 from a v8.0.35 checkout (`ae3ee3be`), network denied:

```sh
cp zz_rsfix_payload_media_test.go <CLIProxyAPI>/internal/runtime/executor/helps/
cd <CLIProxyAPI>
RSFIX_OUT=<cliproxy-rs>/crates/cpa-exec/tests/fixtures/payload_media_go.json \
  go test -count=1 -run '^TestRSFixPayloadMedia$' ./internal/runtime/executor/helps/
rm internal/runtime/executor/helps/zz_rsfix_payload_media_test.go
```
