# String settings written as YAML numbers or booleans

`../../fixtures/scalar_settings_go.json` holds string settings CLIProxyAPI v8.0.35
(`ae3ee3be`) loads from plain YAML scalars that are not strings (`0x1F`, `1.50`, `007`,
`true`), in the v8 and legacy layouts and through an anchored mapping key. yaml.v3
reads each into the Go string as its text. Each check names the field by its Go YAML
path (`ParseConfigBytes`, read by reflection) and by its v8 path in cliproxy-rs.
`config` tests replay it.

Regenerate with Go 1.26.4, network denied, by overlaying the test into the reference
checkout (which stays unmodified):

```sh
reference=/absolute/path/to/CLIProxyAPI   # at ae3ee3be
here=$PWD                                 # this directory
echo "{\"Replace\":{\"$reference/internal/config/zz_scalar_settings_test.go\":\"$here/zz_scalar_settings_test.go\"}}" > /tmp/scalar-settings-overlay.json
(cd "$reference" && CPA_FIXTURE_OUT="$here/../../fixtures/scalar_settings_go.json" \
  go test -count=1 -overlay /tmp/scalar-settings-overlay.json -run TestZZScalarSettings ./internal/config/)
```
