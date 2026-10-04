package main

import (
	"encoding/json"
	"fmt"
	"sort"
	"strings"
)

// plugin is one recorder copy (<id>.so) and its config lines.
type plugin struct {
	ID       string
	Priority int
	Caps     string
	Extra    string
}

// config is the server config: loopback only, no control panel download, and the
// given client keys and recorder plugins (each records to RECDIR under its own ID).
func config(apiKeys []string, plugins []plugin) string {
	var b strings.Builder
	b.WriteString("config-version: 8\nhost: 127.0.0.1\nport: PORT\nauth-dir: AUTHDIR\n")
	b.WriteString("remote-management:\n  disable-control-panel: true\n")
	if len(apiKeys) > 0 {
		b.WriteString("api-keys:\n")
		for _, key := range apiKeys {
			fmt.Fprintf(&b, "  - %q\n", key)
		}
	}
	b.WriteString("plugins:\n  enabled: true\n  dir: PLUGINDIR\n  configs:\n")
	for _, p := range plugins {
		fmt.Fprintf(&b, "    %s:\n      enabled: true\n      record: RECDIR\n      label: %s\n      caps: %s\n", p.ID, p.ID, p.Caps)
		if p.Priority != 0 {
			fmt.Fprintf(&b, "      priority: %d\n", p.Priority)
		}
		b.WriteString(p.Extra)
	}
	return b.String()
}

func files(plugins []plugin) map[string]string {
	out := map[string]string{}
	for _, p := range plugins {
		out[p.ID+".so"] = "recorder"
	}
	return out
}

func get(path string, headers ...[2]string) step {
	return step{Op: "http", Args: httpArgs{Method: "GET", Path: path, Headers: headers}}
}

func post(path, body string, headers ...[2]string) step {
	return step{Op: "http", Args: httpArgs{Method: "POST", Path: path, Headers: headers, Body: body}}
}

func records(methods ...string) step {
	sort.Strings(methods)
	return step{Op: "records", Args: recordsArgs{Methods: methods}}
}

func reply(label, method, envelope string) step {
	return step{Op: "respond", Args: respond{Label: label, Method: method, Envelope: envelope}}
}

const (
	authenticate = "frontend_auth.authenticate"
	accepted     = `{"ok":true,"result":{"authenticated":true,"principal":"user-1","metadata":{"team":"a"}}}`
	declined     = `{"ok":true,"result":{"authenticated":false,"principal":"ignored"}}`
	rpcFailure   = `{"ok":false,"error":{"code":"denied","message":"no"}}`
)

func bearer(key string) [2]string { return [2]string{"Authorization", "Bearer " + key} }

func scenarios() []scenario {
	return []scenario{frontendAuthOpen(), frontendAuthAfterKeys(), frontendAuthExclusive(), modelRouter(), interceptors(), passthroughInterceptors(), droppedBootstrap(), usage(), modelList()}
}

// A usage plugin gets every attempt's record (the usage queue is off): a success, a
// stream and an upstream failure.
func usage() scenario {
	plugins := []plugin{{ID: "us", Caps: "usage_plugin"}}
	compat := func(name, path, model string) string {
		return "  - name: " + name + "\n    base-url: UPSTREAM" + path + "\n    api-key-entries:\n      - api-key: fake-" + name +
			"\n    models:\n      - name: " + model + "\n        alias: " + name + "-alias\n"
	}
	cfg := config([]string{"k1"}, plugins) + "openai-compatibility:\n" + compat("up", "/v1", "up-model") +
		compat("down", "/down", "down-model") + compat("sse", "/sse", "sse-model")
	key := bearer("k1")
	chat := func(body string) step {
		return post("/v1/chat/completions", body, key, [2]string{"Content-Type", "application/json"})
	}
	all := records("usage.")
	pause := step{Op: "sleep", Args: 400}
	sse := `data: {"id":"s1","object":"chat.completion.chunk","created":1,"model":"sse-model","choices":[{"index":0,"delta":{"content":"a"}}]}` + "\n\n" +
		`data: {"id":"s1","object":"chat.completion.chunk","created":1,"model":"sse-model","choices":[{"index":0,"delta":{"content":"b"},"finish_reason":"stop"}],"usage":{"prompt_tokens":4,"completion_tokens":5,"total_tokens":9}}` + "\n\n" +
		"data: [DONE]\n\n"
	return scenario{
		Name:    "usage plugins",
		Config:  cfg,
		Plugins: files(plugins),
		Upstream: map[string]route{
			"/v1/chat/completions": {Status: 200, Headers: map[string]string{"Content-Type": "application/json", "X-Up": "u"},
				Body: `{"id":"up-1","object":"chat.completion","created":1,"model":"up-model-2025","service_tier":"default","choices":[{"index":0,"message":{"role":"assistant","content":"x"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3,"prompt_tokens_details":{"cached_tokens":1}}}`},
			"/sse/chat/completions": {Status: 200, Headers: map[string]string{"Content-Type": "text/event-stream"}, Body: sse},
		},
		Steps: []step{
			chat(`{"model":"up-alias","service_tier":"flex","messages":[{"role":"user","content":"hi"}]}`),
			pause,
			all,
			chat(`{"model":"sse-alias","stream":true,"messages":[]}`),
			pause,
			all,
			chat(`{"model":"down-alias","messages":[]}`),
			pause,
			all,
		},
	}
}

const (
	before        = "request.intercept_before"
	after         = "request.intercept_after"
	respIntercept = "response.intercept_after"
	chunkMethod   = "response.intercept_stream_chunk"
	complete      = "request.complete"
)

// Request interceptors before and after credential selection, response and stream
// chunk interceptors, and the request lifecycle, on built-in providers and on a plugin
// executor. Lifecycle completions are asynchronous, hence the pauses.
func interceptors() scenario {
	plugins := []plugin{
		{ID: "ic", Priority: 3, Caps: "request_interceptor,response_interceptor,response_stream_interceptor,request_lifecycle_plugin"},
		{ID: "rt", Priority: 2, Caps: "model_router,executor"},
	}
	compat := func(name, path, model string) string {
		return "  - name: " + name + "\n    base-url: UPSTREAM" + path + "\n    api-key-entries:\n      - api-key: fake-" + name +
			"\n    models:\n      - name: " + model + "\n        alias: " + name + "-alias\n"
	}
	cfg := config([]string{"k1"}, plugins) + "openai-compatibility:\n" + compat("up", "/v1", "up-model") +
		compat("down", "/down", "down-model") + compat("sse", "/sse", "sse-model")
	key := bearer("k1")
	chat := func(body string, headers ...[2]string) step {
		return post("/v1/chat/completions", body, append([][2]string{key, {"Content-Type", "application/json"}}, headers...)...)
	}
	all := records("request.", "response.", "executor.execute", "executor.execute_stream")
	pause := step{Op: "sleep", Args: 400}
	sse := `data: {"id":"s1","object":"chat.completion.chunk","created":1,"model":"sse-model","choices":[{"index":0,"delta":{"content":"a"}}]}` + "\n\n" +
		`data: {"id":"s1","object":"chat.completion.chunk","created":1,"model":"sse-model","choices":[{"index":0,"delta":{"content":"b"},"finish_reason":"stop"}]}` + "\n\n" +
		"data: [DONE]\n\n"
	return scenario{
		Name:    "interceptors and request lifecycle",
		Config:  cfg,
		Plugins: files(plugins),
		Upstream: map[string]route{
			"/v1/chat/completions": {Status: 200, Headers: map[string]string{"Content-Type": "application/json", "X-Up": "u"},
				Body: `{"id":"up-1","object":"chat.completion","created":1,"model":"up-model","choices":[{"index":0,"message":{"role":"assistant","content":"from upstream"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}}`},
			"/sse/chat/completions": {Status: 200, Headers: map[string]string{"Content-Type": "text/event-stream"}, Body: sse},
		},
		Steps: []step{
			// Interceptors that fail are skipped; the lifecycle still completes.
			chat(`{"model":"up-alias","messages":[{"role":"user","content":"hi"}]}`, [2]string{"X-Custom", "c"}),
			pause,
			all,
			{Op: "upstream"},
			// Interceptors rewrite the request before and after selection, and the response.
			reply("ic", before, envelope(map[string]any{"Headers": map[string][]string{"X-Before": {"1"}},
				"Body": []byte(`{"model":"up-alias","messages":[{"role":"user","content":"rewritten"}]}`), "path": " /v1/custom "})),
			reply("ic", after, envelope(map[string]any{"Headers": map[string][]string{"X-After": {"2"}}, "ClearHeaders": []string{"x-custom"}})),
			reply("ic", respIntercept, envelope(map[string]any{"Headers": map[string][]string{"X-Resp": {"3"}, "X-Up": {"changed"}},
				"Body": []byte(`{"intercepted":true}`)})),
			chat(`{"model":"up-alias","messages":[{"role":"user","content":"hi"}]}`, [2]string{"X-Custom", "c"}),
			pause,
			all,
			{Op: "upstream"},
			// Termination before and after selection, and a status out of range.
			reply("ic", before, envelope(map[string]any{"Terminate": true, "StatusCode": 418,
				"ResponseHeaders": map[string][]string{"X-T": {"t"}, "Content-Type": {"text/plain"}}, "ResponseBody": []byte("teapot")})),
			chat(`{"model":"up-alias","messages":[]}`),
			reply("ic", before, ""),
			reply("ic", after, envelope(map[string]any{"Terminate": true, "StatusCode": 99, "ResponseBody": []byte(`{"blocked":true}`)})),
			chat(`{"model":"up-alias","messages":[]}`),
			pause,
			all,
			{Op: "upstream"},
			reply("ic", after, ""),
			reply("ic", respIntercept, ""),
			// An upstream failure.
			chat(`{"model":"down-alias","messages":[]}`),
			pause,
			all,
			{Op: "upstream"},
			// A stream: the header-init call, then one call per chunk; a dropped chunk.
			reply("ic", chunkMethod, envelope(map[string]any{"Headers": map[string][]string{"X-Chunk": {"h"}}})),
			chat(`{"model":"sse-alias","stream":true,"messages":[]}`),
			pause,
			all,
			{Op: "upstream"},
			reply("ic", chunkMethod, envelope(map[string]any{"DropChunk": true})),
			chat(`{"model":"sse-alias","stream":true,"messages":[]}`),
			pause,
			all,
			{Op: "upstream"},
			reply("ic", chunkMethod, ""),
			// The plugin executor path.
			reply("rt", routeMethod, routeTo("self", "", "")),
			reply("ic", before, envelope(map[string]any{"Headers": map[string][]string{"X-Before": {"p"}}})),
			reply("ic", after, envelope(map[string]any{"Body": []byte(`{"model":"m1","messages":[{"role":"user","content":"after"}]}`)})),
			reply("ic", respIntercept, envelope(map[string]any{"Body": []byte(`{"plugin":"intercepted"}`)})),
			chat(`{"model":"m1","messages":[]}`),
			pause,
			all,
			reply("ic", respIntercept, ""),
			chat(`{"model":"m1","stream":true,"messages":[]}`),
			pause,
			all,
			reply("ic", after, envelope(map[string]any{"Terminate": true, "StatusCode": 429, "ResponseBody": []byte("later")})),
			chat(`{"model":"m1","messages":[]}`),
			pause,
			all,
			reply("ic", before, ""),
			reply("ic", after, ""),
			reply("rt", routeMethod, ""),
		},
	}
}

// Under requests.passthrough-headers the final intercepted headers go downstream: a
// header an interceptor clears stays away, and the handler's own headers win.
func passthroughInterceptors() scenario {
	plugins := []plugin{{ID: "ic", Caps: "response_interceptor,response_stream_interceptor"}}
	cfg := config([]string{"k1"}, plugins) + "requests:\n  passthrough-headers: true\nopenai-compatibility:\n" +
		"  - name: up\n    base-url: UPSTREAM/v1\n    api-key-entries:\n      - api-key: fake-up\n    models:\n      - name: up-model\n        alias: up-alias\n" +
		"  - name: sse\n    base-url: UPSTREAM/sse\n    api-key-entries:\n      - api-key: fake-sse\n    models:\n      - name: sse-model\n        alias: sse-alias\n"
	key := bearer("k1")
	chat := func(body string) step {
		return post("/v1/chat/completions", body, key, [2]string{"Content-Type", "application/json"})
	}
	sse := `data: {"id":"s1","object":"chat.completion.chunk","created":1,"model":"sse-model","choices":[{"index":0,"delta":{"content":"a"},"finish_reason":"stop"}]}` + "\n\n" + "data: [DONE]\n\n"
	return scenario{
		Name:    "interceptors with passthrough headers",
		Config:  cfg,
		Plugins: files(plugins),
		Upstream: map[string]route{
			"/v1/chat/completions": {Status: 200, Headers: map[string]string{"Content-Type": "application/json", "X-Up": "u", "X-Keep": "k"},
				Body: `{"id":"up-1","object":"chat.completion","created":1,"model":"up-model","choices":[{"index":0,"message":{"role":"assistant","content":"x"},"finish_reason":"stop"}]}`},
			"/sse/chat/completions": {Status: 200, Headers: map[string]string{"Content-Type": "text/event-stream", "X-Up": "u"}, Body: sse},
		},
		Responds: []respond{
			{Label: "ic", Method: respIntercept, Envelope: envelope(map[string]any{"ClearHeaders": []string{"x-up"}, "Headers": map[string][]string{"X-New": {"n"}, "Content-Type": {"text/plain"}}})},
			{Label: "ic", Method: chunkMethod, Envelope: envelope(map[string]any{"ClearHeaders": []string{"x-up"}, "Headers": map[string][]string{"X-Chunk": {"c"}, "Content-Type": {"text/plain"}}})},
		},
		Steps: []step{
			chat(`{"model":"up-alias","messages":[]}`),
			chat(`{"model":"sse-alias","stream":true,"messages":[]}`),
		},
	}
}

// A stream whose only chunk an interceptor drops, then a transport error, is still
// before its first delivered chunk: the bootstrap retry runs (one lifecycle).
func droppedBootstrap() scenario {
	plugins := []plugin{{ID: "ic", Caps: "response_stream_interceptor,request_lifecycle_plugin"}}
	cfg := config([]string{"k1"}, plugins) + "requests:\n  streaming:\n    bootstrap-retries: 1\nopenai-compatibility:\n" +
		"  - name: sse\n    base-url: UPSTREAM/sse\n    api-key-entries:\n      - api-key: fake-sse\n    models:\n      - name: sse-model\n        alias: sse-alias\n"
	chunk := `data: {"id":"s1","object":"chat.completion.chunk","created":1,"model":"sse-model","choices":[{"index":0,"delta":{"content":"a"}}]}` + "\n\n"
	done := `data: {"id":"s2","object":"chat.completion.chunk","created":1,"model":"sse-model","choices":[{"index":0,"delta":{"content":"b"},"finish_reason":"stop"}]}` + "\n\ndata: [DONE]\n\n"
	sse := map[string]string{"Content-Type": "text/event-stream"}
	drop := envelope(map[string]any{"DropChunk": true})
	return scenario{
		Name:    "dropped chunks inside the bootstrap retries",
		Config:  cfg,
		Plugins: files(plugins),
		Upstream: map[string]route{
			"/sse/chat/completions": {Status: 200, Headers: sse, Body: chunk, Truncate: true,
				Then: []route{{Status: 200, Headers: sse, Body: done}}},
		},
		Steps: []step{
			reply("ic", chunkMethod, drop),
			post("/v1/chat/completions", `{"model":"sse-alias","stream":true,"messages":[]}`, bearer("k1")),
			step{Op: "sleep", Args: 400},
			records("request.complete"),
			{Op: "upstream"},
		},
	}
}

// Model lists go through the response interceptors with a lifecycle of their own. No
// models are registered: Go's list order follows map iteration.
func modelList() scenario {
	plugins := []plugin{{ID: "ic", Caps: "response_interceptor,request_lifecycle_plugin"}}
	key := bearer("k1")
	return scenario{
		Name:    "model list interceptors",
		Config:  config([]string{"k1"}, plugins),
		Plugins: files(plugins),
		Steps: []step{
			get("/v1/models", key),
			step{Op: "sleep", Args: 400},
			records("request.", "response."),
			reply("ic", respIntercept, envelope(map[string]any{"Headers": map[string][]string{"X-Models": {"m"}}, "Body": []byte(`{"object":"list","data":[{"id":"x"}]}`)})),
			get("/v1/models", key, [2]string{"Anthropic-Version", "2023-06-01"}),
			get("/v1beta/models", key),
			get("/v1/models?client_version=1.0.0", key),
			step{Op: "sleep", Args: 400},
			records("request.", "response."),
		},
	}
}

// envelope is a recorder answer: {"ok":true,"result":<result>}.
func envelope(result any) string {
	raw, err := json.Marshal(map[string]any{"ok": true, "result": result})
	check(err)
	return string(raw)
}

func routeTo(kind, target, model string) string {
	return envelope(map[string]any{"Handled": true, "TargetKind": kind, "Target": target, "TargetModel": model, "Reason": "test"})
}

const (
	routeMethod   = "model.route"
	execute       = "executor.execute"
	executeStream = "executor.execute_stream"
	countTokens   = "executor.count_tokens"
	chatBody      = `{"model":"m1","messages":[{"role":"user","content":"hi"}]}`
	completion    = `{"id":"c1","object":"chat.completion","created":1,"model":"m1","choices":[{"index":0,"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}`
	chunk1        = `data: {"id":"c1","object":"chat.completion.chunk","created":1,"model":"m1","choices":[{"index":0,"delta":{"role":"assistant","content":"he"}}]}` + "\n\n"
	chunk2        = `data: {"id":"c1","object":"chat.completion.chunk","created":1,"model":"m1","choices":[{"index":0,"delta":{"content":"llo"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}` + "\n\n"
)

// A model router sends requests to plugin executors (itself or another plugin) or to a
// built-in provider with credentials; unusable answers fall through to the registry.
// The executors translate between the client's format and their declared formats.
func modelRouter() scenario {
	plugins := []plugin{
		{ID: "rt", Priority: 2, Caps: "model_router,executor"},
		{ID: "exb", Caps: "executor", Extra: "      inputs: claude\n      outputs: claude\n"},
	}
	cfg := config([]string{"k1"}, plugins) + "openai-compatibility:\n  - name: up\n    base-url: UPSTREAM/v1\n" +
		"    api-key-entries:\n      - api-key: fake-up-key\n    models:\n      - name: up-model\n        alias: up-alias\n"
	key := bearer("k1")
	chat := func(body string, headers ...[2]string) step {
		return post("/v1/chat/completions", body, append([][2]string{key, {"Content-Type", "application/json"}}, headers...)...)
	}
	all := records(routeMethod, "executor.")
	return scenario{
		Name:    "model router and plugin executors",
		Config:  cfg,
		Plugins: files(plugins),
		Upstream: map[string]route{
			"/v1/chat/completions": {Status: 200, Headers: map[string]string{"Content-Type": "application/json"},
				Body: `{"id":"up-1","object":"chat.completion","created":1,"model":"up-model","choices":[{"index":0,"message":{"role":"assistant","content":"from upstream"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}}`},
		},
		Steps: []step{
			// No answer from the router: the registry does not know m1.
			chat(chatBody),
			all,
			// Self: the executor echoes the payload.
			reply("rt", routeMethod, routeTo("self", "", "")),
			chat(chatBody, [2]string{"Idempotency-Key", " idem-1 "}, [2]string{"X-Custom", "v"}),
			all,
			post("/v1/chat/completions?alt=sse&x=1", `{"model":"m1","service_tier":"flex","generate":false,"reasoning_effort":"high","messages":[]}`, key),
			all,
			// Translation: a Claude client, an OpenAI executor.
			reply("rt", execute, envelope(map[string]any{"Payload": []byte(completion), "Headers": map[string][]string{"X-Plugin": {"rt"}}})),
			post("/v1/messages", `{"model":"m1","max_tokens":5,"messages":[{"role":"user","content":"hi"}]}`, key),
			all,
			post("/v1/responses", `{"model":"m1","input":"hi"}`, key),
			all,
			// Streams: inline chunks, then chunks over host.stream.emit.
			reply("rt", executeStream, envelope(map[string]any{"headers": map[string][]string{"X-Stream": {"inline"}},
				"chunks": []map[string]any{{"Payload": []byte(chunk1)}, {"Payload": []byte(chunk2)}}})),
			chat(`{"model":"m1","stream":true,"messages":[{"role":"user","content":"hi"}]}`),
			all,
			post("/v1/messages", `{"model":"m1","max_tokens":5,"stream":true,"messages":[{"role":"user","content":"hi"}]}`, key),
			all,
			post("/v1/responses", `{"model":"m1","stream":true,"input":"hi"}`, key),
			all,
			reply("rt", executeStream, ""),
			chat(`{"model":"m1","stream":true,"messages":[]}`),
			all,
			// The plugin closes its stream with an error after the data; then it fails
			// before any data.
			chat(`{"model":"m1","stream":true,"messages":[],"x":"fail"}`),
			reply("rt", executeStream, `{"ok":false,"error":{"code":"denied","message":"nope","http_status":403}}`),
			chat(`{"model":"m1","stream":true,"messages":[]}`),
			post("/v1/messages", `{"model":"m1","max_tokens":5,"stream":true,"messages":[]}`, key),
			reply("rt", executeStream, ""),
			all,
			// Executor errors.
			reply("rt", execute, `{"ok":false,"error":{"code":"rate_limited","message":"slow down","http_status":429}}`),
			chat(chatBody),
			reply("rt", execute, `{"ok":false,"error":{"code":"broken","message":"no status"}}`),
			chat(chatBody),
			all,
			reply("rt", execute, ""),
			// Another plugin's executor, which speaks Claude.
			reply("rt", routeMethod, routeTo("executor", " exb ", "")),
			chat(chatBody),
			post("/v1/messages/count_tokens", `{"model":"m1","messages":[{"role":"user","content":"hi"}]}`, key),
			all,
			// Unusable targets fall through.
			reply("rt", routeMethod, routeTo("executor", "missing", "")),
			chat(chatBody),
			reply("rt", routeMethod, routeTo("provider", "nope", "x")),
			chat(chatBody),
			reply("rt", routeMethod, routeTo("bogus", "rt", "")),
			chat(chatBody),
			reply("rt", routeMethod, `{"ok":false,"error":{"code":"x","message":"router down"}}`),
			chat(chatBody),
			all,
			// A built-in provider with credentials, with and without a target model.
			reply("rt", routeMethod, routeTo("provider", " OpenAI-Compatible-UP ", "up-model")),
			chat(chatBody),
			reply("rt", routeMethod, routeTo("provider", "openai-compatible-up", "")),
			chat(`{"model":"up-alias","messages":[]}`),
			reply("rt", routeMethod, routeTo("provider", "up", "")),
			chat(`{"model":"up-alias","messages":[{"role":"user","content":"registry"}]}`),
			all,
			{Op: "upstream"},
		},
	}
}

// A frontend auth plugin with no client keys: it alone decides, and declining (or
// failing) leaves "Missing API key". What it is asked: method, decoded path, canonical
// headers without Host, the parsed query and the body, which the handler still reads.
func frontendAuthOpen() scenario {
	plugins := []plugin{{ID: "fa", Caps: "frontend_auth_provider"}}
	return scenario{
		Name:    "frontend auth without client keys",
		Config:  config(nil, plugins),
		Plugins: files(plugins),
		Steps: []step{
			get("/v1/models"),
			records(authenticate),
			reply("fa", authenticate, accepted),
			get("/v1/models?x=1&y=a%20b&y=2&bad=%zz&semi=1;2&plus=a+b",
				bearer("anything"), [2]string{"X-Multi", "a"}, [2]string{"x-multi", "b"}, [2]string{"x-lower_case", "v"},
				[2]string{"X-Goog-Api-Key", "g"}),
			records(authenticate),
			post("/v1/chat/completions", `{"model":"no-such-model","messages":[{"role":"user","content":"hi"}]}`,
				[2]string{"Content-Type", "application/json"}),
			records(authenticate),
			get("/v1beta/models/gem%69ni-x%2Fy"),
			records(authenticate),
			reply("fa", authenticate, declined),
			get("/v1/models", bearer("anything")),
			records(authenticate),
			reply("fa", authenticate, rpcFailure),
			get("/v1/models"),
			post("/v1/realtime/client_secrets", `{}`),
			records(authenticate),
			reply("fa", authenticate, `{"ok":true,"result":{"authenticated":true}}`),
			get("/v1/models"),
			get("/healthz"),
			records(authenticate),
		},
	}
}

// Client keys come first (registered before plugins); a request the keys reject is
// still offered to the plugin, and the final error says "Invalid API key" when any
// provider saw an invalid credential.
func frontendAuthAfterKeys() scenario {
	plugins := []plugin{{ID: "fa", Caps: "frontend_auth_provider"}}
	return scenario{
		Name:     "frontend auth after client keys",
		Config:   config([]string{"k1"}, plugins),
		Plugins:  files(plugins),
		Responds: []respond{{Label: "fa", Method: authenticate, Envelope: accepted}},
		Steps: []step{
			get("/v1/models", bearer("k1")),
			records(authenticate),
			get("/v1/models", bearer("wrong")),
			records(authenticate),
			reply("fa", authenticate, declined),
			get("/v1/models", bearer("wrong")),
			get("/v1/models"),
			get("/v1/models?key=k1"),
			post("/v1/realtime/client_secrets", `{}`, bearer("wrong")),
			records(authenticate),
		},
	}
}

// Exclusive frontend auth: the highest-priority exclusive plugin (then the lowest ID) is
// the only provider, so client keys stop working.
func frontendAuthExclusive() scenario {
	plugins := []plugin{
		{ID: "ex-b", Priority: 5, Caps: "frontend_auth_provider,frontend_auth_provider_exclusive"},
		{ID: "ex-a", Priority: 5, Caps: "frontend_auth_provider,frontend_auth_provider_exclusive"},
		{ID: "ex-low", Priority: 1, Caps: "frontend_auth_provider,frontend_auth_provider_exclusive"},
		{ID: "plain", Priority: 9, Caps: "frontend_auth_provider"},
	}
	return scenario{
		Name:    "exclusive frontend auth",
		Config:  config([]string{"k1"}, plugins),
		Plugins: files(plugins),
		Responds: []respond{
			{Label: "ex-b", Method: authenticate, Envelope: accepted},
			{Label: "ex-low", Method: authenticate, Envelope: accepted},
			{Label: "plain", Method: authenticate, Envelope: accepted},
		},
		Steps: []step{
			get("/v1/models", bearer("k1")),
			records(authenticate),
			reply("ex-a", authenticate, accepted),
			get("/v1/models", bearer("k1")),
			records(authenticate),
		},
	}
}
