package main

import "encoding/json"

// Fake credentials; nothing is sent anywhere.
var authFiles = map[string]string{
	"claude.json": `{"type":"claude","email":"a@example.invalid","access_token":"fake-access","refresh_token":"fake-refresh"}`,
	"codex.json":  `{"type":"codex","email":"b@example.invalid","access_token":"fake-codex"}`,
}

const initialConfig = `config-version: 8
auth-dir: AUTHDIR
management:
  secret-key: $HASH
plugins:
  enabled: true
  dir: PLUGINDIR/./sub/..
  configs:
    recorder-a:
      enabled: true
      priority: 5
      record: RECORDDIR
      label: a
      caps: management_api,quota_provider,auth_provider
      nested:
        list: [1, two]
        flag: yes
    recorder-b:
      enabled: true
      record: RECORDDIR
      label: b
      caps: management_api
    recorder-d:
      enabled: false
      store:
        version: 1_bad
        release-tag: 1.0.0
    ghost:
      enabled: true
`

func scenarios(r *runner) {
	get := func(path string) { r.http(httpArgs{Method: "GET", Path: path}) }
	v0 := "/v0/management/plugins"
	v8 := "/v8/management/plugins"

	get(v0)
	get(v8)
	get(v0 + "/recorder-a/config")
	get(v0 + "/recorder-c/config")
	get(v0 + "/nope/config")
	get(v0 + "/bad!id/config")

	// Enabled toggle.
	for _, body := range []string{`{"enabled":"false"}`, ``, `{"enabled":null}`, `[]`} {
		r.http(httpArgs{Method: "PATCH", Path: v0 + "/recorder-b/enabled", Body: body})
	}
	r.http(httpArgs{Method: "PATCH", Path: v0 + "/recorder-b/enabled", Body: `{"enabled":false}`})
	r.settle()
	r.plugins()
	get(v0)

	// Replace a config; invalid bodies first.
	for _, body := range []string{`{"enabled":"maybe"}`, `{"priority":"5"}`, `[1]`, `null`, `{bad`, ``, ` `, `"x"`, `true`, `{"a":`} {
		r.http(httpArgs{Method: "PUT", Path: v0 + "/recorder-c/config", Body: body})
	}
	r.http(httpArgs{Method: "PUT", Path: v0 + "/recorder-c/config",
		Body: `{"label":"c","enabled":true,"record":"RECORDDIR","caps":"management_api","n":1.50,"big":12345678901,"z":{"b":1,"a":[true,null,"s"]}}`})
	r.settle()
	r.plugins()
	get(v0 + "/recorder-c/config")
	get(v0)

	// Shallow merge: null deletes, existing keys keep their place.
	r.http(httpArgs{Method: "PATCH", Path: v0 + "/recorder-a/config", Body: `{"enabled":1}`})
	r.http(httpArgs{Method: "PATCH", Path: v0 + "/recorder-a/config", Body: `{"priority":7,"nested":null,"extra":"x","enabled":null}`})
	r.settle()
	r.plugins()
	get(v0 + "/recorder-a/config")
	r.http(httpArgs{Method: "PATCH", Path: v0 + "/new-one/config", Body: `{"priority":-2}`})
	r.settle()
	r.plugins()

	// Number overflow and duplicate keys.
	r.http(httpArgs{Method: "PUT", Path: v0 + "/new-one/config", Body: `{"z":{"x":1e400},"a":1}`})
	r.http(httpArgs{Method: "PUT", Path: v0 + "/new-one/config", Body: `{"a":[1,1e400]}`})
	r.http(httpArgs{Method: "PATCH", Path: v0 + "/new-one/config", Body: `{"q":-1e400,"enabled":"bad"}`})
	r.http(httpArgs{Method: "PATCH", Path: v0 + "/new-one/config", Body: `{"enabled":"bad","enabled":null,"p":1e300}`})
	r.settle()
	r.plugins()
	get(v0 + "/new-one/config")

	// Plugin-declared routes and resources through NoRoute.
	get("/v0/%6danagement/rec/c")
	get("/v0/management/rec/c")
	r.http(httpArgs{Method: "GET", Path: "/v0/management/rec/c", NoKey: true})
	r.http(httpArgs{Method: "POST", Path: "/v0/management/rec/c/calls", Body: `{"calls":[]}`})
	get("/v0/management/rec/b")
	get("/v0/management/nothing-here")
	r.http(httpArgs{Method: "POST", Path: v0})
	get("/v0/resource/plugins/recorder-c/page")
	get("/v0/resource/plugins/recorder-b/page?x=1")
	get("/v8/management/rec/c")

	quotaScenarios(r)

	// Delete: a loaded configured plugin, a configured plugin without a file, misses.
	r.http(httpArgs{Method: "DELETE", Path: v0 + "/recorder-c"})
	r.settle()
	r.plugins()
	r.http(httpArgs{Method: "DELETE", Path: v8 + "/ghost"})
	r.settle()
	r.plugins()
	r.http(httpArgs{Method: "DELETE", Path: v0 + "/nope"})
	r.http(httpArgs{Method: "DELETE", Path: v8 + "/bad!id"})
	get(v0)
	get("/v0/management/rec/c")
}

func ok(v any) string {
	raw, err := json.Marshal(map[string]any{"ok": true, "result": v})
	check(err)
	return string(raw)
}

func fail(message string) string {
	raw, err := json.Marshal(map[string]any{"ok": false, "error": map[string]any{"code": "x", "message": message}})
	check(err)
	return string(raw)
}

// quotaScenarios covers plugin_quota.go and quota_test.go (recorder-a is the quota provider).
func quotaScenarios(r *runner) {
	claude := "AUTHINDEX(claude.json)"
	codex := "AUTHINDEX(codex.json)"
	post := func(path, body string) { r.http(httpArgs{Method: "POST", Path: path, Body: body}) }
	// An earlier PATCH removed recorder-a's enabled key; turn it back on.
	r.http(httpArgs{Method: "PATCH", Path: "/v0/management/plugins/recorder-a/enabled", Body: `{"enabled":true}`})
	r.settle()
	r.records()
	r.respond("a", "quota.describe", ok(map[string]any{"supported_providers": []string{"Claude"}, "display_name": "Rec <A>", "supports_reset": true}))
	r.http(httpArgs{Method: "GET", Path: "/v0/management/quota/providers"})
	r.http(httpArgs{Method: "GET", Path: "/v8/management/quota/providers"})
	r.http(httpArgs{Method: "POST", Path: "/v8/management/quota/fetch", Body: `{}`})

	r.respond("a", "quota.fetch", ok(map[string]any{"summary": []map[string]any{{"key": "k", "label": "L", "value": 0.5, "unit": "%"}}, "server_time_offset_ms": 3}))
	for _, body := range []string{``, `[]`, `{}`, `null`, `{"auth_index":5}`, `{"auth_index":"  "}`, `{"auth_index":"nope"}`, `{"auth_index":"claude.json"}`} {
		post("/v0/management/quota/fetch", body)
	}
	post("/v0/management/quota/fetch", `{"authIndex":"`+claude+`"}`)
	r.records()
	post("/v0/management/quota/fetch", `{"AUTHINDEX":"`+claude+`","provider":"other"}`)
	post("/v0/management/quota/fetch", `{"auth_index":"`+codex+`"}`)
	post("/v0/management/quota/fetch", `{"auth_index":"`+codex+`","plugin_id":"recorder-a"}`)
	post("/v0/management/quota/fetch", `{"auth_index":"`+claude+`","plugin_id":"recorder-b"}`)
	r.records()
	r.respond("a", "quota.fetch", fail("upstream <down>"))
	post("/v0/management/quota/fetch", `{"auth_index":"`+claude+`"}`)
	r.records()

	r.respond("a", "quota.reset", ok(map[string]any{"success": true, "message": "done"}))
	post("/v0/management/quota/reset", `{"auth_index":"`+claude+`"}`)
	post("/v0/management/quota/reset", `{"auth_index":"`+codex+`"}`)
	post("/v0/management/quota/reset", `{"auth_index":"`+codex+`","plugin_id":"recorder-a"}`)
	post("/v0/management/quota/reset", `{"auth_index":"`+claude+`","plugin_id":"recorder-b"}`)
	r.records()
	r.respond("a", "quota.reset", ok(map[string]any{"success": false}))
	post("/v0/management/quota/reset", `{"auth_index":"`+claude+`"}`)
	r.respond("a", "quota.reset", fail("nope"))
	post("/v0/management/quota/reset", `{"auth_index":"`+claude+`"}`)
	r.records()

	// Per-plugin quota, v0 and v8; the plugin ID is not validated.
	r.respond("a", "quota.fetch", ok(map[string]any{"subscription": map[string]any{"plan": "pro"}}))
	r.respond("a", "quota.reset", ok(map[string]any{"success": true}))
	for _, base := range []string{"/v0/management/plugins/", "/v8/management/plugins/"} {
		r.http(httpArgs{Method: "GET", Path: base + "recorder-a/quota?auth_index=" + claude})
		r.http(httpArgs{Method: "GET", Path: base + "recorder-a/quota?authIndex=" + codex})
		r.http(httpArgs{Method: "GET", Path: base + "recorder-a/quota"})
		r.http(httpArgs{Method: "GET", Path: base + "bad!id/quota?auth_index=" + claude})
		r.http(httpArgs{Method: "GET", Path: base + "recorder-b/quota?auth_index=nope"})
		post(base+"recorder-a/quota", `{"AuthIndex":"`+claude+`"}`)
		post(base+"recorder-a/quota", `{bad`)
		r.http(httpArgs{Method: "DELETE", Path: base + "recorder-a/quota?auth_index=" + claude})
		r.http(httpArgs{Method: "DELETE", Path: base + "recorder-a/quota", Body: `{"auth_index":"` + codex + `"}`})
		r.http(httpArgs{Method: "DELETE", Path: base + "recorder-a/quota", Body: `{bad`})
	}
	// Binding and query corner cases.
	r.http(httpArgs{Method: "DELETE", Path: "/v0/management/plugins/recorder-a/quota", Body: `{"auth_index":"` + claude + `","provider":7}`})
	r.http(httpArgs{Method: "DELETE", Path: "/v0/management/plugins/recorder-a/quota", Body: `{"auth_index":"` + claude + `","auth_index":7}`})
	r.http(httpArgs{Method: "GET", Path: "/v0/management/plugins/recorder-a/quota?auth_index=%ZZ&authIndex=" + claude})
	r.http(httpArgs{Method: "GET", Path: "/v0/management/plugins/recorder-a/quota?auth_index=x;y&authIndex=" + claude})
	r.http(httpArgs{Method: "GET", Path: "/v0/management/plugins/recorder-a/quota?auth_index=+" + claude + "+"})
	post("/v0/management/quota/fetch", `{"auth_index":"`+claude+`","unused":1e400}`)
	post("/v0/management/quota/fetch", `{"auth_index":"`+claude+`"} trailing`)
	post("/v0/management/quota/fetch", `{"auth_index":"`+claude+`","provider":7}`)
	post("/v0/management/plugins/recorder-a/quota/reset", `{"authIndex":"`+claude+`"}`)
	post("/v8/management/plugins/recorder-a/quota/reset", `{"authIndex":"`+claude+`"}`)
	r.records()

	// quota_test.go: the cooldown reset takes only an auth index.
	post("/v0/management/reset-quota", `{"auth_id":"claude.json"}`)
	post("/v0/management/reset-quota", `{"auth_index":"claude.json"}`)
}
