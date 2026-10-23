# Vertex API-key config models

`../../fixtures/vertex_models_go.json` holds the models CLIProxyAPI v8.0.35
(`ae3ee3be`) registers for a Vertex API-key entry: `config.ParseConfigBytes` (whose
`SanitizeVertexCompatKeys` keeps a model with a name and no alias since d9451cb6),
then `buildVertexCompatConfigModels`. Its `execution` section holds the upstream
model each request model executes as, for credentials synthesized from a second
config: `executionModelCandidatesWithAlias`, reached through the non-test overlay
`zz_vertex_models_export.go`. `config::credentials` tests replay both.

Regenerate with Go 1.26.4, network denied, by overlaying both files into the
reference checkout (which stays unmodified):

```sh
reference=/absolute/path/to/CLIProxyAPI   # at ae3ee3be
here=$PWD                                 # this directory
echo "{\"Replace\":{\"$reference/sdk/cliproxy/zz_vertex_models_test.go\":\"$here/zz_vertex_models_test.go\",\"$reference/sdk/cliproxy/auth/zz_vertex_models_export.go\":\"$here/zz_vertex_models_export.go\"}}" > /tmp/vertex-overlay.json
(cd "$reference" && CPA_FIXTURE_OUT="$here/../../fixtures/vertex_models_go.json" \
  go test -count=1 -overlay /tmp/vertex-overlay.json -run TestZZVertexConfigModels ./sdk/cliproxy/)
```
