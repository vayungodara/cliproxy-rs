# Auth-file models and Vertex partner models

`../../fixtures/vertex_openapi_models_go.json` holds what CLIProxyAPI v8.0.35
(`ae3ee3be`, d9451cb6) does with auth files:

- `files`: each file goes through `synthesizer.SynthesizeAuthFile` with the fixture's
  `oauth-model-alias` config, then `registerModelsForAuth`. Each case records:
  - the `model_aliases` attribute, now also read from `models`, a list Go drops whole
    when an entry does not decode, with keys matched case-insensitively;
  - the `openapi_models` attribute;
  - the models the registry lists, sorted by ID;
  - the upstream model each request executes as (`ResolveExecutionModel`), and whether
    the registry lists it (`ClientSupportsModel`).
- `accounts`: `AuthKind` and `AccountInfo` for bare metadata, without an `auth_kind`
  attribute, holding a `service_account` or a `siwc` section.

`config::credentials` tests replay both sections.

Regenerate with Go 1.26.4, network denied, by overlaying the test file into the
reference checkout (which stays unmodified):

```sh
reference=/absolute/path/to/CLIProxyAPI   # at ae3ee3be
here=$PWD                                 # this directory
echo "{\"Replace\":{\"$reference/sdk/cliproxy/zz_vertex_openapi_models_test.go\":\"$here/zz_vertex_openapi_models_test.go\"}}" > /tmp/vom-overlay.json
(cd "$reference" && CPA_FIXTURE_OUT=/tmp/vom.json \
  go test -count=1 -overlay /tmp/vom-overlay.json -run TestZZVertexOpenAPIModels ./sdk/cliproxy/)
python3 -c "import json; d = json.load(open('/tmp/vom.json')); open('$here/../../fixtures/vertex_openapi_models_go.json', 'w').write(json.dumps(d, indent=2, ensure_ascii=False) + '\n')"
```
