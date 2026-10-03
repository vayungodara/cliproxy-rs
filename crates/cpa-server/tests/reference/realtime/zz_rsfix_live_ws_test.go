package live

// WebSocket goldens for cliproxy-rs: the real sideband and standard Realtime handlers
// relaying to a local gorilla upstream. Copy into internal/client/codex/live/ of
// CLIProxyAPI 6fecc6e and run with RSFIX_OUT=<dir>; it writes codex_live_ws_go.json.

import (
	"bufio"
	"encoding/json"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/gin-gonic/gin"
	"github.com/gorilla/websocket"
	"github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/auth"
)

type rsfixFrame struct {
	Kind string `json:"kind"`
	Data string `json:"data,omitempty"`
	Code int    `json:"code,omitempty"`
}

type rsfixUpstreamSeen struct {
	Target   string              `json:"target"`
	Headers  map[string][]string `json:"headers"`
	Received []rsfixFrame        `json:"received"`
}

type rsfixWSCase struct {
	Name       string              `json:"name"`
	Path       string              `json:"path"`
	Principal  string              `json:"principal"`
	Headers    map[string][]string `json:"headers"`
	Send       []rsfixFrame        `json:"send"`
	Status     int                 `json:"status"`
	RespHeader map[string][]string `json:"response_headers"`
	Body       string              `json:"body,omitempty"`
	Protocol   string              `json:"protocol,omitempty"`
	Received   []rsfixFrame        `json:"received"`
	Upstream   *rsfixUpstreamSeen  `json:"upstream,omitempty"`
	CallKept   bool                `json:"call_kept"`
}

func rsfixClose(err error) rsfixFrame {
	if closeErr, ok := err.(*websocket.CloseError); ok {
		return rsfixFrame{Kind: "close", Code: closeErr.Code, Data: closeErr.Text}
	}
	return rsfixFrame{Kind: "gone"}
}

func TestRSFixLiveWebsockets(t *testing.T) {
	dir := os.Getenv("RSFIX_OUT")
	if dir == "" {
		t.Skip("RSFIX_OUT not set")
	}
	t.Setenv("HTTPS_PROXY", "http://127.0.0.1:9")
	t.Setenv("HTTP_PROXY", "http://127.0.0.1:9")
	gin.SetMode(gin.TestMode)

	var mu sync.Mutex
	var seen *rsfixUpstreamSeen
	done := make(chan struct{}, 1)
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		target := r.URL.RequestURI()
		for _, reject := range []struct {
			marker, contentType, body string
			status                    int
		}{
			{"401", "application/json", `{"error":{"message":"token expired"}}`, 401},
			{"404", "text/plain", "no such call", 404},
			{"429", "application/json", `{"error":"slow"}`, 429},
		} {
			if strings.Contains(target, "-"+reject.marker) {
				w.Header().Set("Content-Type", reject.contentType)
				w.Header().Set("X-Request-Id", "up-"+reject.marker)
				w.Header().Set("Retry-After", "9")
				w.Header().Set("X-Other", "dropped")
				w.WriteHeader(reject.status)
				_, _ = w.Write([]byte(reject.body))
				return
			}
		}
		var responseHeader http.Header
		if protocols := websocket.Subprotocols(r); len(protocols) > 0 && protocols[len(protocols)-1] != "no-select" {
			responseHeader = http.Header{"Sec-Websocket-Protocol": {protocols[len(protocols)-1]}}
		}
		upgrader := websocket.Upgrader{CheckOrigin: func(*http.Request) bool { return true }}
		conn, errUpgrade := upgrader.Upgrade(w, r, responseHeader)
		if errUpgrade != nil {
			return
		}
		defer func() { _ = conn.Close() }()
		record := &rsfixUpstreamSeen{Target: target, Headers: map[string][]string{}}
		for _, name := range []string{"Authorization", "Chatgpt-Account-Id", "Openai-Alpha", "Originator", "X-Oai-Attestation", "Session-Id", "X-Operator", "Sec-Websocket-Protocol", "X-Not-Forwarded"} {
			if values := r.Header.Values(name); len(values) > 0 {
				record.Headers[name] = values
			}
		}
		defer func() {
			mu.Lock()
			seen = record
			mu.Unlock()
			done <- struct{}{}
		}()
		for {
			messageType, payload, errRead := conn.ReadMessage()
			if errRead != nil {
				record.Received = append(record.Received, rsfixClose(errRead))
				return
			}
			kind := "text"
			if messageType == websocket.BinaryMessage {
				kind = "binary"
			}
			record.Received = append(record.Received, rsfixFrame{Kind: kind, Data: string(payload)})
			if string(payload) == "upstream-close" {
				_ = conn.WriteMessage(websocket.CloseMessage, websocket.FormatCloseMessage(4001, "bye"))
				_, _, errRead = conn.ReadMessage()
				record.Received = append(record.Received, rsfixClose(errRead))
				return
			}
			_ = conn.WriteMessage(messageType, append([]byte("echo:"), payload...))
		}
	}))
	defer upstream.Close()

	manager := auth.NewManager(nil, nil, nil)
	executor := &captureExecutor{}
	manager.RegisterExecutor(executor)
	registerCredential(t, manager, &auth.Auth{ID: "a-other-oauth", Provider: "codex", Status: auth.StatusActive, Metadata: map[string]any{"access_token": "other-token", "account_id": "other-account"}})
	registerCredential(t, manager, &auth.Auth{ID: "b-pinned-oauth", Provider: "codex", Status: auth.StatusActive, Attributes: map[string]string{"header:X-Operator": "op-value"}, Metadata: map[string]any{"access_token": "pinned-token", "account_id": "pinned-account"}})
	handler := NewHandler(manager, nil)
	handler.sidebandAPIBaseURL = "ws" + strings.TrimPrefix(upstream.URL, "http") + "/v1"
	// The real ephemeral key store; its session is what a client_secrets call stores.
	secretSession := json.RawMessage(`{"instructions":"<be brief>","model":"gpt-live-1-codex","type":"realtime","voice":"alloy"}`)
	token, grant, _, errCreate := handler.clientSecrets.create(secretSession, time.Minute, "owner-key", "config-inline")
	if errCreate != nil {
		t.Fatal(errCreate)
	}
	// Go's realtimeAuthMiddleware, reduced to what these routes read.
	auth := func(c *gin.Context) {
		switch c.GetHeader("X-Test-Principal") {
		case "owner":
			c.Set("userApiKey", "owner-key")
			c.Set("accessProvider", "config-inline")
		case "other":
			c.Set("userApiKey", "other-key")
			c.Set("accessProvider", "config-inline")
		case "secret":
			authorization, _, _ := handler.AuthenticateClientSecret(c.Request)
			c.Set("userApiKey", authorization.IssuerPrincipal)
			c.Set("accessProvider", authorization.IssuerProvider)
			c.Set(ClientSecretSessionContextKey, authorization.Session)
			c.Set(ClientSecretPrincipalContextKey, authorization.Principal)
		}
		c.Next()
	}
	router := gin.New()
	router.GET("/v1/live/:call_id", auth, handler.HandleSideband)
	router.GET("/v1/realtime", auth, handler.HandleRealtimeWebsocket)
	router.GET("/v1/realtime/calls/:call_id", auth, handler.HandleSideband)
	downstream := httptest.NewServer(router)
	defer downstream.Close()

	store := func(callID, principal string) {
		session := liveSession{authID: "b-pinned-oauth", model: defaultLiveModel, ownerPrincipal: "owner-key", ownerProvider: "config-inline"}
		if principal == "secret" {
			session.clientSecretPrincipal = grant.Principal
		}
		handler.sessions.put(callID, session)
	}

	var cases []rsfixWSCase
	run := func(name, path, principal string, headers map[string][]string, send []rsfixFrame) {
		mu.Lock()
		seen = nil
		mu.Unlock()
		select {
		case <-done:
		default:
		}
		header := http.Header{"X-Test-Principal": {principal}}
		if principal == "secret" {
			header.Set("Authorization", "Bearer "+token)
		}
		for k, v := range headers {
			header[k] = v
		}
		result := rsfixWSCase{Name: name, Path: path, Principal: principal, Headers: headers, Send: send, RespHeader: map[string][]string{}}
		conn, response, errDial := websocket.DefaultDialer.Dial("ws"+strings.TrimPrefix(downstream.URL, "http")+path, header)
		if response != nil {
			result.Status = response.StatusCode
			for _, name := range []string{"Content-Type", "Retry-After", "X-Request-Id", "X-Other", "Upgrade", "Sec-Websocket-Protocol"} {
				if values := response.Header.Values(name); len(values) > 0 {
					result.RespHeader[name] = values
				}
			}
			if errDial != nil && response.Body != nil {
				buf := make([]byte, 4096)
				n, _ := response.Body.Read(buf)
				result.Body = string(buf[:n])
			}
		}
		if errDial == nil {
			result.Protocol = conn.Subprotocol()
			for _, frame := range send {
				switch frame.Kind {
				case "text":
					_ = conn.WriteMessage(websocket.TextMessage, []byte(frame.Data))
				case "binary":
					_ = conn.WriteMessage(websocket.BinaryMessage, []byte(frame.Data))
				case "ping":
					_ = conn.WriteControl(websocket.PingMessage, []byte(frame.Data), time.Now().Add(time.Second))
					continue
				case "close":
					_ = conn.WriteMessage(websocket.CloseMessage, websocket.FormatCloseMessage(frame.Code, frame.Data))
				case "read":
				}
				_ = conn.SetReadDeadline(time.Now().Add(2 * time.Second))
				messageType, payload, errRead := conn.ReadMessage()
				if errRead != nil {
					result.Received = append(result.Received, rsfixClose(errRead))
					break
				}
				kind := "text"
				if messageType == websocket.BinaryMessage {
					kind = "binary"
				}
				result.Received = append(result.Received, rsfixFrame{Kind: kind, Data: string(payload)})
			}
			_ = conn.Close()
			select {
			case <-done:
			case <-time.After(2 * time.Second):
			}
		}
		time.Sleep(50 * time.Millisecond)
		mu.Lock()
		result.Upstream = seen
		mu.Unlock()
		if result.Upstream != nil {
			sort.Strings(result.Upstream.Headers["Sec-Websocket-Protocol"])
		}
		callID := strings.TrimPrefix(strings.TrimPrefix(strings.Split(path, "?")[0], "/v1/live/"), "/v1/realtime/calls/")
		if strings.Contains(path, "call_id=") {
			callID = strings.Split(path, "call_id=")[1]
		}
		_, result.CallKept = handler.sessions.peek(callID)
		cases = append(cases, result)
	}

	protocolHeaders := map[string][]string{"Openai-Alpha": {"quicksilver=v2"}, "X-Oai-Attestation": {"attest"}, "Session-Id": {"session-1"}, "X-Not-Forwarded": {"x"}, "Sec-Websocket-Protocol": {"realtime, openai-beta.realtime-v1"}}
	echo := []rsfixFrame{{Kind: "text", Data: "hello"}, {Kind: "ping", Data: "p"}, {Kind: "binary", Data: "\x01\x02"}, {Kind: "text", Data: "upstream-close"}}

	store("call-live", "owner")
	run("live_sideband_relays", "/v1/live/call-live", "owner", protocolHeaders, echo)
	run("live_sideband_consumed", "/v1/live/call-live", "owner", nil, nil)
	store("call-calls", "owner")
	run("calls_sideband_client_close", "/v1/realtime/calls/call-calls", "owner", nil, []rsfixFrame{{Kind: "text", Data: "a"}, {Kind: "close", Code: 4002, Data: "client-bye"}})
	store("call-query", "owner")
	run("query_sideband", "/v1/realtime?call_id=call-query", "owner", nil, []rsfixFrame{{Kind: "text", Data: "q"}, {Kind: "close", Code: 1000}})
	store("call-scope", "owner")
	run("sideband_other_principal", "/v1/live/call-scope", "other", nil, nil)
	run("sideband_after_scope_reject", "/v1/live/call-scope", "owner", nil, []rsfixFrame{{Kind: "close", Code: 1001, Data: "away"}})
	store("call-secret", "secret")
	run("sideband_secret_owner", "/v1/realtime/calls/call-secret", "secret", nil, []rsfixFrame{{Kind: "text", Data: "s"}, {Kind: "close", Code: 1000}})
	store("call-secret2", "owner")
	run("sideband_secret_wrong_call", "/v1/realtime/calls/call-secret2", "secret", nil, nil)
	for _, status := range []string{"401", "404", "429"} {
		store("call-"+status, "owner")
		run("sideband_upstream_"+status, "/v1/realtime/calls/call-"+status, "owner", nil, nil)
		store("live-"+status, "owner")
		run("live_sideband_upstream_"+status, "/v1/live/live-"+status, "owner", nil, nil)
	}
	run("direct_relays", "/v1/realtime?model=gpt-realtime", "owner", map[string][]string{"Openai-Alpha": {"dropped"}, "Sec-Websocket-Protocol": {"realtime"}}, []rsfixFrame{{Kind: "text", Data: "d"}, {Kind: "close", Code: 1000}})
	run("direct_default_model_originator", "/v1/realtime", "owner", map[string][]string{"Originator": {"my-app"}}, []rsfixFrame{{Kind: "close", Code: 1000}})
	run("direct_secret_session_update", "/v1/realtime?model=gpt-realtime-mini", "secret", nil, []rsfixFrame{{Kind: "read"}, {Kind: "text", Data: "after-update"}, {Kind: "close", Code: 1000}})
	for _, status := range []string{"401", "404", "429"} {
		run("direct_upstream_"+status, "/v1/realtime?model=m-"+status, "owner", nil, nil)
	}

	store("call-noselect", "owner")
	run("sideband_upstream_selects_no_protocol", "/v1/live/call-noselect", "owner", map[string][]string{"Sec-Websocket-Protocol": {"realtime, no-select"}}, []rsfixFrame{{Kind: "text", Data: "n"}, {Kind: "close", Code: 1000}})

	// Raw handshakes gorilla's client would never send: the downstream upgrade checks.
	type rawCase struct {
		Name    string   `json:"name"`
		CallID  string   `json:"call_id"`
		Headers []string `json:"headers"`
		Status  int      `json:"status"`
		Kept    bool     `json:"call_kept"`
	}
	var raws []rawCase
	goodKey := "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ=="
	for _, raw := range []rawCase{
		{Name: "raw_valid", Headers: []string{"Connection: Upgrade", "Upgrade: websocket", "Sec-WebSocket-Version: 13", goodKey}},
		{Name: "raw_bad_key", Headers: []string{"Connection: Upgrade", "Upgrade: websocket", "Sec-WebSocket-Version: 13", "Sec-WebSocket-Key: x"}},
		{Name: "raw_short_key", Headers: []string{"Connection: Upgrade", "Upgrade: websocket", "Sec-WebSocket-Version: 13", "Sec-WebSocket-Key: AAAAAAAAAAAAAAAAAAAA"}},
		{Name: "raw_missing_key", Headers: []string{"Connection: Upgrade", "Upgrade: websocket", "Sec-WebSocket-Version: 13"}},
		{Name: "raw_upgrade_token_list", Headers: []string{"Connection: keep-alive, Upgrade", "Upgrade: h2c, websocket", "Sec-WebSocket-Version: 13", goodKey}},
		{Name: "raw_version_list", Headers: []string{"Connection: Upgrade", "Upgrade: websocket", "Sec-WebSocket-Version: 12, 13", goodKey}},
		{Name: "raw_version_wrong", Headers: []string{"Connection: Upgrade", "Upgrade: websocket", "Sec-WebSocket-Version: 12", goodKey}},
	} {
		raw.CallID = "call-" + strings.ReplaceAll(raw.Name, "_", "-")
		store(raw.CallID, "owner")
		conn, errDial := net.Dial("tcp", strings.TrimPrefix(downstream.URL, "http://"))
		if errDial != nil {
			t.Fatal(errDial)
		}
		request := "GET /v1/live/" + raw.CallID + " HTTP/1.1\r\nHost: x\r\nX-Test-Principal: owner\r\n" + strings.Join(raw.Headers, "\r\n") + "\r\n\r\n"
		_, _ = conn.Write([]byte(request))
		_ = conn.SetReadDeadline(time.Now().Add(2 * time.Second))
		response, errRead := http.ReadResponse(bufio.NewReader(conn), nil)
		if errRead == nil {
			raw.Status = response.StatusCode
		}
		_ = conn.Close()
		if raw.Status == 101 {
			select {
			case <-done:
			case <-time.After(2 * time.Second):
			}
		}
		time.Sleep(50 * time.Millisecond)
		_, raw.Kept = handler.sessions.peek(raw.CallID)
		raws = append(raws, raw)
	}
	// How the relay ends when the downstream connection fails mid-session
	// (websocketCloseDetails): what close frame the upstream receives.
	type endCase struct {
		Name     string       `json:"name"`
		Upstream []rsfixFrame `json:"upstream_received"`
	}
	var ends []endCase
	for _, action := range []string{"rsv1", "abrupt", "reset"} {
		callID := "call-end-" + action
		store(callID, "owner")
		mu.Lock()
		seen = nil
		mu.Unlock()
		select {
		case <-done:
		default:
		}
		conn, errDial := net.Dial("tcp", strings.TrimPrefix(downstream.URL, "http://"))
		if errDial != nil {
			t.Fatal(errDial)
		}
		request := "GET /v1/live/" + callID + " HTTP/1.1\r\nHost: x\r\nX-Test-Principal: owner\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\n" + goodKey + "\r\n\r\n"
		_, _ = conn.Write([]byte(request))
		reader := bufio.NewReader(conn)
		response, errRead := http.ReadResponse(reader, nil)
		if errRead != nil || response.StatusCode != 101 {
			t.Fatalf("%s: upgrade failed: %v", action, errRead)
		}
		switch action {
		case "rsv1":
			payload := []byte("x")
			mask := []byte{1, 2, 3, 4}
			frame := []byte{0x80 | 0x40 | 0x1, 0x80 | byte(len(payload))}
			frame = append(frame, mask...)
			for i, b := range payload {
				frame = append(frame, b^mask[i%4])
			}
			_, _ = conn.Write(frame)
			time.Sleep(200 * time.Millisecond)
			_ = conn.Close()
		case "abrupt":
			_ = conn.Close()
		case "reset":
			_ = conn.(*net.TCPConn).SetLinger(0)
			_ = conn.Close()
		}
		// Earlier cases can leave stale completions; wait for this call's own record.
		record := endCase{Name: action}
		for deadline := time.Now().Add(10 * time.Second); time.Now().Before(deadline); time.Sleep(10 * time.Millisecond) {
			select {
			case <-done:
			default:
			}
			mu.Lock()
			if seen != nil && strings.Contains(seen.Target, callID) {
				record.Upstream = seen.Received
			}
			mu.Unlock()
			if record.Upstream != nil {
				break
			}
		}
		if record.Upstream == nil {
			t.Fatalf("%s: upstream did not finish", action)
		}
		ends = append(ends, record)
	}
	endEncoded, errMarshal := json.MarshalIndent(ends, "", " ")
	if errMarshal != nil {
		t.Fatal(errMarshal)
	}
	if errWrite := os.WriteFile(filepath.Join(dir, "codex_live_ws_end_go.json"), endEncoded, 0o644); errWrite != nil {
		t.Fatal(errWrite)
	}

	rawEncoded, errMarshal := json.MarshalIndent(raws, "", " ")
	if errMarshal != nil {
		t.Fatal(errMarshal)
	}
	if errWrite := os.WriteFile(filepath.Join(dir, "codex_live_ws_raw_go.json"), rawEncoded, 0o644); errWrite != nil {
		t.Fatal(errWrite)
	}

	encoded, errMarshal := json.MarshalIndent(cases, "", " ")
	if errMarshal != nil {
		t.Fatal(errMarshal)
	}
	if errWrite := os.WriteFile(filepath.Join(dir, "codex_live_ws_go.json"), encoded, 0o644); errWrite != nil {
		t.Fatal(errWrite)
	}
}
