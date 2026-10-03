package main

import (
	"encoding/json"
	"os"
	"path/filepath"
)

const capConfig = `auth-dir: RECORDDIR/auths
plugins:
  enabled: true
  dir: PLUGINDIR
  configs:
    recorder-a:
      enabled: true
      priority: 5
      record: RECORDDIR
      label: a
      caps: request_interceptor,response_interceptor,response_stream_interceptor,request_lifecycle_plugin,websocket_response_observer,scheduler,model_router,request_normalizer,request_translator,response_translator,response_before_translator,response_after_translator,thinking_applier,auth_provider,frontend_auth_provider,quota_provider,model_provider,command_line_plugin
    recorder-b:
      enabled: true
      priority: 5
      record: RECORDDIR
      label: b
      schema: 5
      caps: request_interceptor,response_interceptor,response_stream_interceptor,request_lifecycle_plugin,request_normalizer,response_before_translator,frontend_auth_provider,quota_provider,thinking_applier,model_registrar,command_line_plugin
    recorder-c:
      enabled: true
      priority: 1
      record: RECORDDIR
      label: c
      scope: static
      caps: executor,model_registrar,model_router,command_line_plugin,auth_provider
    recorder-d:
      enabled: true
      priority: 0
      record: RECORDDIR
      label: d
      caps: auth_provider
`

func ok(v any) string {
	raw, err := json.Marshal(map[string]any{"ok": true, "result": v})
	check(err)
	return string(raw)
}

func fail(code, message string) string {
	raw, err := json.Marshal(map[string]any{"ok": false, "error": map[string]any{"code": code, "message": message}})
	check(err)
	return string(raw)
}

func raw(s string) json.RawMessage { return json.RawMessage(s) }

// clear empties the plugin directory.
func (r *runner) clear() {
	entries, err := os.ReadDir(r.pluginDir)
	check(err)
	for _, entry := range entries {
		check(os.Remove(filepath.Join(r.pluginDir, entry.Name())))
	}
	r.add("clear", nil, nil)
}

func capabilityScenarios(r *runner) {
	r.clear()
	r.files(map[string]string{"recorder-a.so": "recorder", "recorder-b.so": "recorder", "recorder-c.so": "recorder", "recorder-d.so": "recorder"})
	// Identifiers asked at registration or refresh.
	r.respond("b", "thinking.identifier", ok(map[string]string{"identifier": "Claude"}))
	r.respond("c", "auth.identifier", ok(map[string]string{"identifier": " Rec-C "}))
	r.respond("b", "quota.identifier", ok(map[string]string{"identifier": "A"}))
	r.respond("d", "auth.identifier", ok(map[string]string{"identifier": ""}))
	r.apply(capConfig)
	r.call(callArgs{Fn: "has"})
	r.records()

	// Request interceptors: a's header updates and clears, then b sees a's result.
	r.respond("a", "request.intercept_before", ok(map[string]any{
		"Headers": map[string][]string{"X-A": {"1"}, "x-lower": {"l"}}, "ClearHeaders": []string{"x-drop"}, "Body": []byte(`{"a":1}`),
	}))
	r.respond("b", "request.intercept_before", ok(map[string]any{"Headers": map[string][]string{"X-B": {"2"}}}))
	interceptReq := raw(`{"RequestID":"r1","SourceFormat":"openai","ToFormat":"claude","Model":"m","RequestedModel":"m-req","Stream":true,
		"Headers":{"X-Drop":["d"],"X-Keep":["k"]},"Body":"e30=","Metadata":{"n":1,"s":"x"}}`)
	r.call(callArgs{Fn: "intercept_before", Req: interceptReq})
	r.respond("a", "request.intercept_after", ok(map[string]any{
		"Terminate": true, "StatusCode": 403, "ResponseHeaders": map[string][]string{"X-T": {"t"}}, "ResponseBody": []byte("denied"),
	}))
	r.call(callArgs{Fn: "intercept_after", Req: interceptReq})
	r.respond("a", "request.intercept_after", ok(map[string]any{"Terminate": true}))
	r.respond("b", "request.intercept_after", fail("boom", "after failed"))
	r.call(callArgs{Fn: "intercept_after", Req: interceptReq})
	r.records()

	// Response and stream-chunk interceptors; b speaks schema 5.
	r.respond("a", "response.intercept_after", ok(map[string]any{"Headers": map[string][]string{"X-R": {"a"}}, "Body": []byte("resp-a")}))
	r.respond("b", "response.intercept_after", fail("boom", "bad"))
	r.call(callArgs{Fn: "intercept_response", Req: raw(`{"RequestID":"r2","SourceFormat":"openai","Model":"m","Stream":false,
		"RequestHeaders":{"A":["1"]},"ResponseHeaders":{"Content-Type":["application/json"]},"OriginalRequest":"b3JpZw==",
		"RequestBody":"cmVx","Body":"cmVzcA==","StatusCode":200}`)})
	r.respond("a", "response.intercept_stream_chunk", ok(map[string]any{"Body": []byte("chunk-a"), "ClearHeaders": []string{"X-Gone"}}))
	r.respond("b", "response.intercept_stream_chunk", ok(map[string]any{"DropChunk": true}))
	chunkReq := raw(`{"RequestID":"r3","SourceFormat":"openai","Model":"m","RequestHeaders":{"A":["1"]},
		"ResponseHeaders":{"X-Gone":["g"],"X-Stay":["s"]},"OriginalRequest":"b3JpZw==","RequestBody":"cmVx",
		"Body":"Y2h1bms=","HistoryChunks":["aDE=","aDI="],"ChunkIndex":2}`)
	r.call(callArgs{Fn: "intercept_stream_chunk", Req: chunkReq})
	r.call(callArgs{Fn: "intercept_stream_chunk", Req: raw(`{"RequestID":"r3","ChunkIndex":-1,"ResponseHeaders":{"X-Init":["i"]}}`)})
	r.records()

	// Lifecycle completion and WebSocket observation are fire-and-forget.
	r.call(callArgs{Fn: "complete", Req: raw(`{"RequestID":"r4","SourceFormat":"openai","Model":"m","Outcome":"failed","StatusCode":502,
		"Error":"upstream","StartedAt":"2026-10-03T01:02:03.5Z","CompletedAt":"2026-10-03T01:02:04Z","Metadata":{"k":[1,2]}}`)})
	// Completion is asynchronous; let it land before the synchronous observation so the
	// record order is deterministic.
	r.settle()
	r.call(callArgs{Fn: "ws_event", Req: raw(`{"RequestID":"r5","TraceID":"t","SourceFormat":"openai-response","Payload":"eyJ0eXBlIjoicmVzcG9uc2UuZG9uZSJ9"}`)})
	r.settle()
	r.records()

	// Scheduler: the recorder picks the last candidate; then an unknown auth ID.
	pickReq := raw(`{"Provider":"claude","Providers":["claude","codex"],"Model":"m","Stream":true,"Options":{"Headers":{"X":["1"]}},
		"Candidates":[{"ID":"x1","Provider":"claude","Priority":1,"Status":"active"},{"ID":"x2","Provider":"codex","Attributes":{"k":"v"}}]}`)
	r.call(callArgs{Fn: "pick_auth", Req: pickReq})
	r.respond("a", "scheduler.pick", ok(map[string]any{"auth_id": "nope", "handled": true}))
	r.call(callArgs{Fn: "pick_auth", Req: pickReq})
	r.respond("a", "scheduler.pick", fail("pick_failed", "no capacity"))
	r.call(callArgs{Fn: "pick_auth", Req: pickReq})
	r.records()

	// Model routing: a's provider target has no credentials; c routes to itself.
	r.respond("a", "model.route", ok(map[string]any{"Handled": true, "TargetKind": "provider", "Target": " OpenAI "}))
	r.respond("c", "model.route", ok(map[string]any{"Handled": true, "TargetKind": "self", "Target": "ignored", "TargetModel": " m2 ", "Reason": "r"}))
	r.call(callArgs{Fn: "route_model", Req: raw(`{"SourceFormat":"openai","RequestedModel":"m","Stream":false,"Headers":{"H":["1"]},
		"Query":{"q":["1"]},"Body":"e30=","Metadata":{"x":true}}`)})
	r.call(callArgs{Fn: "route_model", Req: raw(`{"SourceFormat":"gemini-cli-unknown","RequestedModel":"m"}`)})
	r.records()

	// Request and response transforms.
	r.respond("a", "request.normalize", ok(map[string]any{"Body": []byte(`{"n":"a"}`)}))
	r.respond("b", "request.normalize", fail("x", "y"))
	r.call(callArgs{Fn: "normalize_request", From: "openai", To: "claude", Model: "m", Body: `{"in":1}`, Stream: true})
	r.respond("a", "request.translate", ok(map[string]any{"Body": []byte(`{"t":1}`)}))
	r.call(callArgs{Fn: "translate_request", From: "openai", To: "claude", Model: "m", Body: `{"in":1}`})
	r.respond("a", "request.translate", ok(map[string]any{}))
	r.call(callArgs{Fn: "translate_request", From: "openai", To: "claude", Model: "m", Body: `{"in":2}`})
	r.respond("a", "response.normalize_before", ok(map[string]any{"Body": []byte("nb-a")}))
	r.respond("b", "response.normalize_before", ok(map[string]any{"Body": []byte("nb-b")}))
	r.call(callArgs{Fn: "normalize_response_before", From: "claude", To: "openai", Model: "m", Original: "o", Request: "q", Body: "body", Stream: true})
	r.respond("a", "response.translate", ok(map[string]any{"Body": []byte("tr")}))
	r.call(callArgs{Fn: "translate_response", From: "claude", To: "openai", Model: "m", Original: "o", Request: "q", Body: "body"})
	r.respond("a", "response.normalize_after", fail("e", "f"))
	r.call(callArgs{Fn: "normalize_response_after", From: "claude", To: "openai", Model: "m", Original: "o", Request: "q", Body: "body"})
	r.records()

	// Thinking: a owns provider "a"; b's "claude" is shadowed by the native applier.
	r.respond("a", "thinking.apply", ok(map[string]any{"Body": []byte(`{"thinking":true}`)}))
	r.call(callArgs{Fn: "thinking", Provider: "A", Model: "m-think", Body: `{"x":1}`, Budget: 1024, Level: "high"})
	r.call(callArgs{Fn: "thinking", Provider: "claude", Model: "m-think", Body: `{"x":1}`, Budget: 1024})
	r.respond("a", "thinking.apply", ok(map[string]any{}))
	r.call(callArgs{Fn: "thinking", Provider: "a", Model: "m", Body: `{"x":2}`})
	r.records()

	// Auth providers.
	authData := raw(`{"Provider":" A ","FileName":"f.json","Label":" L ","Prefix":" p ","ProxyURL":" http://proxy.invalid ",
		"StorageJSON":"eyJ0b2tlbiI6InQiLCJwcmlvcml0eSI6MX0=","Metadata":{"email":"e@x","n":1.5,"base-url":"u"},"Attributes":{"k":"v"},
		"NextRefreshAfter":"2026-10-04T00:00:00Z"}`)
	r.call(callArgs{Fn: "auth_data", Req: authData, Path: "/auth/dir/f.json", FileName: "other.json"})
	r.call(callArgs{Fn: "auth_data", Req: raw(`{"Provider":"x","Disabled":true}`), FileName: "g.json"})
	r.respond("a", "auth.parse", ok(map[string]any{"Handled": true, "Auths": []json.RawMessage{authData, raw(`{"Provider":""}`)}}))
	r.call(callArgs{Fn: "parse_auths", Req: raw(`{"Provider":"A","Path":"/auth/f.json","FileName":"f.json","RawJSON":"e30="}`)})
	r.respond("a", "auth.parse", ok(map[string]any{"Handled": true, "Auth": json.RawMessage(authData)}))
	r.call(callArgs{Fn: "parse_auths", Req: raw(`{"Provider":"a","Path":"/auth/f.json"}`)})
	r.respond("a", "auth.parse", ok(map[string]any{"Handled": false}))
	r.call(callArgs{Fn: "parse_auths", Req: raw(`{"Provider":"a"}`)})
	// An RPC error is not handled; a handled answer with invalid auth data is.
	r.respond("a", "auth.parse", fail("parse_failed", "bad material"))
	r.call(callArgs{Fn: "parse_auths", Req: raw(`{"Provider":"a"}`)})
	r.call(callArgs{Fn: "parse_auths", Req: raw(`{"Path":"/auth/any.json"}`)})
	r.respond("a", "auth.parse", ok(map[string]any{"Handled": false}))
	r.respond("c", "auth.parse", ok(map[string]any{"Handled": true, "Auths": []map[string]any{{"Provider": " "}}}))
	r.call(callArgs{Fn: "parse_auths", Req: raw(`{"Provider":"rec-c"}`)})
	r.respond("c", "auth.parse", ok(map[string]any{"Handled": false}))
	r.respond("d", "auth.parse", ok(map[string]any{"Handled": true}))
	r.call(callArgs{Fn: "parse_auths", Req: raw(`{"Path":"/auth/any.json"}`)})
	r.call(callArgs{Fn: "parse_auths", Req: raw(`{"Provider":"nobody"}`)})
	r.respond("a", "auth.login.start", ok(map[string]any{"URL": "https://login.invalid/a", "State": "s1", "ExpiresAt": "2026-10-03T00:00:00Z"}))
	r.call(callArgs{Fn: "start_login", Provider: " A ", BaseURL: "http://127.0.0.1:8317", Metadata: map[string]any{"m": "v"}})
	r.respond("a", "auth.login.poll", ok(map[string]any{"Status": "complete", "Auth": map[string]any{"Provider": "a", "ID": "p1"}}))
	r.call(callArgs{Fn: "poll_login", Provider: "a", State: "s1"})
	r.respond("c", "auth.refresh", ok(map[string]any{"Auth": map[string]any{"Label": "new", "Attributes": map[string]string{"n": "1"}}, "NextRefreshAfter": "2026-10-05T00:00:00Z"}))
	r.call(callArgs{Fn: "refresh_auth", Req: raw(`{"Provider":"rec-c","ID":"c1","Label":"old","Metadata":{"priority":3},
		"Attributes":{"source_backend":"file","file_priority":"true","priority":"9"},"StorageJSON":"eyJ0b2tlbiI6InQifQ=="}`), Path: "/auth/c1.json"})
	r.respond("c", "auth.refresh", fail("refresh_failed", "expired"))
	r.call(callArgs{Fn: "refresh_auth", Req: raw(`{"Provider":"rec-c","ID":"c1"}`)})
	r.records()

	// Frontend auth: a authenticates, b declines.
	r.respond("a", "frontend_auth.authenticate", ok(map[string]any{"Authenticated": true, "Principal": "alice", "Metadata": map[string]string{"k": "v"}}))
	r.respond("b", "frontend_auth.authenticate", ok(map[string]any{"Authenticated": false}))
	r.call(callArgs{Fn: "frontend_auth", Method: "POST", Target: "/v1/chat/completions?key=1&key=2", Headers: map[string][]string{"Authorization": {"Bearer t"}}, Body: `{"m":1}`})
	r.records()

	// Quota providers.
	r.respond("a", "quota.describe", ok(map[string]any{"supported_providers": []string{"Claude", "codex"}, "display_name": "A quota", "supports_reset": true}))
	r.respond("b", "quota.describe", fail("x", "y"))
	r.call(callArgs{Fn: "quota_providers"})
	r.call(callArgs{Fn: "describe_quota", PluginID: "recorder-b"})
	r.respond("a", "quota.fetch", ok(map[string]any{"summary": []map[string]any{{"key": "k", "label": "L", "value": 1.5, "unit": "%"}},
		"server_time_offset_ms": 7}))
	r.call(callArgs{Fn: "fetch_quota", Req: raw(`{"auth_index":"3","auth_id":"x","provider":"codex"}`)})
	r.call(callArgs{Fn: "fetch_quota", Req: raw(`{"auth_index":"3","provider":"recorder-a"}`)})
	r.call(callArgs{Fn: "fetch_quota", Req: raw(`{"provider":"nobody"}`)})
	r.respond("b", "quota.reset", ok(map[string]any{"success": true, "message": "done"}))
	r.call(callArgs{Fn: "reset_quota_by_plugin", PluginID: "recorder-b", Req: raw(`{"auth_index":"1","provider":"b"}`)})
	r.call(callArgs{Fn: "reset_quota_by_plugin", PluginID: "recorder-a", Req: raw(`{"auth_index":"1"}`)})
	r.call(callArgs{Fn: "fetch_quota_by_plugin", PluginID: "recorder-c", Req: raw(`{}`)})
	r.records()

	// Models: a static provider, b registrar, c registrar with an executor.
	r.respond("a", "model.static", ok(map[string]any{"Provider": " A ", "Models": []map[string]any{{"ID": " m1 ", "DisplayName": "M1"}, {"ID": ""}}}))
	r.respond("b", "model.register", ok(map[string]any{"Provider": "b", "Models": []map[string]any{{"ID": "mb"}}}))
	r.respond("c", "model.register", ok(map[string]any{"Provider": "c", "Models": []map[string]any{{"ID": "mc"}}}))
	r.call(callArgs{Fn: "register_models"})
	r.respond("b", "model.register", fail("gone", "gone"))
	r.call(callArgs{Fn: "register_models"})
	r.respond("a", "model.for_auth", ok(map[string]any{"Provider": "a", "Models": []map[string]any{{"ID": "fa"}, {"ID": " "}}, "AuthUpdate": map[string]any{"Label": "upd"}}))
	r.call(callArgs{Fn: "models_for_auth", Req: raw(`{"Provider":"a","ID":"auth-1","Metadata":{"email":"e"}}`), Path: "/auth/auth-1.json"})
	r.call(callArgs{Fn: "models_for_auth", Req: raw(`{"Provider":"zzz","ID":"auth-2"}`)})
	r.records()

	// Command-line plugins.
	r.respond("a", "command_line.register", ok(map[string]any{"Flags": []map[string]any{
		{"Name": "mine", "Type": "bool", "Usage": "u"}, {"Name": "count", "Type": "int", "DefaultValue": "7"},
		{"Name": "config", "Type": "string"}, {"Name": "bad name"}, {"Name": "dur", "Type": "Duration", "DefaultValue": "90s"},
		{"Name": "x", "Type": "weird"}, {"Name": "n", "Type": "int", "DefaultValue": "x"},
	}}))
	r.respond("b", "command_line.register", ok(map[string]any{"Flags": []map[string]any{
		{"Name": "mine", "Type": "string"}, {"Name": "ratio", "Type": "float64", "DefaultValue": "1e6"}, {"Name": "help"},
	}}))
	r.respond("c", "command_line.register", fail("no", "flags"))
	r.respond("a", "command_line.execute", ok(map[string]any{"Stdout": []byte("out-a"), "Stderr": []byte("err-a\n"), "Auths": []map[string]any{{"Provider": ""}}}))
	r.respond("b", "command_line.execute", ok(map[string]any{"Stdout": []byte("out-b\n"), "ExitCode": 3}))
	r.call(callArgs{Fn: "command_line", Builtin: []string{"config", "tui"}, Args: []string{"-mine", "--count=3", "-ratio", "0.5", "-config", "c.yaml", "rest", "-dur", "1s"}})
	r.records()

	r.shutdown()
	r.records()
}
