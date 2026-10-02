// Generates byte-level goldens for the OpenAI-compatible executor by running the pinned
// Go executor against a local raw-TCP capture server. It never contacts a provider.
package main

import (
	"bufio"
	"bytes"
	"compress/gzip"
	"context"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"regexp"
	"strconv"
	"strings"
	"time"
	"unicode/utf8"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/modelconfig"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/registry"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/runtime/executor"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/thinking"
	// Production registers every translator through this package (cmd/server/main.go).
	_ "github.com/router-for-me/CLIProxyAPI/v8/internal/translator"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/watcher/synthesizer"
	cliproxyauth "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/auth"
	cliproxyexecutor "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/executor"
	sdktranslator "github.com/router-for-me/CLIProxyAPI/v8/sdk/translator"
)

type upstream struct {
	Status   int         `json:"status"`
	Headers  [][2]string `json:"headers,omitempty"`
	Body     string      `json:"body"`
	NoLength bool        `json:"no_length,omitempty"`
	// Gzip compresses Body on the wire; the declared encoding is in Headers.
	Gzip bool `json:"gzip,omitempty"`
}

type errOut struct {
	Status       int    `json:"status"`
	Message      string `json:"message"`
	RetryAfterMS int64  `json:"retry_after_ms"`
}

type scenario struct {
	Name string `json:"name"`
	// Config is YAML; UPSTREAM is replaced with the capture server address.
	Config string `json:"config,omitempty"`
	// ConfigAuth selects the Nth synthesized config credential; -1 uses Attributes.
	ConfigAuth     int               `json:"config_auth"`
	Provider       string            `json:"provider"`
	Attributes     map[string]string `json:"attributes,omitempty"`
	Metadata       map[string]any    `json:"metadata,omitempty"`
	Model          string            `json:"model"`
	RequestedModel string            `json:"requested_model,omitempty"`
	Payload        string            `json:"payload"`
	// PayloadB64 replaces Payload for bodies that are not valid UTF-8.
	PayloadB64 string `json:"payload_b64,omitempty"`
	Original       string            `json:"original,omitempty"`
	Source         string            `json:"source"`
	Response       string            `json:"response,omitempty"`
	Stream         bool              `json:"stream,omitempty"`
	// Op is execute, stream, count, images or images_stream.
	Op          string            `json:"op"`
	Alt         string            `json:"alt,omitempty"`
	Headers     map[string]string `json:"headers,omitempty"`
	RequestPath string            `json:"request_path,omitempty"`
	ContentType string            `json:"content_type,omitempty"`
	Session     string            `json:"session,omitempty"`
	// Needs names shared helpers whose real port must land before Rust can match.
	Needs    []string  `json:"needs,omitempty"`
	Upstream *upstream `json:"upstream,omitempty"`

	Request string `json:"request,omitempty"`
	// RequestB64 holds the capture instead of Request when it is not valid UTF-8.
	RequestB64 string   `json:"request_b64,omitempty"`
	Output  string   `json:"output,omitempty"`
	Chunks  []string `json:"chunks,omitempty"`
	Error   *errOut  `json:"error,omitempty"`
}

var boundaryRe = regexp.MustCompile(`boundary=([0-9a-f]{60})`)

// capture serves one connection: records the raw request and replies with the script.
func capture(ln net.Listener, up *upstream, done chan<- string) {
	conn, err := ln.Accept()
	if err != nil {
		done <- ""
		return
	}
	defer conn.Close()
	_ = conn.SetDeadline(time.Now().Add(10 * time.Second))
	reader := bufio.NewReader(conn)
	var raw bytes.Buffer
	length := 0
	for {
		line, errLine := reader.ReadString('\n')
		raw.WriteString(line)
		if errLine != nil {
			done <- raw.String()
			return
		}
		if strings.HasPrefix(strings.ToLower(line), "content-length:") {
			length, _ = strconv.Atoi(strings.TrimSpace(line[len("content-length:"):]))
		}
		if line == "\r\n" {
			break
		}
	}
	body := make([]byte, length)
	_, _ = io.ReadFull(reader, body)
	raw.Write(body)
	payload := []byte(up.Body)
	if up.Gzip {
		var zipped bytes.Buffer
		zw := gzip.NewWriter(&zipped)
		_, _ = zw.Write(payload)
		_ = zw.Close()
		payload = zipped.Bytes()
	}
	var out bytes.Buffer
	fmt.Fprintf(&out, "HTTP/1.1 %d %s\r\n", up.Status, http.StatusText(up.Status))
	for _, h := range up.Headers {
		fmt.Fprintf(&out, "%s: %s\r\n", h[0], h[1])
	}
	if !up.NoLength {
		fmt.Fprintf(&out, "Content-Length: %d\r\n", len(payload))
	}
	out.WriteString("Connection: close\r\n\r\n")
	out.Write(payload)
	_, _ = conn.Write(out.Bytes())
	done <- raw.String()
}

func normalizeRequest(raw, addr string) string {
	raw = strings.ReplaceAll(raw, addr, "UPSTREAM")
	if m := boundaryRe.FindStringSubmatch(raw); m != nil {
		raw = strings.ReplaceAll(raw, m[1], "BOUNDARY")
	}
	return raw
}

// compatModelInfo mirrors the conductor's API-key capability binding for
// openai-compatibility credentials (sdk/cliproxy/auth/api_key_model_capabilities.go
// compileOpenAICompatibleModelCapabilities and lookupAPIKeyModelCapability): the route
// is the requested model (alias or name, suffix-insensitive) and the configured upstream
// must equal the selected model, or match its suffix-free name.
func compatModelInfo(cfg *config.Config, index int, requested, upstream string) *registry.ModelInfo {
	if cfg == nil || index < 0 || index >= len(cfg.OpenAICompatibility) {
		return nil
	}
	base := func(s string) string { return strings.TrimSpace(thinking.ParseSuffix(strings.TrimSpace(s)).ModelName) }
	selected := strings.TrimSpace(upstream)
	for _, exact := range []bool{true, false} {
		for _, m := range cfg.OpenAICompatibility[index].Models {
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
			route := strings.EqualFold(base(requested), base(alias)) || strings.EqualFold(base(requested), base(name))
			if !route {
				continue
			}
			matched := strings.EqualFold(name, selected)
			if !exact {
				matched = !thinking.ParseSuffix(name).HasSuffix && strings.EqualFold(name, base(selected))
			}
			if !matched {
				continue
			}
			support := m.Thinking
			if support == nil && !m.Image {
				support = &registry.ThinkingSupport{Levels: []string{"low", "medium", "high"}}
			}
			info := modelconfig.ResolveModelInfo(name, "openai-compatibility", support)
			info.IsCompat = m.IsCompat
			return info
		}
	}
	return nil
}

func statusOf(err error) *errOut {
	out := &errOut{Message: err.Error(), RetryAfterMS: -1}
	if s, ok := err.(interface{ StatusCode() int }); ok {
		out.Status = s.StatusCode()
	}
	if r, ok := err.(interface{ RetryAfter() *time.Duration }); ok && r.RetryAfter() != nil {
		out.RetryAfterMS = r.RetryAfter().Milliseconds()
	}
	return out
}

func run(s *scenario) {
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		panic(err)
	}
	defer ln.Close()
	addr := ln.Addr().String()
	done := make(chan string, 1)
	if s.Upstream != nil {
		go capture(ln, s.Upstream, done)
	}

	cfg := &config.Config{}
	if s.Config != "" {
		cfg, err = config.ParseConfigBytes([]byte(strings.ReplaceAll(s.Config, "UPSTREAM", addr)))
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
		var compat []*cliproxyauth.Auth
		for _, a := range auths {
			if a.Attributes["compat_name"] != "" || a.Provider == "openai-compatibility" {
				compat = append(compat, a)
			}
		}
		auth = compat[s.ConfigAuth]
		s.Provider = auth.Provider
	} else {
		attrs := map[string]string{}
		for k, v := range s.Attributes {
			attrs[k] = strings.ReplaceAll(v, "UPSTREAM", addr)
		}
		auth = &cliproxyauth.Auth{Provider: s.Provider, Attributes: attrs, Metadata: s.Metadata}
	}

	exec := executor.NewOpenAICompatExecutor(s.Provider, cfg)
	payload := []byte(s.Payload)
	if s.PayloadB64 != "" {
		payload, _ = base64.StdEncoding.DecodeString(s.PayloadB64)
	}
	req := cliproxyexecutor.Request{Model: s.Model, Payload: payload, Metadata: map[string]any{}}
	headers := http.Header{}
	for k, v := range s.Headers {
		headers.Set(k, v)
	}
	if s.ContentType != "" {
		headers.Set("Content-Type", s.ContentType)
	}
	opts := cliproxyexecutor.Options{
		Stream:       s.Stream,
		Alt:          s.Alt,
		Headers:      headers,
		SourceFormat: sdktranslator.FromString(s.Source),
		Metadata:     map[string]any{},
	}
	if s.Response != "" {
		opts.ResponseFormat = sdktranslator.FromString(s.Response)
	}
	if s.Original != "" {
		opts.OriginalRequest = []byte(s.Original)
	}
	if s.RequestedModel != "" {
		opts.Metadata[cliproxyexecutor.RequestedModelMetadataKey] = s.RequestedModel
	}
	if s.RequestPath != "" {
		opts.Metadata[cliproxyexecutor.RequestPathMetadataKey] = s.RequestPath
	}
	if strings.HasPrefix(s.Op, "images") {
		opts.SourceFormat = sdktranslator.FromString("openai-image")
	}
	if index, errIndex := strconv.Atoi(auth.Attributes["config_index"]); errIndex == nil {
		requested := s.RequestedModel
		if requested == "" {
			requested = s.Model
		}
		if info := compatModelInfo(cfg, index, requested, s.Model); info != nil {
			req.Metadata["cliproxy.resolved_api_key_model_info"] = info
		}
	}
	ctx := context.Background()

	switch s.Op {
	case "execute", "images":
		resp, errExec := exec.Execute(ctx, auth, req, opts)
		if errExec != nil {
			s.Error = statusOf(errExec)
		} else {
			s.Output = string(resp.Payload)
		}
	case "stream", "images_stream":
		opts.Stream = true
		result, errExec := exec.ExecuteStream(ctx, auth, req, opts)
		if errExec != nil {
			s.Error = statusOf(errExec)
			break
		}
		var joined bytes.Buffer
		for chunk := range result.Chunks {
			if chunk.Err != nil {
				s.Error = statusOf(chunk.Err)
				continue
			}
			if s.Op == "images_stream" {
				joined.Write(chunk.Payload)
				continue
			}
			s.Chunks = append(s.Chunks, string(chunk.Payload))
		}
		if s.Op == "images_stream" {
			s.Output = joined.String()
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
	if s.Upstream != nil {
		select {
		case raw := <-done:
			if normalized := normalizeRequest(raw, addr); utf8.ValidString(normalized) {
				s.Request = normalized
			} else {
				s.RequestB64 = base64.StdEncoding.EncodeToString([]byte(normalized))
			}
		case <-time.After(200 * time.Millisecond):
			// No upstream call was made; the scripted reply is unused.
		}
	}
}

func main() {
	if len(os.Args) != 2 {
		fmt.Fprintln(os.Stderr, "usage: generator OUTPUT.json")
		os.Exit(2)
	}
	all := scenarios()
	for i := range all {
		run(&all[i])
	}
	data, err := json.MarshalIndent(all, "", "  ")
	if err != nil {
		panic(err)
	}
	if err := os.WriteFile(os.Args[1], append(data, '\n'), 0o644); err != nil {
		panic(err)
	}
}
