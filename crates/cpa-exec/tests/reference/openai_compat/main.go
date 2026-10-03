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
	"mime"
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
	"github.com/router-for-me/CLIProxyAPI/v8/internal/runtime/executor/helps"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/thinking"
	// Production registers every translator through this package (cmd/server/main.go).
	_ "github.com/router-for-me/CLIProxyAPI/v8/internal/translator"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/watcher/synthesizer"
	cliproxyauth "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/auth"
	cliproxyexecutor "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/executor"
	"github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/usage"
	sdktranslator "github.com/router-for-me/CLIProxyAPI/v8/sdk/translator"
	"github.com/tidwall/gjson"
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
	// CredentialScoped is IsCredentialScoped(): a 429 that cools the whole credential.
	CredentialScoped bool `json:"credential_scoped"`
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
	Original   string `json:"original,omitempty"`
	Source     string `json:"source"`
	Response   string `json:"response,omitempty"`
	Stream     bool   `json:"stream,omitempty"`
	// Op is execute, stream, count, images or images_stream.
	Op          string            `json:"op"`
	Alt         string            `json:"alt,omitempty"`
	Headers     map[string]string `json:"headers,omitempty"`
	RequestPath string            `json:"request_path,omitempty"`
	ContentType string            `json:"content_type,omitempty"`
	Session     string            `json:"session,omitempty"`
	// ExecutionSession is opts.Metadata[execution_session_id].
	ExecutionSession string `json:"execution_session,omitempty"`
	// DerivedSession is req.Metadata[derived_session_id] (session.Enrich).
	DerivedSession string `json:"derived_session,omitempty"`
	// Needs names shared helpers whose real port must land before Rust can match.
	Needs    []string  `json:"needs,omitempty"`
	Upstream *upstream `json:"upstream,omitempty"`
	// ResolvedModel is the model info the conductor binds to the attempt
	// (ResolvedAPIKeyModelInfo), JSON plus its json:"-" flags, for the Rust dispatch stand-in.
	ResolvedModel map[string]any `json:"resolved_model,omitempty"`
	// Usage is the record Go's usage reporter published for the attempt.
	Usage *usageOut `json:"usage,omitempty"`

	Request string `json:"request,omitempty"`
	// RequestB64 holds the capture instead of Request when it is not valid UTF-8.
	RequestB64 string   `json:"request_b64,omitempty"`
	Output     string   `json:"output,omitempty"`
	Chunks     []string `json:"chunks,omitempty"`
	Error      *errOut  `json:"error,omitempty"`
}

type usageOut struct {
	Input         int64  `json:"input"`
	Output        int64  `json:"output"`
	Reasoning     int64  `json:"reasoning"`
	Cached        int64  `json:"cached"`
	Total         int64  `json:"total"`
	Effort        string `json:"effort"`
	ResponseModel string `json:"response_model"`
	Failed        bool   `json:"failed"`
}

var records = make(chan usage.Record, 64)

type capturePlugin struct{}

func (capturePlugin) HandleUsage(_ context.Context, r usage.Record) { records <- r }

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

type capabilityRoute struct {
	upstream string
	info     *registry.ModelInfo
}

// aliasCandidates is modelAliasLookupCandidates: the model, then its suffix-free name.
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

// compatModelInfo reproduces the conductor's API-key capability binding for
// openai-compatibility credentials (sdk/cliproxy/auth/api_key_model_capabilities.go:
// compileOpenAICompatibleModelCapabilities, addConfiguredModelCapability and
// lookupAPIKeyModelCapability), including route-key order and the suffix fallback.
func compatModelInfo(cfg *config.Config, index int, requested, upstream string) *registry.ModelInfo {
	if cfg == nil || index < 0 || index >= len(cfg.OpenAICompatibility) {
		return nil
	}
	routes := map[string][]capabilityRoute{}
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
		support := m.Thinking
		if support == nil && !m.Image {
			support = &registry.ThinkingSupport{Levels: []string{"low", "medium", "high"}}
		}
		info := modelconfig.ResolveModelInfo(name, "openai-compatibility", support)
		info.IsCompat = m.IsCompat
		seen := map[string]bool{}
		for _, routeModel := range []string{alias, name} {
			for _, candidate := range aliasCandidates(routeModel) {
				key := strings.ToLower(strings.TrimSpace(candidate))
				if key == "" || seen[key] {
					continue
				}
				seen[key] = true
				duplicate := false
				for _, existing := range routes[key] {
					if strings.EqualFold(existing.upstream, name) {
						duplicate = true
						break
					}
				}
				if !duplicate {
					routes[key] = append(routes[key], capabilityRoute{upstream: name, info: info})
				}
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

// resolvedJSON is the bound ModelInfo as the Rust registry stores it: Go's JSON with
// the internal flags thinking reads added when set.
func resolvedJSON(info *registry.ModelInfo) map[string]any {
	raw, err := json.Marshal(info)
	if err != nil {
		panic(err)
	}
	out := map[string]any{}
	if err := json.Unmarshal(raw, &out); err != nil {
		panic(err)
	}
	if info.IsCompat {
		out["is_compat"] = true
	}
	if info.UserDefined {
		out["user_defined"] = true
	}
	if info.SupportConfigurationUpdate {
		out["support_configuration_update"] = true
	}
	return out
}

func statusOf(err error) *errOut {
	out := &errOut{Message: err.Error(), RetryAfterMS: -1}
	if s, ok := err.(interface{ StatusCode() int }); ok {
		out.Status = s.StatusCode()
	}
	if r, ok := err.(interface{ RetryAfter() *time.Duration }); ok && r.RetryAfter() != nil {
		out.RetryAfterMS = r.RetryAfter().Milliseconds()
	}
	if c, ok := err.(interface{ IsCredentialScoped() bool }); ok {
		out.CredentialScoped = c.IsCredentialScoped()
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
	if s.ExecutionSession != "" {
		opts.Metadata[cliproxyexecutor.ExecutionSessionMetadataKey] = s.ExecutionSession
	}
	if s.DerivedSession != "" {
		req.Metadata[cliproxyexecutor.DerivedSessionIDMetadataKey] = s.DerivedSession
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
			s.ResolvedModel = resolvedJSON(info)
		}
	}
	ctx := context.Background()
	for len(records) > 0 {
		<-records
	}

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
	if s.Op != "count" {
		select {
		case r := <-records:
			s.Usage = &usageOut{Input: r.Detail.InputTokens, Output: r.Detail.OutputTokens, Reasoning: r.Detail.ReasoningTokens,
				Cached: r.Detail.CachedTokens, Total: r.Detail.TotalTokens, Effort: r.ReasoningEffort, ResponseModel: r.ResponseModel, Failed: r.Failed}
		case <-time.After(500 * time.Millisecond):
		}
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
	usage.RegisterPlugin(capturePlugin{})
	all := scenarios()
	for i := range all {
		run(&all[i])
	}
	data, err := json.MarshalIndent(map[string]any{"scenarios": all, "vectors": vectors()}, "", "  ")
	if err != nil {
		panic(err)
	}
	if err := os.WriteFile(os.Args[1], append(data, '\n'), 0o644); err != nil {
		panic(err)
	}
}

// vectors records Go standard-library answers for the primitives the executor ports.
func vectors() map[string]any {
	httpTimes := []string{
		"Fri, 02 Oct 2026 12:00:30 GMT", "Fri,  02 Oct 2026 12:00:30 GMT", "Fri, 02 Oct 2026 12:00:00.500 GMT",
		"fri, 02 oct 2026 12:00:00 GMT", "Mon, 01 Jan 1970 00:00:00 GMT", "Xyz, 02 Oct 2026 12:00:00 GMT",
		"Fri, 31 Feb 2026 12:00:00 GMT", "Fri, 2 Oct 2026 12:00:00 GMT", "Fri, 02 Oct 2026 9:00:00 GMT",
		"Friday, 02-Oct-26 12:00:30 GMT", "Friday, 02-Oct-26 12:00:30 ABCDEF", "Friday, 02-Oct-26 12:00:30 ABCD",
		"Friday, 02-Oct-26 12:00:30 ABCT", "Friday, 02-Oct-26 12:00:30 GMT+3", "Friday, 02-Oct-26 12:00:30 UTC",
		"Friday, 02-Oct-69 12:00:30 PST", "Fri Oct  2 12:00:30 2026", "Fri Oct 2 12:00:30 2026",
		"Fri Oct 12 12:00:30 2026", "2026-10-02T12:00:00Z", "", "Fri, 02 Oct 2026 24:00:00 GMT",
	}
	times := []any{}
	for _, raw := range httpTimes {
		t, err := http.ParseTime(raw)
		if err != nil {
			times = append(times, []any{raw, nil})
			continue
		}
		times = append(times, []any{raw, t.UnixNano()})
	}
	jsonInputs := []string{"{}", "[1,-0.5e+3,\"a\\u00e9\",true,null,{\"k\":[]}]", "", "{", "[1,]", "01", "1.", "\"\x01\"", "nul", "[1] x",
		"{\"a\":\"\xff\"}", strings.Repeat("[", 10000) + strings.Repeat("]", 10000), strings.Repeat("[", 10001) + strings.Repeat("]", 10001),
		" \t{\"a\" : 1 }\r\n", "{\"a\":1,}", "\"\\x\"", "\"\\u12\"", "-", "1e", "0.1E+5"}
	valids := []any{}
	for _, in := range jsonInputs {
		valids = append(valids, []any{base64.StdEncoding.EncodeToString([]byte(in)), json.Valid([]byte(in))})
	}
	ints := []any{}
	for _, in := range []string{`"429.5"`, `"-7"`, `"+7"`, `429.9`, `4e2`, `true`, `"18446744073709552045"`, `1e300`, `-9007199254740993`, `"x"`, `null`, `"-"`} {
		ints = append(ints, []any{in, gjson.Parse(in).Int()})
	}
	trims := []any{}
	for _, in := range []string{"\u00a0 x \u3000\r", "\xff ", " \x85y\u2028", "\t\v\f z"} {
		trims = append(trims, []any{base64.StdEncoding.EncodeToString([]byte(in)), base64.StdEncoding.EncodeToString(bytes.TrimSpace([]byte(in)))})
	}
	media := []any{}
	for _, in := range []string{
		`multipart/form-data; boundary="a b"`, `multipart/form-data`, `multipart/form-data; boundary=b;`, `multipart/form-data; boundary=`,
		`form-data; name="a\"b"; filename*0="x"; filename*1*=%41`, `form-data; name="image"; filename*0*=utf-8''%C3; filename*1*=%A9.png`,
		`form-data; name=x; name=y`, `text/`, `/x`, `form-data; filename="C:\dev\go\foo.txt"`, `Multipart/Mixed ; Boundary = q`,
	} {
		mt, params, err := mime.ParseMediaType(in)
		errText := ""
		if err != nil {
			errText = err.Error()
		}
		media = append(media, map[string]any{"input": in, "media": mt, "params": params, "error": errText})
	}
	counts := []any{}
	for _, in := range [][2]string{
		{"gpt-4", `{"messages":{"content":"a"}}`},
		{"gpt-4", `{"messages":[{"tool_calls":{"id":"a"}}]}`},
		{"gpt-4", `{"functions":{"name":"a"}}`},
		{"gpt-4", `{"tools":{"type":"function","function":{"name":"lookup"}}}`},
		{"gpt-4o", `{"messages":[{"role":"user","content":[{"type":"text","text":"hi"},["nested",{"k":1}],7,{"type":"tool_result","name":"t","content":"out"}]}],"input":"in","prompt":{"p":1}}`},
		{"", `{"messages":[{"content":{"type":"x"}}],"tool_choice":{"type":"function"}}`},
	} {
		enc, errEnc := helps.TokenizerForModel(in[0])
		if errEnc != nil {
			panic(errEnc)
		}
		n, errCount := helps.CountOpenAIChatTokens(enc, []byte(in[1]))
		if errCount != nil {
			panic(errCount)
		}
		counts = append(counts, []any{in[0], in[1], n})
	}
	return map[string]any{"http_time": times, "json_valid": valids, "gjson_int": ints, "trim_space": trims, "media_type": media, "token_counts": counts}
}
