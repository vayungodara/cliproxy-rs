// Generates byte-level goldens for the Gemini API-key and native Interactions executors
// by running the pinned Go executors against a local raw-TCP capture server. It never
// contacts a provider; every key is fake.
package main

import (
	"bufio"
	"bytes"
	"compress/gzip"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
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
	"github.com/router-for-me/CLIProxyAPI/v8/internal/util"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/watcher/synthesizer"
	cliproxyauth "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/auth"
	cliproxyexecutor "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/executor"
	cliproxysession "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/session"
	coreusage "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/usage"
	sdktranslator "github.com/router-for-me/CLIProxyAPI/v8/sdk/translator"
	"github.com/tidwall/gjson"
	"github.com/tidwall/sjson"
)

type upstream struct {
	Status  int         `json:"status"`
	Headers [][2]string `json:"headers,omitempty"`
	Body    string      `json:"body"`
	// Gzip compresses Body on the wire; the declared encoding is in Headers.
	Gzip bool `json:"gzip,omitempty"`
}

type errOut struct {
	Status  int    `json:"status"`
	Message string `json:"message"`
}

type scenario struct {
	Name string `json:"name"`
	// Config is YAML; UPSTREAM is replaced with the capture server address.
	Config string `json:"config,omitempty"`
	// Provider selects synthesized config credentials of this provider.
	Provider string `json:"provider"`
	// ConfigAuth selects the Nth synthesized credential of Provider; -1 uses Attributes.
	ConfigAuth     int               `json:"config_auth"`
	Attributes     map[string]string `json:"attributes,omitempty"`
	Model          string            `json:"model"`
	RequestedModel string            `json:"requested_model,omitempty"`
	Payload        string            `json:"payload"`
	Original       string            `json:"original,omitempty"`
	Source         string            `json:"source"`
	Response       string            `json:"response,omitempty"`
	// Op is execute, stream or count.
	Op      string            `json:"op"`
	Alt     string            `json:"alt,omitempty"`
	Headers map[string]string `json:"headers,omitempty"`
	// Session is the canonical session the conductor binds (ExecRequest.session).
	Session string `json:"session,omitempty"`
	// Resolved is the model info the conductor binds (ExecRequest.resolved_model).
	Resolved *resolvedRecord `json:"resolved,omitempty"`
	// Needs lists translator registrations Go used whose result is not the
	// identity: "pair:<client>-><upstream>" and "token_count:<client>-><upstream>".
	Needs    []string  `json:"needs,omitempty"`
	Upstream *upstream `json:"upstream,omitempty"`

	Request string   `json:"request,omitempty"`
	Output  string   `json:"output,omitempty"`
	Chunks  []string `json:"chunks,omitempty"`
	Error   *errOut  `json:"error,omitempty"`
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
	fmt.Fprintf(&out, "Content-Length: %d\r\n", len(payload))
	out.WriteString("Connection: close\r\n\r\n")
	out.Write(payload)
	_, _ = conn.Write(out.Bytes())
	done <- raw.String()
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

// resolvedModelInfo reproduces the conductor's API-key capability binding for Gemini
// and Interactions credentials (sdk/cliproxy/auth/api_key_model_capabilities.go:
// compileConfiguredModelCapabilities, addConfiguredModelCapability and
// lookupAPIKeyModelCapability), including route-key order and the suffix fallback.
func resolvedModelInfo(models []config.GeminiModel, modelType, requested, upstream string) *registry.ModelInfo {
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
		info := modelconfig.ResolveModelInfo(name, modelType, m.Thinking)
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

// fallbackRequest is sdktranslator's no-transformer result: only a differing model is set.
func fallbackRequest(model string, payload []byte) []byte {
	if model != "" && gjson.GetBytes(payload, "model").String() != model {
		if updated, err := sjson.SetBytes(payload, "model", model); err == nil {
			return updated
		}
	}
	return payload
}

func nativeInteractionsSource(format string) bool {
	switch format {
	case "interactions", "openai", "openai-response", "claude", "gemini":
		return true
	}
	return false
}

// needs records the translator registrations whose output differs from the identity
// (or the request fallback) for this scenario, so Rust can run the scenario as soon as
// each one is registered.
func needs(s *scenario, upstreamFormat string, payload []byte, countBody []byte) []string {
	var out []string
	add := func(entry string) {
		for _, existing := range out {
			if existing == entry {
				return
			}
		}
		out = append(out, entry)
	}
	from := sdktranslator.FromString(s.Source)
	to := sdktranslator.FromString(upstreamFormat)
	response := s.Response
	if response == "" {
		response = s.Source
	}
	base := thinking.ParseSuffix(s.Model).ModelName
	skipRequest := upstreamFormat == "interactions" && s.Source == "interactions"
	// Unregistered pairs are Go's fallback (model rewrite, passthrough response), which
	// needs no translator.
	if !skipRequest && sdktranslator.HasRequestTransformer(from, to) {
		if s.Source != upstreamFormat {
			add("pair:" + s.Source + "->" + upstreamFormat)
		} else {
			translated := sdktranslator.TranslateRequest(from, to, base, bytes.Clone(payload), s.Op == "stream")
			if !bytes.Equal(translated, fallbackRequest(base, bytes.Clone(payload))) {
				add("pair:" + s.Source + "->" + upstreamFormat)
			}
		}
	}
	if s.Op == "count" {
		if s.Upstream == nil || s.Upstream.Status < 200 || s.Upstream.Status >= 300 {
			return out
		}
		translated := sdktranslator.TranslateTokenCount(context.Background(), to, sdktranslator.FromString(response), gjson.GetBytes(countBody, "totalTokens").Int(), countBody)
		if !bytes.Equal(translated, countBody) {
			add("token_count:" + response + "->" + upstreamFormat)
		}
	} else if response != upstreamFormat && sdktranslator.HasResponseTransformer(sdktranslator.FromString(response), to) {
		add("pair:" + response + "->" + upstreamFormat)
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
		var matching []*cliproxyauth.Auth
		for _, a := range auths {
			if a.Provider == s.Provider {
				matching = append(matching, a)
			}
		}
		auth = matching[s.ConfigAuth]
	} else {
		attrs := map[string]string{}
		for k, v := range s.Attributes {
			attrs[k] = strings.ReplaceAll(v, "UPSTREAM", addr)
		}
		auth = &cliproxyauth.Auth{Provider: s.Provider, Attributes: attrs, Metadata: map[string]any{}}
	}

	var exec *executor.GeminiExecutor
	if s.Provider == "gemini-interactions" {
		exec = executor.NewGeminiInteractionsExecutor(cfg)
	} else {
		exec = executor.NewGeminiExecutor(cfg)
	}
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
	if s.Response != "" {
		opts.ResponseFormat = sdktranslator.FromString(s.Response)
	}
	if s.Original != "" {
		opts.OriginalRequest = []byte(s.Original)
	}
	if s.RequestedModel != "" {
		opts.Metadata[cliproxyexecutor.RequestedModelMetadataKey] = s.RequestedModel
	}
	if index, errIndex := strconv.Atoi(auth.Attributes["config_index"]); errIndex == nil {
		requested := s.RequestedModel
		if requested == "" {
			requested = s.Model
		}
		var models []config.GeminiModel
		modelType := "gemini"
		switch s.Provider {
		case "gemini":
			models = cfg.GeminiKey[index].Models
		case "gemini-interactions":
			models = cfg.InteractionsKey[index].Models
			modelType = "interactions"
		}
		// rewriteModelForAuth: the credential's prefix is not part of the route.
		if prefix := strings.TrimSpace(auth.Prefix); prefix != "" {
			requested = strings.TrimPrefix(strings.TrimSpace(requested), prefix+"/")
		}
		if info := resolvedModelInfo(models, modelType, requested, s.Model); info != nil {
			req.Metadata["cliproxy.resolved_api_key_model_info"] = info
			s.Resolved = recordResolved(info)
		}
	}
	upstreamFormat := "gemini"
	if s.Provider == "gemini-interactions" && nativeInteractionsSource(s.Source) && s.Op != "count" {
		upstreamFormat = "interactions"
	}
	countBody := []byte(nil)
	if s.Upstream != nil {
		countBody = []byte(s.Upstream.Body)
	}
	s.Needs = needs(s, upstreamFormat, payload, countBody)
	// The conductor binds the attempt's canonical session to the context before the
	// executor runs (session.Enrich, ensureCanonicalSessionMetadata and
	// syncMetadataSessionToContext in sdk/cliproxy/auth); $CPA-SESSION-ID reads it.
	// The bound identity is recorded as the scenario's session (Rust ExecRequest.session).
	req, opts = cliproxysession.Enrich(req, opts)
	sessionPayload := opts.OriginalRequest
	if len(sessionPayload) == 0 {
		sessionPayload = req.Payload
	}
	if opts.Metadata == nil {
		opts.Metadata = map[string]any{}
	}
	if id, _ := opts.Metadata[cliproxyexecutor.CanonicalSessionIDMetadataKey].(string); strings.TrimSpace(id) == "" {
		if canonical := cliproxyauth.CanonicalSessionID(opts.Headers, sessionPayload, opts.Metadata); canonical != "" {
			opts.Metadata[cliproxyexecutor.CanonicalSessionIDMetadataKey] = canonical
		}
	}
	canonical, _ := opts.Metadata[cliproxyexecutor.CanonicalSessionIDMetadataKey].(string)
	if canonical == "" {
		canonical, _ = opts.Metadata[cliproxyexecutor.LCPAffinitySessionIDMetadataKey].(string)
	}
	if canonical == "" {
		if id, _ := opts.Metadata[cliproxyexecutor.ExecutionSessionMetadataKey].(string); strings.TrimSpace(id) != "" {
			canonical = "execution:" + strings.TrimPrefix(strings.TrimSpace(id), "execution:")
		}
	}
	if canonical == "" {
		if id, _ := opts.Metadata[cliproxyexecutor.DerivedSessionIDMetadataKey].(string); strings.TrimSpace(id) != "" {
			canonical = "derived:" + strings.TrimPrefix(strings.TrimSpace(id), "derived:")
		}
	}
	if canonical = strings.TrimSpace(canonical); canonical != "" {
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
	if s.Upstream != nil {
		select {
		case raw := <-done:
			normalized := strings.ReplaceAll(raw, addr, "UPSTREAM")
			if !utf8.ValidString(normalized) {
				panic(s.Name + ": capture is not UTF-8")
			}
			s.Request = normalized
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
	coreusage.RegisterPlugin(captured)
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
