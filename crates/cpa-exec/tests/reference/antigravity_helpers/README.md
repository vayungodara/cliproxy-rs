# Antigravity request-helper recorder

`tests/fixtures/antigravity_helpers_go.json` records every call Go's tests make to
seven request helpers of the Antigravity executor, with Go's results:
`validateAntigravityRequestSignatures` (with the signature-cache settings in force),
`sanitizeAntigravityGeminiRequestSignatures`,
`normalizeAntigravityGeminiFunctionResponseRoles`,
`repairAntigravityGeminiFunctionResponseNames`, `sanitizeAntigravityRequestSchemasForModel` (with its Claude flag),
`shouldResolveAntigravityWebSearchGroundingURLs` and `resolveAntigravityRequestBaseURL`.
`antigravity_request::tests::helpers_match_go` replays each call.

A record keeps its Go test name only when the test is defined in one of the suites
the fixture ports (`HELPER_SUITES`); calls made by other tests stay, unnamed.

To regenerate it from a scratch copy of the pinned Go tree, with external network
denied:

    tar -C CLIProxyAPI --exclude=.git -cf - . | tar -C /tmp/cpa-rec2 -xf -
    python3 patch.py /tmp/cpa-rec2
    cp antigravity_helpers_rec.go zz_helpers_dump_test.go /tmp/cpa-rec2/internal/runtime/executor/
    cd /tmp/cpa-rec2 && E=$PWD/internal/runtime/executor && \
      HELPER_SUITES=$E/antigravity_executor_signature_test.go,$E/antigravity_schema_sanitize_test.go,$E/antigravity_executor_buildrequest_test.go,$E/antigravity_preupstream_rewrite_differential_test.go \
      HELPER_DUMP=/tmp/helpers_dump.json go test ./internal/runtime/executor/ -count=1 -run 'Antigravity|ZZHelpersDump'

then keep at most 60 records per helper and named test (100 unnamed), written
compactly.
