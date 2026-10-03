package main

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"

	sdk "github.com/router-for-me/CLIProxyAPI/v8/sdk/translator"
)

// sdk/translator registry vectors (tests/fixtures/sdk_registry.json, replayed by
// tests/sdk_translator.rs): which format pairs Go registers, and what TranslateRequest
// and TranslateTokenCount do for pairs without a translator.

var sdkFormats = []string{"openai", "openai-response", "claude", "gemini", "codex", "antigravity", "interactions"}

// latin1 writes each byte as one rune so arbitrary (malformed) bytes survive JSON.
func latin1(s string) string {
	r := make([]rune, len(s))
	for i := 0; i < len(s); i++ {
		r[i] = rune(s[i])
	}
	return string(r)
}

type sdkRegistration struct {
	Client     string `json:"client"`
	Upstream   string `json:"upstream"`
	Request    bool   `json:"request"`
	Stream     bool   `json:"stream"`
	NonStream  bool   `json:"non_stream"`
	TokenCount bool   `json:"token_count"`
}

type sdkVector struct {
	Name     string `json:"name"`
	Path     string `json:"path"`
	Client   string `json:"client"`
	Upstream string `json:"upstream"`
	Model    string `json:"model,omitempty"`
	Count    int64  `json:"count,omitempty"`
	Input    string `json:"input"`
	Output   string `json:"output"`
}

func sdkRegistry(root, outDir string) {
	tokenCounts := map[string]bool{}
	for _, r := range registrations(root) {
		tokenCounts[r.client+":"+r.upstream] = r.tokenCount != ""
	}
	var regs []sdkRegistration
	for _, client := range sdkFormats {
		for _, upstream := range sdkFormats {
			c, u := sdk.FromString(client), sdk.FromString(upstream)
			regs = append(regs, sdkRegistration{
				Client: client, Upstream: upstream,
				Request:    sdk.HasRequestTransformerByFormatName(c, u),
				Stream:     sdk.HasStreamResponseTransformerByFormatName(c, u),
				NonStream:  sdk.HasNonStreamResponseTransformerByFormatName(c, u),
				TokenCount: tokenCounts[client+":"+upstream],
			})
		}
	}

	var vectors []sdkVector
	request := func(name, client, upstream, model, input string) {
		if sdk.HasRequestTransformerByFormatName(sdk.FromString(client), sdk.FromString(upstream)) {
			panic("fallback vector on a registered pair: " + name)
		}
		out := sdk.TranslateRequest(sdk.FromString(client), sdk.FromString(upstream), model, []byte(input), false)
		vectors = append(vectors, sdkVector{Name: name, Path: "request", Client: client, Upstream: upstream, Model: model, Input: latin1(input), Output: latin1(string(out))})
	}
	// registry_test.go TestTranslateRequest_FallbackNormalizesModel.
	for _, pair := range [][2]string{{"claude", "claude"}, {"openai-response", "openai-response"}, {"codex", "codex"}, {"gemini", "openai-response"}} {
		c, u := pair[0], pair[1]
		request("prefixed model is rewritten", c, u, "gpt-5-mini", `{"model":"copilot/gpt-5-mini","input":"ping"}`)
		request("matching model is left unchanged", c, u, "gpt-5-mini", `{"model":"gpt-5-mini","input":"ping"}`)
		request("empty model leaves payload unchanged", c, u, "", `{"model":"copilot/gpt-5-mini","input":"ping"}`)
		request("deeply prefixed model is rewritten", c, u, "gpt-5.3-codex", `{"model":"team/gpt-5.3-codex","stream":true}`)
	}
	// registry_summary_test.go TestRegistryTranslateRequestDoesNotMixSummaryIntoFallback.
	request("summary is not mixed into the fallback", "openai-response", "openai-response", "gemini-3.6-flash", `{"model":"gemini-3.6-flash","reasoning":{"summary":"auto"},"input":"hi"}`)
	// Go compares gjson Result.String() with the model and keeps the body on sjson errors.
	for _, v := range []struct{ name, model, input string }{
		{"missing model is appended", "m", `{"input":"x"}`},
		{"nested model is not the top-level model", "m", `{"meta":{"model":"m"}}`},
		{"null model", "m", `{"model":null}`},
		{"integer model equal as string", "5", `{"model":5}`},
		{"integer model differs from float text", "5.0", `{"model":5}`},
		{"exponent model stringifies", "1000", `{"model":1e3}`},
		{"boolean model", "true", `{"model":true}`},
		{"object model", "m", `{"model":{"id":"m"}}`},
		{"escaped model compares unescaped", "ab", `{"model":"a\u0062"}`},
		{"plain ASCII model with html stays raw", "a<b>&c", `{"model":"x"}`},
		{"non-ASCII model is marshaled", "modèle<&>", `{"model":"x"}`},
		{"quoted model is marshaled", "a\"b", `{"model":"x"}`},
		{"line separator model", "a\u2028b", `{"model":"x"}`},
		{"duplicate model keys", "m", `{"model":"a","model":"m"}`},
		{"duplicate keys first matches", "a", `{"model":"a","model":"m"}`},
		{"malformed bytes are kept", "m", "{\"model\":\"x\",\"t\":\"\xff\xfe\"}"},
		{"array body", "m", `[1,2]`},
		{"invalid JSON", "m", `not json`},
		{"empty body", "m", ``},
		{"surrounding whitespace", "m", " \n{\"model\":\"x\"} \n"},
		{"empty object", "m", `{}`},
		{"string body", "m", `"text"`},
	} {
		request(v.name, "claude", "claude", v.model, v.input)
	}
	// TranslateTokenCount without a registered TokenCount returns the upstream body.
	for _, pair := range [][2]string{{"claude", "claude"}, {"openai", "claude"}, {"openai-response", "codex"}} {
		c, u := pair[0], pair[1]
		input := "{\"input_tokens\":7,\"raw\":\"\xff\"}"
		out := sdk.TranslateTokenCount(context.Background(), sdk.FromString(u), sdk.FromString(c), 9, []byte(input))
		vectors = append(vectors, sdkVector{Name: "token count without TokenCount", Path: "token_count", Client: c, Upstream: u, Count: 9, Input: latin1(input), Output: latin1(string(out))})
	}

	raw, err := json.MarshalIndent(map[string]any{"registrations": regs, "vectors": vectors}, "", " ")
	if err != nil {
		panic(err)
	}
	if err := os.WriteFile(filepath.Join(outDir, "..", "sdk_registry.json"), append(raw, '\n'), 0o644); err != nil {
		panic(err)
	}
}
