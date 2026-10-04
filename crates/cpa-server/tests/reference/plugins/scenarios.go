package main

import "encoding/json"

// Fake credentials; nothing is sent anywhere. The quota probes reach only the local
// UPSTREAM server (or a closed loopback port).
var authFiles = map[string]string{
	"claude.json": `{"type":"claude","email":"a@example.invalid","access_token":"fake-access","refresh_token":"fake-refresh"}`,
	"codex.json":  `{"type":"codex","email":"b@example.invalid","access_token":"fake-codex"}`,
	"probe-ok.json": `{"type":"codex","email":"p1@example.invalid","access_token":"tok-1","quota_probe":{"url":"UPSTREAM/quota?t=$TOKEN$","method":" post ",` +
		`"data":"{\"k\":\"$TOKEN$\"}","header":{"Authorization":"Bearer $TOKEN$","x-other":"1","x-num":2}}}`,
	"probe-map.json": `{"type":"codex","email":"p2@example.invalid","access_token":"t2","quota_probe":{"url":"UPSTREAM/mapped","headers":{"X-H":"h"},"mapping":{` +
		`"plan":"account.plan","tier_name":"","tierName":"account.tier","tier_id":"missing.path","tierId":"account.tier_id","groups":[` +
		`{"display_name":"usage.label","buckets_path":"usage.items","window_key":"w","remaining_amount_key":"left","total_amount_key":"max","reset_time_key":"resets"},` +
		`{"displayName":"Literal name","buckets":[{"remaining_fraction":"fixed.frac","window":"fixed.window","description":"just text","reset_time":"fixed.reset"},` +
		`{"remaining_amount":"fixed.left","total_amount":"fixed.zero"},{"remaining_amount":"fixed.left","total_amount":"fixed.max","window":"weekly"}]},` +
		`{"display_name":"nothing","buckets_path":"usage.none"}]}}}`,
	"probe-mapfail.json":   `{"type":"codex","email":"p3@example.invalid","quota_probe":{"url":"UPSTREAM/mapped","mapping":{"plan":"no.such","groups":[{"buckets_path":"usage.none"}]}}}`,
	"probe-status.json":    `{"type":"codex","email":"p4@example.invalid","quota_probe":{"url":"UPSTREAM/status500"}}`,
	"probe-notjson.json":   `{"type":"codex","email":"p5@example.invalid","quota_probe":{"url":"UPSTREAM/notjson"}}`,
	"probe-empty.json":     `{"type":"codex","email":"p6@example.invalid","quota_probe":{"url":"UPSTREAM/empty"}}`,
	"probe-redirect.json":  `{"type":"codex","email":"p7@example.invalid","quota_probe":{"url":"UPSTREAM/redirect"}}`,
	"probe-refused.json":   `{"type":"codex","email":"p8@example.invalid","quota_probe":{"url":"http://127.0.0.1:1/x"}}`,
	"probe-notoken.json":   `{"type":"codex","email":"p9@example.invalid","quota_probe":{"url":"UPSTREAM/quota","header":{"A":"$TOKEN$"}}}`,
	"probe-badurl.json":    `{"type":"codex","email":"p10@example.invalid","quota_probe":{"url":"http://[::1"}}`,
	"probe-nourl.json":     `{"type":"codex","email":"p11@example.invalid","quota_probe":{"method":"GET","url":5}}`,
	"probe-string.json":    `{"type":"codex","email":"p12@example.invalid","quota_probe":"UPSTREAM/quota"}`,
	"probe-badgroup.json":  `{"type":"codex","email":"p13@example.invalid","quota_probe":{"url":"UPSTREAM/badgroup"}}`,
	"probe-summary.json":   `{"type":"codex","email":"p14@example.invalid","quota_probe":{"url":"UPSTREAM/summary"}}`,
	"probe-nohost.json":    `{"type":"codex","email":"p15@example.invalid","quota_probe":{"url":"http:///127.0.0.1:1/quota"}}`,
	"probe-ftp.json":       `{"type":"codex","email":"p16@example.invalid","quota_probe":{"url":"ftp://127.0.0.1:1/quota"}}`,
	"probe-headers.json":   `{"type":"codex","email":"p17@example.invalid","quota_probe":{"url":"UPSTREAM/quota","method":"ſ","header":{"Host":"other.example","Content-Length":"5","Transfer-Encoding":"chunked","Trailer":"X-T","x-a":"1"}}}`,
	"probe-badheader.json": `{"type":"codex","email":"p18@example.invalid","quota_probe":{"url":"http://127.0.0.1:1/quota","header":{"Content-Length":"1\r\nX-Test: injected"}}}`,
	"probe-badname.json":   `{"type":"codex","email":"p19@example.invalid","quota_probe":{"url":"http://127.0.0.1:1/quota","header":{"bad name":"1"}}}`,
	"probe-sharp.json":     `{"type":"codex","email":"p20@example.invalid","quota_probe":{"url":"UPSTREAM/quota","method":"ß"}}`,
	"probe-case.json":      `{"type":"codex","email":"p21@example.invalid","quota_probe":{"url":"UPSTREAM/case"}}`,
}

// upstreamRoutes are the probe server's answers by path (no Date header, so the
// server time offset stays 0).
var upstreamRoutes = map[string]struct {
	Status   int
	Body     string
	Location string
}{
	"/quota": {200, `{"subscription":{"plan":"pro","tier_name":"T"},"serverTimeOffsetMs":0,"summary":[` +
		`{"key":"k1","label":"Spend","value":1.5,"unit":" USD ","format":"currency","currency":" usd "},` +
		`{"key":"k2","label":"Old","value":2,"format":"currency","currency":"DEM"},` +
		`{"key":"k3","label":"Bad","value":3,"format":"currency","currency":"XYZ"},` +
		`{"key":"k4","label":"N","value":4,"format":"number"},{"key":"k5","label":"S","value":"5"},` +
		`{"key":" ","label":"x","value":1},{"key":"k6","label":"P","value":6,"format":"percent"}],` +
		`"groups":[{"display_name":"G1","buckets":[{"window":"5h","remainingFraction":0.5},{"window":"d","remaining_fraction":0.25,"reset_time":"r"},` +
		`{"window":"none"},{"window":"zero","remainingFraction":0}]},{"displayName":"G2","buckets":[{"remainingFraction":null}]},` +
		`{"displayName":"G3"}]}`, ""},
	"/case":     {200, `{"subscription":{"plan":"paid"},"Subscription":null}`, ""},
	"/badgroup": {200, `{"subscription":{"plan":"pro"},"groups":[{"buckets":[{"remainingFraction":"0.5"}]}]}`, ""},
	"/summary":  {200, `{"Summary":[{"key":"a","label":"A","value":1}],"summary":[{"key":"b","label":"B","value":2}],"groups":[]}`, ""},
	"/mapped": {200, `{"account":{"plan":"team","tier":"Gold","tier_id":7},"usage":{"label":"Usage","items":[` +
		`{"w":"5h","left":3,"max":4,"resets":"soon"},{"w":"day","remaining_fraction":"0.1","description":"d"},{"w":"bad","left":1,"max":0}]},` +
		`"fixed":{"frac":"0.75","window":"monthly","reset":"later","left":2,"max":8,"zero":0}}`, ""},
	"/status500": {500, "upstream exploded", ""},
	"/notjson":   {200, "{not json", ""},
	"/empty":     {200, `{"groups":[],"subscription":{"plan":" "}}`, ""},
	"/redirect":  {302, "", "/quota?from=redirect"},
}

const initialConfig = `config-version: 8
server:
  port: 18317
auth-dir: AUTHDIR
management:
  secret-key: $HASH
plugins:
  enabled: true
  dir: PLUGINDIR/./sub/..
  store-sources:
    - https://third.example/registry.json
  store-auth:
    - match: https://third.example/
      type: bearer
      token-env: CPA_STORE_ROUTES_TOKEN
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
	probeScenarios(r)
	oauthScenarios(r)

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

	storeScenarios(r)
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

// oauthScenarios covers ServePluginAuthURL, the plugin branch of GetAuthStatus and the
// OAuth callback for plugin sessions (recorder-a is the auth provider "a").
func oauthScenarios(r *runner) {
	get := func(path string) { r.http(httpArgs{Method: "GET", Path: path}) }
	post := func(path, body string) { r.http(httpArgs{Method: "POST", Path: path, Body: body}) }
	start := func(state string) {
		r.respond("a", "auth.login.start", ok(map[string]any{"Provider": "a", "URL": "https://login.example/a?x=1&y=<2>", "State": state, "Metadata": map[string]any{"k": "v", "n": 2}}))
	}
	start("st-1")
	get("/v0/management/a-auth-url?foo=1&foo=2&bar=3")
	r.records()
	get("/v0/management/a-auth-url")
	start("st-2")
	get("/v8/management/oauth/auth-url?provider=A&x=1")
	r.records()
	start("bad state!")
	get("/v0/management/a-auth-url")
	start("  ")
	get("/v0/management/a-auth-url")
	r.respond("a", "auth.login.start", fail("cannot start"))
	get("/v0/management/a-auth-url")
	get("/v0/management/zzz-auth-url")
	get("/v8/management/oauth/auth-url?provider=zzz")
	get("/v8/management/oauth/auth-url?provider=a_b")
	r.http(httpArgs{Method: "POST", Path: "/v0/management/a-auth-url"})
	r.http(httpArgs{Method: "GET", Path: "/v0/management/a-auth-url", NoKey: true})
	r.records()

	// Callbacks for plugin sessions are written to the auth directory.
	post("/v0/management/oauth-callback", `{"provider":"b","state":"st-1","code":"c1"}`)
	post("/v0/management/oauth-callback", `{"provider":"A_B","state":"st-1","code":"c1"}`)
	post("/v0/management/oauth-callback", `{"provider":"A","state":"st-1","code":" c1 ","error":""}`)
	get("/v0/management/oauth-callback?state=st-2&error=denied")
	r.authFiles()

	// Polling.
	r.respond("a", "auth.login.poll", ok(map[string]any{"Status": "pending"}))
	get("/v0/management/get-auth-status?state=st-1")
	r.respond("a", "auth.login.poll", ok(map[string]any{"Status": "later"}))
	get("/v8/management/oauth/status?state=st-1")
	r.records()
	r.respond("a", "auth.login.poll", ok(map[string]any{"Status": "error", "Message": " denied <x> "}))
	get("/v0/management/get-auth-status?state=st-1")
	get("/v0/management/get-auth-status?state=st-1")
	post("/v0/management/oauth-callback", `{"state":"st-1","code":"again"}`)
	r.respond("a", "auth.login.poll", ok(map[string]any{"Status": "success", "Auths": []map[string]any{
		{"Provider": "A", "FileName": "a-user.json", "Label": "user", "StorageJSON": []byte(`{"access_token":"t","expired":"2030-01-01T00:00:00Z"}`), "Metadata": map[string]any{"email": "u@example.invalid"}},
		{"Provider": "a", "FileName": "a-two.json", "Disabled": true, "StorageJSON": []byte(`{"k":1}`)},
	}}))
	get("/v0/management/get-auth-status?state=st-2")
	r.authFiles()
	get("/v0/management/get-auth-status?state=st-2")
	start("st-3")
	get("/v0/management/a-auth-url")
	r.respond("a", "auth.login.poll", ok(map[string]any{"Status": "success", "Auth": map[string]any{"Provider": " "}}))
	get("/v0/management/get-auth-status?state=st-3")
	start("st-4")
	get("/v0/management/a-auth-url")
	r.respond("a", "auth.login.poll", fail("poll broke"))
	get("/v0/management/get-auth-status?state=st-4")
	start("st-5")
	get("/v0/management/a-auth-url")
	r.http(httpArgs{Method: "DELETE", Path: "/v0/management/oauth-session?state=st-5"})
	get("/v0/management/get-auth-status?state=st-5")
	r.records()
}

// probeScenarios covers the declarative quota probe of FetchCredentialQuota.
func probeScenarios(r *runner) {
	for _, name := range []string{"probe-ok", "probe-map", "probe-mapfail", "probe-status", "probe-notjson", "probe-empty",
		"probe-redirect", "probe-refused", "probe-notoken", "probe-badurl", "probe-nourl", "probe-string", "probe-badgroup", "probe-summary", "probe-nohost", "probe-ftp", "probe-headers",
		"probe-badheader", "probe-badname", "probe-sharp", "probe-case"} {
		r.http(httpArgs{Method: "POST", Path: "/v0/management/quota/fetch", Body: `{"auth_index":"AUTHINDEX(` + name + `.json)"}`})
	}
	r.upstreamRequests()
}
