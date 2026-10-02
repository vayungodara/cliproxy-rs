// Command main writes server-core goldens from CLIProxyAPI at 6fecc6e. It calls exported
// Go functions in-process only; it opens no sockets and calls no provider endpoint.
package main

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"strings"
	"time"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/registry"
	"github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/auth"
)

type pair struct {
	In  string `json:"in"`
	Out string `json:"out"`
}

var sanitizeInputs = []string{
	"",
	"   ",
	"  plain message  ",
	"open /home/user/.cli-proxy-api/claude-a.json: permission denied",
	"copy /tmp/a to /tmp/b: denied",
	"copy /tmp/a TO /tmp/b: denied",
	"request to https://user:pass@api.example.com/v1?key=abc123&x=1 failed",
	"Post \"https://api.example.com/v1/messages?beta=true\": dial tcp 10.0.0.1:443: i/o timeout",
	"Authorization: Bearer sk-ant-api03-abcdefghijkl",
	"invalid api key: sk-abcdefghijklmnop",
	"invalid x-api-key",
	"the api key is abc123, retry later",
	`{"token": "secret-value", "other": 1}`,
	"Cookie: session=abc; other=1",
	`C:\Users\me\auth.json not found`,
	`\\server\share\file failed`,
	"failed reading '/etc/secret.pem'",
	"failed reading \"/etc/secret.pem\" now",
	"failed reading `/etc/secret.pem` now",
	"error at (/var/lib/x/y): no such file or directory",
	"read /a b/c d: is a directory",
	"upstream returned 401: invalid token abc.def.ghi",
	"password=hunter2 and user=bob",
	"client_secret is s3cr3t; next",
	"auth basic dXNlcjpwYXNz",
	"ghp_abcdefghijk1234 leaked",
	"path /usr/local/bin/tool:12 crashed",
	"multiple /a/b and /c/d paths",
	"config at /opt/app/config.yaml loaded",
	"token: XYZ αβγ",
	"connect: connection refused",
	"sessionid=abcd1234",
	"rate limited, please retry in 30s",
	"model claude-sonnet-4 is not supported: use another",
	"see http://example.com/docs/errors: failed",
	"x=/srv/data/file.txt, y=2",
	"Set-Cookie: a=b\nnext line",
	"key: \"quoted value\" rest",
	"bearer abc.def and more",
	"The access token provided is expired via oauth",
	strings.Repeat("é", 300),
	strings.Repeat("ab ", 100),
}

var extractInputs = []string{
	"",
	`{"error":{"code":"x","message":"boom"}}`,
	`{"error":{"type":"overloaded","message":"overloaded now"}}`,
	`{"error":{"type":"rate_limit_error","message":"Rate limited"}}`,
	`{"error":{"code":"insufficient_quota","message":"You exceeded your quota: insufficient_quota"}}`,
	`status 500: {"message":"m at /home/u/f.json: denied"}`,
	`{"error":"flat sk-abcdefghijkl"}`,
	`{"code":"c","message":""}`,
	`{"type":"t"}`,
	`{"error":{"code":42,"message":"numeric"}}`,
	`{"other":1}`,
	"plain text",
	`[1,2]`,
}

// availabilityCases are per-credential projections of one model, as the conductor
// writes them (clientModelProjectionForAuth): "none", "quota" (a 429 model cooldown:
// suspended with reason quota and quota-exceeded), "other" (suspended for another
// reason) and "other_qe" (suspended for another reason and quota-exceeded).
var availabilityCases = [][]string{
	{"none"},
	{"quota"},
	{"other"},
	{"other_qe"},
	{"none", "none"},
	{"quota", "quota"},
	{"other", "other"},
	{"other", "none"},
	{"quota", "none"},
	{"quota", "other"},
	{"other_qe", "none"},
	{"other_qe", "quota"},
	{"other_qe", "other_qe"},
	{"quota", "quota", "other"},
	{"other", "other", "none"},
}

type availability struct {
	States    []string `json:"states"`
	Available bool     `json:"available"`
}

// modelAvailability registers one model per case on the real registry, applies the
// projections and reads GetAvailableModels.
func modelAvailability() []availability {
	reg := registry.GetGlobalRegistry()
	models := make([]string, len(availabilityCases))
	for i, states := range availabilityCases {
		models[i] = fmt.Sprintf("golden-model-%d", i)
		for j, state := range states {
			client := fmt.Sprintf("golden-client-%d-%d", i, j)
			reg.RegisterClient(client, "openai", []*registry.ModelInfo{{ID: models[i], Object: "model", OwnedBy: "golden"}})
			projection := registry.ClientModelProjection{ModelID: models[i]}
			switch state {
			case "quota":
				projection.Suspended, projection.SuspendReason, projection.QuotaExceeded = true, "quota", true
			case "other":
				projection.Suspended, projection.SuspendReason = true, "unauthorized"
			case "other_qe":
				projection.Suspended, projection.SuspendReason, projection.QuotaExceeded = true, "cloudflare_challenge", true
			}
			if !reg.ApplyClientModelProjections(client, reg.ClientRegistrationEpoch(client), 1, []registry.ClientModelProjection{projection}) {
				panic("projection rejected for " + client)
			}
		}
	}
	listed := map[string]bool{}
	for _, m := range reg.GetAvailableModels("openai") {
		listed[m["id"].(string)] = true
	}
	out := make([]availability, len(availabilityCases))
	for i, states := range availabilityCases {
		out[i] = availability{states, listed[models[i]]}
	}
	return out
}

// step is one MarkResult on a fresh claude credential. RetryAfterMs < 0 means no hint.
type step struct {
	Model           string `json:"model"`
	Status          int    `json:"status"`
	Message         string `json:"message"`
	RetryAfterMs    int64  `json:"retry_after_ms"`
	CredentialScope bool   `json:"credential_scope"`
}

type cooldownCase struct {
	Name  string `json:"name"`
	Steps []step `json:"steps"`
	// Seconds until each touched model's NextRetryAfter, rounded; 0 when none.
	Seconds map[string]int64 `json:"seconds"`
	// Quota marks a model state left quota-exceeded.
	Quota map[string]bool `json:"quota"`
}

func s(model string, status int, message string, retryAfterMs int64) step {
	return step{Model: model, Status: status, Message: message, RetryAfterMs: retryAfterMs}
}

var cooldownInputs = []cooldownCase{
	{Name: "429_no_hint", Steps: []step{s("m1", 429, "rate limited", -1)}},
	{Name: "429_twice_reuses_deadline", Steps: []step{s("m1", 429, "rate limited", -1), s("m1", 429, "rate limited", -1)}},
	{Name: "429_zero_hint_floor", Steps: []step{s("m1", 429, "rate limited", 0)}},
	{Name: "429_small_hint_floor", Steps: []step{s("m1", 429, "rate limited", 3000)}},
	{Name: "429_large_hint", Steps: []step{s("m1", 429, "rate limited", 30000)}},
	{Name: "429_hint_then_none", Steps: []step{s("m1", 429, "rate limited", 120000), s("m1", 429, "rate limited", -1)}},
	{Name: "401", Steps: []step{s("m1", 401, "unauthorized", -1)}},
	{Name: "402", Steps: []step{s("m1", 402, "payment required", -1)}},
	{Name: "403", Steps: []step{s("m1", 403, "forbidden", -1)}},
	{Name: "404", Steps: []step{s("m1", 404, "not found", -1)}},
	{Name: "404_hint", Steps: []step{s("m1", 404, "not found", 60000)}},
	{Name: "500", Steps: []step{s("m1", 500, "boom", -1)}},
	{Name: "500_hint", Steps: []step{s("m1", 500, "boom", 5000)}},
	{Name: "503", Steps: []step{s("m1", 503, "overloaded", -1)}},
	{Name: "418_default", Steps: []step{s("m1", 418, "teapot", -1)}},
	{Name: "400_request_fault", Steps: []step{s("m1", 400, "bad request", -1)}},
	{Name: "400_model_support", Steps: []step{s("m1", 400, "The requested model is not supported", -1)}},
	{Name: "404_model_support_hint", Steps: []step{s("m1", 404, "model_not_supported", 90000)}},
	{Name: "400_invalid_grant", Steps: []step{s("m1", 400, `{"error":"invalid_grant"}`, -1)}},
	{Name: "403_cloudflare", Steps: []step{s("m1", 403, "<html>Just a moment... cloudflare</html>", -1)}},
	{Name: "403_cloudflare_twice", Steps: []step{s("m1", 403, "challenge-platform", -1), s("m1", 403, "challenge-platform", -1)}},
	{Name: "500_then_429", Steps: []step{s("m1", 500, "boom", -1), s("m1", 429, "rate limited", -1)}},
	{Name: "401_then_429_short", Steps: []step{s("m1", 401, "unauthorized", -1), s("m1", 429, "rate limited", 5000)}},
	{Name: "credential_scope", Steps: []step{
		s("m2", 500, "boom", -1),
		{Model: "m1", Status: 429, Message: "credential quota", RetryAfterMs: 600000, CredentialScope: true},
	}},
	{Name: "credential_scope_short_keeps_sibling", Steps: []step{
		s("m2", 401, "unauthorized", -1),
		{Model: "m1", Status: 429, Message: "credential quota", RetryAfterMs: 20000, CredentialScope: true},
	}},
}

// cooldowns drives Manager.MarkResult on one fresh credential per case.
func cooldowns() []cooldownCase {
	ctx := context.Background()
	m := auth.NewManager(nil, nil, nil)
	out := make([]cooldownCase, len(cooldownInputs))
	for i, c := range cooldownInputs {
		id := fmt.Sprintf("golden-%d.json", i)
		if _, err := m.Register(ctx, &auth.Auth{ID: id, Provider: "claude", Metadata: map[string]any{"type": "claude"}}); err != nil {
			panic(err)
		}
		for _, st := range c.Steps {
			result := auth.Result{
				AuthID:          id,
				Provider:        "claude",
				Model:           st.Model,
				CredentialScope: st.CredentialScope,
				Error:           &auth.Error{HTTPStatus: st.Status, Message: st.Message},
			}
			if st.RetryAfterMs >= 0 {
				d := time.Duration(st.RetryAfterMs) * time.Millisecond
				result.RetryAfter = &d
			}
			m.MarkResult(ctx, result)
		}
		current, _ := m.GetByID(id)
		c.Seconds = map[string]int64{}
		c.Quota = map[string]bool{}
		for _, st := range c.Steps {
			state := current.ModelStates[st.Model]
			if state == nil || !state.NextRetryAfter.After(time.Now()) {
				c.Seconds[st.Model] = 0
			} else {
				c.Seconds[st.Model] = int64(time.Until(state.NextRetryAfter).Round(time.Second) / time.Second)
			}
			c.Quota[st.Model] = state != nil && state.Quota.Exceeded
		}
		out[i] = c
	}
	return out
}

func main() {
	out := map[string]any{}
	var sanitized, extracted []pair
	for _, in := range sanitizeInputs {
		sanitized = append(sanitized, pair{in, auth.SanitizeUpstreamErrorSummary(in)})
	}
	for _, in := range extractInputs {
		extracted = append(extracted, pair{in, auth.ExtractUpstreamErrorSummary(in)})
	}
	out["sanitize"] = sanitized
	out["extract"] = extracted
	out["availability"] = modelAvailability()
	out["cooldown"] = cooldowns()
	data, err := json.MarshalIndent(out, "", "  ")
	if err != nil {
		panic(err)
	}
	if err := os.WriteFile(os.Args[1], append(data, '\n'), 0o644); err != nil {
		panic(err)
	}
}
