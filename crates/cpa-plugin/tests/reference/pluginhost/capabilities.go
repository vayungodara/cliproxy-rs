package main

// Capability adapters: canned plugin answers (respond) and calls into the exported Go
// Host API (call). Results are projected to JSON the Rust test reproduces.

import (
	"context"
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"net/http/httptest"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"time"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/registry"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/thinking"
	sdkaccess "github.com/router-for-me/CLIProxyAPI/v8/sdk/access"
	coreauth "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/auth"
	"github.com/router-for-me/CLIProxyAPI/v8/sdk/pluginapi"
	sdktranslator "github.com/router-for-me/CLIProxyAPI/v8/sdk/translator"
)

// respond stores the envelope a recorder answers method with; an empty envelope
// removes it.
func (r *runner) respond(label, method, envelope string) {
	dir := filepath.Join(r.recordDir, "respond", label)
	check(os.MkdirAll(dir, 0o755))
	path := filepath.Join(dir, method+".json")
	if envelope == "" {
		_ = os.Remove(path)
	} else {
		check(os.WriteFile(path, []byte(envelope), 0o644))
	}
	r.add("respond", map[string]string{"label": label, "method": method, "envelope": envelope}, nil)
}

// settle waits for asynchronous plugin calls (request.complete, usage) to land.
func (r *runner) settle() {
	time.Sleep(500 * time.Millisecond)
	r.add("settle", nil, nil)
}

type callArgs struct {
	Fn       string              `json:"fn"`
	Req      json.RawMessage     `json:"req,omitempty"`
	From     string              `json:"from,omitempty"`
	To       string              `json:"to,omitempty"`
	Model    string              `json:"model,omitempty"`
	Original string              `json:"original,omitempty"`
	Request  string              `json:"request,omitempty"`
	Body     string              `json:"body,omitempty"`
	Stream   bool                `json:"stream,omitempty"`
	Provider string              `json:"provider,omitempty"`
	BaseURL  string              `json:"base_url,omitempty"`
	State    string              `json:"state,omitempty"`
	Metadata map[string]any      `json:"metadata,omitempty"`
	PluginID string              `json:"plugin_id,omitempty"`
	Path     string              `json:"path,omitempty"`
	FileName string              `json:"file_name,omitempty"`
	Args     []string            `json:"args,omitempty"`
	Builtin  []string            `json:"builtin,omitempty"`
	Method   string              `json:"method,omitempty"`
	Target   string              `json:"target,omitempty"`
	Headers  map[string][]string `json:"headers,omitempty"`
	Budget   int                 `json:"budget,omitempty"`
	Level    string              `json:"level,omitempty"`
}

func decode[T any](raw json.RawMessage) T {
	var out T
	if len(raw) > 0 {
		check(json.Unmarshal(raw, &out))
	}
	return out
}

func errString(err error) string {
	if err == nil {
		return ""
	}
	return err.Error()
}

func outcome(resp any, handled bool, err error) map[string]any {
	if !handled {
		resp = nil
	}
	return map[string]any{"resp": resp, "handled": handled, "error": errString(err)}
}

func authJSON(a *coreauth.Auth) any {
	if a == nil {
		return nil
	}
	storage := ""
	if s, ok := a.Storage.(interface{ RawJSON() []byte }); ok {
		storage = string(s.RawJSON())
	}
	return map[string]any{
		"id": a.ID, "provider": a.Provider, "file_name": a.FileName, "label": a.Label,
		"prefix": a.Prefix, "proxy_url": a.ProxyURL, "disabled": a.Disabled,
		"status": string(a.Status), "metadata": a.Metadata, "attributes": a.Attributes,
		"next_refresh_after": a.NextRefreshAfter, "storage": storage,
	}
}

func modelIDs(models []*registry.ModelInfo) []string {
	out := []string{}
	for _, m := range models {
		out = append(out, m.ID)
	}
	return out
}

type fakeRegistry struct{ calls []any }

func (f *fakeRegistry) RegisterClient(clientID, provider string, models []*registry.ModelInfo) {
	f.calls = append(f.calls, map[string]any{"op": "register", "client": clientID, "provider": provider, "models": modelIDs(models)})
}

func (f *fakeRegistry) UnregisterClient(clientID string) {
	f.calls = append(f.calls, map[string]any{"op": "unregister", "client": clientID})
}

// captureOutput runs fn with os.Stdout and os.Stderr redirected.
func captureOutput(fn func()) (string, string) {
	outR, outW, err := os.Pipe()
	check(err)
	errR, errW, err := os.Pipe()
	check(err)
	stdout, stderr := os.Stdout, os.Stderr
	os.Stdout, os.Stderr = outW, errW
	done := make(chan [2]string)
	go func() {
		o, _ := io.ReadAll(outR)
		e, _ := io.ReadAll(errR)
		done <- [2]string{string(o), string(e)}
	}()
	fn()
	os.Stdout, os.Stderr = stdout, stderr
	_ = outW.Close()
	_ = errW.Close()
	got := <-done
	return got[0], got[1]
}

func (r *runner) call(args callArgs) {
	ctx := context.Background()
	h := r.host
	var result any
	switch args.Fn {
	case "has":
		result = map[string]any{
			"request_interceptors":    h.HasRequestInterceptors(),
			"stream_interceptors":     h.HasStreamInterceptors(),
			"stream_request_body":     h.StreamChunkPayloadIncludesRequestBody(),
			"stream_history":          h.StreamChunkPayloadIncludesHistory(),
			"websocket_observers":     h.HasWebSocketResponseObservers(),
			"scheduler":               h.HasScheduler(),
			"scheduler_across":        h.SchedulerWantsAcrossPriorities(),
			"model_routers":           h.HasModelRouters(),
			"quota_identifiers":       h.QuotaProviderIdentifiers(),
			"auth_identifiers":        h.AuthProviderIdentifiers(),
			"auth_provider_rec_c":     h.HasAuthProvider("REC-C"),
			"quota_provider_plugin_a": h.HasQuotaProviderForPlugin("recorder-a"),
		}
	case "intercept_before":
		result = h.InterceptRequestBeforeAuth(ctx, decode[pluginapi.RequestInterceptRequest](args.Req))
	case "intercept_after":
		result = h.InterceptRequestAfterAuth(ctx, decode[pluginapi.RequestInterceptRequest](args.Req))
	case "intercept_response":
		result = h.InterceptResponse(ctx, decode[pluginapi.ResponseInterceptRequest](args.Req))
	case "intercept_stream_chunk":
		result = h.InterceptStreamChunk(ctx, decode[pluginapi.StreamChunkInterceptRequest](args.Req))
	case "complete":
		h.CompleteRequest(ctx, decode[pluginapi.RequestCompletion](args.Req))
	case "ws_event":
		h.ObserveWebSocketResponseEvent(ctx, decode[pluginapi.WebSocketResponseEvent](args.Req))
	case "pick_auth":
		resp, handled, err := h.PickAuth(ctx, decode[pluginapi.SchedulerPickRequest](args.Req))
		result = outcome(resp, handled, err)
	case "route_model":
		resp, handled := h.RouteModel(ctx, decode[pluginapi.ModelRouteRequest](args.Req))
		result = outcome(resp, handled, nil)
	case "normalize_request":
		result = string(h.NormalizeRequest(ctx, sdktranslator.FromString(args.From), sdktranslator.FromString(args.To), args.Model, []byte(args.Body), args.Stream))
	case "translate_request":
		body, ok := h.TranslateRequest(ctx, sdktranslator.FromString(args.From), sdktranslator.FromString(args.To), args.Model, []byte(args.Body), args.Stream)
		result = map[string]any{"body": string(body), "ok": ok}
	case "normalize_response_before":
		result = string(h.NormalizeResponseBefore(ctx, sdktranslator.FromString(args.From), sdktranslator.FromString(args.To), args.Model, []byte(args.Original), []byte(args.Request), []byte(args.Body), args.Stream))
	case "translate_response":
		body, ok := h.TranslateResponse(ctx, sdktranslator.FromString(args.From), sdktranslator.FromString(args.To), args.Model, []byte(args.Original), []byte(args.Request), []byte(args.Body), args.Stream)
		result = map[string]any{"body": string(body), "ok": ok}
	case "normalize_response_after":
		result = string(h.NormalizeResponseAfter(ctx, sdktranslator.FromString(args.From), sdktranslator.FromString(args.To), args.Model, []byte(args.Original), []byte(args.Request), []byte(args.Body), args.Stream))
	case "thinking":
		applier := thinking.GetProviderApplier(args.Provider)
		plugin := fmt.Sprintf("%T", applier) == "*pluginhost.thinkingAdapter"
		body := args.Body
		if plugin {
			out, err := applier.Apply([]byte(args.Body), thinking.ThinkingConfig{Mode: thinking.ModeBudget, Budget: args.Budget, Level: thinking.ThinkingLevel(args.Level)}, &registry.ModelInfo{ID: args.Model, Object: "model", OwnedBy: "tests", Type: "recorder"})
			check(err)
			body = string(out)
		}
		result = map[string]any{"plugin": plugin, "body": body}
	case "auth_data":
		result = authJSON(h.AuthDataToCoreAuth(decode[pluginapi.AuthData](args.Req), args.Path, args.FileName))
	case "parse_auths":
		auths, handled, err := h.ParseAuths(ctx, decode[pluginapi.AuthParseRequest](args.Req))
		list := []any{}
		for _, a := range auths {
			list = append(list, authJSON(a))
		}
		result = outcome(list, handled, err)
	case "start_login":
		resp, handled, err := h.StartLogin(ctx, args.Provider, args.BaseURL, args.Metadata)
		result = outcome(resp, handled, err)
	case "poll_login":
		resp, handled, err := h.PollLogin(ctx, args.Provider, args.State, args.Metadata)
		result = outcome(resp, handled, err)
	case "refresh_auth":
		auth := h.AuthDataToCoreAuth(decode[pluginapi.AuthData](args.Req), args.Path, "")
		refreshed, handled, err := h.RefreshAuth(ctx, auth)
		result = outcome(authJSON(refreshed), handled, err)
	case "quota_providers":
		result = h.QuotaProviders(ctx)
	case "describe_quota":
		resp, handled, err := h.DescribeQuota(ctx, args.PluginID)
		result = outcome(resp, handled, err)
	case "fetch_quota":
		resp, handled, err := h.FetchQuota(ctx, decode[pluginapi.QuotaFetchRequest](args.Req))
		result = outcome(resp, handled, err)
	case "fetch_quota_by_plugin":
		resp, handled, err := h.FetchQuotaByPlugin(ctx, args.PluginID, decode[pluginapi.QuotaFetchRequest](args.Req))
		result = outcome(resp, handled, err)
	case "reset_quota":
		resp, handled, err := h.ResetQuota(ctx, decode[pluginapi.QuotaResetRequest](args.Req))
		result = outcome(resp, handled, err)
	case "reset_quota_by_plugin":
		resp, handled, err := h.ResetQuotaByPlugin(ctx, args.PluginID, decode[pluginapi.QuotaResetRequest](args.Req))
		result = outcome(resp, handled, err)
	case "register_models":
		reg := &fakeRegistry{calls: []any{}}
		h.RegisterModels(ctx, reg)
		result = reg.calls
	case "models_for_auth":
		auth := h.AuthDataToCoreAuth(decode[pluginapi.AuthData](args.Req), args.Path, "")
		res := h.ModelsForAuth(ctx, auth)
		result = map[string]any{"provider": res.Provider, "models": modelIDs(res.Models), "auth": authJSON(res.Auth), "handled": res.Handled, "error": errString(res.Err)}
	case "frontend_auth":
		h.RegisterFrontendAuthProviders()
		req := httptest.NewRequest(args.Method, args.Target, strings.NewReader(args.Body))
		for k, vs := range args.Headers {
			for _, v := range vs {
				req.Header.Add(k, v)
			}
		}
		providers := []any{}
		for _, p := range sdkaccess.RegisteredProviders() {
			res, authErr := p.Authenticate(ctx, req)
			entry := map[string]any{"provider": p.Identifier(), "result": nil, "error": ""}
			if res != nil {
				entry["result"] = map[string]any{"provider": res.Provider, "principal": res.Principal, "metadata": res.Metadata}
			}
			if authErr != nil {
				entry["error"] = string(authErr.Code)
			}
			providers = append(providers, entry)
		}
		result = providers
	case "command_line":
		fs := flag.NewFlagSet("cliproxy", flag.ContinueOnError)
		fs.SetOutput(io.Discard)
		for _, name := range args.Builtin {
			fs.String(name, "", "")
		}
		h.RegisterCommandLineFlags(ctx, fs)
		names := []string{}
		fs.VisitAll(func(f *flag.Flag) { names = append(names, f.Name+"="+f.Value.String()) })
		sort.Strings(names)
		parseErr := fs.Parse(args.Args)
		exit, handled := 0, false
		stdout, stderr := captureOutput(func() {
			exit, handled = h.ExecuteCommandLine(ctx, "cliproxy", args.Args, "/etc/cliproxy/config.yaml", fs)
		})
		result = map[string]any{
			"flags": names, "parse_error": errString(parseErr), "triggered": h.HasTriggeredCommandLineFlags(),
			"exit": exit, "handled": handled, "stdout": stdout, "stderr": stderr,
		}
	default:
		panic("unknown call " + args.Fn)
	}
	r.add("call", args, result)
}
