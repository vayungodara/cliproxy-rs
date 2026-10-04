// Generates Codex goldens by running the pinned Go code against local mock upstreams.
// It never contacts OpenAI: auth.openai.com is rerouted to an in-process server and every
// executor case uses a loopback base URL. Credentials are fake.
package main

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"sort"
	"strings"
	"sync"
	"time"

	"github.com/gin-gonic/gin"
	codexauth "github.com/router-for-me/CLIProxyAPI/v8/internal/auth/codex"
	internalcache "github.com/router-for-me/CLIProxyAPI/v8/internal/cache"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/misc"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/registry"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/runtime/executor"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/runtime/executor/helps"
	sdkauth "github.com/router-for-me/CLIProxyAPI/v8/sdk/auth"
	cliproxyauth "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/auth"
	cliproxyexecutor "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/executor"
	sdktranslator "github.com/router-for-me/CLIProxyAPI/v8/sdk/translator"
	// Built-in translators register in init(), as cmd/server/main.go imports them.
	_ "github.com/router-for-me/CLIProxyAPI/v8/internal/translator"
	"github.com/tidwall/sjson"
)

type captured struct {
	Method  string              `json:"method"`
	Path    string              `json:"path"`
	Query   string              `json:"query,omitempty"`
	Headers map[string][]string `json:"headers"`
	Body    string              `json:"body"`
}

func capture(r *http.Request) captured {
	body, _ := io.ReadAll(r.Body)
	headers := map[string][]string{}
	for k, v := range r.Header {
		headers[strings.ToLower(k)] = v
	}
	return captured{Method: r.Method, Path: r.URL.Path, Query: r.URL.RawQuery, Headers: headers, Body: string(body)}
}

// ---------------------------------------------------------------- sjson

type sjsonCase struct {
	Op    string `json:"op"`
	JSON  string `json:"json"`
	Path  string `json:"path"`
	Value string `json:"value,omitempty"`
	Out   string `json:"out"`
}

func sjsonCases() []sjsonCase {
	type in struct{ op, json, path, value string }
	inputs := []in{
		{"delete", `{"a":1, "b":2}`, "b", ""},
		{"delete", `{"a":1, "b":2}`, "a", ""},
		{"delete", "{\n  \"a\": 1,\n  \"b\": 2\n}", "b", ""},
		{"delete", "{\n  \"a\": 1,\n  \"b\": 2\n}", "a", ""},
		{"delete", `{"only":true}`, "only", ""},
		{"delete", `{"a":1}`, "missing", ""},
		{"delete", `{"x":{"a":1,"b":2}}`, "x.a", ""},
		{"delete", `{"x":{"a":1}}`, "x.missing", ""},
		{"delete", `{"a" : 1 ,"b":[1,2] , "c":3}`, "b", ""},
		{"set_raw", `{"a": 1 , "b":2}`, "a", "true"},
		{"set_raw", `{"a":1}`, "b", "2"},
		{"set_raw", `{}`, "b", "2"},
		{"set_raw", "{\"a\":1}\n", "b", "2"},
		{"set_raw", "  {\"a\":1}  ", "b", "2"},
		{"set_raw", `{"a":1}`, "x.y", "2"},
		{"set_raw", `{"t":[1]}`, "t.-1", "2"},
		{"set_raw", `{"t":[]}`, "t.-1", "2"},
		{"set_raw", `{"t":[ 1 ]}`, "t.-1", "2"},
		{"set_raw", `{"t":[1]}`, "t.0", "9"},
		{"set_raw", `{"t":[1]}`, "t.3", "9"},
		{"set_raw", `{"a":1}`, "p.0.q", "2"},
		{"set_raw", `{"a.b":1}`, `a\.b`, "2"},
		{"set_raw", `{"r":{"o":[{"id":""},{"id":null}]}}`, "r.o.1.id", `"x"`},
		{"set_raw", ``, "a", "1"},
		{"set_raw", `"str"`, "a", "1"},
		{"set_str", `{"a":1}`, "m", "a<b"},
		{"set_str", `{"a":1}`, "m", "a<b\n"},
		{"set_str", `{"a":1}`, "m", "é\u2028"},
		{"set_str", `{"a":1}`, "m", "\u0001\b\f\"\\"},
		{"set_str", `{"model":"old"}`, "model", "gpt-5"},
	}
	out := make([]sjsonCase, 0, len(inputs))
	for _, c := range inputs {
		var res []byte
		switch c.op {
		case "delete":
			res, _ = sjson.DeleteBytes([]byte(c.json), c.path)
		case "set_raw":
			res, _ = sjson.SetRawBytes([]byte(c.json), c.path, []byte(c.value))
		case "set_str":
			res, _ = sjson.SetBytes([]byte(c.json), c.path, c.value)
		}
		out = append(out, sjsonCase{Op: c.op, JSON: c.json, Path: c.path, Value: c.value, Out: string(res)})
	}
	return out
}

// ---------------------------------------------------------------- OAuth

// rerouter sends auth.openai.com traffic to the local mock and leaves loopback alone.
type rerouter struct {
	base   http.RoundTripper
	target string
}

func (r *rerouter) RoundTrip(req *http.Request) (*http.Response, error) {
	if req.URL.Host == "auth.openai.com" {
		clone := req.Clone(req.Context())
		clone.URL.Scheme = "http"
		clone.URL.Host = r.target
		clone.Host = r.target
		return r.base.RoundTrip(clone)
	}
	return r.base.RoundTrip(req)
}

// fakeJWT builds an unsigned JWT with the given claims.
func fakeJWT(claims string) string {
	enc := func(s string) string {
		return strings.TrimRight(base64URL([]byte(s)), "=")
	}
	return enc(`{"alg":"none"}`) + "." + enc(claims) + ".sig"
}

func base64URL(b []byte) string {
	const table = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"
	var out strings.Builder
	for i := 0; i < len(b); i += 3 {
		var n uint32
		chunk := b[i:min(i+3, len(b))]
		for j := 0; j < 3; j++ {
			n <<= 8
			if j < len(chunk) {
				n |= uint32(chunk[j])
			}
		}
		for j := 0; j < len(chunk)+1; j++ {
			out.WriteByte(table[(n>>(18-6*j))&63])
		}
	}
	return out.String()
}

const idClaims = `{"email":"user@example.invalid","aud":["app_EMoamEEZ73f0CkXaXp7hrann"],"https://api.openai.com/auth":{"chatgpt_account_id":"acct-FAKE-1","chatgpt_plan_type":"Pro Plus","organizations":[{"id":"org-1","is_default":true}]}}`

type jwtCase struct {
	Token     string `json:"token"`
	Ok        bool   `json:"ok"`
	Email     string `json:"email"`
	AccountID string `json:"account_id"`
	PlanType  string `json:"plan_type"`
}

type oauthExchange struct {
	Name     string     `json:"name"`
	Requests []captured `json:"requests"`
	Result   any        `json:"result,omitempty"`
	Error    string     `json:"error,omitempty"`
}

func oauthCases() map[string]any {
	var mu sync.Mutex
	var requests []captured
	script := map[string][]func(w http.ResponseWriter){}
	mock := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		mu.Lock()
		requests = append(requests, capture(r))
		queue := script[r.URL.Path]
		var next func(w http.ResponseWriter)
		if len(queue) > 0 {
			next, script[r.URL.Path] = queue[0], queue[1:]
		}
		mu.Unlock()
		if next == nil {
			w.WriteHeader(500)
			return
		}
		next(w)
	}))
	defer mock.Close()
	original := http.DefaultTransport
	http.DefaultTransport = &rerouter{base: original, target: strings.TrimPrefix(mock.URL, "http://")}
	defer func() { http.DefaultTransport = original }()

	reply := func(status int, body string) func(w http.ResponseWriter) {
		return func(w http.ResponseWriter) {
			w.Header().Set("Content-Type", "application/json")
			w.WriteHeader(status)
			_, _ = w.Write([]byte(body))
		}
	}
	idToken := fakeJWT(idClaims)
	tokenBody := fmt.Sprintf(`{"access_token":"at-FAKE","refresh_token":"rt-FAKE-2","id_token":%q,"token_type":"Bearer","expires_in":3600}`, idToken)
	reset := func(s map[string][]func(w http.ResponseWriter)) {
		mu.Lock()
		requests = nil
		script = s
		mu.Unlock()
	}
	take := func() []captured {
		mu.Lock()
		defer mu.Unlock()
		out := requests
		requests = nil
		return out
	}
	svc := codexauth.NewCodexAuth(&config.Config{})
	var exchanges []oauthExchange
	normalize := func(td *codexauth.CodexTokenData) map[string]string {
		if td == nil {
			return nil
		}
		return map[string]string{"id_token": td.IDToken, "access_token": td.AccessToken, "refresh_token": td.RefreshToken,
			"account_id": td.AccountID, "email": td.Email, "plan_type": td.PlanType}
	}

	reset(map[string][]func(w http.ResponseWriter){"/oauth/token": {reply(200, tokenBody)}})
	bundle, err := svc.ExchangeCodeForTokens(context.Background(), "code-FAKE", &codexauth.PKCECodes{CodeVerifier: "verifier-FAKE", CodeChallenge: "challenge"})
	ex := oauthExchange{Name: "exchange", Requests: take()}
	if err != nil {
		ex.Error = err.Error()
	} else {
		ex.Result = normalize(&bundle.TokenData)
	}
	exchanges = append(exchanges, ex)

	reset(map[string][]func(w http.ResponseWriter){"/oauth/token": {reply(200, tokenBody)}})
	td, err := svc.RefreshTokensWithRetry(context.Background(), "rt-FAKE-1", 3)
	ex = oauthExchange{Name: "refresh", Requests: take(), Result: normalize(td)}
	if err != nil {
		ex.Error = err.Error()
	}
	exchanges = append(exchanges, ex)

	reset(map[string][]func(w http.ResponseWriter){"/oauth/token": {reply(500, `{"error":"server_error"}`), reply(200, tokenBody)}})
	td, err = svc.RefreshTokensWithRetry(context.Background(), "rt-FAKE-1", 3)
	ex = oauthExchange{Name: "refresh_retry", Requests: take(), Result: normalize(td)}
	if err != nil {
		ex.Error = err.Error()
	}
	exchanges = append(exchanges, ex)

	reset(map[string][]func(w http.ResponseWriter){"/oauth/token": {reply(400, `{"error":"refresh_token_reused"}`), reply(200, tokenBody)}})
	td, err = svc.RefreshTokensWithRetry(context.Background(), "rt-FAKE-1", 3)
	ex = oauthExchange{Name: "refresh_reused", Requests: take(), Result: normalize(td)}
	if err != nil {
		ex.Error = err.Error()
	}
	exchanges = append(exchanges, ex)

	reset(map[string][]func(w http.ResponseWriter){"/oauth/token": {reply(400, `{"error":"invalid_grant"}`), reply(400, `{"error":"invalid_grant"}`), reply(400, `{"error":"invalid_grant"}`)}})
	td, err = svc.RefreshTokensWithRetry(context.Background(), "rt-FAKE-1", 3)
	ex = oauthExchange{Name: "refresh_invalid_grant", Requests: take(), Result: normalize(td)}
	if err != nil {
		ex.Error = err.Error()
	}
	exchanges = append(exchanges, ex)

	// Device flow through the public authenticator, with browser opening disabled.
	reset(map[string][]func(w http.ResponseWriter){
		"/api/accounts/deviceauth/usercode": {reply(200, `{"device_auth_id":"dev-FAKE","usercode":"ABCD-EFGH","interval":"1"}`)},
		"/api/accounts/deviceauth/token":    {reply(403, `{}`), reply(200, `{"authorization_code":"devcode-FAKE","code_verifier":"devverifier-FAKE","code_challenge":"devchallenge"}`)},
		"/oauth/token":                      {reply(200, tokenBody)},
	})
	auth, err := sdkauth.NewCodexAuthenticator().Login(context.Background(), &config.Config{}, &sdkauth.LoginOptions{NoBrowser: true, Metadata: map[string]string{"codex_login_mode": "device"}})
	ex = oauthExchange{Name: "device", Requests: take()}
	if err != nil {
		ex.Error = err.Error()
	} else {
		ex.Result = map[string]any{"id": auth.ID, "metadata": auth.Metadata, "attributes": auth.Attributes}
	}
	exchanges = append(exchanges, ex)

	// Refresh as the executor applies it to credential metadata.
	reset(map[string][]func(w http.ResponseWriter){"/oauth/token": {reply(200, tokenBody)}})
	refreshed, err := executor.NewCodexExecutor(&config.Config{}).Refresh(context.Background(), &cliproxyauth.Auth{
		ID: "codex-a.json", Provider: "codex",
		Metadata: map[string]any{"type": "codex", "refresh_token": "rt-FAKE-1", "access_token": "old", "email": "old@example.invalid", "custom": "kept"},
	})
	ex = oauthExchange{Name: "executor_refresh", Requests: take()}
	if err != nil {
		ex.Error = err.Error()
	} else {
		meta := map[string]any{}
		for k, v := range refreshed.Metadata {
			if k == "expired" || k == "last_refresh" {
				v = "<time>"
			}
			meta[k] = v
		}
		ex.Result = meta
	}
	exchanges = append(exchanges, ex)

	// Auth file bytes for a fixed bundle, through the real storage serializer.
	dir, _ := os.MkdirTemp("", "codex-fixture")
	defer os.RemoveAll(dir)
	storage := &codexauth.CodexTokenStorage{IDToken: idToken, AccessToken: "at-<FAKE>&", RefreshToken: "rt-FAKE-2", AccountID: "acct-FAKE-1",
		LastRefresh: "2026-10-02T12:00:00Z", Email: "user@example.invalid", Expire: "2026-10-12T12:00:00Z", PlanType: "Pro Plus"}
	storage.SetMetadata(map[string]any{"email": "user@example.invalid", "plan_type": "Pro Plus", "disabled": false})
	path := dir + "/file.json"
	_ = storage.SaveTokenToFile(path)
	saved, _ := os.ReadFile(path)

	names := [][4]string{
		{"user@example.invalid", "Pro Plus", "1a2b3c4d"},
		{"user@example.invalid", "", "1a2b3c4d"},
		{" user@example.invalid ", "team_ENTERPRISE", ""},
		{"user@example.invalid", "", ""},
		{"user@example.invalid", "--", ""},
	}
	var fileNames []map[string]string
	for _, n := range names {
		fileNames = append(fileNames, map[string]string{"email": n[0], "plan": n[1], "hash": n[2],
			"out": codexauth.CredentialFileName(n[0], n[1], n[2], true)})
	}

	var jwts []jwtCase
	for _, claims := range []string{
		idClaims,
		`{"email":"a@b","https://api.openai.com/auth":{"chatgpt_plan_type":"  "}}`,
		`{"email":"a@b","aud":"single-string-audience"}`,
		`{"email":"a@b","auth_time":1.5}`,
		`{"email":"a@b","https://api.openai.com/auth":{"chatgpt_subscription_last_checked":"2026-01-01T00:00:00Z","chatgpt_account_id":"x"}}`,
		`{"email":"a@b","https://api.openai.com/auth":{"chatgpt_subscription_last_checked":"yesterday"}}`,
		`{"email":null,"https://api.openai.com/auth":null}`,
	} {
		token := fakeJWT(claims)
		c, err := codexauth.ParseJWTToken(token)
		jc := jwtCase{Token: token, Ok: err == nil}
		if c != nil {
			jc.Email, jc.AccountID, jc.PlanType = c.Email, c.GetAccountID(), c.GetPlanType()
		}
		jwts = append(jwts, jc)
	}

	var callbacks []map[string]any
	for _, input := range []string{
		"http://localhost:1455/auth/callback?code=abc&state=xyz",
		"code=abc#xyz",
		"?code=abc%23xyz",
		"?error_description=nope",
		"localhost:1455/auth/callback#code=f&state=g",
		"nonsense",
		"?state=only",
		"  ",
	} {
		parsed, errParse := misc.ParseOAuthCallback(input)
		entry := map[string]any{"input": input, "ok": errParse == nil && parsed != nil}
		if parsed != nil {
			entry["code"], entry["state"], entry["error"] = parsed.Code, parsed.State, parsed.Error
		}
		callbacks = append(callbacks, entry)
	}

	authURL, _ := svc.GenerateAuthURL("state-FAKE", &codexauth.PKCECodes{CodeChallenge: "challenge-FAKE"})
	return map[string]any{
		"exchanges":  exchanges,
		"auth_file":  string(saved),
		"id_token":   idToken,
		"file_names": fileNames,
		"jwt":        jwts,
		"auth_url":   authURL,
		"callbacks":  callbacks,
	}
}

// ---------------------------------------------------------------- executor

type execCase struct {
	Name           string            `json:"name"`
	Config         string            `json:"config"`
	Attributes     map[string]string `json:"attributes"`
	Metadata       map[string]any    `json:"metadata"`
	Source         string            `json:"source"`
	Response       string            `json:"response,omitempty"`
	Headers        map[string]string `json:"headers"`
	Model          string            `json:"model"`
	Payload        string            `json:"payload"`
	Stream         bool              `json:"stream"`
	Alt            string            `json:"alt,omitempty"`
	ExecMetadata   map[string]any    `json:"exec_metadata,omitempty"`
	UpstreamStatus int               `json:"upstream_status"`
	UpstreamType   string            `json:"upstream_type"`
	UpstreamBody   string            `json:"upstream_body"`
	// Redirect answers the first request with this status and Location /moved<path>;
	// the reply above is served there.
	Redirect int `json:"redirect,omitempty"`
	// ResolvedCompat binds an is-compat API-key model to the attempt, as Go's conductor does.
	ResolvedCompat bool `json:"resolved_compat,omitempty"`
	// Filled by the generator.
	Upstream *captured  `json:"upstream,omitempty"`
	Hops     []captured `json:"hops,omitempty"`
	Output   any        `json:"output"`
}

const sseOK = "event: response.created\n" +
	"data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"status\":\"in_progress\"}}\n\n" +
	"event: response.output_item.done\n" +
	"data:{\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"Hi\"}]}}\n\n" +
	"event: response.completed\n" +
	"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}}\n\n"

func executorCases() []execCase {
	oauthMeta := map[string]any{"type": "codex", "access_token": "at-FAKE", "account_id": "acct-FAKE-1", "email": "user@example.invalid"}
	codexHeaders := map[string]string{
		"User-Agent": "codex_cli_rs/0.150.0 (Linux; x86_64)", "Originator": "codex_cli_rs", "Version": "0.150.0",
		"Session_id": "sess-FAKE", "X-Codex-Turn-Metadata": `{"turn":1}`, "X-Client-Request-Id": "req-FAKE",
		"Authorization": "Bearer client-key-FAKE", "X-Codex-Beta-Features": "beta-a",
	}
	grokKeepalive := "event: keepalive\ndata: {\"type\":\"keepalive\"}\n\n" +
		"data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n\n" +
		": ping\ndata:  {\"type\":\"keepalive\",\"n\":1} \n\n" +
		"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"output\":[]}}\n\n"
	claudeThinking := `{"model":"claude-x","max_tokens":64,"messages":[{"role":"user","content":"hi"},{"role":"assistant","content":[{"type":"thinking","thinking":"plan","signature":""},{"type":"text","text":"ok"}]},{"role":"user","content":"go"}],"thinking":{"type":"enabled","budget_tokens":2048}}`
	native := `{"model":"gpt-5.4","instructions":"be brief","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}],"tools":[{"type":"function","name":"shell","parameters":{"type":"object","properties":{}}}],"tool_choice":"auto","parallel_tool_calls":true,"reasoning":{"effort":"high","summary":"auto"},"store":false,"stream":true,"include":["reasoning.encrypted_content"],"prompt_cache_key":"pck-FAKE","previous_response_id":"resp_0","safety_identifier":"sid","prompt_cache_retention":"24h","stream_options":{"include_obfuscation":false,"reasoning_summary_delivery":"inline"},"service_tier":"priority"}`
	return []execCase{
		{Name: "oauth_stream_native", Attributes: map[string]string{}, Metadata: oauthMeta, Source: "codex", Headers: codexHeaders,
			Model: "gpt-5.4", Payload: native, Stream: true, UpstreamStatus: 200, UpstreamType: "text/event-stream", UpstreamBody: sseOK},
		{Name: "oauth_nonstream_no_instructions_free_plan", Attributes: map[string]string{"plan_type": "free"}, Metadata: oauthMeta, Source: "codex",
			Headers: map[string]string{}, Model: "gpt-5.4",
			Payload:        `{"model":"gpt-5.4","input":[{"type":"message","role":"user","content":"hi"}],"parallel_tool_calls":true}`,
			UpstreamStatus: 200, UpstreamType: "text/event-stream", UpstreamBody: sseOK},
		{Name: "apikey_stream_cloak_disabled_headers", Config: "codex:\n  disable-codex-cloaking: true\n",
			Attributes: map[string]string{"api_key": "sk-FAKE", "header:X-Custom": "fixed", "header:X-From-Client": "$X-Client-Thing"},
			Metadata:   map[string]any{}, Source: "codex",
			Headers: map[string]string{"User-Agent": "my-client/1.0", "X-Client-Thing": "echoed"}, Model: "gpt-5.4-mini",
			Payload: `{"model":"alias-model","instructions":null,"input":[],"tools":[{"type":"image_generation"}],"stream_options":{"include_obfuscation":true}}`,
			Stream:  true, UpstreamStatus: 200, UpstreamType: "text/event-stream", UpstreamBody: sseOK},
		{Name: "oauth_usage_limit_429", Attributes: map[string]string{}, Metadata: oauthMeta, Source: "codex", Headers: map[string]string{},
			Model: "gpt-5.4", Payload: `{"model":"gpt-5.4","input":[]}`, Stream: true, UpstreamStatus: 429, UpstreamType: "application/json",
			UpstreamBody: `{"error":{"type":"usage_limit_reached","message":"The usage limit has been reached","resets_in_seconds":120}}`},
		{Name: "oauth_context_too_large_400", Attributes: map[string]string{}, Metadata: oauthMeta, Source: "codex", Headers: map[string]string{},
			Model: "gpt-5.4", Payload: `{"model":"gpt-5.4","input":[]}`, UpstreamStatus: 400, UpstreamType: "application/json",
			UpstreamBody: `{"error":{"message":"Your input exceeds the context window of this model.","type":"invalid_request_error"}}`},
		{Name: "oauth_stream_terminal_failed_in_stream", Attributes: map[string]string{}, Metadata: oauthMeta, Source: "codex", Headers: map[string]string{},
			Model: "gpt-5.4", Payload: `{"model":"gpt-5.4","input":[]}`, Stream: true, UpstreamStatus: 200, UpstreamType: "text/event-stream",
			UpstreamBody: "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n\ndata: {\"type\":\"response.failed\",\"sequence_number\":4,\"response\":{\"error\":{\"code\":\"rate_limit_exceeded\",\"message\":\"slow down\"}}}\n\n"},
		{Name: "oauth_stream_truncated", Attributes: map[string]string{}, Metadata: oauthMeta, Source: "codex", Headers: map[string]string{},
			Model: "gpt-5.4", Payload: `{"model":"gpt-5.4","input":[]}`, Stream: true, UpstreamStatus: 200, UpstreamType: "text/event-stream",
			UpstreamBody: "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"x\"}\n\n"},
		{Name: "oauth_nonstream_empty_incomplete", Attributes: map[string]string{}, Metadata: oauthMeta, Source: "codex", Headers: map[string]string{},
			Model: "gpt-5.4", Payload: `{"model":"gpt-5.4","input":[]}`, UpstreamStatus: 200, UpstreamType: "text/event-stream",
			UpstreamBody: "data: {\"type\":\"response.incomplete\",\"response\":{\"output\":[],\"usage\":{\"output_tokens\":0}}}\n\n"},
		{Name: "oauth_compact", Attributes: map[string]string{}, Metadata: oauthMeta, Source: "codex", Headers: map[string]string{"Session_id": "s-1"},
			Model: "gpt-5.4", Alt: "responses/compact", Payload: `{"model":"gpt-5.4","input":[{"type":"message","role":"user","content":"x"}],"stream":true,"instructions":"keep"}`,
			UpstreamStatus: 200, UpstreamType: "application/json", UpstreamBody: `{"object":"response.compaction","output":[{"type":"compaction","encrypted_content":"e"}]}`},
		{Name: "oauth_execution_session_prompt_cache", Attributes: map[string]string{}, Metadata: oauthMeta, Source: "codex", Headers: map[string]string{},
			Model: "gpt-5.4", Payload: `{"model":"gpt-5.4","input":[{"type":"function_call","id":"call_x","call_id":"c1","name":"f","arguments":"{}"},{"type":"reasoning","id":"rs_` + strings.Repeat("y", 70) + `","encrypted_content":"enc"},{"type":"message","id":"item_` + strings.Repeat("z", 80) + `","role":"user","content":"x"}]}`,
			Stream: true, ExecMetadata: map[string]any{"execution_session_id": "ws-session-1"},
			UpstreamStatus: 200, UpstreamType: "text/event-stream", UpstreamBody: sseOK},
		{Name: "oauth_tool_schema_enum_collapse", Attributes: map[string]string{}, Metadata: oauthMeta, Source: "codex", Headers: map[string]string{},
			Model: "gpt-5.4", Payload: `{"model":"gpt-5.4","input":[],"tools":[{"type":"function","name":"pick","parameters":{"type":"object","properties":{"c":{"oneOf":[{"const":"a"},{"const":"b"},{"const":"c"},{"const":"d"},{"const":"e"},{"const":"f"},{"const":"g"},{"const":"h","title":"H"}]},"n":{"anyOf":[{"const":1},{"const":2}]}}}},{"type":"namespace","name":"ns","tools":[{"type":"function","name":"inner","parameters":{"type":"object","properties":{"k":{"anyOf":[{"const":1},{"const":2},{"const":3},{"const":4},{"const":5},{"const":6},{"const":7},{"const":8.0}],"enum":[1,2,3,4,5,6,7,8]}}}}]}]}`,
			Stream: true, UpstreamStatus: 200, UpstreamType: "text/event-stream", UpstreamBody: sseOK},
		{Name: "oauth_model_override_header", Attributes: map[string]string{}, Metadata: oauthMeta, Source: "codex",
			Headers: map[string]string{"User-Agent": "other/1"}, Model: "gpt-5.6-luna",
			Payload: `{"model":"gpt-5.6-luna","input":[]}`, Stream: true, UpstreamStatus: 200, UpstreamType: "text/event-stream", UpstreamBody: sseOK},
		{Name: "oauth_capacity_in_stream_500", Attributes: map[string]string{}, Metadata: oauthMeta, Source: "codex", Headers: map[string]string{},
			Model: "gpt-5.4", Payload: `{"model":"gpt-5.4","input":[]}`, UpstreamStatus: 500, UpstreamType: "application/json",
			UpstreamBody: `{"error":{"message":"Selected model is at capacity. Please try a different model."}}`},
		{Name: "apikey_payload_rules_and_session_headers",
			Config:     "payload:\n  override:\n    - models:\n        - name: gpt-5.4\n      params:\n        reasoning.effort: low\n  default:\n    - models:\n        - name: \"gpt-*\"\n      params:\n        text.verbosity: low\n        reasoning.effort: minimal\n  filter:\n    - models:\n        - name: gpt-5.4\n      params:\n        - tool_choice\n",
			Attributes: map[string]string{"api_key": "sk-FAKE", "header:X-Session": "$CPA-SESSION-ID", "header:X-Combo": "s=$cpa-session-id;x"},
			Source:     "codex", Headers: map[string]string{"Session_id": "sess-FAKE", "User-Agent": "codex_cli_rs/0.150.0"},
			Model: "gpt-5.4", Payload: native, Stream: true, UpstreamStatus: 200, UpstreamType: "text/event-stream", UpstreamBody: sseOK},
		{Name: "apikey_session_header_without_session",
			Attributes: map[string]string{"api_key": "sk-FAKE", "header:X-Session": "$CPA-SESSION-ID", "header:X-Fixed": "v"},
			Source:     "codex", Headers: map[string]string{},
			Model: "gpt-5.4", Payload: `{"model":"gpt-5.4","input":[]}`, Stream: true, UpstreamStatus: 200, UpstreamType: "text/event-stream", UpstreamBody: sseOK},
		{Name: "oauth_bootstrap_overload_failover", Config: "codex:\n  stream-bootstrap-buffering: true\n", Attributes: map[string]string{}, Metadata: oauthMeta,
			Source: "codex", Headers: map[string]string{}, Model: "gpt-5.4", Payload: `{"model":"gpt-5.4","input":[]}`, Stream: true,
			UpstreamStatus: 200, UpstreamType: "text/event-stream",
			UpstreamBody: ": keepalive\n\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n\ndata: {\"type\":\"codex.rate_limits\",\"rate_limits\":{}}\n\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"type\":\"service_unavailable_error\",\"code\":\"server_is_overloaded\",\"message\":\"busy\"}}}\n\n"},
		{Name: "oauth_bootstrap_time_budget_spent", Config: "codex:\n  stream-bootstrap-buffering: true\n  stream-bootstrap-timeout: 1ns\n", Attributes: map[string]string{}, Metadata: oauthMeta,
			Source: "codex", Headers: map[string]string{}, Model: "gpt-5.4", Payload: `{"model":"gpt-5.4","input":[]}`, Stream: true,
			UpstreamStatus: 200, UpstreamType: "text/event-stream",
			UpstreamBody: "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"type\":\"service_unavailable_error\",\"code\":\"server_is_overloaded\",\"message\":\"busy\"}}}\n\n"},
		{Name: "oauth_stream_redirect_307", Attributes: map[string]string{}, Metadata: oauthMeta, Source: "codex", Headers: map[string]string{},
			Model: "gpt-5.4", Payload: `{"model":"gpt-5.4","input":[]}`, Stream: true, Redirect: 307,
			UpstreamStatus: 200, UpstreamType: "text/event-stream", UpstreamBody: sseOK},
		{Name: "apikey_nonstream_redirect_302", Attributes: map[string]string{"api_key": "sk-FAKE"}, Metadata: map[string]any{}, Source: "codex",
			Headers: map[string]string{}, Model: "gpt-5.4", Payload: `{"model":"gpt-5.4","input":[]}`, Redirect: 302,
			UpstreamStatus: 200, UpstreamType: "text/event-stream", UpstreamBody: sseOK},
		{Name: "apikey_claude_compat_from_config",
			Config:     "codex-api-key:\n  - api-key: sk-FAKE\n    base-url: \"{{base_url}}\"\n    models:\n      - name: gpt-5.4\n        is-compat: true\n",
			Attributes: map[string]string{"api_key": "sk-FAKE"}, Metadata: map[string]any{}, Source: "claude",
			Headers: map[string]string{}, Model: "gpt-5.4", Payload: claudeThinking, Stream: true,
			UpstreamStatus: 200, UpstreamType: "text/event-stream", UpstreamBody: sseOK},
		{Name: "apikey_claude_not_compat", Attributes: map[string]string{"api_key": "sk-FAKE"}, Metadata: map[string]any{}, Source: "claude",
			Headers: map[string]string{}, Model: "gpt-5.4", Payload: claudeThinking, Stream: true,
			UpstreamStatus: 200, UpstreamType: "text/event-stream", UpstreamBody: sseOK},
		{Name: "apikey_responses_compat_resolved", Attributes: map[string]string{"api_key": "sk-FAKE"}, Metadata: map[string]any{}, Source: "openai-response",
			Headers: map[string]string{}, Model: "gpt-5.4", ResolvedCompat: true, Stream: true,
			Payload:        `{"model":"gpt-5.4","input":[{"type":"reasoning","id":"rs_1","summary":[],"content":[{"type":"reasoning_text","text":"plan"}]},{"type":"reasoning","id":"rs_2","encrypted_content":" bad "},{"type":"message","role":"user","content":"go"}],"reasoning":{"effort":"high"}}`,
			UpstreamStatus: 200, UpstreamType: "text/event-stream", UpstreamBody: sseOK},
		{Name: "oauth_claude_nonstream_empty_translation", Attributes: map[string]string{}, Metadata: oauthMeta, Source: "claude", Headers: map[string]string{},
			Model: "gpt-5.4", Payload: `{"model":"claude-x","max_tokens":64,"messages":[{"role":"user","content":"hi"}]}`,
			UpstreamStatus: 200, UpstreamType: "text/event-stream", UpstreamBody: "data: {\"type\":\"response.completed\"}\n\n"},
		{Name: "oauth_grok_client_keepalive", Attributes: map[string]string{}, Metadata: oauthMeta, Source: "codex",
			Headers: map[string]string{"User-Agent": "Grok-Shell/1.2"}, Model: "gpt-5.4", Payload: `{"model":"gpt-5.4","input":[]}`, Stream: true,
			UpstreamStatus: 200, UpstreamType: "text/event-stream", UpstreamBody: grokKeepalive},
		{Name: "oauth_non_grok_keepalive", Attributes: map[string]string{}, Metadata: oauthMeta, Source: "codex",
			Headers: map[string]string{"User-Agent": "codex_cli_rs/0.150.0"}, Model: "gpt-5.4", Payload: `{"model":"gpt-5.4","input":[]}`, Stream: true,
			UpstreamStatus: 200, UpstreamType: "text/event-stream", UpstreamBody: grokKeepalive},
		{Name: "oauth_grok_keepalive_bootstrap", Config: "codex:\n  stream-bootstrap-buffering: true\n", Attributes: map[string]string{}, Metadata: oauthMeta, Source: "codex",
			Headers: map[string]string{"User-Agent": "grok-pager"}, Model: "gpt-5.4", Payload: `{"model":"gpt-5.4","input":[]}`, Stream: true,
			UpstreamStatus: 200, UpstreamType: "text/event-stream", UpstreamBody: grokKeepalive},
		{Name: "oauth_bootstrap_holds_then_releases", Config: "codex:\n  stream-bootstrap-buffering: true\n", Attributes: map[string]string{}, Metadata: oauthMeta,
			Source: "codex", Headers: map[string]string{}, Model: "gpt-5.4", Payload: `{"model":"gpt-5.4","input":[]}`, Stream: true,
			UpstreamStatus: 200, UpstreamType: "text/event-stream",
			UpstreamBody: "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"content\":[]}}\n\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"Hi\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"output\":[]}}\n\n"},
	}
}

func runExecutor(c *execCase) {
	var got captured
	var mu sync.Mutex
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		hop := capture(r)
		mu.Lock()
		got = hop
		if c.Redirect != 0 {
			c.Hops = append(c.Hops, hop)
		}
		mu.Unlock()
		if c.Redirect != 0 && !strings.HasPrefix(r.URL.Path, "/moved") {
			w.Header().Set("Location", "/moved"+r.URL.Path)
			w.WriteHeader(c.Redirect)
			return
		}
		w.Header().Set("Content-Type", c.UpstreamType)
		w.Header().Set("X-Codex-Primary-Used-Percent", "42")
		w.WriteHeader(c.UpstreamStatus)
		_, _ = w.Write([]byte(c.UpstreamBody))
	}))
	defer server.Close()
	if strings.TrimSpace(c.Config) == "" {
		c.Config = "{}\n"
	}
	cfg, err := config.ParseConfigBytes([]byte(strings.ReplaceAll(c.Config, "{{base_url}}", server.URL)))
	if err != nil {
		panic(err)
	}
	attrs := map[string]string{"base_url": server.URL}
	for k, v := range c.Attributes {
		attrs[k] = v
	}
	auth := &cliproxyauth.Auth{ID: "codex-fixture.json", Provider: "codex", Attributes: attrs, Metadata: c.Metadata}
	headers := http.Header{}
	for k, v := range c.Headers {
		headers.Set(k, v)
	}
	opts := cliproxyexecutor.Options{
		Stream: c.Stream, Alt: c.Alt, Headers: headers, OriginalRequest: []byte(c.Payload),
		SourceFormat: sdktranslator.FromString(c.Source), Metadata: c.ExecMetadata,
	}
	if c.Response != "" {
		opts.ResponseFormat = sdktranslator.FromString(c.Response)
	}
	req := cliproxyexecutor.Request{Model: c.Model, Payload: []byte(c.Payload), Metadata: c.ExecMetadata}
	if c.ResolvedCompat {
		req.Metadata = map[string]any{"cliproxy.resolved_api_key_model_info": &registry.ModelInfo{ID: c.Model, IsCompat: true}}
	}
	exec := executor.NewCodexExecutor(cfg)
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	result := map[string]any{}
	if c.Stream {
		res, errStream := exec.ExecuteStream(ctx, auth, req, opts)
		if errStream != nil {
			result["error"] = describe(errStream)
		} else {
			var chunks []string
			for chunk := range res.Chunks {
				if chunk.Err != nil {
					result["stream_error"] = describe(chunk.Err)
					break
				}
				chunks = append(chunks, string(chunk.Payload))
			}
			result["chunks"] = chunks
		}
	} else {
		res, errExec := exec.Execute(ctx, auth, req, opts)
		if errExec != nil {
			result["error"] = describe(errExec)
		} else {
			result["payload"] = string(res.Payload)
		}
	}
	mu.Lock()
	if got.Method != "" {
		g := got
		c.Upstream = &g
	}
	mu.Unlock()
	c.Output = result
}

func describe(err error) map[string]any {
	out := map[string]any{"message": err.Error()}
	var sc interface{ StatusCode() int }
	if errors.As(err, &sc) {
		out["status"] = sc.StatusCode()
	}
	var ra interface{ RetryAfter() *time.Duration }
	if errors.As(err, &ra) && ra.RetryAfter() != nil {
		out["retry_after_s"] = ra.RetryAfter().Seconds()
	}
	var cs interface{ IsCredentialScoped() bool }
	if errors.As(err, &cs) {
		out["credential_scoped"] = cs.IsCredentialScoped()
	}
	var rs interface{ IsRequestScoped() bool }
	if errors.As(err, &rs) {
		out["request_scoped"] = rs.IsRequestScoped()
	}
	return out
}

// sanitizeCodexAlphaSearchBody and rewriteCodexAlphaSearchModel are unexported in
// internal/api (server_routes.go); these are verbatim copies so the fixture records what
// encoding/json does with them.
func sanitizeCodexAlphaSearchBody(body []byte) []byte {
	var payload map[string]json.RawMessage
	if errUnmarshal := json.Unmarshal(body, &payload); errUnmarshal != nil || payload == nil {
		return body
	}
	removed := false
	for _, field := range []string{"prompt_cache_key", "prompt_cache_retention"} {
		if _, exists := payload[field]; exists {
			delete(payload, field)
			removed = true
		}
	}
	if !removed {
		return body
	}
	sanitizedBody, errMarshal := json.Marshal(payload)
	if errMarshal != nil {
		return body
	}
	return sanitizedBody
}

func rewriteCodexAlphaSearchModel(body []byte, upstreamModel string) []byte {
	upstreamModel = strings.TrimSpace(upstreamModel)
	if upstreamModel == "" {
		return body
	}
	var payload map[string]json.RawMessage
	if errUnmarshal := json.Unmarshal(body, &payload); errUnmarshal != nil || payload == nil {
		return body
	}
	if _, exists := payload["model"]; !exists {
		return body
	}
	modelJSON, errMarshalModel := json.Marshal(upstreamModel)
	if errMarshalModel != nil {
		return body
	}
	if string(payload["model"]) == string(modelJSON) {
		return body
	}
	payload["model"] = modelJSON
	rewrittenBody, errMarshal := json.Marshal(payload)
	if errMarshal != nil {
		return body
	}
	return rewrittenBody
}

func alphaCases() []map[string]string {
	var out []map[string]string
	for _, body := range []string{
		`{"model":"m","prompt_cache_key":"k","q":"a<b", "n": [1, 2]}`,
		`{"q": 1, "model":"m"}`,
		`{"z":1,"model":"alias","prompt_cache_retention":"24h","s":"\u00e9 & \u2028 <x>","o":{"b" : true , "a":null}}`,
		`{"model":"m","prompt_cache_key":"k","model":"dup"}`,
		`[1,2]`,
		`{"model":"m","prompt_cache_key":"k"} trailing`,
	} {
		out = append(out, map[string]string{
			"input":     body,
			"sanitized": string(sanitizeCodexAlphaSearchBody([]byte(body))),
			"rewritten": string(rewriteCodexAlphaSearchModel(sanitizeCodexAlphaSearchBody([]byte(body)), "real-model")),
		})
	}
	return out
}

func quotaCases() map[string]any {
	var events []map[string]any
	for _, payload := range []string{
		`{"type":"codex.rate_limits","plan_type":"pro","metered_limit_name":"codex","rate_limits":{"allowed":true,"limit_reached":false,"primary":{"used_percent":12.5,"window_minutes":300,"reset_after_seconds":100,"reset_at":1760000000},"secondary":{"used_percent":101,"window_minutes":10080,"reset_at":1}},"credits":{"has_credits":true,"unlimited":false,"balance":"9.5"},"code_review_rate_limits":{"allowed":false}}`,
		`{"type":"codex.rate_limits","additional_rate_limits":{"GPT 5 / mini!":{"rate_limit":{"primary":{"used_percent":1,"window_minutes":60,"reset_after_seconds":0}}},"bad name":{"allowed":true},"x9":{}}}`,
		`{"type":"codex.rate_limits","additional_rate_limits":[{"limit_name":"a.b","allowed":true},{"name":"c_d","limit_reached":true},{"allowed":true}]}`,
		`{"type":"error","status":429,"headers":{"x-codex-primary-used-percent":"100","retry-after":7,"x-unrelated":"no","x-ratelimit-remaining-requests":"0","X-Codex-Plan-Type":" plus "}}`,
		`{"type":"response.created","rate_limits":{"allowed":true}}`,
		`{"type":"codex.rate_limits","rate_limits":{}}`,
	} {
		headers := helps.ParseCodexQuotaEventHeaders([]byte(payload))
		events = append(events, map[string]any{"payload": payload, "headers": headers})
	}
	q := &cliproxyauth.QuotaState{}
	h := http.Header{}
	h.Set("Retry-After", "30")
	h.Set("X-Codex-Primary-Used-Percent", "42")
	h.Set("X-Codex-Plan-Type", "pro")
	h.Set("X-Ratelimit-Limit-Requests", "100")
	h.Set("X-Codex-Additional-Foo-Allowed", "true")
	h.Set("X-Codex-Unknown", "dropped")
	h.Set("Anthropic-Ratelimit-Unified-Status", "allowed")
	h.Set("X-Codex-Credits-Balance", strings.Repeat("9", 600))
	h.Add("X-Codex-Secondary-Used-Percent", "1")
	h.Add("X-Codex-Secondary-Used-Percent", "2")
	q.ObserveResponseHeadersForProvider("codex", h, time.Unix(1, 0))
	return map[string]any{"events": events, "signals": q.Signals}
}

// ---------------------------------------------------------------- reasoning replay

type replayTurn struct {
	Payload        string `json:"payload"`
	Stream         bool   `json:"stream"`
	UpstreamStatus int    `json:"upstream_status"`
	UpstreamBody   string `json:"upstream_body"`
	// Filled by the generator.
	Upstream  string `json:"upstream"`
	SessionID string `json:"session_id"`
	Error     any    `json:"error,omitempty"`
}

type replayScenario struct {
	Name    string            `json:"name"`
	Headers map[string]string `json:"headers"`
	Model   string            `json:"model"`
	Turns   []replayTurn      `json:"turns"`
}

func replaySignature(seed byte) string {
	payload := make([]byte, 1+8+16+16+32)
	payload[0] = 0x80
	for i := 9; i < len(payload); i++ {
		payload[i] = seed + byte(i)
	}
	return base64.RawURLEncoding.EncodeToString(payload)
}

func replayScenarios() []replayScenario {
	sse := func(events ...string) string {
		var b strings.Builder
		for _, e := range events {
			b.WriteString("data: " + e + "\n\n")
		}
		return b.String()
	}
	created := `{"type":"response.created","response":{"id":"resp_r","status":"in_progress"}}`
	completed := `{"type":"response.completed","response":{"id":"resp_r","status":"completed","output":[],"usage":{"input_tokens":3,"output_tokens":1}}}`
	reasoning := func(seed byte, index int) string {
		return fmt.Sprintf(`{"type":"response.output_item.done","output_index":%d,"item":{"type":"reasoning","id":"rs_%d","summary":[{"type":"summary_text","text":"thought"}],"encrypted_content":"%s"}}`, index, seed, replaySignature(seed))
	}
	call := func(id string, index int) string {
		return fmt.Sprintf(`{"type":"response.output_item.done","output_index":%d,"item":{"type":"function_call","id":"fc_1","call_id":%q,"name":"shell","arguments":"{\"cmd\":\"ls\"}"}}`, index, id)
	}
	text := func(t string) string {
		return `{"type":"response.output_item.done","output_index":0,"item":{"type":"message","role":"assistant","id":"msg_1","content":[{"type":"output_text","text":"` + t + `"}]}}`
	}
	first := `{"model":"claude-x","max_tokens":64,"messages":[{"role":"user","content":"list files"}]}`
	followUp := func(toolID string) string {
		return `{"model":"claude-x","max_tokens":64,"messages":[{"role":"user","content":"list files"},` +
			`{"role":"assistant","content":[{"type":"tool_use","id":"` + toolID + `","name":"shell","input":{"cmd":"ls"}}]},` +
			`{"role":"user","content":[{"type":"tool_result","tool_use_id":"` + toolID + `","content":"a.txt"}]}]}`
	}
	longID := "call:" + strings.Repeat("x", 70)
	visible := strings.ReplaceAll(longID, ":", "_")
	sum := sha256.Sum256([]byte(visible))
	suffix := "_" + hex.EncodeToString(sum[:8])
	visible = visible[:64-len(suffix)] + suffix
	invalid := `{"error":{"message":"Invalid signature in thinking block","type":"invalid_request_error"}}`
	return []replayScenario{
		{Name: "tool_turn_replayed_then_cleared", Model: "gpt-5.4", Headers: map[string]string{"X-Claude-Code-Session-Id": "cc-replay-1"},
			Turns: []replayTurn{
				{Payload: first, Stream: true, UpstreamStatus: 200, UpstreamBody: sse(created, reasoning(1, 0), call("call_abc", 1), completed)},
				{Payload: followUp("call_abc"), Stream: true, UpstreamStatus: 200, UpstreamBody: sse(created, text("a.txt found"), completed)},
				{Payload: followUp("call_abc"), Stream: false, UpstreamStatus: 400, UpstreamBody: invalid},
				{Payload: followUp("call_abc"), Stream: true, UpstreamStatus: 200, UpstreamBody: sse(created, text("again"), completed)},
			}},
		{Name: "long_call_id_matches_claude_visible_form", Model: "gpt-5.4", Headers: map[string]string{"X-Claude-Code-Session-Id": "cc-replay-2", "X-Claude-Code-Agent-Id": "agent-7"},
			Turns: []replayTurn{
				{Payload: first, Stream: false, UpstreamStatus: 200, UpstreamBody: sse(created, reasoning(3, 0), call(longID, 1), completed)},
				{Payload: followUp(visible), Stream: true, UpstreamStatus: 200, UpstreamBody: sse(created, text("ok"), completed)},
			}},
	}
}

func runReplay(sc *replayScenario) {
	internalcache.ClearCodexReasoningReplayCache()
	var mu sync.Mutex
	turn := 0
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, _ := io.ReadAll(r.Body)
		mu.Lock()
		t := &sc.Turns[turn]
		t.Upstream = string(body)
		t.SessionID = r.Header.Get("Session_id")
		turn++
		mu.Unlock()
		if t.UpstreamStatus != 200 {
			w.Header().Set("Content-Type", "application/json")
			w.WriteHeader(t.UpstreamStatus)
		} else {
			w.Header().Set("Content-Type", "text/event-stream")
		}
		_, _ = w.Write([]byte(t.UpstreamBody))
	}))
	defer server.Close()
	cfg, _ := config.ParseConfigBytes([]byte("{}\n"))
	exec := executor.NewCodexExecutor(cfg)
	auth := &cliproxyauth.Auth{ID: "codex-replay.json", Provider: "codex", Attributes: map[string]string{"base_url": server.URL},
		Metadata: map[string]any{"type": "codex", "access_token": "at-FAKE", "account_id": "acct-FAKE-1"}}
	headers := http.Header{}
	for k, v := range sc.Headers {
		headers.Set(k, v)
	}
	for i := range sc.Turns {
		t := &sc.Turns[i]
		opts := cliproxyexecutor.Options{Stream: t.Stream, Headers: headers, OriginalRequest: []byte(t.Payload), SourceFormat: sdktranslator.FromString("claude")}
		req := cliproxyexecutor.Request{Model: sc.Model, Payload: []byte(t.Payload)}
		ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
		if t.Stream {
			res, err := exec.ExecuteStream(ctx, auth, req, opts)
			if err != nil {
				t.Error = describe(err)
			} else {
				for chunk := range res.Chunks {
					if chunk.Err != nil {
						t.Error = describe(chunk.Err)
					}
				}
			}
		} else if _, err := exec.Execute(ctx, auth, req, opts); err != nil {
			t.Error = describe(err)
		}
		cancel()
	}
}

// ---------------------------------------------------------------- images

// imageCase runs the Images API path (source format openai-image, request_path
// metadata) of CodexExecutor. Payload is base64 so multipart bytes survive JSON.
type imageCase struct {
	Name           string            `json:"name"`
	Config         string            `json:"config"`
	Attributes     map[string]string `json:"attributes"`
	Metadata       map[string]any    `json:"metadata"`
	Headers        map[string]string `json:"headers"`
	Model          string            `json:"model"`
	PayloadB64     string            `json:"payload_b64"`
	Stream         bool              `json:"stream"`
	RequestPath    string            `json:"request_path"`
	UpstreamStatus int               `json:"upstream_status"`
	UpstreamType   string            `json:"upstream_type"`
	UpstreamBody   string            `json:"upstream_body"`
	// Filled by the generator.
	Upstream *captured `json:"upstream,omitempty"`
	Output   any       `json:"output"`
}

func multipartBody(boundary string, parts []string) string {
	var b strings.Builder
	for _, p := range parts {
		b.WriteString("--" + boundary + "\r\n" + p + "\r\n")
	}
	b.WriteString("--" + boundary + "--\r\n")
	return b.String()
}

func imageCases() []imageCase {
	oauthMeta := map[string]any{"type": "codex", "access_token": "at-FAKE", "account_id": "acct-FAKE-1", "email": "user@example.invalid"}
	clientHeaders := map[string]string{"User-Agent": "my-image-client/2.0", "Originator": "my_app", "Session_id": "sess-FAKE", "X-Codex-Turn-Metadata": `{"turn":1}`}
	b64 := func(s string) string { return base64.StdEncoding.EncodeToString([]byte(s)) }
	png := "\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR-fake"
	call := func(index int, result, format string) string {
		return fmt.Sprintf(`data: {"type":"response.output_item.done","output_index":%d,"item":{"type":"image_generation_call","id":"ig_%d","status":"completed","result":%q,"revised_prompt":" a calm cat ","output_format":%q,"size":"1024x1024","background":"opaque","quality":"high"}}`+"\n\n", index, index, result, format)
	}
	completed := func(output string) string {
		return `data: {"type":"response.completed","response":{"id":"resp_img","created_at":1700000000,"status":"completed","output":` + output + `,"usage":{"input_tokens":10,"output_tokens":20,"total_tokens":30},"tool_usage":{"image_gen":{"input_tokens":5,"output_tokens":7,"total_tokens":12}}}}` + "\n\n"
	}
	toolSSE := "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_img\"}}\n\n" +
		"data: {\"type\":\"response.image_generation_call.partial_image\",\"partial_image_index\":0,\"partial_image_b64\":\"UEFSVElBTA==\",\"output_format\":\"webp\"}\n\n" +
		call(1, "SU1HMg==", "webp") + call(0, "SU1HMQ==", "webp") + completed("[]")
	editBoundary := "XBOUNDARYX"
	editMultipart := multipartBody(editBoundary, []string{
		"Content-Disposition: form-data; name=\"prompt\"\r\n\r\n  add a hat  ",
		"Content-Disposition: form-data; name=\"model\"\r\n\r\ngpt-image-1.5",
		"Content-Disposition: form-data; name=\"size\"\r\n\r\n1024x1536",
		"Content-Disposition: form-data; name=\"input_fidelity\"\r\n\r\nhigh",
		"Content-Disposition: form-data; name=\"output_compression\"\r\n\r\n70",
		"Content-Disposition: form-data; name=\"partial_images\"\r\n\r\nnope",
		"Content-Disposition: form-data; name=\"response_format\"\r\n\r\nURL",
		"Content-Disposition: form-data; name=\"image[]\"; filename=\"a.png\"\r\nContent-Type: image/png\r\n\r\n" + png,
		"Content-Disposition: form-data; name=\"image[]\"; filename=\"b.bin\"\r\n\r\n" + png,
		"Content-Disposition: form-data; name=\"mask\"; filename=\"m.png\"\r\nContent-Type: image/png\r\n\r\nMASK",
	})
	directEditMultipart := multipartBody(editBoundary, []string{
		"Content-Disposition: form-data; name=\"prompt\"\r\n\r\nadd a hat",
		"Content-Disposition: form-data; name=\"model\"\r\n\r\ngpt-image-2",
		"Content-Disposition: form-data; name=\"n\"\r\n\r\n2",
		"Content-Disposition: form-data; name=\"quality\"\r\n\r\nhigh",
		"Content-Disposition: form-data; name=\"mask[file_id]\"\r\n\r\nfile-mask",
		"Content-Disposition: form-data; name=\"image\"; filename=\"a.png\"\r\nContent-Type: image/png\r\n\r\n" + png,
	})
	multipartHeaders := func(h map[string]string) map[string]string {
		out := map[string]string{"Content-Type": "multipart/form-data; boundary=" + editBoundary}
		for k, v := range h {
			out[k] = v
		}
		return out
	}
	generation := `{"model":"gpt-5.4","prompt":"  a cat  ","size":"1024x1024","quality":"high","n":2,"output_compression":50,"partial_images":"2","background":"transparent","output_format":"webp","moderation":"low","style":"vivid"}`
	return []imageCase{
		{Name: "tool_generation_nonstream_b64", Attributes: map[string]string{}, Metadata: oauthMeta, Headers: clientHeaders,
			Model: "gpt-5.4", PayloadB64: b64(generation), RequestPath: "/v1/images/generations",
			UpstreamStatus: 200, UpstreamType: "text/event-stream", UpstreamBody: toolSSE},
		{Name: "tool_generation_nonstream_url_completed_output", Attributes: map[string]string{"api_key": "sk-FAKE"}, Metadata: map[string]any{}, Headers: map[string]string{},
			Model: "route-model", PayloadB64: b64(`{"prompt":"p","response_format":"url"}`), RequestPath: "/v1/images/generations",
			UpstreamStatus: 200, UpstreamType: "text/event-stream",
			UpstreamBody: call(0, "SUdOT1JFRA==", "png") + completed(`[{"type":"message"},{"type":"image_generation_call","result":"SU1HSlBH","output_format":"jpeg"},{"type":"image_generation_call","result":"  "}]`)},
		{Name: "tool_generation_stream", Attributes: map[string]string{}, Metadata: oauthMeta, Headers: clientHeaders,
			Model: "gpt-5.4", PayloadB64: b64(`{"prompt":"a cat","partial_images":1,"response_format":"url"}`), Stream: true, RequestPath: "/v1/images/generations",
			UpstreamStatus: 200, UpstreamType: "text/event-stream", UpstreamBody: toolSSE},
		{Name: "tool_edit_json", Config: "gpt-image-2-base-model: gpt-5.5\n", Attributes: map[string]string{}, Metadata: oauthMeta, Headers: clientHeaders,
			Model: "gpt-5.4", PayloadB64: b64(`{"prompt":"edit it","images":[{"image_url":"data:image/png;base64,QUJD"},{"image_url":"  "},{"file_id":"f"}],"mask":{"image_url":"data:image/png;base64,TUFTSw=="},"input_fidelity":"low","output_compression":20.5}`),
			RequestPath: "/v1/images/edits", UpstreamStatus: 200, UpstreamType: "text/event-stream", UpstreamBody: toolSSE},
		{Name: "tool_edit_multipart_stream", Config: "multimedia:\n  gpt-image-2-base-model: o4-image\n", Attributes: map[string]string{}, Metadata: oauthMeta,
			Headers: multipartHeaders(clientHeaders), Model: "gpt-5.4", PayloadB64: b64(editMultipart), Stream: true,
			RequestPath: "/v1/images/edits", UpstreamStatus: 200, UpstreamType: "text/event-stream", UpstreamBody: toolSSE},
		{Name: "tool_no_image_output", Attributes: map[string]string{}, Metadata: oauthMeta, Headers: map[string]string{},
			Model: "gpt-5.4", PayloadB64: b64(`{"prompt":"p"}`), RequestPath: "/v1/images/generations",
			UpstreamStatus: 200, UpstreamType: "text/event-stream", UpstreamBody: completed("[]")},
		{Name: "tool_disconnected_before_completion", Attributes: map[string]string{}, Metadata: oauthMeta, Headers: map[string]string{},
			Model: "gpt-5.4", PayloadB64: b64(`{"prompt":"p"}`), RequestPath: "/v1/images/generations",
			UpstreamStatus: 200, UpstreamType: "text/event-stream", UpstreamBody: call(0, "SU1H", "png")},
		{Name: "tool_upstream_429", Attributes: map[string]string{}, Metadata: oauthMeta, Headers: map[string]string{},
			Model: "gpt-5.4", PayloadB64: b64(`{"prompt":"p"}`), RequestPath: "/v1/images/generations",
			UpstreamStatus: 429, UpstreamType: "application/json", UpstreamBody: `{"error":{"type":"usage_limit_reached","message":"limit","resets_in_seconds":60}}`},
		{Name: "tool_invalid_json", Attributes: map[string]string{}, Metadata: oauthMeta, Headers: map[string]string{},
			Model: "gpt-5.4", PayloadB64: b64(`{"prompt":`), RequestPath: "/v1/images/generations",
			UpstreamStatus: 200, UpstreamType: "text/event-stream", UpstreamBody: toolSSE},
		{Name: "direct_generation_nonstream", Attributes: map[string]string{}, Metadata: oauthMeta, Headers: clientHeaders,
			Model: "gpt-image-2", PayloadB64: b64(`{"model":"gpt-image-2","prompt":"a cat","stream":true,"n":1}`), RequestPath: "/v1/images/generations",
			UpstreamStatus: 200, UpstreamType: "application/json", UpstreamBody: `{"created":1,"data":[{"b64_json":"QQ=="}],"usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}`},
		{Name: "direct_generation_stream_suffix_model", Attributes: map[string]string{"api_key": "sk-FAKE"}, Metadata: map[string]any{}, Headers: clientHeaders,
			Model: "openai/gpt-image-2.5(high)", PayloadB64: b64(`{"prompt":"a cat"}`), Stream: true, RequestPath: "/v1/images/generations",
			UpstreamStatus: 200, UpstreamType: "text/event-stream", UpstreamBody: "event: image_generation.completed\ndata: {\"type\":\"image_generation.completed\",\"b64_json\":\"QQ==\"}\n\n"},
		{Name: "direct_edit_multipart", Attributes: map[string]string{}, Metadata: oauthMeta, Headers: multipartHeaders(clientHeaders),
			Model: "gpt-image-2", PayloadB64: b64(directEditMultipart), RequestPath: "/v1/images/edits",
			UpstreamStatus: 200, UpstreamType: "application/json", UpstreamBody: `{"created":2,"data":[{"b64_json":"Qg=="}]}`},
		{Name: "direct_edit_json", Attributes: map[string]string{}, Metadata: oauthMeta, Headers: map[string]string{"Content-Type": "application/json"},
			Model: "gpt-image-1.5", PayloadB64: b64(`{"model":"gpt-image-1.5","prompt":"edit","images":[{"image_url":"data:image/png;base64,QUJD"}]}`), RequestPath: "/v1/images/edits",
			UpstreamStatus: 200, UpstreamType: "application/json", UpstreamBody: `{"created":3,"data":[]}`},
		// Without cloaking the client's User-Agent shows: the tool path forwards it, the
		// direct path drops it (Cloudflare 1010 blocks).
		{Name: "tool_generation_uncloaked_client_ua", Config: "codex:\n  disable-codex-cloaking: true\n", Attributes: map[string]string{}, Metadata: oauthMeta, Headers: clientHeaders,
			Model: "gpt-5.4", PayloadB64: b64(`{"prompt":"p"}`), RequestPath: "/v1/images/generations",
			UpstreamStatus: 200, UpstreamType: "text/event-stream", UpstreamBody: toolSSE},
		{Name: "direct_generation_uncloaked_drops_client_ua", Config: "codex:\n  disable-codex-cloaking: true\n", Attributes: map[string]string{}, Metadata: oauthMeta, Headers: clientHeaders,
			Model: "gpt-image-2", PayloadB64: b64(`{"prompt":"p"}`), RequestPath: "/v1/images/generations",
			UpstreamStatus: 200, UpstreamType: "application/json", UpstreamBody: `{"created":4,"data":[]}`},
		{Name: "direct_upstream_400", Attributes: map[string]string{}, Metadata: oauthMeta, Headers: map[string]string{},
			Model: "gpt-image-2", PayloadB64: b64(`{"prompt":"x"}`), RequestPath: "/v1/images/generations",
			UpstreamStatus: 400, UpstreamType: "application/json", UpstreamBody: `{"error":{"message":"bad size","type":"invalid_request_error"}}`},
	}
}

func runImage(c *imageCase) {
	var got captured
	var mu sync.Mutex
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		hop := capture(r)
		mu.Lock()
		got = hop
		mu.Unlock()
		w.Header().Set("Content-Type", c.UpstreamType)
		w.Header().Set("X-Codex-Primary-Used-Percent", "42")
		w.WriteHeader(c.UpstreamStatus)
		_, _ = w.Write([]byte(c.UpstreamBody))
	}))
	defer server.Close()
	if strings.TrimSpace(c.Config) == "" {
		c.Config = "{}\n"
	}
	cfg, err := config.ParseConfigBytes([]byte(c.Config))
	if err != nil {
		panic(err)
	}
	attrs := map[string]string{"base_url": server.URL}
	for k, v := range c.Attributes {
		attrs[k] = v
	}
	auth := &cliproxyauth.Auth{ID: "codex-fixture.json", Provider: "codex", Attributes: attrs, Metadata: c.Metadata}
	headers := http.Header{}
	for k, v := range c.Headers {
		headers.Set(k, v)
	}
	payload, err := base64.StdEncoding.DecodeString(c.PayloadB64)
	if err != nil {
		panic(err)
	}
	meta := map[string]any{cliproxyexecutor.RequestPathMetadataKey: c.RequestPath}
	opts := cliproxyexecutor.Options{
		Stream: c.Stream, Headers: headers, OriginalRequest: payload,
		SourceFormat: sdktranslator.FromString("openai-image"), Metadata: meta,
	}
	req := cliproxyexecutor.Request{Model: c.Model, Payload: payload, Metadata: meta}
	exec := executor.NewCodexExecutor(cfg)
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	// The direct path reads client headers from the gin context, as in production.
	ginCtx, _ := gin.CreateTestContext(httptest.NewRecorder())
	ginCtx.Request = httptest.NewRequest(http.MethodPost, c.RequestPath, nil)
	ginCtx.Request.Header = headers.Clone()
	ctx = context.WithValue(ctx, "gin", ginCtx)
	result := map[string]any{}
	if c.Stream {
		res, errStream := exec.ExecuteStream(ctx, auth, req, opts)
		if errStream != nil {
			result["error"] = describe(errStream)
		} else {
			chunks := []string{}
			for chunk := range res.Chunks {
				if chunk.Err != nil {
					result["stream_error"] = describe(chunk.Err)
					break
				}
				chunks = append(chunks, string(chunk.Payload))
			}
			result["chunks"] = chunks
		}
	} else {
		res, errExec := exec.Execute(ctx, auth, req, opts)
		if errExec != nil {
			result["error"] = describe(errExec)
		} else {
			result["payload"] = string(res.Payload)
		}
	}
	mu.Lock()
	if got.Method != "" {
		g := got
		c.Upstream = &g
	}
	mu.Unlock()
	c.Output = result
}

func main() {
	if len(os.Args) != 2 {
		fmt.Fprintln(os.Stderr, "usage: generate <output.json>")
		os.Exit(2)
	}
	cases := executorCases()
	for i := range cases {
		runExecutor(&cases[i])
	}
	sort.SliceStable(cases, func(i, j int) bool { return false })
	replays := replayScenarios()
	for i := range replays {
		runReplay(&replays[i])
	}
	images := imageCases()
	for i := range images {
		runImage(&images[i])
	}
	out := map[string]any{"sjson": sjsonCases(), "oauth": oauthCases(), "executor": cases,
		"alpha_search": alphaCases(), "quota": quotaCases(), "replay": replays, "images": images}
	var buf bytes.Buffer
	enc := json.NewEncoder(&buf)
	enc.SetEscapeHTML(false)
	enc.SetIndent("", "  ")
	if err := enc.Encode(out); err != nil {
		panic(err)
	}
	if err := os.WriteFile(os.Args[1], buf.Bytes(), 0o644); err != nil {
		panic(err)
	}
}
