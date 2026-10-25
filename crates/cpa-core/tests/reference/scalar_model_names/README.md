# Model names written as YAML numbers or booleans

`../../fixtures/scalar_model_names_go.json` holds the models CLIProxyAPI v8.0.35
(`ae3ee3be`) registers, and the upstream model each executes as, for config entries
whose model names and aliases are plain YAML scalars that are not strings (`0x1F`,
`1.50`, `007`, `true`, aliased and merged in too, in the v8 and legacy layouts), or
tagged `!!int`, `!!float` or `!!bool`. The `lists` sections also hold `excluded-models`
written that way, and the parsed `oauth-excluded-models` and `oauth-model-alias`.
yaml.v3 reads each into the string field as its text. `config::credentials` tests
replay it.

Regenerate with Go 1.26.4, network denied, by overlaying the test and the export
from `../vertex_models` into the reference checkout (which stays unmodified):

```sh
reference=/absolute/path/to/CLIProxyAPI   # at ae3ee3be
here=$PWD                                 # this directory
echo "{\"Replace\":{\"$reference/sdk/cliproxy/zz_scalar_model_names_test.go\":\"$here/zz_scalar_model_names_test.go\",\"$reference/sdk/cliproxy/auth/zz_vertex_models_export.go\":\"$here/../vertex_models/zz_vertex_models_export.go\"}}" > /tmp/scalar-names-overlay.json
(cd "$reference" && CPA_FIXTURE_OUT="$here/../../fixtures/scalar_model_names_go.json" \
  go test -count=1 -overlay /tmp/scalar-names-overlay.json -run TestZZScalarModelNames ./sdk/cliproxy/)
```
