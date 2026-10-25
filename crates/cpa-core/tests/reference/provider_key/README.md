# OpenAI-compatible provider keys

`../../fixtures/provider_key_go.json` holds the provider key CLIProxyAPI v8.0.35
(`ae3ee3be`) registers an OpenAI-compatible credential's models under
(`registerModelsForAuth`, read back with `GetModelProviders`) for credentials that
config synthesis did not build, such as Home-dispatched and plugin auths.
`registry::dynamic` tests replay it.

Regenerate with Go 1.26.4, network denied, by overlaying the test into the reference
checkout (which stays unmodified):

```sh
reference=/absolute/path/to/CLIProxyAPI   # at ae3ee3be
here=$PWD                                 # this directory
echo "{\"Replace\":{\"$reference/sdk/cliproxy/zz_provider_key_test.go\":\"$here/zz_provider_key_test.go\"}}" > /tmp/provider-key-overlay.json
(cd "$reference" && CPA_FIXTURE_OUT="$here/../../fixtures/provider_key_go.json" \
  go test -count=1 -overlay /tmp/provider-key-overlay.json -run TestZZProviderKey ./sdk/cliproxy/)
```
