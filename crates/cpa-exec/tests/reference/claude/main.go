// Generates crates/cpa-exec/src/claude/testdata/go_executor.json from CLIProxyAPI at
// 6fecc6e by running the real Claude executor in-process. Upstream traffic goes to a
// capturing http.RoundTripper (the executor's documented "cliproxy.roundtripper"
// context hook); nothing leaves the process. Credentials are fake.
package main

import (
	"bytes"
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"time"

	"github.com/gin-gonic/gin"
	claudeauth "github.com/router-for-me/CLIProxyAPI/v8/internal/auth/claude"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/misc"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/modelconfig"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/registry"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/runtime/executor"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/watcher/synthesizer"
	cliproxyauth "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/auth"
	cliproxyexecutor "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/executor"
	cliproxysession "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/session"
	sdktranslator "github.com/router-for-me/CLIProxyAPI/v8/sdk/translator"
	"github.com/tidwall/gjson"
	"github.com/tidwall/sjson"
)

type reply struct {
	Status      int         `json:"status"`
	ContentType string      `json:"content_type"`
	Body        string      `json:"body"`
	BodyB64     string      `json:"body_b64,omitempty"`
	Headers     [][2]string `json:"headers,omitempty"`
}

type scenario struct {
	Name      string      `json:"name"`
	Config    string      `json:"config"`
	AuthFile  string      `json:"auth_file,omitempty"`
	ClientKey string      `json:"client_key"`
	Model     string      `json:"model"`
	Headers   [][2]string `json:"headers"`
	Body      string      `json:"body"`
	Stream    bool        `json:"stream"`
	Count     bool        `json:"count"`
	Reply     reply       `json:"reply"`
	// ExecutionSession is ExecutionSessionMetadataKey (websocket executions).
	ExecutionSession string `json:"execution_session,omitempty"`
	// Source is the client format (default claude); responses use the same format.
	Source string `json:"source,omitempty"`
	// Resolved binds the conductor's API-key model capability snapshot
	// (lookupAPIKeyModelCapability -> modelconfig.ResolveModelInfo + IsCompat).
	Resolved *resolvedModel `json:"resolved,omitempty"`
	// RequestedModel is the client's route model when it differs from Model.
	RequestedModel string `json:"requested_model,omitempty"`
}

type resolvedModel struct {
	Name     string                    `json:"name"`
	Compat   bool                      `json:"compat"`
	Thinking *registry.ThinkingSupport `json:"thinking,omitempty"`
}

type upstream struct {
	URL     string      `json:"url"`
	Headers [][2]string `json:"headers"`
	Body    string      `json:"body"`
	Reply   string      `json:"reply"`
}

type result struct {
	scenario
	Date      string     `json:"date"`
	Upstream  []upstream `json:"upstream"`
	Output    []string   `json:"output"`
	Error     string     `json:"error,omitempty"`
	Status    int        `json:"error_status,omitempty"`
	ErrorInfo *errInfo   `json:"error_info,omitempty"`
	// ResolvedInfo is the exact ModelInfo bound to the request (Rust tests bind it as
	// ExecRequest.resolved_model, as dispatch does), with the json:"-" fields the
	// executor reads added under cpa_core's raw keys.
	ResolvedInfo map[string]any `json:"resolved_info,omitempty"`
}

// errInfo is what the conductor and handlers read from an executor error.
type errInfo struct {
	RequestScoped    bool        `json:"request_scoped"`
	CredentialScoped bool        `json:"credential_scoped"`
	RetryAfter       bool        `json:"retry_after"`
	Direct           bool        `json:"direct"`
	DirectStatus     int         `json:"direct_status,omitempty"`
	DirectHeaders    [][2]string `json:"direct_headers,omitempty"`
	DirectBody       string      `json:"direct_body,omitempty"`
}

func describe(err error) *errInfo {
	info := &errInfo{}
	var requestScoped interface{ IsRequestScoped() bool }
	if errors.As(err, &requestScoped) {
		info.RequestScoped = requestScoped.IsRequestScoped()
	}
	var credentialScoped interface{ IsCredentialScoped() bool }
	if errors.As(err, &credentialScoped) {
		info.CredentialScoped = credentialScoped.IsCredentialScoped()
	}
	var retry interface{ RetryAfter() *time.Duration }
	if errors.As(err, &retry) {
		info.RetryAfter = retry.RetryAfter() != nil
	}
	var terminated *cliproxyexecutor.RequestTerminatedError
	if errors.As(err, &terminated) {
		info.Direct = true
		info.DirectStatus = terminated.HTTPStatus
		info.DirectBody = string(terminated.Body)
		for key, values := range terminated.Header {
			for _, value := range values {
				info.DirectHeaders = append(info.DirectHeaders, [2]string{key, value})
			}
		}
		sort.Slice(info.DirectHeaders, func(i, j int) bool { return info.DirectHeaders[i][0] < info.DirectHeaders[j][0] })
	}
	return info
}

type capture struct {
	requests []upstream
	reply    reply
}

func (c *capture) RoundTrip(r *http.Request) (*http.Response, error) {
	body, _ := io.ReadAll(r.Body)
	var headers [][2]string
	for key, values := range r.Header {
		for _, value := range values {
			headers = append(headers, [2]string{key, value})
		}
	}
	sort.Slice(headers, func(i, j int) bool { return headers[i][0] < headers[j][0] })
	replyBody := substitute(c.reply.Body, string(body))
	if c.reply.BodyB64 != "" {
		raw, err := base64.StdEncoding.DecodeString(c.reply.BodyB64)
		must(err)
		replyBody = string(raw)
	}
	c.requests = append(c.requests, upstream{URL: r.URL.String(), Headers: headers, Body: string(body), Reply: replyBody})
	h := http.Header{}
	h.Set("Content-Type", c.reply.ContentType)
	for _, kv := range c.reply.Headers {
		h.Add(kv[0], kv[1])
	}
	return &http.Response{StatusCode: c.reply.Status, Header: h, Body: io.NopCloser(strings.NewReader(replyBody)), Request: r}, nil
}

// substitute replaces {{ALIAS:name}} with the upstream tool name Go assigned to
// name, and {{DRIFT:name}} with that alias using a different tool word.
func substitute(reply, request string) string {
	for _, kind := range []string{"ALIAS", "DRIFT"} {
		for {
			start := strings.Index(reply, "{{"+kind+":")
			if start < 0 {
				break
			}
			end := strings.Index(reply[start:], "}}") + start
			original := reply[start+len(kind)+3 : end]
			alias := original
			for _, tool := range gjsonArray(request, "tools") {
				name := gjsonString(tool, "name")
				if strings.HasPrefix(name, "mcp__") && strings.HasSuffix(name, "_"+original) {
					alias = name
				}
			}
			if kind == "DRIFT" {
				parts := strings.SplitN(alias, "__", 3)
				if len(parts) == 3 {
					word := strings.SplitN(parts[2], "_", 2)
					if len(word) == 2 {
						alias = parts[0] + "__" + parts[1] + "__zebra_" + word[1]
					}
				}
			}
			reply = reply[:start] + alias + reply[end+2:]
		}
	}
	return reply
}

func gjsonArray(raw, path string) []string {
	var out []string
	for _, item := range gjson.Get(raw, path).Array() {
		out = append(out, item.Raw)
	}
	return out
}

func gjsonString(raw, path string) string { return gjson.Get(raw, path).String() }

func run(root string, s scenario) result {
	dir := filepath.Join(root, s.Name)
	must(os.MkdirAll(filepath.Join(dir, "auth"), 0o700))
	cfgText := strings.ReplaceAll(s.Config, "AUTH_DIR", filepath.Join(dir, "auth"))
	must(os.WriteFile(filepath.Join(dir, "config.yaml"), []byte(cfgText), 0o600))
	if s.AuthFile != "" {
		must(os.WriteFile(filepath.Join(dir, "auth", "fixture.json"), []byte(s.AuthFile), 0o600))
	}
	cfg, err := config.LoadConfig(filepath.Join(dir, "config.yaml"))
	must(err)
	sctx := &synthesizer.SynthesisContext{Config: cfg, AuthDir: filepath.Join(dir, "auth"), Now: time.Now(), IDGenerator: synthesizer.NewStableIDGenerator()}
	var auths []*cliproxyauth.Auth
	fromConfig, err := synthesizer.NewConfigSynthesizer().Synthesize(sctx)
	must(err)
	fromFiles, err := synthesizer.NewFileSynthesizer().Synthesize(sctx)
	must(err)
	auths = append(append(auths, fromConfig...), fromFiles...)
	var auth *cliproxyauth.Auth
	for _, a := range auths {
		if a.Provider == "claude" {
			auth = a
			break
		}
	}
	if auth == nil {
		panic("no claude auth for " + s.Name)
	}
	// The Rust credential ID is the auth-dir-relative path; Go's file synthesizer
	// uses the same relative ID, so continuity keys and seeds agree.
	exec := executor.NewClaudeExecutor(cfg)
	cap := &capture{reply: s.Reply}

	gin.SetMode(gin.ReleaseMode)
	w := httptest.NewRecorder()
	ginCtx, _ := gin.CreateTestContext(w)
	path := "/v1/messages"
	if s.Count {
		path = "/v1/messages/count_tokens"
	}
	ginCtx.Request = httptest.NewRequest(http.MethodPost, path, bytes.NewReader([]byte(s.Body)))
	headers := http.Header{}
	for _, kv := range s.Headers {
		headers.Add(kv[0], kv[1])
	}
	ginCtx.Request.Header = headers
	if s.ClientKey != "" {
		ginCtx.Set("userApiKey", s.ClientKey)
	}
	ctx := context.WithValue(context.Background(), "gin", ginCtx)
	ctx = context.WithValue(ctx, "cliproxy.roundtripper", http.RoundTripper(cap))

	metadata := map[string]any{}
	if s.ClientKey != "" {
		metadata[cliproxyexecutor.CallerScopeMetadataKey] = cliproxysession.CallerScope(s.ClientKey)
	}
	if s.ExecutionSession != "" {
		metadata[cliproxyexecutor.ExecutionSessionMetadataKey] = s.ExecutionSession
	}
	source := s.Source
	if source == "" {
		source = "claude"
	}
	req := cliproxyexecutor.Request{Model: s.Model, Payload: []byte(s.Body), Format: sdktranslator.FromString(source)}
	var resolvedInfo map[string]any
	if s.Resolved != nil {
		info := modelconfig.ResolveModelInfo(s.Resolved.Name, "claude", s.Resolved.Thinking)
		info.IsCompat = s.Resolved.Compat
		req.Metadata = map[string]any{"cliproxy.resolved_api_key_model_info": info}
		raw, _ := json.Marshal(info)
		_ = json.Unmarshal(raw, &resolvedInfo)
		resolvedInfo["is_compat"] = info.IsCompat
		resolvedInfo["user_defined"] = info.UserDefined
		resolvedInfo["support_configuration_update"] = info.SupportConfigurationUpdate
	}
	opts := cliproxyexecutor.Options{
		Stream:          s.Stream,
		Headers:         headers.Clone(),
		OriginalRequest: []byte(s.Body),
		SourceFormat:    sdktranslator.FromString(source),
		ResponseFormat:  sdktranslator.FromString(source),
		Metadata:        metadata,
	}
	req, opts = cliproxysession.Enrich(req, opts)

	out := result{scenario: s, Date: time.Now().Format("2006-01-02"), ResolvedInfo: resolvedInfo}
	var errRun error
	switch {
	case s.Count:
		resp, err := exec.CountTokens(ctx, auth, req, opts)
		errRun = err
		if err == nil {
			out.Output = []string{string(resp.Payload)}
		}
	case s.Stream:
		stream, err := exec.ExecuteStream(ctx, auth, req, opts)
		errRun = err
		if err == nil {
			for chunk := range stream.Chunks {
				if chunk.Err != nil {
					out.Error = chunk.Err.Error()
					break
				}
				out.Output = append(out.Output, string(chunk.Payload))
			}
		}
	default:
		resp, err := exec.Execute(ctx, auth, req, opts)
		errRun = err
		if err == nil {
			out.Output = []string{string(resp.Payload)}
		}
	}
	if errRun != nil {
		out.Error = errRun.Error()
		if sc, ok := errRun.(interface{ StatusCode() int }); ok {
			out.Status = sc.StatusCode()
		}
		out.ErrorInfo = describe(errRun)
	}
	out.Upstream = cap.requests
	return out
}

type sjsonCase struct {
	Op    string `json:"op"`
	JSON  string `json:"json"`
	Path  string `json:"path"`
	Value string `json:"value,omitempty"`
	Out   string `json:"out"`
}

func sjsonCases() []sjsonCase {
	inputs := []sjsonCase{
		{Op: "raw", JSON: `{"a":1 }`, Path: "b", Value: "2"},
		{Op: "raw", JSON: `{}`, Path: "b", Value: "2"},
		{Op: "raw", JSON: ` { } `, Path: "b", Value: "2"},
		{Op: "raw", JSON: `{"a":{"x":1}}`, Path: "a.x", Value: "true"},
		{Op: "raw", JSON: `{"a":1}`, Path: "m.user_id", Value: `"u"`},
		{Op: "raw", JSON: `{"a":[1,2]}`, Path: "a.3", Value: "9"},
		{Op: "raw", JSON: `{"a":[]}`, Path: "a.1", Value: "9"},
		{Op: "raw", JSON: `{"a":[1]}`, Path: "a.-1", Value: "9"},
		{Op: "raw", JSON: `{"a":[{"b":1}]}`, Path: "a.0.c.d", Value: "[]"},
		{Op: "raw", JSON: "{\n  \"a\": 1\n}\n", Path: "z", Value: "0"},
		{Op: "delete", JSON: `{"a":1, "b":2,"c":3}`, Path: "b"},
		{Op: "delete", JSON: `{"a":1, "b":2}`, Path: "a"},
		{Op: "delete", JSON: `{ "a" : 1 }`, Path: "a"},
		{Op: "delete", JSON: `{"a":[1,2,3]}`, Path: "a.0"},
		{Op: "delete", JSON: `{"a":[1,2,3]}`, Path: "a.2"},
		{Op: "delete", JSON: `{"a":[1, 2 ,3]}`, Path: "a.1"},
		{Op: "delete", JSON: `{"a":1}`, Path: "zz"},
		{Op: "delete", JSON: `{"a":{"b":{"c":1,"d":2}}}`, Path: "a.b.d"},
		{Op: "delete", JSON: `{"a" :1 , "b":{"x":[]} }`, Path: "b.x"},
		{Op: "string", JSON: `{"a":1}`, Path: "t", Value: "x<y"},
		{Op: "string", JSON: `{"a":1}`, Path: "t", Value: "é<&>\u2028\x01\"\\\n"},
		{Op: "string", JSON: `{"a":"old"}`, Path: "a", Value: "new"},
		{Op: "string", JSON: `{"a":1}`, Path: "t", Value: "b\bf\fu\x0b"},
	}
	for i, c := range inputs {
		var out []byte
		var err error
		switch c.Op {
		case "raw":
			out, err = sjson.SetRawBytes([]byte(c.JSON), c.Path, []byte(c.Value))
		case "delete":
			out, err = sjson.DeleteBytes([]byte(c.JSON), c.Path)
		case "string":
			out, err = sjson.SetBytes([]byte(c.JSON), c.Path, c.Value)
		}
		if err != nil {
			out = []byte(c.JSON)
		}
		inputs[i].Out = string(out)
	}
	return inputs
}

type loginCase struct {
	Method      string `json:"method"`
	Target      string `json:"target"`
	Status      int    `json:"status"`
	ContentType string `json:"content_type"`
	Location    string `json:"location,omitempty"`
	NoSniff     string `json:"nosniff,omitempty"`
	Body        string `json:"body"`
}

// loginServerCases drives the real claude.OAuthServer on a free local port.
func loginServerCases() []loginCase {
	probe, err := net.Listen("tcp", "127.0.0.1:0")
	must(err)
	port := probe.Addr().(*net.TCPAddr).Port
	must(probe.Close())
	server := claudeauth.NewOAuthServer(port)
	must(server.Start())
	defer server.Stop(context.Background())
	client := &http.Client{CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse }}
	var out []loginCase
	for _, c := range []loginCase{
		{Method: "GET", Target: "/callback?code=abc%23frag&state=xyz"},
		{Method: "GET", Target: "/callback?error=access_denied&error_description=nope"},
		{Method: "GET", Target: "/callback?state=xyz"},
		{Method: "GET", Target: "/callback?code=abc"},
		{Method: "POST", Target: "/callback?code=abc&state=xyz"},
		{Method: "GET", Target: "/success"},
		{Method: "GET", Target: "/success?setup_required=true&platform_url=https%3A%2F%2Fplatform.example.invalid%2F"},
		{Method: "POST", Target: "/success"},
		{Method: "GET", Target: "/other"},
	} {
		req, err := http.NewRequest(c.Method, fmt.Sprintf("http://127.0.0.1:%d%s", port, c.Target), nil)
		must(err)
		resp, err := client.Do(req)
		must(err)
		body, _ := io.ReadAll(resp.Body)
		_ = resp.Body.Close()
		c.Status = resp.StatusCode
		c.ContentType = resp.Header.Get("Content-Type")
		c.Location = resp.Header.Get("Location")
		c.NoSniff = resp.Header.Get("X-Content-Type-Options")
		c.Body = string(body)
		out = append(out, c)
	}
	return out
}

type callbackParse struct {
	Input string `json:"input"`
	Nil   bool   `json:"nil,omitempty"`
	Err   string `json:"err,omitempty"`
	Code  string `json:"code,omitempty"`
	State string `json:"state,omitempty"`
	Error string `json:"error,omitempty"`
	Desc  string `json:"desc,omitempty"`
}

func callbackParses() []callbackParse {
	var out []callbackParse
	for _, input := range []string{
		"", "   ",
		"http://localhost:54545/callback?code=c1&state=s1",
		"localhost:54545/callback?code=c2&state=s2",
		"?code=c3&state=s3",
		"code=c4&state=s4",
		"c5#s5",
		"http://localhost/callback?code=c6%23s6",
		"http://localhost/callback#code=c7&state=s7",
		"http://localhost/callback?code=c8&state=s8#state=ignored&error=e",
		"http://localhost/callback?error_description=only+desc",
		"http://localhost/callback?error=denied&error_description=why",
		"http://localhost/callback?state=s9",
		"justtext",
		"http://[::1/bad",
	} {
		c := callbackParse{Input: input}
		parsed, err := misc.ParseOAuthCallback(input)
		switch {
		case err != nil:
			c.Err = err.Error()
		case parsed == nil:
			c.Nil = true
		default:
			c.Code, c.State, c.Error, c.Desc = parsed.Code, parsed.State, parsed.Error, parsed.ErrorDescription
		}
		out = append(out, c)
	}
	return out
}

type fileName struct {
	Email        string `json:"email"`
	Organization string `json:"organization"`
	Account      string `json:"account"`
	Out          string `json:"out"`
}

func fileNames() []fileName {
	var out []fileName
	for _, c := range []fileName{
		{Email: "a@example.invalid"},
		{Email: " a@example.invalid ", Account: "acct-1"},
		{Email: "a@example.invalid", Organization: " org-1 ", Account: "acct-1"},
	} {
		c.Out = claudeauth.CredentialFileName(c.Email, c.Organization, c.Account)
		out = append(out, c)
	}
	return out
}

func must(err error) {
	if err != nil {
		panic(err)
	}
}

func main() {
	if len(os.Args) != 3 {
		panic("usage: generator SCENARIOS_JSON OUTPUT_JSON")
	}
	raw, err := os.ReadFile(os.Args[1])
	must(err)
	var scenarios []scenario
	must(json.Unmarshal(raw, &scenarios))
	root, err := os.MkdirTemp("", "cpa-claude-fixture-")
	must(err)
	defer os.RemoveAll(root)
	var results []result
	for _, s := range scenarios {
		results = append(results, run(root, s))
	}
	doc := map[string]any{
		"source":    "CLIProxyAPI 6fecc6e ClaudeExecutor, in-process; see tests/reference/claude/README.md",
		"scenarios": results,
		"sjson":     sjsonCases(),
		"login": map[string]any{
			"server":         loginServerCases(),
			"callback_parse": callbackParses(),
			"file_names":     fileNames(),
			"success_html":   claudeauth.LoginSuccessHtml,
			"setup_notice":   claudeauth.SetupNoticeHtml,
		},
	}
	encoded, err := json.MarshalIndent(doc, "", " ")
	must(err)
	must(os.WriteFile(os.Args[2], append(encoded, '\n'), 0o644))
	fmt.Println("wrote", len(results), "scenarios")
}
