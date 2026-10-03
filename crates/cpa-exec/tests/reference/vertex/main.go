// Generates byte-level goldens for the Vertex executor (service-account and API-key
// paths), the service-account key normalization and `-vertex-import`, by running the
// pinned Go code against local capture servers. It never contacts Google: Google hosts
// are reached through a local CONNECT proxy that terminates TLS with a generated test
// CA, which the Go process trusts through SSL_CERT_FILE. Every key is fake.
package main

import (
	"bufio"
	"bytes"
	"context"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/rsa"
	"crypto/tls"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/json"
	"encoding/pem"
	"fmt"
	log "github.com/sirupsen/logrus"
	"io"
	"math/big"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"sync"
	"time"
	"unicode/utf8"

	vertexauth "github.com/router-for-me/CLIProxyAPI/v8/internal/auth/vertex"
	internalcmd "github.com/router-for-me/CLIProxyAPI/v8/internal/cmd"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/modelconfig"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/registry"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/runtime/executor"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/thinking"
	// Production registers every translator through this package (cmd/server/main.go).
	_ "github.com/router-for-me/CLIProxyAPI/v8/internal/translator"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/util"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/watcher/synthesizer"
	cliproxyauth "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/auth"
	cliproxyexecutor "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/executor"
	cliproxysession "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/session"
	coreusage "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/usage"
	sdktranslator "github.com/router-for-me/CLIProxyAPI/v8/sdk/translator"
)

type reply struct {
	Status  int         `json:"status"`
	Headers [][2]string `json:"headers,omitempty"`
	Body    string      `json:"body"`
}

type errOut struct {
	Status  int    `json:"status"`
	Message string `json:"message"`
}

type scenario struct {
	Name string `json:"name"`
	// Config is YAML; UPSTREAM is the plain capture server, PROXY the CONNECT proxy.
	Config string `json:"config,omitempty"`
	// ConfigAuth selects the Nth synthesized vertex credential; -1 uses Metadata.
	ConfigAuth int `json:"config_auth"`
	// Metadata is a vertex auth file; SA_KEY in its service_account private_key is
	// replaced with the scenario key.
	Metadata map[string]any `json:"metadata,omitempty"`
	// Key names the private key variant in "keys".
	Key            string            `json:"key,omitempty"`
	Model          string            `json:"model"`
	RequestedModel string            `json:"requested_model,omitempty"`
	Payload        string            `json:"payload"`
	Source         string            `json:"source"`
	Op             string            `json:"op"`
	Alt            string            `json:"alt,omitempty"`
	Headers        map[string]string `json:"headers,omitempty"`
	Session        string            `json:"session,omitempty"`
	// Resolved is the model info the conductor binds (ExecRequest.resolved_model).
	Resolved *resolvedRecord `json:"resolved,omitempty"`
	// Via is "plain" for the capture server at UPSTREAM, else the TLS proxy.
	Via string `json:"via,omitempty"`
	// Replies answer the upstream connections in order (token exchange first).
	Replies []reply `json:"replies,omitempty"`

	// Requests are the raw HTTP requests received, in order. Addresses are UPSTREAM
	// and PROXY.
	Requests []string `json:"requests,omitempty"`
	Output   string   `json:"output,omitempty"`
	Chunks   []string `json:"chunks,omitempty"`
	Error    *errOut  `json:"error,omitempty"`
	// Usage is the record the executor's UsageReporter published, if any.
	Usage *usageOut `json:"usage,omitempty"`
}

// usageOut is the part of a published usage.Record the executor decides: the parsed
// upstream usage, the observed response model and the translated reasoning effort.
type usageOut struct {
	InputTokens         int64  `json:"input_tokens"`
	OutputTokens        int64  `json:"output_tokens"`
	ReasoningTokens     int64  `json:"reasoning_tokens"`
	CachedTokens        int64  `json:"cached_tokens"`
	CacheReadTokens     int64  `json:"cache_read_tokens"`
	CacheCreationTokens int64  `json:"cache_creation_tokens"`
	TotalTokens         int64  `json:"total_tokens"`
	ResponseModel       string `json:"response_model"`
	ReasoningEffort     string `json:"reasoning_effort"`
	Failed              bool   `json:"failed"`
}

// usageCapture receives every record the default usage manager dispatches.
type usageCapture chan coreusage.Record

func (c usageCapture) HandleUsage(_ context.Context, record coreusage.Record) { c <- record }

var captured = make(usageCapture, 16)

// awaitUsage returns the scenario's record (its trace ID is the scenario name), or nil
// when the executor published none.
func awaitUsage(name string) *usageOut {
	select {
	case r := <-captured:
		if r.TraceID != name {
			panic(fmt.Sprintf("%s: usage record of %q", name, r.TraceID))
		}
		d := r.Detail
		return &usageOut{
			InputTokens: d.InputTokens, OutputTokens: d.OutputTokens, ReasoningTokens: d.ReasoningTokens,
			CachedTokens: d.CachedTokens, CacheReadTokens: d.CacheReadTokens, CacheCreationTokens: d.CacheCreationTokens,
			TotalTokens: d.TotalTokens, ResponseModel: r.ResponseModel, ReasoningEffort: r.ReasoningEffort, Failed: r.Failed,
		}
	case <-time.After(300 * time.Millisecond):
		return nil
	}
}

// capture answers connections in order with the scripted replies. With tlsConfig set
// every connection is a CONNECT tunnel that is then TLS-terminated.
type capture struct {
	ln        net.Listener
	tlsConfig *tls.Config
	mu        sync.Mutex
	replies   []reply
	requests  []string
	wg        sync.WaitGroup
}

func newCapture(tlsConfig *tls.Config) *capture {
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		panic(err)
	}
	c := &capture{ln: ln, tlsConfig: tlsConfig}
	go c.serve()
	return c
}

func (c *capture) addr() string { return c.ln.Addr().String() }

func (c *capture) script(replies []reply) {
	c.mu.Lock()
	c.replies = append([]reply(nil), replies...)
	c.requests = nil
	c.mu.Unlock()
}

func (c *capture) take() []string {
	c.wg.Wait()
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.requests
}

func (c *capture) serve() {
	for {
		conn, err := c.ln.Accept()
		if err != nil {
			return
		}
		c.mu.Lock()
		if len(c.replies) == 0 {
			c.mu.Unlock()
			_ = conn.Close()
			continue
		}
		next := c.replies[0]
		c.replies = c.replies[1:]
		c.wg.Add(1)
		c.mu.Unlock()
		c.handle(conn, next)
	}
}

func readRequest(r *bufio.Reader) string {
	var raw bytes.Buffer
	length := 0
	for {
		line, err := r.ReadString('\n')
		raw.WriteString(line)
		if err != nil {
			return raw.String()
		}
		if strings.HasPrefix(strings.ToLower(line), "content-length:") {
			length, _ = strconv.Atoi(strings.TrimSpace(line[len("content-length:"):]))
		}
		if line == "\r\n" {
			break
		}
	}
	body := make([]byte, length)
	_, _ = io.ReadFull(r, body)
	raw.Write(body)
	return raw.String()
}

func (c *capture) handle(conn net.Conn, rep reply) {
	defer c.wg.Done()
	defer conn.Close()
	_ = conn.SetDeadline(time.Now().Add(10 * time.Second))
	var stream net.Conn = conn
	reader := bufio.NewReader(conn)
	if c.tlsConfig != nil {
		_ = readRequest(reader)
		if _, err := conn.Write([]byte("HTTP/1.1 200 OK\r\n\r\n")); err != nil {
			return
		}
		tlsConn := tls.Server(conn, c.tlsConfig)
		if err := tlsConn.Handshake(); err != nil {
			panic(fmt.Sprintf("tls handshake: %v", err))
		}
		stream = tlsConn
		reader = bufio.NewReader(tlsConn)
	}
	raw := readRequest(reader)
	c.mu.Lock()
	c.requests = append(c.requests, raw)
	c.mu.Unlock()
	var out bytes.Buffer
	fmt.Fprintf(&out, "HTTP/1.1 %d %s\r\n", rep.Status, http.StatusText(rep.Status))
	for _, h := range rep.Headers {
		fmt.Fprintf(&out, "%s: %s\r\n", h[0], h[1])
	}
	fmt.Fprintf(&out, "Content-Length: %d\r\n", len(rep.Body))
	out.WriteString("Connection: close\r\n\r\n")
	out.WriteString(rep.Body)
	_, _ = stream.Write(out.Bytes())
}

type tlsMaterial struct {
	CA      string `json:"ca_pem"`
	Leaf    string `json:"leaf_pem"`
	LeafKey string `json:"leaf_key_pem"`
}

func pemBlock(kind string, der []byte) string {
	return string(pem.EncodeToMemory(&pem.Block{Type: kind, Bytes: der}))
}

// testCA makes a CA and a leaf for *.googleapis.com, valid for a century.
func testCA() (tlsMaterial, *tls.Config) {
	caKey, _ := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	caTemplate := &x509.Certificate{
		SerialNumber:          big.NewInt(1),
		Subject:               pkix.Name{CommonName: "cliproxy-rs test CA"},
		NotBefore:             time.Date(2020, 1, 1, 0, 0, 0, 0, time.UTC),
		NotAfter:              time.Date(2120, 1, 1, 0, 0, 0, 0, time.UTC),
		IsCA:                  true,
		KeyUsage:              x509.KeyUsageCertSign | x509.KeyUsageDigitalSignature,
		BasicConstraintsValid: true,
	}
	caDER, err := x509.CreateCertificate(rand.Reader, caTemplate, caTemplate, &caKey.PublicKey, caKey)
	if err != nil {
		panic(err)
	}
	caCert, _ := x509.ParseCertificate(caDER)
	leafKey, _ := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	leafTemplate := &x509.Certificate{
		SerialNumber: big.NewInt(2),
		Subject:      pkix.Name{CommonName: "*.googleapis.com"},
		DNSNames:     []string{"*.googleapis.com", "googleapis.com"},
		NotBefore:    caTemplate.NotBefore,
		NotAfter:     caTemplate.NotAfter,
		KeyUsage:     x509.KeyUsageDigitalSignature,
		ExtKeyUsage:  []x509.ExtKeyUsage{x509.ExtKeyUsageServerAuth},
	}
	leafDER, err := x509.CreateCertificate(rand.Reader, leafTemplate, caCert, &leafKey.PublicKey, caKey)
	if err != nil {
		panic(err)
	}
	leafKeyDER, _ := x509.MarshalPKCS8PrivateKey(leafKey)
	material := tlsMaterial{
		CA:      pemBlock("CERTIFICATE", caDER),
		Leaf:    pemBlock("CERTIFICATE", leafDER),
		LeafKey: pemBlock("PRIVATE KEY", leafKeyDER),
	}
	pair, err := tls.X509KeyPair([]byte(material.Leaf), []byte(material.LeafKey))
	if err != nil {
		panic(err)
	}
	return material, &tls.Config{Certificates: []tls.Certificate{pair}, NextProtos: []string{"http/1.1"}}
}

// resolvedRecord is a bound *registry.ModelInfo: its JSON plus the json:"-" fields
// executors read.
type resolvedRecord struct {
	Info                       json.RawMessage `json:"info"`
	IsCompat                   bool            `json:"is_compat,omitempty"`
	UserDefined                bool            `json:"user_defined,omitempty"`
	SupportConfigurationUpdate bool            `json:"support_configuration_update,omitempty"`
}

func recordResolved(info *registry.ModelInfo) *resolvedRecord {
	raw, err := json.Marshal(info)
	if err != nil {
		panic(err)
	}
	return &resolvedRecord{Info: raw, IsCompat: info.IsCompat, UserDefined: info.UserDefined, SupportConfigurationUpdate: info.SupportConfigurationUpdate}
}

func statusOf(err error) *errOut {
	out := &errOut{Message: err.Error()}
	if s, ok := err.(interface{ StatusCode() int }); ok {
		out.Status = s.StatusCode()
	}
	return out
}

func replaceAll(v any, old, new string) any {
	switch t := v.(type) {
	case string:
		return strings.ReplaceAll(t, old, new)
	case map[string]any:
		out := make(map[string]any, len(t))
		for k, x := range t {
			out[k] = replaceAll(x, old, new)
		}
		return out
	case []any:
		out := make([]any, len(t))
		for i, x := range t {
			out[i] = replaceAll(x, old, new)
		}
		return out
	}
	return v
}

type capabilityRoute struct {
	upstream string
	info     *registry.ModelInfo
}

func aliasCandidates(model string) []string {
	model = strings.TrimSpace(model)
	if model == "" {
		return nil
	}
	base := thinking.ParseSuffix(model).ModelName
	if base == "" {
		base = model
	}
	if base != model {
		return []string{model, base}
	}
	return []string{model}
}

// resolvedModelInfo is the conductor's API-key capability binding for vertex keys.
func resolvedModelInfo(models []config.VertexCompatModel, requested, upstream string) *registry.ModelInfo {
	routes := map[string][]capabilityRoute{}
	for _, m := range models {
		name, alias := strings.TrimSpace(m.Name), strings.TrimSpace(m.Alias)
		if name == "" {
			name = alias
		}
		if alias == "" {
			alias = name
		}
		if name == "" {
			continue
		}
		info := modelconfig.ResolveModelInfo(name, "vertex", m.Thinking)
		seen := map[string]bool{}
		for _, routeModel := range []string{alias, name} {
			for _, candidate := range aliasCandidates(routeModel) {
				key := strings.ToLower(strings.TrimSpace(candidate))
				if key == "" || seen[key] {
					continue
				}
				seen[key] = true
				routes[key] = append(routes[key], capabilityRoute{upstream: name, info: info})
			}
		}
	}
	var matched []capabilityRoute
	for _, candidate := range aliasCandidates(requested) {
		matched = append(matched, routes[strings.ToLower(strings.TrimSpace(candidate))]...)
	}
	selected := strings.TrimSpace(upstream)
	for _, route := range matched {
		if strings.EqualFold(strings.TrimSpace(route.upstream), selected) {
			return route.info
		}
	}
	for _, route := range matched {
		configured := thinking.ParseSuffix(strings.TrimSpace(route.upstream))
		if !configured.HasSuffix && strings.EqualFold(strings.TrimSpace(configured.ModelName), strings.TrimSpace(thinking.ParseSuffix(selected).ModelName)) {
			return route.info
		}
	}
	return nil
}

func run(s *scenario, plain, proxy *capture, keys map[string]string) {
	plain.script(nil)
	proxy.script(nil)
	if s.Via == "plain" {
		plain.script(s.Replies)
	} else {
		proxy.script(s.Replies)
	}
	cfg := &config.Config{}
	var err error
	if s.Config != "" {
		text := strings.ReplaceAll(s.Config, "UPSTREAM", plain.addr())
		text = strings.ReplaceAll(text, "PROXY", proxy.addr())
		cfg, err = config.ParseConfigBytes([]byte(text))
		if err != nil {
			panic(fmt.Sprintf("%s: %v", s.Name, err))
		}
	}
	var auth *cliproxyauth.Auth
	if s.ConfigAuth >= 0 {
		auths, errSynth := synthesizer.NewConfigSynthesizer().Synthesize(&synthesizer.SynthesisContext{
			Config:      cfg,
			Now:         time.Unix(0, 0),
			IDGenerator: synthesizer.NewStableIDGenerator(),
		})
		if errSynth != nil {
			panic(errSynth)
		}
		var vertex []*cliproxyauth.Auth
		for _, a := range auths {
			if a.Provider == "vertex" {
				vertex = append(vertex, a)
			}
		}
		auth = vertex[s.ConfigAuth]
	} else {
		meta := replaceAll(s.Metadata, "PROXY", proxy.addr()).(map[string]any)
		if sa, ok := meta["service_account"].(map[string]any); ok && s.Key != "" {
			sa["private_key"] = keys[s.Key]
		}
		auth = &cliproxyauth.Auth{ID: "vertex-test.json", Provider: "vertex", Metadata: meta, Attributes: map[string]string{}}
		if p, _ := meta["proxy_url"].(string); p != "" {
			auth.ProxyURL = p
		}
	}

	exec := executor.NewGeminiVertexExecutor(cfg)
	payload := []byte(s.Payload)
	req := cliproxyexecutor.Request{Model: s.Model, Payload: payload, Metadata: map[string]any{}}
	headers := http.Header{}
	for k, v := range s.Headers {
		headers.Set(k, v)
	}
	opts := cliproxyexecutor.Options{
		Stream:       s.Op == "stream",
		Alt:          s.Alt,
		Headers:      headers,
		SourceFormat: sdktranslator.FromString(s.Source),
		Metadata:     map[string]any{},
	}
	if s.RequestedModel != "" {
		opts.Metadata[cliproxyexecutor.RequestedModelMetadataKey] = s.RequestedModel
	}
	if index, errIndex := strconv.Atoi(auth.Attributes["config_index"]); errIndex == nil {
		requested := s.RequestedModel
		if requested == "" {
			requested = s.Model
		}
		if prefix := strings.TrimSpace(auth.Prefix); prefix != "" {
			requested = strings.TrimPrefix(strings.TrimSpace(requested), prefix+"/")
		}
		if info := resolvedModelInfo(cfg.VertexCompatAPIKey[index].Models, requested, s.Model); info != nil {
			req.Metadata["cliproxy.resolved_api_key_model_info"] = info
			s.Resolved = recordResolved(info)
		}
	}
	// The conductor binds the canonical session (see the gemini generator).
	req, opts = cliproxysession.Enrich(req, opts)
	sessionPayload := opts.OriginalRequest
	if len(sessionPayload) == 0 {
		sessionPayload = req.Payload
	}
	if canonical := cliproxyauth.CanonicalSessionID(opts.Headers, sessionPayload, opts.Metadata); canonical != "" {
		s.Session = cliproxysession.BoundSessionIdentity(canonical)
	}
	ctx := coreusage.WithTraceID(util.WithSessionID(context.Background(), s.Session), s.Name)

	switch s.Op {
	case "execute":
		resp, errExec := exec.Execute(ctx, auth, req, opts)
		if errExec != nil {
			s.Error = statusOf(errExec)
		} else {
			s.Output = string(resp.Payload)
		}
	case "stream":
		result, errExec := exec.ExecuteStream(ctx, auth, req, opts)
		if errExec != nil {
			s.Error = statusOf(errExec)
			break
		}
		for chunk := range result.Chunks {
			if chunk.Err != nil {
				s.Error = statusOf(chunk.Err)
				continue
			}
			s.Chunks = append(s.Chunks, string(chunk.Payload))
		}
	case "count":
		resp, errExec := exec.CountTokens(ctx, auth, req, opts)
		if errExec != nil {
			s.Error = statusOf(errExec)
		} else {
			s.Output = string(resp.Payload)
		}
	default:
		panic(s.Op)
	}
	s.Usage = awaitUsage(s.Name)
	time.Sleep(50 * time.Millisecond)
	for _, raw := range append(plain.take(), proxy.take()...) {
		raw = strings.ReplaceAll(raw, plain.addr(), "UPSTREAM")
		raw = strings.ReplaceAll(raw, proxy.addr(), "PROXY")
		if !utf8.ValidString(raw) {
			panic(s.Name + ": capture is not UTF-8")
		}
		s.Requests = append(s.Requests, raw)
	}
}

func rsaKeys() map[string]string {
	key, err := rsa.GenerateKey(rand.Reader, 2048)
	if err != nil {
		panic(err)
	}
	pkcs1 := pemBlock("RSA PRIVATE KEY", x509.MarshalPKCS1PrivateKey(key))
	pkcs8DER, _ := x509.MarshalPKCS8PrivateKey(key)
	pkcs8 := pemBlock("PRIVATE KEY", pkcs8DER)
	ecKey, _ := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	ecDER, _ := x509.MarshalPKCS8PrivateKey(ecKey)
	body := strings.TrimSuffix(strings.TrimPrefix(pkcs8, "-----BEGIN PRIVATE KEY-----\n"), "-----END PRIVATE KEY-----\n")
	return map[string]string{
		"pkcs1": pkcs1,
		"pkcs8": pkcs8,
		// Pasted keys: CRLF line ends, a terminal escape sequence and surrounding spaces.
		"pkcs8_crlf_ansi": "  \x1b[32m" + strings.ReplaceAll(pkcs8, "\n", "\r\n") + "\x1b[0m  ",
		// Line breaks lost: pem.Decode fails and rebuildPEM recovers the base64 body.
		"pkcs8_one_line": "-----BEGIN PRIVATE KEY----- " + strings.ReplaceAll(body, "\n", " ") + " -----END PRIVATE KEY-----",
		"pkcs1_headers":  strings.Replace(pkcs1, "-----\n", "-----\nProc-Type: 4,ENCRYPTED\n\n", 1),
		"ec":             pemBlock("PRIVATE KEY", ecDER),
		"garbage":        "not a key",
		"no_markers":     "MIIEvQIBADANBgkqhkiG9w0BAQEFAASC",
		"bad_base64":     "-----BEGIN PRIVATE KEY-----\n!!!!\n-----END PRIVATE KEY-----\n",
		"unknown_type":   pemBlock("KEY", x509.MarshalPKCS1PrivateKey(key)),
		// asn1.Unmarshal rejects bytes after the key.
		"pkcs1_trailing": pemBlock("RSA PRIVATE KEY", append(x509.MarshalPKCS1PrivateKey(key), 0x00, 0x01)),
		// rebuildPEM payloads base64 rejects: Go reports the offending input byte.
		"b64_incomplete_quantum": "-----BEGIN PRIVATE KEY----- AAAAA -----END PRIVATE KEY-----",
		"b64_after_padding":      "-----BEGIN PRIVATE KEY----- AAA=A -----END PRIVATE KEY-----",
		"b64_single_padding":     "-----BEGIN PRIVATE KEY----- AA= -----END PRIVATE KEY-----",
		"b64_padding_mismatch":   "-----BEGIN PRIVATE KEY----- AA=A -----END PRIVATE KEY-----",
		"b64_leading_padding":    "-----BEGIN PRIVATE KEY----- =AAA -----END PRIVATE KEY-----",
		"empty":                  "   ",
	}
}

type normalized struct {
	Key   string `json:"key"`
	Out   string `json:"out,omitempty"`
	Error string `json:"error,omitempty"`
}

type imported struct {
	Name   string `json:"name"`
	Input  string `json:"input"`
	Prefix string `json:"prefix"`
	// AuthDir is the auth dir under the case's temp dir DIR; "blocked" is a regular file.
	AuthDir string `json:"auth_dir"`
	// NoKeyFile leaves the key file unwritten.
	NoKeyFile bool `json:"no_key_file,omitempty"`
	// Files written to the auth dir, by name.
	Files map[string]string `json:"files"`
	// Errors are the error-level log messages, Imported the path Go printed on success.
	Errors   []string `json:"errors,omitempty"`
	Imported string   `json:"imported,omitempty"`
}

// logHook records error-level log messages.
type logHook struct{ errors []string }

func (h *logHook) Levels() []log.Level { return []log.Level{log.ErrorLevel} }

func (h *logHook) Fire(e *log.Entry) error {
	h.errors = append(h.errors, e.Message)
	return nil
}

// stdout runs f with os.Stdout captured.
func stdout(f func()) string {
	r, w, _ := os.Pipe()
	saved := os.Stdout
	os.Stdout = w
	done := make(chan string)
	go func() {
		b, _ := io.ReadAll(r)
		done <- string(b)
	}()
	f()
	_ = w.Close()
	os.Stdout = saved
	return <-done
}

func importVectors(keys map[string]string) []imported {
	sa := func(extra map[string]any) string {
		m := map[string]any{
			"type":            "service_account",
			"project_id":      "proj-1",
			"private_key_id":  "kid-1",
			"private_key":     keys["pkcs8"],
			"client_email":    "svc@proj-1.iam.gserviceaccount.com",
			"client_id":       "1234567890",
			"token_uri":       "https://oauth2.googleapis.com/token",
			"universe_domain": "googleapis.com",
			"weird<>&":        1.5e3,
		}
		for k, v := range extra {
			if v == nil {
				delete(m, k)
			} else {
				m[k] = v
			}
		}
		b, _ := json.Marshal(m)
		return string(b)
	}
	cases := []imported{
		{Name: "basic", Input: sa(nil)},
		{Name: "prefix", Input: sa(nil), Prefix: " /team a/ "},
		{Name: "prefix_slash_rejected", Input: sa(nil), Prefix: "a/b"},
		{Name: "missing_project", Input: sa(map[string]any{"project_id": nil})},
		{Name: "missing_email", Input: sa(map[string]any{"client_email": nil, "project_id": "my:proj/x y"})},
		{Name: "bad_key", Input: sa(map[string]any{"private_key": "nope"})},
		{Name: "invalid_json", Input: "{"},
		{Name: "syntax_error", Input: `{"a":}`},
		{Name: "null_root", Input: "null"},
		{Name: "array_root", Input: "[1]"},
		{Name: "number_overflow", Input: `{"x":1e400}`},
		{Name: "private_key_not_string", Input: sa(map[string]any{"private_key": 5})},
		{Name: "bad_base64_key", Input: sa(map[string]any{"private_key": "-----BEGIN PRIVATE KEY----- AAA=A -----END PRIVATE KEY-----"})},
		{Name: "missing_key_file", NoKeyFile: true},
		{Name: "auth_dir_blocked", Input: sa(nil), AuthDir: "blocked/auths"},
		{Name: "unclean_auth_dir", Input: sa(nil), AuthDir: "x/../auths/./"},
	}
	hook := &logHook{}
	log.AddHook(hook)
	for i := range cases {
		dir, _ := os.MkdirTemp("", "vertex-import")
		keyPath := filepath.Join(dir, "key.json")
		if !cases[i].NoKeyFile {
			_ = os.WriteFile(keyPath, []byte(cases[i].Input), 0o600)
		}
		_ = os.WriteFile(filepath.Join(dir, "blocked"), nil, 0o600)
		if cases[i].AuthDir == "" {
			cases[i].AuthDir = "auths"
		}
		// Joined by hand: filepath.Join would clean it.
		authDir := dir + "/" + cases[i].AuthDir
		hook.errors = nil
		printed := stdout(func() {
			internalcmd.DoVertexImport(&config.Config{AuthDir: authDir}, keyPath, cases[i].Prefix)
		})
		for _, line := range hook.errors {
			cases[i].Errors = append(cases[i].Errors, strings.ReplaceAll(line, dir, "DIR"))
		}
		for _, line := range strings.Split(printed, "\n") {
			if path, ok := strings.CutPrefix(line, "Vertex credentials imported: "); ok {
				cases[i].Imported = strings.ReplaceAll(path, dir, "DIR")
			}
		}
		authDir = filepath.Clean(authDir)
		cases[i].Files = map[string]string{}
		entries, _ := os.ReadDir(authDir)
		for _, e := range entries {
			data, _ := os.ReadFile(filepath.Join(authDir, e.Name()))
			cases[i].Files[e.Name()] = string(data)
		}
		_ = os.RemoveAll(dir)
	}
	return cases
}

func main() {
	if len(os.Args) != 2 {
		fmt.Fprintln(os.Stderr, "usage: generator OUTPUT.json")
		os.Exit(2)
	}
	material, tlsConfig := testCA()
	caFile, _ := os.CreateTemp("", "vertex-ca-*.pem")
	_, _ = caFile.WriteString(material.CA)
	_ = caFile.Close()
	defer os.Remove(caFile.Name())
	// Must precede the first TLS verification: Go loads system roots once.
	_ = os.Setenv("SSL_CERT_FILE", caFile.Name())
	_ = os.Unsetenv("HTTPS_PROXY")
	_ = os.Unsetenv("HTTP_PROXY")
	_ = os.Unsetenv("https_proxy")
	_ = os.Unsetenv("http_proxy")

	keys := rsaKeys()
	var norm []normalized
	names := make([]string, 0, len(keys))
	for name := range keys {
		names = append(names, name)
	}
	sort.Strings(names)
	for _, name := range names {
		out, errNorm := vertexauth.NormalizeServiceAccountMap(map[string]any{"private_key": keys[name]})
		entry := normalized{Key: name}
		if errNorm != nil {
			entry.Error = errNorm.Error()
		} else {
			entry.Out = out["private_key"].(string)
		}
		norm = append(norm, entry)
	}

	plain := newCapture(nil)
	proxy := newCapture(tlsConfig)
	coreusage.RegisterPlugin(captured)
	all := scenarios()
	for i := range all {
		run(&all[i], plain, proxy, keys)
	}
	data, err := json.MarshalIndent(map[string]any{
		"tls":       material,
		"keys":      keys,
		"normalize": norm,
		"import":    importVectors(keys),
		"scenarios": all,
	}, "", "  ")
	if err != nil {
		panic(err)
	}
	if err := os.WriteFile(os.Args[1], append(data, '\n'), 0o644); err != nil {
		panic(err)
	}
}
