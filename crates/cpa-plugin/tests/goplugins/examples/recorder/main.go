// recorder is a test plugin for comparing plugin hosts. It appends every call it
// receives (method and exact request bytes) to <record>/<label>.jsonl and answers
// deterministically. Its config YAML is read line by line:
//
//	record: <dir>          where to append records (the plugin's Go runtime only sees
//	                       the environment the process started with, so not an env var)
//	label: <name>          metadata name and record file name
//	caps: a,b,c            rpcCapabilities flags to declare
//	schema: <n>            schema_version to declare (default 6)
//	fail: register         answer plugin.register with an error envelope
//	inputs: a,b            executor_input_formats (default chat-completions)
//	outputs: a,b           executor_output_formats (default chat-completions)
//	scope: <scope>         executor_model_scope (default both)
//
// Any other method is answered from <record>/respond/<label>/<method>.json when that
// file exists: the file is the complete response envelope, written by the test before
// the call.
//
// A management.handle request whose body is {"calls":[{"method":...,"request":{...}}]}
// makes those host callbacks (with host_callback_id filled in when absent) and returns
// the raw host responses, so callers can drive every host.* method.
package main

/*
#include <stdint.h>
#include <stdlib.h>

typedef struct {
	void* ptr;
	size_t len;
} cliproxy_buffer;

typedef int (*cliproxy_host_call_fn)(void*, const char*, const uint8_t*, size_t, cliproxy_buffer*);
typedef void (*cliproxy_host_free_fn)(void*, size_t);

typedef struct {
	uint32_t abi_version;
	void* host_ctx;
	cliproxy_host_call_fn call;
	cliproxy_host_free_fn free_buffer;
} cliproxy_host_api;

typedef int (*cliproxy_plugin_call_fn)(char*, uint8_t*, size_t, cliproxy_buffer*);
typedef void (*cliproxy_plugin_free_fn)(void*, size_t);
typedef void (*cliproxy_plugin_shutdown_fn)(void);

typedef struct {
	uint32_t abi_version;
	cliproxy_plugin_call_fn call;
	cliproxy_plugin_free_fn free_buffer;
	cliproxy_plugin_shutdown_fn shutdown;
} cliproxy_plugin_api;

extern int cliproxyPluginCall(char*, uint8_t*, size_t, cliproxy_buffer*);
extern void cliproxyPluginFree(void*, size_t);
extern void cliproxyPluginShutdown(void);

static const cliproxy_host_api* stored_host;

static void store_host_api(const cliproxy_host_api* host) {
	stored_host = host;
}

static int call_host_api(const char* method, const uint8_t* request, size_t request_len, cliproxy_buffer* response) {
	if (stored_host == NULL || stored_host->call == NULL) {
		return 1;
	}
	return stored_host->call(stored_host->host_ctx, method, request, request_len, response);
}

static void free_host_buffer(void* ptr, size_t len) {
	if (stored_host != NULL && stored_host->free_buffer != NULL && ptr != NULL) {
		stored_host->free_buffer(ptr, len);
	}
}
*/
import "C"

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"unsafe"

	"github.com/router-for-me/CLIProxyAPI/v8/sdk/pluginabi"
	"github.com/router-for-me/CLIProxyAPI/v8/sdk/pluginapi"
)

var (
	mu      sync.Mutex
	label   = "unlabeled"
	caps    = map[string]bool{}
	schema  = pluginabi.SchemaVersion
	fail    = ""
	dir     = ""
	inputs  = []string{"chat-completions"}
	outputs = []string{"chat-completions"}
	scope   = "both"
)

type envelope struct {
	OK     bool            `json:"ok"`
	Result json.RawMessage `json:"result,omitempty"`
	Error  *envelopeError  `json:"error,omitempty"`
}

type envelopeError struct {
	Code       string `json:"code"`
	Message    string `json:"message"`
	HTTPStatus int    `json:"http_status,omitempty"`
}

func main() {}

//export cliproxy_plugin_init
func cliproxy_plugin_init(host *C.cliproxy_host_api, plugin *C.cliproxy_plugin_api) C.int {
	if plugin == nil {
		return 1
	}
	C.store_host_api(host)
	plugin.abi_version = C.uint32_t(pluginabi.ABIVersion)
	plugin.call = C.cliproxy_plugin_call_fn(C.cliproxyPluginCall)
	plugin.free_buffer = C.cliproxy_plugin_free_fn(C.cliproxyPluginFree)
	plugin.shutdown = C.cliproxy_plugin_shutdown_fn(C.cliproxyPluginShutdown)
	return 0
}

//export cliproxyPluginCall
func cliproxyPluginCall(method *C.char, request *C.uint8_t, requestLen C.size_t, response *C.cliproxy_buffer) C.int {
	if response != nil {
		response.ptr = nil
		response.len = 0
	}
	var requestBytes []byte
	if request != nil && requestLen > 0 {
		requestBytes = C.GoBytes(unsafe.Pointer(request), C.int(requestLen))
	}
	name := C.GoString(method)
	if name == pluginabi.MethodPluginRegister || name == pluginabi.MethodPluginReconfigure {
		configure(requestBytes)
	}
	record(name, requestBytes)
	raw, rc := handle(name, requestBytes)
	writeResponse(response, raw)
	return C.int(rc)
}

//export cliproxyPluginFree
func cliproxyPluginFree(ptr unsafe.Pointer, len C.size_t) {
	if ptr != nil {
		C.free(ptr)
	}
}

//export cliproxyPluginShutdown
func cliproxyPluginShutdown() {
	record("<shutdown>", nil)
}

func configure(raw []byte) {
	var req struct {
		ConfigYAML []byte `json:"config_yaml"`
	}
	_ = json.Unmarshal(raw, &req)
	mu.Lock()
	defer mu.Unlock()
	caps = map[string]bool{}
	schema = pluginabi.SchemaVersion
	fail = ""
	inputs = []string{"chat-completions"}
	outputs = []string{"chat-completions"}
	scope = "both"
	for _, line := range strings.Split(string(req.ConfigYAML), "\n") {
		key, value, ok := strings.Cut(strings.TrimSpace(line), ":")
		if !ok {
			continue
		}
		value = strings.Trim(strings.TrimSpace(value), `"'`)
		switch key {
		case "label":
			label = value
		case "caps":
			for _, name := range strings.Split(value, ",") {
				if name = strings.TrimSpace(name); name != "" {
					caps[name] = true
				}
			}
		case "schema":
			if n, err := strconv.Atoi(value); err == nil {
				schema = uint32(n)
			}
		case "fail":
			fail = value
		case "record":
			dir = value
		case "inputs":
			inputs = splitList(value)
		case "outputs":
			outputs = splitList(value)
		case "scope":
			scope = value
		}
	}
}

func splitList(value string) []string {
	out := []string{}
	for _, item := range strings.Split(value, ",") {
		if item = strings.TrimSpace(item); item != "" {
			out = append(out, item)
		}
	}
	return out
}

// canned returns the envelope the test stored for this method, if any.
func canned(label, method string) ([]byte, int, bool) {
	mu.Lock()
	base := dir
	mu.Unlock()
	if base == "" {
		return nil, 0, false
	}
	raw, err := os.ReadFile(filepath.Join(base, "respond", label, method+".json"))
	if err != nil {
		return nil, 0, false
	}
	var env envelope
	if json.Unmarshal(raw, &env) == nil && env.OK {
		return raw, 0, true
	}
	return raw, 1, true
}

func record(method string, request []byte) {
	mu.Lock()
	name, dir := label, dir
	mu.Unlock()
	if dir == "" {
		return
	}
	line, _ := json.Marshal(map[string]string{"method": method, "request": string(request)})
	f, err := os.OpenFile(filepath.Join(dir, name+".jsonl"), os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0o600)
	if err != nil {
		return
	}
	defer f.Close()
	_, _ = f.Write(append(line, '\n'))
}

func ok(v any) ([]byte, int) {
	raw, _ := json.Marshal(v)
	out, _ := json.Marshal(envelope{OK: true, Result: raw})
	return out, 0
}

func failure(code, message string, status int) ([]byte, int) {
	out, _ := json.Marshal(envelope{OK: false, Error: &envelopeError{Code: code, Message: message, HTTPStatus: status}})
	return out, 1
}

func handle(method string, request []byte) ([]byte, int) {
	mu.Lock()
	name, declared, version, failing := label, caps, schema, fail
	ins, outs, modelScope := inputs, outputs, scope
	mu.Unlock()
	if method != pluginabi.MethodPluginRegister && method != pluginabi.MethodPluginReconfigure {
		if raw, rc, found := canned(name, method); found {
			return raw, rc
		}
	}
	switch method {
	case pluginabi.MethodPluginRegister, pluginabi.MethodPluginReconfigure:
		if failing == "register" {
			return failure("register_failed", "recorder told to fail", 0)
		}
		capabilities := map[string]any{"executor_model_scope": modelScope}
		for capName := range declared {
			capabilities[capName] = true
		}
		if declared["executor"] {
			capabilities["executor_input_formats"] = ins
			capabilities["executor_output_formats"] = outs
		}
		return ok(map[string]any{
			"schema_version": version,
			"metadata": pluginapi.Metadata{
				Name:             name,
				Version:          "1.0.0",
				Author:           "cliproxy-rs tests",
				GitHubRepository: "https://example.invalid/recorder",
			},
			"capabilities": capabilities,
		})
	case pluginabi.MethodPluginQuiesce, pluginabi.MethodRequestComplete, pluginabi.MethodUsageHandle, pluginabi.MethodWebSocketResponseEvent:
		return ok(map[string]any{})
	case pluginabi.MethodAuthIdentifier, pluginabi.MethodFrontendAuthIdentifier, pluginabi.MethodExecutorIdentifier,
		pluginabi.MethodThinkingIdentifier, pluginabi.MethodQuotaIdentifier:
		return ok(map[string]string{"identifier": name})
	case pluginabi.MethodManagementRegister:
		return ok(map[string]any{
			"routes": []pluginapi.ManagementRoute{
				{Method: "get", Path: "rec/" + name},
				{Method: "POST", Path: "/v0/management/rec/" + name + "/calls/"},
				{Method: "GET", Path: "/rec/" + name + "/menu", Menu: "Legacy " + name, Description: "legacy menu"},
				{Method: "GET", Path: "/bad:path"},
			},
			"resources": []pluginapi.ResourceRoute{
				{Path: "page", Menu: "Recorder " + name, Description: "<b>" + name + "</b>"},
				{Path: "/plugins/" + name + "/raw/"},
			},
		})
	case pluginabi.MethodManagementHandle:
		return managementHandle(name, request)
	case pluginabi.MethodSchedulerPick:
		var req pluginapi.SchedulerPickRequest
		_ = json.Unmarshal(request, &req)
		if len(req.Candidates) == 0 {
			return ok(map[string]any{"handled": false})
		}
		return ok(map[string]any{"auth_id": req.Candidates[len(req.Candidates)-1].ID, "handled": true})
	case pluginabi.MethodExecutorExecute, pluginabi.MethodExecutorCountTokens:
		var req pluginapi.ExecutorRequest
		_ = json.Unmarshal(request, &req)
		return ok(pluginapi.ExecutorResponse{Payload: req.Payload, Headers: map[string][]string{"X-Recorder": {name}}})
	case pluginabi.MethodExecutorExecuteStream:
		return executeStream(request)
	default:
		return failure("unknown_method", "unknown method: "+method, 0)
	}
}

type hostCall struct {
	Method  string          `json:"method"`
	Request json.RawMessage `json:"request"`
}

func managementHandle(name string, request []byte) ([]byte, int) {
	var req struct {
		pluginapi.ManagementRequest
		HostCallbackID string `json:"host_callback_id"`
	}
	_ = json.Unmarshal(request, &req)
	var body struct {
		Calls []hostCall `json:"calls"`
	}
	_ = json.Unmarshal(req.Body, &body)
	results := make([]json.RawMessage, 0, len(body.Calls))
	for _, call := range body.Calls {
		payload := call.Request
		var fields map[string]any
		if json.Unmarshal(payload, &fields) == nil && fields != nil {
			if _, set := fields["host_callback_id"]; !set {
				fields["host_callback_id"] = req.HostCallbackID
			}
			payload, _ = json.Marshal(fields)
		}
		resp, rc := callHost(call.Method, payload)
		results = append(results, json.RawMessage(strconv.Quote(strconv.Itoa(rc)+" "+string(resp))))
	}
	out, _ := json.Marshal(map[string]any{
		"label":   name + " <&>",
		"method":  req.Method,
		"path":    req.Path,
		"query":   req.Query,
		"results": results,
	})
	return ok(pluginapi.ManagementResponse{
		StatusCode: 201,
		Headers:    map[string][]string{"content-type": {"application/json"}, "X-Recorder": {name}},
		Body:       out,
	})
}

func executeStream(request []byte) ([]byte, int) {
	var req struct {
		pluginapi.ExecutorRequest
		StreamID string `json:"stream_id"`
	}
	_ = json.Unmarshal(request, &req)
	if strings.Contains(string(req.Payload), "inline") {
		return ok(map[string]any{
			"headers": map[string][]string{"X-Stream": {"inline"}},
			"chunks":  []pluginapi.ExecutorStreamChunk{{Payload: []byte("a")}, {Payload: []byte("b")}},
		})
	}
	go func() {
		for _, chunk := range []string{"one", "two", "three"} {
			raw, _ := json.Marshal(map[string]any{"stream_id": req.StreamID, "payload": []byte(chunk)})
			callHost(pluginabi.MethodHostStreamEmit, raw)
		}
		closeReq := map[string]any{"stream_id": req.StreamID}
		if strings.Contains(string(req.Payload), "fail") {
			closeReq["error"] = "stream failed"
		}
		raw, _ := json.Marshal(closeReq)
		callHost(pluginabi.MethodHostStreamClose, raw)
	}()
	return ok(map[string]any{"headers": map[string][]string{"X-Stream": {"async"}}})
}

func writeResponse(response *C.cliproxy_buffer, raw []byte) {
	if response == nil || len(raw) == 0 {
		return
	}
	ptr := C.CBytes(raw)
	if ptr == nil {
		return
	}
	response.ptr = ptr
	response.len = C.size_t(len(raw))
}

func callHost(method string, payload []byte) ([]byte, int) {
	cMethod := C.CString(method)
	defer C.free(unsafe.Pointer(cMethod))
	var response C.cliproxy_buffer
	var req *C.uint8_t
	if len(payload) > 0 {
		req = (*C.uint8_t)(C.CBytes(payload))
		defer C.free(unsafe.Pointer(req))
	}
	rc := C.call_host_api(cMethod, req, C.size_t(len(payload)), &response)
	var out []byte
	if response.ptr != nil {
		out = C.GoBytes(response.ptr, C.int(response.len))
		C.free_host_buffer(response.ptr, response.len)
	}
	return out, int(rc)
}
