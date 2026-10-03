package api

// HTTP goldens for cliproxy-rs's realtime routes: the real server (routes, access manager,
// realtime middleware, live handler) with an executor that runs the real Codex
// PrepareRequest and records the upstream request instead of sending it. Nothing leaves
// the process. Copy into internal/api/ of CLIProxyAPI 6fecc6e and run with RSFIX_OUT=<dir>;
// it writes realtime_http_go.json.

import (
	"bytes"
	"context"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"testing"

	gin "github.com/gin-gonic/gin"
	configaccess "github.com/router-for-me/CLIProxyAPI/v8/internal/access/config_access"
	proxyconfig "github.com/router-for-me/CLIProxyAPI/v8/internal/config"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/runtime/executor"
	sdkaccess "github.com/router-for-me/CLIProxyAPI/v8/sdk/access"
	"github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/auth"
	coreexecutor "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/executor"
)

type rsfixUpstream struct {
	Status  int                 `json:"status"`
	Headers map[string][]string `json:"headers,omitempty"`
	Body    string              `json:"body"`
}

type rsfixCaptured struct {
	Method  string              `json:"method"`
	URL     string              `json:"url"`
	Headers map[string][]string `json:"headers"`
	Body    string              `json:"body"`
	AuthID  string              `json:"auth_id"`
}

type rsfixLiveExecutor struct {
	real     *executor.CodexExecutor
	next     rsfixUpstream
	captured []rsfixCaptured
}

func (*rsfixLiveExecutor) Identifier() string { return "codex" }
func (*rsfixLiveExecutor) Execute(context.Context, *auth.Auth, coreexecutor.Request, coreexecutor.Options) (coreexecutor.Response, error) {
	return coreexecutor.Response{}, nil
}
func (*rsfixLiveExecutor) ExecuteStream(context.Context, *auth.Auth, coreexecutor.Request, coreexecutor.Options) (*coreexecutor.StreamResult, error) {
	return nil, nil
}
func (*rsfixLiveExecutor) Refresh(_ context.Context, a *auth.Auth) (*auth.Auth, error) { return a, nil }
func (*rsfixLiveExecutor) CountTokens(context.Context, *auth.Auth, coreexecutor.Request, coreexecutor.Options) (coreexecutor.Response, error) {
	return coreexecutor.Response{}, nil
}
func (e *rsfixLiveExecutor) PrepareRequest(req *http.Request, a *auth.Auth) error {
	return e.real.PrepareRequest(req, a)
}
func (e *rsfixLiveExecutor) HttpRequest(ctx context.Context, a *auth.Auth, req *http.Request) (*http.Response, error) {
	if errPrepare := e.real.PrepareRequest(req.WithContext(ctx), a); errPrepare != nil {
		return nil, errPrepare
	}
	body, _ := io.ReadAll(req.Body)
	e.captured = append(e.captured, rsfixCaptured{Method: req.Method, URL: req.URL.String(), Headers: req.Header.Clone(), Body: string(body), AuthID: a.ID})
	header := make(http.Header)
	for name, values := range e.next.Headers {
		for _, value := range values {
			header.Add(name, value)
		}
	}
	return &http.Response{StatusCode: e.next.Status, Header: header, Body: io.NopCloser(strings.NewReader(e.next.Body))}, nil
}

type rsfixRequest struct {
	Method  string              `json:"method"`
	Path    string              `json:"path"`
	Headers map[string][]string `json:"headers,omitempty"`
	Body    string              `json:"body"`
}

type rsfixCase struct {
	Name     string              `json:"name"`
	Server   string              `json:"server"`
	Request  rsfixRequest        `json:"request"`
	Upstream *rsfixUpstream      `json:"upstream,omitempty"`
	Status   int                 `json:"status"`
	Headers  map[string][]string `json:"headers"`
	Body     string              `json:"body"`
	Sent     []rsfixCaptured     `json:"sent"`
}

var (
	rsfixSecret  = regexp.MustCompile(`ek_[A-Za-z0-9_-]{43}`)
	rsfixSession = regexp.MustCompile(`sess_[A-Za-z0-9_-]{24}`)
	rsfixExpires = regexp.MustCompile(`"expires_at":\d+`)
)

func rsfixMask(text string, secrets map[string]string) string {
	for real, alias := range secrets {
		text = strings.ReplaceAll(text, real, alias)
	}
	text = rsfixSecret.ReplaceAllString(text, "ek_<secret>")
	text = rsfixSession.ReplaceAllString(text, "sess_<id>")
	return rsfixExpires.ReplaceAllString(text, `"expires_at":0`)
}

func newRSFixServer(t *testing.T, withCredentials bool) (*Server, *rsfixLiveExecutor) {
	t.Helper()
	gin.SetMode(gin.TestMode)
	cfg := &proxyconfig.Config{}
	cfg.APIKeys = []string{"good-key", "other-key"}
	tmp := t.TempDir()
	cfg.AuthDir = filepath.Join(tmp, "auth")
	_ = os.MkdirAll(cfg.AuthDir, 0o700)
	cfg.UsageStatisticsEnabled = false
	configaccess.Register(&cfg.SDKConfig)
	accessManager := sdkaccess.NewManager()
	accessManager.SetProviders(sdkaccess.RegisteredProviders())
	manager := auth.NewManager(nil, nil, nil)
	exec := &rsfixLiveExecutor{real: executor.NewCodexExecutor(cfg)}
	manager.RegisterExecutor(exec)
	if withCredentials {
		for _, credential := range []*auth.Auth{
			{ID: "a-codex-apikey", Provider: "codex", Status: auth.StatusActive, Attributes: map[string]string{auth.AttributeAPIKey: "sk-must-not-be-used"}},
			{ID: "b-codex-oauth", Provider: "codex", Status: auth.StatusActive, Attributes: map[string]string{"header:X-Operator": "op-value"}, Metadata: map[string]any{"access_token": "oauth-token", "account_id": "acct-1", "email": "user@example.com"}},
		} {
			if _, errRegister := manager.Register(context.Background(), credential); errRegister != nil {
				t.Fatal(errRegister)
			}
		}
	}
	return NewServer(cfg, manager, accessManager, filepath.Join(tmp, "config.yaml")), exec
}

func TestRSFixRealtimeHTTP(t *testing.T) {
	dir := os.Getenv("RSFIX_OUT")
	if dir == "" {
		t.Skip("RSFIX_OUT not set")
	}
	// Any dial that escapes the capture executor fails locally instead of reaching OpenAI.
	t.Setenv("HTTPS_PROXY", "http://127.0.0.1:9")
	t.Setenv("HTTP_PROXY", "http://127.0.0.1:9")
	t.Setenv("NO_PROXY", "")
	servers := map[string]*Server{}
	executors := map[string]*rsfixLiveExecutor{}
	servers["main"], executors["main"] = newRSFixServer(t, true)
	servers["empty"], executors["empty"] = newRSFixServer(t, false)
	secrets := map[string]string{}
	var cases []rsfixCase

	const sdp = "v=0\r\no=- 1 2 IN IP4 127.0.0.1\r\ns=-\r\n"
	const boundary = "rsfix-boundary"
	multipart := "--" + boundary + "\r\nContent-Disposition: form-data; name=\"sdp\"\r\n\r\n" + sdp + "\r\n--" + boundary +
		"\r\nContent-Disposition: form-data; name=\"session\"\r\n\r\n{\"type\":\"realtime\",\"model\":\"gpt-realtime\",\"instructions\":\"<hi>\"}\r\n--" + boundary + "--\r\n"
	good := map[string][]string{"Authorization": {"Bearer good-key"}}
	with := func(base map[string][]string, extra ...string) map[string][]string {
		out := map[string][]string{}
		for k, v := range base {
			out[k] = append([]string(nil), v...)
		}
		for i := 0; i+1 < len(extra); i += 2 {
			out[extra[i]] = append(out[extra[i]], extra[i+1])
		}
		return out
	}
	upgrade := []string{"Connection", "Upgrade", "Upgrade", "websocket", "Sec-WebSocket-Version", "13", "Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ=="}
	answer := &rsfixUpstream{Status: 201, Headers: map[string][]string{
		"Content-Type": {"application/sdp"}, "Location": {"/v1/live/call-123"}, "Set-Cookie": {"session=secret"},
		"X-Request-Id": {"req-1"}, "Openai-Request-Id": {"oreq-1"}, "Retry-After": {"3"}, "X-Live-Session": {"leak"},
	}, Body: "v=0\r\no=upstream-answer\r\n"}

	run := func(name, server string, request rsfixRequest, upstream *rsfixUpstream) string {
		exec := executors[server]
		exec.captured = nil
		if upstream != nil {
			exec.next = *upstream
		} else {
			exec.next = rsfixUpstream{Status: 599, Body: "unexpected upstream call"}
		}
		body := request.Body
		req := httptest.NewRequest(request.Method, request.Path, bytes.NewReader([]byte(body)))
		for name, values := range request.Headers {
			for _, value := range values {
				req.Header.Add(name, value)
			}
		}
		recorder := httptest.NewRecorder()
		servers[server].engine.ServeHTTP(recorder, req)
		headers := map[string][]string{}
		for name, values := range recorder.Header() {
			for _, value := range values {
				headers[name] = append(headers[name], rsfixMask(value, secrets))
			}
		}
		sent := []rsfixCaptured{}
		for _, captured := range exec.captured {
			for name, values := range captured.Headers {
				for i := range values {
					values[i] = rsfixMask(values[i], secrets)
				}
				captured.Headers[name] = values
			}
			captured.Body = rsfixMask(captured.Body, secrets)
			sent = append(sent, captured)
		}
		masked := request
		masked.Headers = map[string][]string{}
		for name, values := range request.Headers {
			for _, value := range values {
				masked.Headers[name] = append(masked.Headers[name], rsfixMask(value, secrets))
			}
		}
		cases = append(cases, rsfixCase{Name: name, Server: server, Request: masked, Upstream: upstream, Status: recorder.Code, Headers: headers, Body: rsfixMask(recorder.Body.String(), secrets), Sent: sent})
		return recorder.Body.String()
	}
	post := func(path string, headers map[string][]string, body string) rsfixRequest {
		return rsfixRequest{Method: http.MethodPost, Path: path, Headers: headers, Body: body}
	}
	get := func(path string, headers map[string][]string) rsfixRequest {
		return rsfixRequest{Method: http.MethodGet, Path: path, Headers: headers}
	}
	jsonHeaders := with(good, "Content-Type", "application/json")

	// Access control.
	run("live_missing_key", "main", post("/v1/live", nil, "{}"), nil)
	run("live_invalid_key", "main", post("/v1/live", map[string][]string{"Authorization": {"Bearer nope"}}, "{}"), nil)
	run("calls_missing_key", "main", post("/v1/realtime/calls", nil, "{}"), nil)
	run("calls_invalid_key", "main", post("/v1/realtime/calls", map[string][]string{"X-Api-Key": {"nope"}}, "{}"), nil)
	run("calls_unknown_client_secret", "main", post("/v1/realtime/calls", map[string][]string{"Authorization": {"Bearer ek_unknown"}}, "{}"), nil)
	run("secrets_reject_client_secret", "main", post("/v1/realtime/client_secrets", map[string][]string{"Authorization": {"Bearer ek_unknown"}}, "{}"), nil)

	// Call creation.
	run("live_json", "main", post("/v1/live", with(jsonHeaders,
		"OpenAI-Alpha", "quicksilver=v2", "Session-Id", "session-123", "Thread-Id", "thread-1", "Originator", "Codex Desktop",
		"X-Oai-Attestation", "attest", "X-Not-Forwarded", "x", "Openai-Project", "p1", "Openai-Project", "p2"),
		`{"sdp":"v=0\r\n","model":"gpt-realtime","session":{"instructions":"<hi>","model":"gpt-realtime-mini"}}`), answer)
	run("calls_multipart", "main", post("/v1/realtime/calls", with(good, "Content-Type", "multipart/form-data; boundary="+boundary), multipart),
		&rsfixUpstream{Status: 201, Headers: map[string][]string{"Location": {"https://chatgpt.com/backend-api/codex/realtime/calls/rtc_multi?x=1"}}, Body: "v=0\r\n"})
	run("realtime_raw_sdp_query_key", "main", post("/v1/realtime?key=good-key", map[string][]string{"Content-Type": {"application/sdp"}}, sdp),
		&rsfixUpstream{Status: 200, Headers: map[string][]string{"Content-Type": {"application/sdp"}, "Location": {"rtc_raw"}}, Body: "answer"})
	run("live_no_content_type", "main", post("/v1/live", good, `{"model":"custom-live"}`), &rsfixUpstream{Status: 201, Body: "v=0"})
	run("live_invalid_multipart", "main", post("/v1/live", with(good, "Content-Type", "multipart/form-data; boundary=x"), "--x\r\nContent-Disposition: form-data; name=\"session\"\r\n\r\n{}\r\n--x--\r\n"), nil)
	run("calls_invalid_multipart", "main", post("/v1/realtime/calls", with(good, "Content-Type", "multipart/form-data"), "x"), nil)
	run("calls_bad_session_json", "main", post("/v1/realtime/calls", jsonHeaders, `{"sdp":"x","session":"str"}`), nil)
	run("calls_null_session_panics", "main", post("/v1/realtime/calls", jsonHeaders, `{"sdp":"x","session":null}`), nil)
	run("live_upstream_unauthorized", "main", post("/v1/live", jsonHeaders, `{"sdp":"x"}`),
		&rsfixUpstream{Status: 401, Headers: map[string][]string{"Content-Type": {"application/json"}, "Www-Authenticate": {"Bearer"}}, Body: `{"error":{"message":"expired"}}`})
	run("calls_upstream_rate_limited", "main", post("/v1/realtime/calls", jsonHeaders, `{"sdp":"x"}`),
		&rsfixUpstream{Status: 429, Headers: map[string][]string{"Retry-After": {"7"}, "Content-Type": {"application/json"}, "Location": {"/v1/live/ignored"}}, Body: `{"error":"slow down"}`})
	run("live_success_without_location", "main", post("/v1/live", jsonHeaders, `{"sdp":"x"}`), &rsfixUpstream{Status: 201, Headers: map[string][]string{"Content-Type": {"application/sdp"}}, Body: "v=0"})
	run("live_no_credentials", "empty", post("/v1/live", jsonHeaders, `{"sdp":"x"}`), nil)
	run("calls_no_credentials", "empty", post("/v1/realtime/calls", jsonHeaders, `{"sdp":"x"}`), nil)

	// Client secrets and legacy sessions.
	created := run("secret_create", "main", post("/v1/realtime/client_secrets", jsonHeaders,
		`{"session":{"type":"realtime","model":"gpt-realtime","instructions":"<help>","n":1.50},"expires_after":{"anchor":"created_at","seconds":60}}`), nil)
	var secret struct {
		Value string `json:"value"`
	}
	_ = json.Unmarshal([]byte(created), &secret)
	secrets[secret.Value] = "ek_<secret-1>"
	run("secret_default_session", "main", post("/v1/realtime/client_secrets", good, ``), nil)
	run("secret_invalid_json", "main", post("/v1/realtime/client_secrets", good, `{"session":`), nil)
	run("secret_bad_session", "main", post("/v1/realtime/client_secrets", good, `{"session":[1]}`), nil)
	run("secret_bad_anchor", "main", post("/v1/realtime/client_secrets", good, `{"expires_after":{"anchor":"now","seconds":60}}`), nil)
	run("secret_bad_seconds", "main", post("/v1/realtime/client_secrets", good, `{"expires_after":{"seconds":5}}`), nil)
	run("secret_unsupported", "main", post("/v1/realtime/client_secrets", good, `{"session":{"type":"transcription"}}`), nil)
	run("secret_too_large", "main", post("/v1/realtime/client_secrets", good, strings.Repeat(" ", 64<<10+1)), nil)
	run("legacy_session", "main", post("/v1/realtime/sessions", good, `{"model":"gpt-realtime-mini","voice":"alloy"}`), nil)
	run("legacy_session_array", "main", post("/v1/realtime/sessions", good, `[]`), nil)

	// Calls with the ephemeral key.
	ek := map[string][]string{"Authorization": {"Bearer " + secret.Value}}
	run("ek_call_sdp", "main", post("/v1/realtime/calls", with(ek, "Content-Type", "application/sdp"), sdp),
		&rsfixUpstream{Status: 201, Headers: map[string][]string{"Location": {"/v1/realtime/calls/call-ek"}, "Content-Type": {"application/sdp"}}, Body: "v=0 ek"})
	run("ek_hangup_rejected", "main", post("/v1/realtime/calls/call-ek/hangup", ek, ""), nil)
	run("ek_direct_model_mismatch", "main", get("/v1/realtime?model=gpt-4o", with(ek, upgrade...)), nil)
	run("ek_translation", "main", get("/v1/realtime/translations", ek), nil)

	// Sideband and direct WebSocket checks that end before any upstream dial.
	run("live_sideband_no_upgrade", "main", get("/v1/live/call-123", good), nil)
	run("calls_sideband_no_upgrade", "main", get("/v1/realtime/calls/call-123", good), nil)
	run("realtime_query_sideband_no_upgrade", "main", get("/v1/realtime?call_id=call-123", good), nil)
	run("realtime_direct_no_upgrade", "main", get("/v1/realtime", good), nil)
	run("sideband_invalid_call_id", "main", get("/v1/live/"+strings.Repeat("a", 129), with(good, upgrade...)), nil)
	run("sideband_query_invalid_call_id", "main", get("/v1/realtime?call_id=a%20b", with(good, upgrade...)), nil)
	run("sideband_unknown_call", "main", get("/v1/realtime/calls/call-unknown", with(good, upgrade...)), nil)
	// Never reach a sideband dial here: the dialer targets the real API base. Success paths
	// run in the live package against a local WebSocket server.
	run("sideband_ek_principal_mismatch", "main", get("/v1/realtime/calls/call-123", with(ek, upgrade...)), nil)

	// Hangup.
	run("hangup_invalid_call_id", "main", post("/v1/realtime/calls/a%20b/hangup", good, ""), nil)
	run("hangup_unknown_call", "main", post("/v1/realtime/calls/call-unknown/hangup", good, ""), nil)
	run("hangup_other_principal", "main", post("/v1/realtime/calls/call-123/hangup", map[string][]string{"Authorization": {"Bearer other-key"}}, ""), nil)
	run("hangup_upstream_error_keeps_call", "main", post("/v1/realtime/calls/call-123/hangup", with(good, "Content-Type", "application/json", "OpenAI-Alpha", "v"), `{"reason":"bye"}`),
		&rsfixUpstream{Status: 500, Headers: map[string][]string{"Content-Type": {"application/json"}, "X-Request-Id": {"h-1"}}, Body: `{"error":"boom"}`})
	run("hangup", "main", post("/v1/realtime/calls/call-123/hangup", good, ""), &rsfixUpstream{Status: 200, Headers: map[string][]string{"Content-Type": {"application/json"}}, Body: `{"status":"ok"}`})
	run("hangup_again_not_found", "main", post("/v1/realtime/calls/call-123/hangup", good, ""), nil)
	run("hangup_ek_call", "main", post("/v1/realtime/calls/call-ek/hangup", good, ""), &rsfixUpstream{Status: 204})

	// Capability stubs.
	run("transcription_sessions", "main", post("/v1/realtime/transcription_sessions", good, "{}"), nil)
	run("translations_get", "main", get("/v1/realtime/translations", good), nil)
	run("translations_post", "main", post("/v1/realtime/translations", good, "{}"), nil)
	run("translations_client_secrets", "main", post("/v1/realtime/translations/client_secrets", good, "{}"), nil)
	for _, action := range []string{"accept", "reject", "refer"} {
		run("sip_"+action, "main", post("/v1/realtime/calls/call-1/"+action, good, "{}"), nil)
	}
	run("sip_without_key", "main", post("/v1/realtime/calls/call-1/accept", nil, "{}"), nil)

	encoded, errMarshal := json.MarshalIndent(cases, "", " ")
	if errMarshal != nil {
		t.Fatal(errMarshal)
	}
	if errWrite := os.WriteFile(filepath.Join(dir, "realtime_http_go.json"), encoded, 0o644); errWrite != nil {
		t.Fatal(errWrite)
	}
}
