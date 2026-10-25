package executor

import (
	"context"
	"runtime"
	"strings"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/cache"
	cliproxyauth "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/auth"
	sdktranslator "github.com/router-for-me/CLIProxyAPI/v8/sdk/translator"
)

// Recording hooks for the Rust port's helper fixture (not part of CLIProxyAPI): every
// call of the wrapped request helpers, with the Go test on the stack.

type helperRecord struct {
	Fn         string            `json:"fn"`
	Test       string            `json:"test"`
	Model      string            `json:"model,omitempty"`
	From       string            `json:"from,omitempty"`
	In         string            `json:"in,omitempty"`
	Original   string            `json:"original,omitempty"`
	Flag       bool              `json:"flag,omitempty"`
	Claude     bool              `json:"claude,omitempty"`
	CacheOn    bool              `json:"cache_on"`
	Strict     bool              `json:"strict"`
	Attributes map[string]string `json:"attributes,omitempty"`
	Metadata   map[string]any    `json:"metadata,omitempty"`
	Out        string            `json:"out"`
	Bool       bool              `json:"bool,omitempty"`
	Error      string            `json:"error,omitempty"`
}

var helperRecords []helperRecord

func helperTest() string {
	pcs := make([]uintptr, 64)
	frames := runtime.CallersFrames(pcs[:runtime.Callers(2, pcs)])
	for {
		frame, more := frames.Next()
		if i := strings.Index(frame.Function, ".Test"); i >= 0 {
			name := frame.Function[i+1:]
			if j := strings.Index(name, "."); j >= 0 {
				name = name[:j]
			}
			return name
		}
		if !more {
			return ""
		}
	}
}

func recordHelper(r helperRecord) {
	r.Test = helperTest()
	r.CacheOn = cache.SignatureCacheEnabled()
	r.Strict = cache.SignatureBypassStrictMode()
	helperRecords = append(helperRecords, r)
}

func validateAntigravityRequestSignatures(ctx context.Context, modelName string, from sdktranslator.Format, rawJSON []byte) ([]byte, error) {
	in := string(rawJSON)
	out, err := validateAntigravityRequestSignaturesInner(ctx, modelName, from, rawJSON)
	r := helperRecord{Fn: "validate", Model: modelName, From: from.String(), In: in, Out: string(out)}
	if err != nil {
		r.Error = err.Error()
	}
	recordHelper(r)
	return out, err
}

func sanitizeAntigravityGeminiRequestSignatures(modelName string, rawJSON []byte) []byte {
	in := string(rawJSON)
	out := sanitizeAntigravityGeminiRequestSignaturesInner(modelName, rawJSON)
	recordHelper(helperRecord{Fn: "sanitize_signatures", Model: modelName, In: in, Out: string(out)})
	return out
}

func normalizeAntigravityGeminiFunctionResponseRoles(rawJSON []byte) []byte {
	in := string(rawJSON)
	out := normalizeAntigravityGeminiFunctionResponseRolesInner(rawJSON)
	recordHelper(helperRecord{Fn: "normalize_roles", In: in, Out: string(out)})
	return out
}

func repairAntigravityGeminiFunctionResponseNames(rawJSON []byte) []byte {
	in := string(rawJSON)
	out := repairAntigravityGeminiFunctionResponseNamesInner(rawJSON)
	recordHelper(helperRecord{Fn: "repair_names", In: in, Out: string(out)})
	return out
}

func shouldResolveAntigravityWebSearchGroundingURLs(from sdktranslator.Format, originalRequestRawJSON, requestRawJSON []byte) bool {
	out := shouldResolveAntigravityWebSearchGroundingURLsInner(from, originalRequestRawJSON, requestRawJSON)
	recordHelper(helperRecord{Fn: "resolves_grounding", From: from.String(), Original: string(originalRequestRawJSON), In: string(requestRawJSON), Bool: out})
	return out
}

func sanitizeAntigravityRequestSchemasForModel(payloadStr string, useAntigravitySchema, isClaudeModel bool) string {
	out := sanitizeAntigravityRequestSchemasForModelInner(payloadStr, useAntigravitySchema, isClaudeModel)
	recordHelper(helperRecord{Fn: "sanitize_schemas", In: payloadStr, Flag: useAntigravitySchema, Claude: isClaudeModel, Out: out})
	return out
}

func resolveAntigravityRequestBaseURL(auth *cliproxyauth.Auth) string {
	out := resolveAntigravityRequestBaseURLInner(auth)
	r := helperRecord{Fn: "base_url", Out: out}
	if auth != nil {
		r.Attributes, r.Metadata = auth.Attributes, auth.Metadata
	}
	recordHelper(r)
	return out
}
