import sys, re
root = sys.argv[1] + '/internal/runtime/executor/'
def patch(file, pairs):
    p = root + file
    s = open(p).read()
    for a, b in pairs:
        assert a in s, (file, a)
        s = s.replace(a, b, 1)
    open(p, 'w').write(s)
patch('antigravity_executor.go', [
    ('func validateAntigravityRequestSignatures(ctx', 'func validateAntigravityRequestSignaturesInner(ctx'),
    ('func sanitizeAntigravityGeminiRequestSignatures(modelName', 'func sanitizeAntigravityGeminiRequestSignaturesInner(modelName'),
    ('func normalizeAntigravityGeminiFunctionResponseRoles(rawJSON', 'func normalizeAntigravityGeminiFunctionResponseRolesInner(rawJSON'),
    ('func repairAntigravityGeminiFunctionResponseNames(rawJSON', 'func repairAntigravityGeminiFunctionResponseNamesInner(rawJSON'),
    ('func shouldResolveAntigravityWebSearchGroundingURLs(from', 'func shouldResolveAntigravityWebSearchGroundingURLsInner(from'),
])
patch('antigravity_executor_request.go', [
    ('func sanitizeAntigravityRequestSchemasForModel(payloadStr', 'func sanitizeAntigravityRequestSchemasForModelInner(payloadStr'),
    ('func resolveAntigravityRequestBaseURL(auth', 'func resolveAntigravityRequestBaseURLInner(auth'),
])
