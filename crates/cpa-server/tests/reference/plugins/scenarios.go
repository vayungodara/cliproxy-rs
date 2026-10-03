package main

const initialConfig = `config-version: 8
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
