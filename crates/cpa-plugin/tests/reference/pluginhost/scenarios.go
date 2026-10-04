package main

const baseConfig = `plugins:
  enabled: true
  dir: PLUGINDIR
  configs:
    simple:
      enabled: true
      priority: 2
      config1: true
      mode: fast
    scheduler:
      enabled: true
      priority: 2
    recorder-a:
      enabled: true
      priority: 5
      record: RECORDDIR
      label: a
      caps: management_api,scheduler,quota_provider,auth_provider
    recorder-b:
      enabled: true
      priority: 5
      record: RECORDDIR
      label: b
      schema: 5
      caps: management_api
    recorder-c:
      enabled: true
      record: RECORDDIR
      label: c
      caps: management_api
      nested:
        list: [1, two]
    recorder-d:
      enabled: false
      record: RECORDDIR
      label: d
      caps: management_api
    recorder-f:
      enabled: true
      record: RECORDDIR
      label: f
      fail: register
      caps: management_api
`

const reloadConfig = `plugins:
  enabled: true
  dir: PLUGINDIR
  configs:
    simple:
      enabled: true
      priority: 2
    scheduler:
      enabled: true
      priority: 2
    recorder-a:
      enabled: true
      priority: 1
      record: RECORDDIR
      label: a
      caps: management_api,scheduler
    recorder-b:
      enabled: true
      priority: 5
      record: RECORDDIR
      label: b
      schema: 5
      caps: management_api
    recorder-c:
      enabled: true
      record: RECORDDIR
      label: c
      caps: management_api
    recorder-d:
      enabled: true
      record: RECORDDIR
      label: d
      caps: management_api
`

const disabledConfig = `plugins:
  enabled: false
  dir: PLUGINDIR
`

const hostCalls = `{"calls":[
 {"method":"host.log","request":{"level":"info","message":"hello from recorder","fields":{"k":1}}},
 {"method":"host.log","request":{"message":"  "}},
 {"method":"host.stream.emit","request":{"stream_id":"999","payload":"eA=="}},
 {"method":"host.stream.emit","request":{"payload":"eA=="}},
 {"method":"host.stream.close","request":{"stream_id":"999"}},
 {"method":"host.unknown","request":{}},
 {"method":"host.log","request":"not an object"}
]}`

// host.http.* against the raw upstream; UPSTREAM is its host:port.
const hostHTTPCalls = `{"calls":[
 {"method":"host.http.do","request":{"method":"POST","url":"http://UPSTREAM/echo?q=1","headers":{"X-B":["1","2"],"x-lower":["l"],"Host":["ignored.invalid"]},"body":"aGk="}},
 {"method":"host.http.do","request":{"url":"http://UPSTREAM/echo"}},
 {"method":"host.http.do","request":{"method":"PUT","url":"http://UPSTREAM/echo","wire_profile":{"disable_auto_compression":true}}},
 {"method":"host.http.do","request":{"wire_profile":{"http1_only":true},"request":{"method":"PATCH","url":"http://UPSTREAM/echo","headers":{"Accept-Encoding":["identity"]},"body":"e30="}}},
 {"method":"host.http.do","request":{"method":"POST","url":"http://UPSTREAM/echo","headers":{"X-B":["1"],"Zeta":["z"]},"body":"aGk=","wire_profile":{"header_profile":["zeta","Host","content-length","Nope"]}}},
 {"method":"host.http.do","request":{"method":"GET","url":"http://UPSTREAM/status"}},
 {"method":"host.http.do","request":{"method":"BAD METHOD","url":"http://UPSTREAM/echo"}},
 {"method":"host.http.do_stream","request":{"url":"http://UPSTREAM/stream"}},
 {"method":"host.http.stream_read","request":{"stream_id":"1"}},
 {"method":"host.http.stream_read","request":{"stream_id":"1"}},
 {"method":"host.http.stream_read","request":{"stream_id":"1"}},
 {"method":"host.http.stream_read","request":{"stream_id":"1"}},
 {"method":"host.http.stream_read","request":{"stream_id":"1"}},
 {"method":"host.http.stream_read","request":{}},
 {"method":"host.http.do_stream","request":{"url":"http://UPSTREAM/stream"}},
 {"method":"host.http.stream_close","request":{"stream_id":"2"}},
 {"method":"host.http.stream_read","request":{"stream_id":"2"}},
 {"method":"host.http.operation_open","request":{}},
 {"method":"host.http.cancel","request":{"operation_id":"10"}},
 {"method":"host.http.do","request":{"operation_id":"10","url":"http://UPSTREAM/echo"}},
 {"method":"host.http.operation_open","request":{}},
 {"method":"host.http.do","request":{"operation_id":"11","url":"http://UPSTREAM/echo"}},
 {"method":"host.http.do","request":{"operation_id":"11","url":"http://UPSTREAM/echo"}},
 {"method":"host.http.cancel","request":{}},
 {"method":"host.http.do","request":{"host_callback_id":"999","url":"http://UPSTREAM/echo"}},
 {"method":"host.http.operation_open","request":{"host_callback_id":"999"}},
 {"method":"host.http.do","request":{"url":"/relative"}},
 {"method":"host.http.do","request":{"url":"http:///nohost"}},
 {"method":"host.http.do","request":{"url":"ftp://UPSTREAM/x"}},
 {"method":"host.http.do","request":{"url":"http://UPSTREAM/%zz"}},
 {"method":"host.http.do","request":{"url":"http://UPSTREAM/echo","headers":{"Bad Name":["x"]}}},
 {"method":"host.http.do","request":{"url":"http://UPSTREAM/echo","headers":{"X-Bad":["a\r\nb"]}}},
 {"method":"host.http.do","request":{"url":"http://UPSTREAM/truncated"}},
 {"method":"host.http.do","request":{"method":"POST","url":"http://u:secret@UPSTREAM/hangup"}},
 {"method":"host.http.do","request":{"url":"http://127.0.0.1:1/refused"}},
 {"method":"host.http.do","request":{"url":"http://UPSTREAM/http10"}},
 {"method":"host.http.do","request":{"url":"http://UPSTREAM/trailer"}},
 {"method":"host.http.do","request":{"url":"http://UPSTREAM/echo","wire_profile":{"header_profile":["connection","host"]}}},
 {"method":"host.http.do","request":{"url":"http://UPSTREAM/echo","body":"aGk"}},
 {"method":"host.http.do","request":{"url":"http://UPSTREAM/echo","body":"a=Gk="}},
 {"method":"host.http.do","request":{"url":"http://UPSTREAM/echo","body":"aGk=\nx"}},
 {"method":"host.http.stream_read","request":{"stream_id":" "}},
 {"method":"host.http.stream_close","request":{"stream_id":7}},
 {"method":"host.http.do_stream","request":{"url":"http://UPSTREAM/truncated"}},
 {"method":"host.http.stream_read","request":{"stream_id":"3"}},
 {"method":"host.http.stream_read","request":{"stream_id":"3"}},
 {"method":"host.http.do","request":{"url":"//u:secret@example.invalid/p"}},
 {"method":"host.http.do","request":{"method":"PUT","url":"http://UPSTREAM/redirect"}}
]}`

// host.http.* with an unusable proxy-url: a wire-profile request is refused, the
// ordinary client falls back to the default transport.
const hostHTTPProxyCalls = `{"calls":[
 {"method":"host.http.do","request":{"url":"http://UPSTREAM/echo","wire_profile":{"http1_only":true}}},
 {"method":"host.http.do","request":{"url":"http://UPSTREAM/echo","wire_profile":{"disable_auto_compression":true}}},
 {"method":"host.http.do","request":{"url":"http://UPSTREAM/status"}}
]}`

const proxyConfig = `requests:
  proxy-url: ftp://user:pw@proxy.invalid:1
plugins:
  enabled: true
  dir: PLUGINDIR
  configs:
    recorder-a:
      enabled: true
      record: RECORDDIR
      label: a
      caps: management_api
`

// proxyScenarios runs host.http.* under proxyConfig.
func proxyScenarios(r *runner) {
	r.clear()
	r.files(map[string]string{"recorder-a.so": "recorder"})
	r.apply(proxyConfig)
	r.registerManagement()
	r.serve("management", httpArgs{Method: "POST", Target: "/v0/management/rec/a/calls", Body: hostHTTPProxyCalls})
	r.records()
	r.shutdown()
	r.records()
}

func scenarios(r *runner) {
	r.files(map[string]string{
		"simple.so":            "simple",
		"scheduler.so":         "scheduler",
		"recorder-a.so":        "recorder",
		"recorder-b.so":        "recorder",
		"recorder-c-v1.0.0.so": "recorder",
		"recorder-c-v1.2.0.so": "recorder",
		"recorder-d.so":        "recorder",
		"recorder-f.so":        "recorder",
	})
	r.apply(baseConfig)
	r.loaded("simple", "scheduler", "recorder-c", "recorder-d", "recorder-f", "missing")
	r.records()

	r.registerManagement("GET /v0/management/rec/b")
	r.records()
	r.serve("management", httpArgs{Method: "GET", Target: "/v0/management/rec/a?x=1&x=2&y=%3C", Headers: map[string][]string{"X-Test": {"1", "2"}}})
	r.serve("management", httpArgs{Method: "POST", Target: "/v0/management/rec/a/calls", Body: hostCalls})
	r.serve("management", httpArgs{Method: "POST", Target: "/v0/management/rec/a/calls", Body: hostHTTPCalls})
	r.serve("management", httpArgs{Method: "GET", Target: "/v0/management/rec/b"})
	r.serve("management", httpArgs{Method: "POST", Target: "/v0/management/rec/b/calls", Body: `{}`})
	r.serve("management", httpArgs{Method: "GET", Target: "/v0/management/rec/a/calls"})
	r.serve("management", httpArgs{Method: "GET", Target: "/v0/management/rec/f"})
	r.serve("resource", httpArgs{Method: "GET", Target: "/v0/resource/plugins/recorder-a/page?q=1"})
	r.serve("resource", httpArgs{Method: "GET", Target: "/v0/resource/plugins/recorder-c/raw"})
	r.serve("resource", httpArgs{Method: "GET", Target: "/v0/resource/plugins/recorder-a/rec/a/menu"})
	r.serve("resource", httpArgs{Method: "POST", Target: "/v0/resource/plugins/recorder-a/page"})
	r.serve("resource", httpArgs{Method: "GET", Target: "/v0/resource/plugins/simple/status"})
	r.records()

	// Hot reload: a newer recorder-c file replaces the loaded one; priorities change; a
	// disabled plugin is enabled.
	r.files(map[string]string{"recorder-c-v1.3.0.so": "recorder"})
	r.apply(reloadConfig)
	r.loaded("recorder-c", "recorder-d", "recorder-f")
	r.records()

	r.apply(disabledConfig)
	r.loaded("simple", "recorder-a")
	r.records()

	r.apply(baseConfig)
	r.records()

	r.unload("recorder-a")
	r.unload("missing")
	r.loaded("recorder-a", "recorder-b")
	r.records()

	r.shutdown()
	r.loaded("recorder-b", "simple")
	r.records()
}
