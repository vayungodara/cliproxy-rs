// Generates goldens for the xAI OAuth port by running the pinned Go code (CLIProxyAPI
// 6fecc6e): the real login manager with the xAI authenticator and FileTokenStore, the
// real XAIExecutor.Refresh, ValidateOAuthEndpoint and CredentialFileName. Requests to
// https://auth.x.ai are rewritten to a local raw-TCP capture server; nothing contacts xAI.
package main

import (
	"bufio"
	"bytes"
	"context"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"regexp"
	"strconv"
	"strings"
	"sync"
	"time"

	xaiauth "github.com/router-for-me/CLIProxyAPI/v8/internal/auth/xai"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/runtime/executor"
	sdkauth "github.com/router-for-me/CLIProxyAPI/v8/sdk/auth"
	cliproxyauth "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/auth"
)

// reply is one scripted answer, served in order to requests for Path.
type reply struct {
	Path   string `json:"path"`
	Status int    `json:"status"`
	Body   string `json:"body"`
}

type loginCase struct {
	Name   string  `json:"name"`
	Script []reply `json:"script"`
	// Existing is a credential file already at the target path (JSON text).
	Existing     string   `json:"existing,omitempty"`
	ExistingName string   `json:"existing_name,omitempty"`
	Requests     []string `json:"requests"`
	FileName     string   `json:"file_name,omitempty"`
	File         string   `json:"file,omitempty"`
	Label        string   `json:"label,omitempty"`
	Error        string   `json:"error,omitempty"`
}

type refreshCase struct {
	Name       string            `json:"name"`
	Metadata   map[string]any    `json:"metadata"`
	Attributes map[string]string `json:"attributes,omitempty"`
	Script     []reply           `json:"script"`
	Requests   []string          `json:"requests"`
	// MetadataOut and AttributesOut are the credential after a successful refresh.
	MetadataOut   map[string]any    `json:"metadata_out,omitempty"`
	AttributesOut map[string]string `json:"attributes_out,omitempty"`
	Error         string            `json:"error,omitempty"`
}

type fixture struct {
	// Sprint is xaiMetadataString's fmt.Sprint of JSON-decoded metadata values.
	Sprint [][2]string `json:"sprint"`
	// Expiry is buildTokenData's expiry for expires_in at Unix 1e9.
	Expiry    [][2]any      `json:"expiry"`
	Validate  [][3]any      `json:"validate"`
	FileNames [][3]string   `json:"file_names"`
	Login     []loginCase   `json:"login"`
	Refresh   []refreshCase `json:"refresh"`
}

// server is a one-request-per-connection HTTP/1.1 capture server.
type server struct {
	ln       net.Listener
	mu       sync.Mutex
	script   []reply
	requests []string
}

func newServer() *server {
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		panic(err)
	}
	s := &server{ln: ln}
	go s.serve()
	return s
}

func (s *server) addr() string { return s.ln.Addr().String() }

func (s *server) reset(script []reply) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.script = append([]reply(nil), script...)
	s.requests = nil
}

func (s *server) taken() []string {
	s.mu.Lock()
	defer s.mu.Unlock()
	out := make([]string, len(s.requests))
	for i, r := range s.requests {
		out[i] = strings.ReplaceAll(r, s.addr(), "UPSTREAM")
	}
	return out
}

func (s *server) serve() {
	for {
		conn, err := s.ln.Accept()
		if err != nil {
			return
		}
		go s.handle(conn)
	}
}

func (s *server) handle(conn net.Conn) {
	defer conn.Close()
	reader := bufio.NewReader(conn)
	var raw bytes.Buffer
	length := 0
	path := ""
	for {
		line, err := reader.ReadString('\n')
		if err != nil {
			return
		}
		raw.WriteString(line)
		if path == "" {
			if parts := strings.Fields(line); len(parts) >= 2 {
				path = parts[1]
			}
		}
		lower := strings.ToLower(line)
		if strings.HasPrefix(lower, "content-length:") {
			length, _ = strconv.Atoi(strings.TrimSpace(line[len("content-length:"):]))
		}
		if line == "\r\n" {
			break
		}
	}
	body := make([]byte, length)
	if _, err := io.ReadFull(reader, body); err != nil {
		return
	}
	raw.Write(body)
	s.mu.Lock()
	s.requests = append(s.requests, raw.String())
	answer := reply{Status: 599, Body: "unscripted"}
	for i, r := range s.script {
		if r.Path == path {
			answer = r
			s.script = append(s.script[:i], s.script[i+1:]...)
			break
		}
	}
	s.mu.Unlock()
	fmt.Fprintf(conn, "HTTP/1.1 %d %s\r\nContent-Type: application/json\r\nContent-Length: %d\r\nConnection: close\r\n\r\n%s",
		answer.Status, http.StatusText(answer.Status), len(answer.Body), answer.Body)
}

// rewrite sends https://auth.x.ai traffic to the capture server.
type rewrite struct {
	target string
	inner  http.RoundTripper
}

func (r rewrite) RoundTrip(req *http.Request) (*http.Response, error) {
	if req.URL.Hostname() == "auth.x.ai" {
		clone := req.Clone(req.Context())
		clone.URL.Scheme = "http"
		clone.URL.Host = r.target
		clone.Host = ""
		return r.inner.RoundTrip(clone)
	}
	return r.inner.RoundTrip(req)
}

func jwt(claims string) string {
	enc := base64.RawURLEncoding
	return enc.EncodeToString([]byte(`{"alg":"none"}`)) + "." + enc.EncodeToString([]byte(claims)) + ".sig"
}

const discoveryOK = `{"issuer":"https://auth.x.ai","device_authorization_endpoint":"https://auth.x.ai/oauth2/device/code","token_endpoint":"https://auth.x.ai/oauth2/token"}`
const deviceOK = `{"device_code":"dev-1","user_code":"ABCD-EFGH","verification_uri":"https://accounts.x.ai/device","verification_uri_complete":"https://accounts.x.ai/device?user_code=ABCD-EFGH","expires_in":600,"interval":0}`

var timestamp = regexp.MustCompile(`\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ`)

func normalize(s string) string { return timestamp.ReplaceAllString(s, "TIME") }

func loginCases() []loginCase {
	idFull := jwt(`{"email":" Dev.User+test@Example.com ","sub":"user-42"}`)
	idSub := jwt(`{"sub":"acct|7"}`)
	return []loginCase{
		{Name: "success_after_pending", Script: []reply{
			{"/.well-known/openid-configuration", 200, discoveryOK},
			{"/oauth2/device/code", 200, deviceOK},
			{"/oauth2/token", 400, `{"error":"authorization_pending"}`},
			{"/oauth2/token", 200, `{"access_token":" at-1 ","refresh_token":"rt-1","id_token":"` + idFull + `","token_type":"Bearer","expires_in":3600}`},
		}},
		{Name: "subject_only_merges_existing_file", ExistingName: "xai-acct-7.json",
			Existing: `{"type":"xai","access_token":"old","refresh_token":"old-rt","expired":"2020-01-01T00:00:00Z","prefix":"team","proxy-url":"socks5://127.0.0.1:1080","disabled":true,"priority":1e1,"note":"<kept>"}`,
			Script: []reply{
				{"/.well-known/openid-configuration", 200, discoveryOK},
				{"/oauth2/device/code", 200, `{"device_code":"dev-2","user_code":"WXYZ","verification_uri":"https://accounts.x.ai/device"}`},
				{"/oauth2/token", 200, `{"access_token":"at-2","id_token":"` + idSub + `"}`},
			}},
		{Name: "slow_down_then_denied", Script: []reply{
			{"/.well-known/openid-configuration", 200, discoveryOK},
			{"/oauth2/device/code", 200, deviceOK},
			{"/oauth2/token", 400, `{"error":"slow_down"}`},
			{"/oauth2/token", 400, `{"error":"access_denied"}`},
		}},
		{Name: "expired_token", Script: []reply{
			{"/.well-known/openid-configuration", 200, discoveryOK},
			{"/oauth2/device/code", 200, deviceOK},
			{"/oauth2/token", 400, `{"error":"expired_token"}`},
		}},
		{Name: "token_error_with_description", Script: []reply{
			{"/.well-known/openid-configuration", 200, discoveryOK},
			{"/oauth2/device/code", 200, deviceOK},
			{"/oauth2/token", 400, `{"error":"invalid_grant","error_description":" code reused "}`},
		}},
		{Name: "token_error_without_description", Script: []reply{
			{"/.well-known/openid-configuration", 200, discoveryOK},
			{"/oauth2/device/code", 200, deviceOK},
			{"/oauth2/token", 400, `{"error":"server_error","error_description":"  "}`},
		}},
		{Name: "token_status_without_error_field", Script: []reply{
			{"/.well-known/openid-configuration", 200, discoveryOK},
			{"/oauth2/device/code", 200, deviceOK},
			{"/oauth2/token", 500, `{"message":" busy "}`},
		}},
		{Name: "token_missing_access_token", Script: []reply{
			{"/.well-known/openid-configuration", 200, discoveryOK},
			{"/oauth2/device/code", 200, deviceOK},
			{"/oauth2/token", 200, `{"access_token":"  ","refresh_token":"rt"}`},
		}},
		{Name: "discovery_status_error", Script: []reply{
			{"/.well-known/openid-configuration", 503, " maintenance \n"},
		}},
		{Name: "discovery_http_device_endpoint", Script: []reply{
			{"/.well-known/openid-configuration", 200, `{"device_authorization_endpoint":"http://auth.x.ai/oauth2/device/code","token_endpoint":"https://auth.x.ai/oauth2/token"}`},
		}},
		{Name: "discovery_foreign_token_host", Script: []reply{
			{"/.well-known/openid-configuration", 200, `{"device_authorization_endpoint":"https://auth.x.ai/oauth2/device/code","token_endpoint":"https://x.ai.example.com/oauth2/token"}`},
		}},
		{Name: "discovery_missing_token_endpoint", Script: []reply{
			{"/.well-known/openid-configuration", 200, `{"device_authorization_endpoint":"https://auth.x.ai/oauth2/device/code","token_endpoint":null}`},
		}},
		{Name: "device_missing_user_code", Script: []reply{
			{"/.well-known/openid-configuration", 200, discoveryOK},
			{"/oauth2/device/code", 200, `{"device_code":"d","verification_uri":"https://accounts.x.ai/device"}`},
		}},
		{Name: "device_missing_verification_uri", Script: []reply{
			{"/.well-known/openid-configuration", 200, discoveryOK},
			{"/oauth2/device/code", 200, `{"device_code":"d","user_code":"u","verification_uri":" "}`},
		}},
		{Name: "token_keys_fold_and_last_duplicate_wins", Script: []reply{
			{"/.well-known/openid-configuration", 200, `{"Device_Authorization_Endpoint":"https://auth.x.ai/oauth2/device/code","token_endpoint":"https://evil.example/t","TOKEN_ENDPOINT":"https://auth.x.ai/oauth2/token"}`},
			{"/oauth2/device/code", 200, `{"DEVICE_CODE":"dev-3","user_code":"U","verification_uri":"https://accounts.x.ai/device","interval":null,"expires_in":null}`},
			{"/oauth2/token", 200, `{"ACCESS_TOKEN":"at-ci","Refresh_Token":"rt-ci","expires_in":10,"expires_in":20,"error":null,"extra":[1,{"a":2}]}`},
		}},
		{Name: "discovery_not_json", Script: []reply{
			{"/.well-known/openid-configuration", 200, `not json`},
		}},
		{Name: "discovery_top_level_array", Script: []reply{
			{"/.well-known/openid-configuration", 200, `["https://auth.x.ai/a"]`},
		}},
		{Name: "discovery_top_level_null", Script: []reply{
			{"/.well-known/openid-configuration", 200, `null`},
		}},
		{Name: "device_interval_wrong_type", Script: []reply{
			{"/.well-known/openid-configuration", 200, discoveryOK},
			{"/oauth2/device/code", 200, `{"device_code":"d","user_code":"u","verification_uri":"https://accounts.x.ai/device","interval":"5"}`},
		}},
		{Name: "token_expires_in_float", Script: []reply{
			{"/.well-known/openid-configuration", 200, discoveryOK},
			{"/oauth2/device/code", 200, deviceOK},
			{"/oauth2/token", 200, `{"access_token":"a","expires_in":1.5}`},
		}},
		{Name: "token_body_not_json", Script: []reply{
			{"/.well-known/openid-configuration", 200, discoveryOK},
			{"/oauth2/device/code", 200, deviceOK},
			{"/oauth2/token", 502, `<html>bad gateway</html>`},
		}},
		{Name: "jwt_trailing_bits_names_file", Script: []reply{
			{"/.well-known/openid-configuration", 200, discoveryOK},
			{"/oauth2/device/code", 200, deviceOK},
			{"/oauth2/token", 200, `{"access_token":"a","id_token":"a.eyJzdWIiOiJ1In1.sig"}`},
		}},
		{Name: "jwt_identity_from_untrimmed_token", Script: []reply{
			{"/.well-known/openid-configuration", 200, discoveryOK},
			{"/oauth2/device/code", 200, deviceOK},
			{"/oauth2/token", 200, `{"access_token":"a","id_token":"a.eyJzdWIiOiJ1In0 "}`},
		}},
		{Name: "existing_invalid_weight_blocks_save", ExistingName: "xai-w.json",
			Existing: `{"type":"xai","weight":1000001}`,
			Script: []reply{
				{"/.well-known/openid-configuration", 200, discoveryOK},
				{"/oauth2/device/code", 200, deviceOK},
				{"/oauth2/token", 200, `{"access_token":"a","id_token":"` + jwt(`{"sub":"w"}`) + `"}`},
			}},
		{Name: "device_expires_in_overflows", Script: []reply{
			{"/.well-known/openid-configuration", 200, discoveryOK},
			{"/oauth2/device/code", 200, `{"device_code":"d","user_code":"u","verification_uri":"https://accounts.x.ai/device","expires_in":9223372037}`},
			{"/oauth2/token", 400, `{"error":"authorization_pending"}`},
			{"/oauth2/token", 200, `{"access_token":"never"}`},
		}},
		{Name: "token_expires_in_overflows", Script: []reply{
			{"/.well-known/openid-configuration", 200, discoveryOK},
			{"/oauth2/device/code", 200, deviceOK},
			{"/oauth2/token", 200, `{"access_token":"a","expires_in":9223372036854775807}`},
		}},
		{Name: "device_status_error", Script: []reply{
			{"/.well-known/openid-configuration", 200, discoveryOK},
			{"/oauth2/device/code", 429, `{"error":"slow"}`},
		}},
	}
}

func refreshCases() []refreshCase {
	id := jwt(`{"email":"new@example.com","sub":"user-9"}`)
	return []refreshCase{
		{Name: "explicit_endpoint", Metadata: map[string]any{"type": "xai", "refresh_token": " rt-old ", "token_endpoint": "http://UPSTREAM/oauth2/token", "base_url": "", "email": "old@example.com"},
			Script: []reply{{"/oauth2/token", 200, `{"access_token":"at-new","refresh_token":"rt-new","id_token":"` + id + `","token_type":"Bearer","expires_in":7200}`}}},
		{Name: "keeps_fields_server_omits", Metadata: map[string]any{"type": "xai", "refresh_token": "rt-old", "token_endpoint": "http://UPSTREAM/oauth2/token", "base_url": "https://custom.example/v1", "id_token": "old-id"},
			Attributes: map[string]string{"base_url": "https://custom.example/v1"},
			Script:     []reply{{"/oauth2/token", 200, `{"access_token":"at-new"}`}}},
		{Name: "discovers_endpoint", Metadata: map[string]any{"type": "xai", "refresh_token": "rt-old"},
			Script: []reply{
				{"/.well-known/openid-configuration", 200, discoveryOK},
				{"/oauth2/token", 200, `{"access_token":"at-new","expires_in":60}`},
			}},
		{Name: "no_refresh_token", Metadata: map[string]any{"type": "xai", "access_token": "at"}},
		{Name: "numeric_refresh_token_and_bool_base_url", Metadata: map[string]any{"type": "xai", "refresh_token": float64(100000000), "token_endpoint": "http://UPSTREAM/oauth2/token", "base_url": true},
			Script: []reply{{"/oauth2/token", 200, `{"access_token":"at-new"}`}}},
		{Name: "status_error", Metadata: map[string]any{"type": "xai", "refresh_token": "rt-old", "token_endpoint": "http://UPSTREAM/oauth2/token"},
			Script: []reply{{"/oauth2/token", 401, `{"error":"invalid_grant"}`}}},
		{Name: "missing_access_token", Metadata: map[string]any{"type": "xai", "refresh_token": "rt-old", "token_endpoint": "http://UPSTREAM/oauth2/token"},
			Script: []reply{{"/oauth2/token", 200, `{"refresh_token":"rt-2"}`}}},
		{Name: "discovery_rejected", Metadata: map[string]any{"type": "xai", "refresh_token": "rt-old"},
			Script: []reply{{"/.well-known/openid-configuration", 200, `{"device_authorization_endpoint":"https://auth.x.ai/d","token_endpoint":"https://auth.x.ai.evil.com/t"}`}}},
	}
}

func main() {
	srv := newServer()
	http.DefaultTransport = rewrite{target: srv.addr(), inner: http.DefaultTransport}
	var out fixture

	for _, raw := range []string{
		"", "   ", "https://auth.x.ai/oauth2/token", " https://x.ai/t ", "https://AUTH.X.AI/t", "HTTPS://auth.x.ai/t",
		"http://auth.x.ai/t", "https://evilx.ai/t", "https://x.ai.example.com/t", "https://auth.x.ai:8443/t",
		"https://user:pw@auth.x.ai/t", "https://auth.x.ai./t", "https://[::1]/t", "ftp://x.ai/t", "https://%61uth.x.ai/t",
		"https://auth.x.ai\\@evil.com/t", "https://auth.x.ai\t/t", "https://auth.x.ai/t\n", "https:auth.x.ai/t",
		"https:///auth.x.ai/t", "//auth.x.ai/t", "https://auth.x.ai#frag", "https://auth.x.ai?q=1", "https://a_b.x.ai/t",
		"https://xn--fsq.x.ai/t", "https://例.x.ai/t", "https://auth.x.ai:/t", "https://auth.x.ai:99999/t", "https://auth.x.ai%2f.evil.com/t",
		"https://auth.x.ai:80:90/t", "https://[fe80::1%25en0]/t", "https://[1.2.3.4]/t", "https://[::ffff:1.2.3.4]/t", "*", ":foo",
		"https://auth.x.ai/%zz", "https://auth.x.ai/#%zz", "https://auth.x.ai/#ok%41", "https://a%25b.x.ai/t", "https://user%zz@auth.x.ai/t",
		"https://[::1/t", "https://foo%e4%be%8b.x.ai/t", "https://auth.x.ai/%e4", "https://auth.x.ai/t?%zz", "https://AUTH.x.AI",
		"https://auth.x.ai%", "https://auth.x.ai%4", "https://u:p@w@auth.x.ai/t", "https://auth.x.ai /t", "https://x.ai:443",
		"https://sub.X.Ai./t", "https:/auth.x.ai/t", "https://auth\u00a0.x.ai/t", "https://[::1]:8443/t", "https://[::1]x/t",
		"postgres://a:1,b:2/t", "postgresql://a:1:2/t", "https://a:1,b:2/t", "https://attacker.example\\[::1%25.x.ai]/oauth2/token",
	} {
		value, err := xaiauth.ValidateOAuthEndpoint(raw, "token_endpoint")
		if err != nil {
			out.Validate = append(out.Validate, [3]any{raw, false, err.Error()})
		} else {
			out.Validate = append(out.Validate, [3]any{raw, true, value})
		}
	}
	for _, pair := range [][2]string{
		{"a@b.com", "s"}, {"  A.B+c@x.ai ", ""}, {"", "user|1"}, {"é@x.ai", ""}, {"---", "sub"}, {"-a b-", ""}, {"", " -z- "},
	} {
		out.FileNames = append(out.FileNames, [3]string{pair[0], pair[1], xaiauth.CredentialFileName(pair[0], pair[1])})
	}

	for _, raw := range []string{`"  s  "`, `123`, `100000000`, `1e21`, `1e20`, `123456`, `1234567`, `0.0001`, `0.00001`, `-2.5`, `1.5e300`, `true`, `false`, `[1,"a",null]`, `{"b":1,"a":"x"}`, `0`, `-0`} {
		var v any
		if err := json.Unmarshal([]byte(raw), &v); err != nil {
			panic(err)
		}
		out.Sprint = append(out.Sprint, [2]string{raw, strings.TrimSpace(fmt.Sprint(v))})
	}
	for _, v := range []int64{3600, 1, 9223372036854775807, 9223372037, 1 << 40} {
		at := time.Unix(1_000_000_000, 0).Add(time.Duration(v) * time.Second).UTC().Format(time.RFC3339)
		out.Expiry = append(out.Expiry, [2]any{v, at})
	}
	for _, c := range loginCases() {
		dir, err := os.MkdirTemp("", "xai-login-")
		if err != nil {
			panic(err)
		}
		if c.Existing != "" {
			if err = os.WriteFile(filepath.Join(dir, c.ExistingName), []byte(c.Existing), 0o600); err != nil {
				panic(err)
			}
		}
		srv.reset(c.Script)
		manager := sdkauth.NewManager(sdkauth.NewFileTokenStore(), sdkauth.NewXAIAuthenticator())
		cfg := &config.Config{AuthDir: dir}
		record, savedPath, errLogin := manager.Login(context.Background(), "xai", cfg, &sdkauth.LoginOptions{NoBrowser: true})
		c.Requests = srv.taken()
		if errLogin != nil {
			c.Error = errLogin.Error()
		} else {
			c.FileName = filepath.Base(savedPath)
			data, errRead := os.ReadFile(savedPath)
			if errRead != nil {
				panic(errRead)
			}
			c.File = normalize(string(data))
			c.Label = record.Label
		}
		out.Login = append(out.Login, c)
	}

	exec := executor.NewXAIExecutor(&config.Config{})
	for _, c := range refreshCases() {
		srv.reset(c.Script)
		metadata := map[string]any{}
		for k, v := range c.Metadata {
			if s, ok := v.(string); ok {
				v = strings.ReplaceAll(s, "UPSTREAM", srv.addr())
			}
			metadata[k] = v
		}
		attrs := map[string]string{}
		for k, v := range c.Attributes {
			attrs[k] = v
		}
		auth := &cliproxyauth.Auth{ID: "xai-test.json", Provider: "xai", Metadata: metadata, Attributes: attrs}
		updated, errRefresh := exec.Refresh(context.Background(), auth)
		c.Requests = srv.taken()
		if errRefresh != nil {
			c.Error = errRefresh.Error()
		} else {
			c.MetadataOut = map[string]any{}
			for k, v := range updated.Metadata {
				if s, ok := v.(string); ok {
					v = normalize(strings.ReplaceAll(s, srv.addr(), "UPSTREAM"))
				}
				c.MetadataOut[k] = v
			}
			c.AttributesOut = updated.Attributes
		}
		out.Refresh = append(out.Refresh, c)
	}

	data, err := json.MarshalIndent(out, "", "  ")
	if err != nil {
		panic(err)
	}
	if err = os.WriteFile(os.Args[1], append(data, '\n'), 0o644); err != nil {
		panic(err)
	}
}
