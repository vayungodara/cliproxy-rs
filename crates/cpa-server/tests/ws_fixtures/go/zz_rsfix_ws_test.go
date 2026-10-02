package openai

// Fixture generator for the cliproxy-rs Responses WebSocket (GET /v1/responses).
// Copy into sdk/api/handlers/openai/ of CLIProxyAPI 6fecc6e and run with
// RSFIX_OUT=<dir>; without it every generator test is skipped. Nothing leaves the
// machine: the upstream is a loopback mock and every credential is fake.

import (
	"context"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/gin-gonic/gin"
	"github.com/gorilla/websocket"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/interfaces"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/registry"
	runtimeexecutor "github.com/router-for-me/CLIProxyAPI/v8/internal/runtime/executor"
	"github.com/router-for-me/CLIProxyAPI/v8/sdk/api/handlers"
	coreauth "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/auth"
	"github.com/tidwall/gjson"
)

func rsfixWSOut(t *testing.T) string {
	dir := os.Getenv("RSFIX_OUT")
	if dir == "" {
		t.Skip("RSFIX_OUT not set")
	}
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	return dir
}

func rsfixWSWrite(t *testing.T, dir, name string, v any) {
	data, err := json.MarshalIndent(v, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, name), append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}

// ---------------------------------------------------------------- pure vectors

type rsfixWSVector struct {
	Fn      string   `json:"fn"`
	Raw     string   `json:"raw,omitempty"`
	Last    string   `json:"last,omitempty"`
	Output  string   `json:"output,omitempty"`
	LastID  string   `json:"last_id,omitempty"`
	Pending []string `json:"pending,omitempty"`
	Bypass  bool     `json:"bypass,omitempty"`
	Model   string   `json:"model,omitempty"`
	Status  int      `json:"status,omitempty"`
	Max     int      `json:"max,omitempty"`
	Events  []string `json:"events,omitempty"`
	Key     string   `json:"key,omitempty"`
	// Outputs.
	Out     string   `json:"out"`
	Updated string   `json:"updated,omitempty"`
	List    []string `json:"list,omitempty"`
	Bool    bool     `json:"bool,omitempty"`
	ErrCode int      `json:"err_status,omitempty"`
	Err     string   `json:"err,omitempty"`
}

func rsfixErr(v *rsfixWSVector, errMsg *interfaces.ErrorMessage) {
	if errMsg != nil {
		v.ErrCode = errMsg.StatusCode
		v.Err = errMsg.Error.Error()
	}
}

func TestRSFixWSVectors(t *testing.T) {
	dir := rsfixWSOut(t)
	var vectors []rsfixWSVector

	last := `{"model":"gpt-fixture","instructions":"be brief","stream":true,"input":[{"type":"message","role":"user","id":"m1","content":[{"type":"input_text","text":"hi"}]}]}`
	output := `[{"type":"message","role":"assistant","id":"o1","content":[{"type":"output_text","text":"hello"}]},{"type":"function_call","call_id":"c1","name":"f","arguments":"{}","id":"fc1"}]`
	summary := codexLocalCompactionSummaryPrefix + "\nsummary text"
	summaryJSON, _ := json.Marshal(summary)
	normalizeCases := []rsfixWSVector{
		{Raw: `{"type":"response.create","model":"gpt-fixture","input":[{"type":"message","role":"user","content":"hi"}],"generate":true}`},
		{Raw: `{"type":"response.create","model":"gpt-fixture"}`},
		{Raw: `{"type":"response.create","input":[]}`},
		{Raw: `{"type":"response.create","model":"m","input":"text"}`},
		{Raw: `{"type":"response.cancel","model":"m"}`},
		{Raw: `{"type":"response.append","input":[{"type":"message","role":"user","content":"more"}]}`},
		{Raw: `{"type":"response.append","input":[{"type":"function_call_output","call_id":"c1","output":"ok"}]}`, Last: last, Output: output, LastID: "r1"},
		{Raw: `{"type":"response.create","input":[{"type":"message","role":"user","id":"m2","content":"next"}]}`, Last: last, Output: output, LastID: "r1"},
		{Raw: `{"type":"response.create","model":"other","instructions":"new","previous_response_id":"r1","input":[{"type":"message","role":"user","content":"x"}]}`, Last: last, Output: output},
		{Raw: `{"type":"response.create","input":[{"type":"message","role":"assistant","content":"replayed"},{"type":"message","role":"user","content":"q"}]}`, Last: last, Output: output},
		{Raw: `{"type":"response.create","input":[{"type":"function_call","call_id":"c9","name":"g","arguments":"{}"}]}`, Last: last, Output: output},
		{Raw: `{"type":"response.create","input":[{"type":"message","role":"user","content":` + string(summaryJSON) + `}]}`, Last: last, Output: output},
		{Raw: `{"type":"response.create","input":[{"type":"additional_tools","role":"developer","tools":[{"type":"function","name":"f"}]},{"type":"message","role":"user","content":[{"type":"input_text","text":` + string(summaryJSON) + `}]}]}`, Last: last, Output: output},
		{Raw: `{"type":"response.create","input":[{"type":"compaction","encrypted_content":"e"},{"type":"message","role":"user","content":"after"}]}`, Last: last, Output: output, Bypass: true},
		{Raw: `{"type":"response.create","input":[{"type":"compaction","encrypted_content":"e"},{"type":"message","role":"user","content":"after"}]}`, Last: last, Output: output, Bypass: false},
		{Raw: `{"type":"response.create","input":[{"type":"message","role":"user","id":"m1","content":"dup id"}]}`, Last: last, Output: output},
		{Raw: `{"type":"response.create","input":[{"type":"function_call","call_id":"c1","name":"f","arguments":"{}","id":"fc2"},{"type":"function_call_output","call_id":"c1","output":"r"}],"previous_response_id":""}`, Last: last, Output: output},
		{Raw: `{"type":"response.append","input":{"not":"array"}}`, Last: last},
		{Raw: `{"type":"response.append","input":[1, "two", null]}`, Last: last, Output: `[]`},
		{Raw: `{"type":"response.create","input":[{"type":"message","role":"user","content":"&<>"}]}`, Last: `{"model":"m","input":[{"type":"message","role":"user","content":"a"}],"Input":[{"type":"message","role":"user","content":"b"}]}`, Output: `[{"type":"compaction_trigger"}]`},
		{Raw: `{"type":"response.create","input":[{"type":"message","role":"user","content":"x"}]}`, Last: `{"model":"m","input":"bad"}`},
		{Raw: `{"type":"response.create","input":[{"type":"message","role":"user","content":"x"}]}`, Last: `{"model":"m","input":[{"type":"compaction_trigger"},{"type":"message","role":"user","content":"a"}]}`, Output: `[{"type":"compaction","encrypted_content":"z"}]`},
	}
	for _, c := range normalizeCases {
		c.Fn = "normalize"
		req, updated, errMsg := normalizeResponsesWebsocketRequestWithIncrementalState([]byte(c.Raw), []byte(c.Last), []byte(c.Output), c.LastID, c.Pending, false, c.Bypass)
		c.Out, c.Updated = string(req), string(updated)
		if errMsg != nil {
			c.Out, c.Updated = "", ""
		}
		rsfixErr(&c, errMsg)
		vectors = append(vectors, c)
	}

	for _, c := range []rsfixWSVector{
		{Raw: `{"type":"response.create","previous_response_id":"r1","input":[{"type":"message","role":"user","content":"x"}]}`, Model: "gpt-fixture"},
		{Raw: `{"type":"response.append","model":"m2","input":[]}`},
		{Raw: `{"type":"response.create","input":[]}`},
		{Raw: `{"type":"response.create","input":[]`, Model: "m"},
		{Raw: `{"type":"other","model":"m"}`},
	} {
		c.Fn = "passthrough"
		out, errMsg := normalizeResponsesWebsocketPassthroughRequest([]byte(c.Raw), c.Model)
		c.Out = string(out)
		rsfixErr(&c, errMsg)
		vectors = append(vectors, c)
	}

	warmup := `{"model":"gpt-fixture","instructions":"warm","stream":true,"input":[{"type":"message","role":"user","id":"w1","content":"setup"}]}`
	for _, c := range []rsfixWSVector{
		{Raw: `{"type":"response.create","previous_response_id":"resp_prewarm_x","input":[{"type":"message","role":"user","content":"go"}]}`, Last: warmup},
		{Raw: `{"type":"response.append","input":[{"type":"message","role":"user","id":"w1","content":"again"}]}`, Last: warmup},
		{Raw: `{"type":"response.append","input":"x"}`, Last: warmup},
		{Raw: `{"type":"bogus","input":[]}`, Last: warmup},
	} {
		c.Fn = "prewarm_followup"
		req, updated, errMsg := normalizeResponsesWebsocketPrewarmFollowup([]byte(c.Raw), []byte(c.Last))
		c.Out, c.Updated = string(req), string(updated)
		if errMsg != nil {
			c.Out, c.Updated = "", ""
		}
		rsfixErr(&c, errMsg)
		vectors = append(vectors, c)
	}

	for _, raw := range []string{
		`{"type":"response.create","generate":false}`,
		`{"type":"response.create","generate":"FALSE"}`,
		`{"type":"response.create","generate":0}`,
		`{"type":"response.create","generate":true}`,
		`{"type":"response.create"}`,
		`{"type":"response.append","generate":false}`,
	} {
		vectors = append(vectors, rsfixWSVector{Fn: "prewarm_local", Raw: raw, Bool: shouldHandleResponsesWebsocketPrewarmLocally([]byte(raw), false)})
	}

	for _, chunk := range []string{
		"event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\"}}\n\n",
		"data: {\"a\":1}\r\ndata: [DONE]\n\ndata: not json\n",
		"{\"type\":\"response.completed\"}",
		"data:{\"x\":true}",
		"  \n\n",
		"data: [DONE]",
		"{\"a\":1}\n{\"b\":2}",
	} {
		vectors = append(vectors, rsfixWSVector{Fn: "chunk", Raw: chunk, List: rsfixStrings(websocketJSONPayloadsFromChunk([]byte(chunk)))})
	}

	for _, c := range []rsfixWSVector{
		{Status: 400, Raw: "websocket request requires array field: input"},
		{Status: 409, Raw: `{"error":{"message":"Previous response is not available","type":"invalid_request_error","code":"previous_response_not_found"}}`},
		{Status: 401, Raw: "bad key <x>"},
		{Status: 429, Raw: ""},
		{Status: 0, Raw: "boom"},
		{Status: 408, Raw: "stream closed before response.completed"},
		{Status: 502, Raw: `{"detail":"upstream"}`},
		{Status: 404, Raw: "  "},
		{Status: 422, Raw: "[1,2]"},
	} {
		c.Fn = "error_payload"
		var errMsg *interfaces.ErrorMessage
		if c.Raw == "" && c.Status == 429 {
			errMsg = &interfaces.ErrorMessage{StatusCode: c.Status}
		} else {
			errMsg = &interfaces.ErrorMessage{StatusCode: c.Status, Error: errors.New(c.Raw)}
		}
		payload, err := buildResponsesWebsocketErrorPayload(errMsg)
		if err != nil {
			t.Fatal(err)
		}
		c.Out = string(payload)
		vectors = append(vectors, c)
	}

	for _, c := range []rsfixWSVector{
		{Status: 400, Raw: `{"error":{"code":"context_length_exceeded","message":"too long"}}`},
		{Status: 400, Raw: "plain"},
		{Status: 429, Raw: `{"error":{"type":"invalid_request_error"}}`},
		{Status: 404, Raw: `{"error":{"code":"model_not_found"}}`},
		{Status: 401, Raw: `{"error":{"type":"authentication_error"}}`},
		{Status: 502, Raw: `{"error":{"code":"previous_response_not_found"}}`},
		{Status: 500, Raw: "Item with id 'x' not found. Items are not persisted when `store` is set to false."},
		{Status: 503, Raw: "unavailable"},
		{Status: 413, Raw: `{"error":{"code":"message_too_big"}}`},
	} {
		c.Fn = "expose"
		c.Bool = shouldExposeResponsesUpstreamError(&interfaces.ErrorMessage{StatusCode: c.Status, Error: errors.New(c.Raw)})
		vectors = append(vectors, c)
	}

	completionSequences := [][]string{
		{
			`{"type":"response.created","response":{"id":"r1"}}`,
			`{"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","call_id":"c1","name":"f","arguments":"{\"a\":1}"}}`,
			`{"type":"response.output_item.done","output_index":0,"item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"a < b"}]}}`,
			`{"type":"response.output_item.done","item":{"type":"function_call","call_id":"c2","name":"g"}}`,
			`{"type":"response.completed","response":{"id":"r1","output":[]}}`,
		},
		{
			`{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"c1","name":"f","arguments":"{\"full\":true}"}}`,
			`{"type":"response.completed","response":{"id":"r2","output":[{"type":"function_call","call_id":"c1","name":"f","arguments":""},{"type":"message","role":"assistant","content":[]}]}}`,
		},
		{
			`{"type":"response.output_item.done","output_index":0,"item":{"type":"message","role":"assistant"}}`,
			`{"type":"response.done","response":{"id":"r3"}}`,
			`{"type":"response.output_item.done","output_index":1,"item":{"type":"function_call_output","call_id":"c1","output":"x"}}`,
		},
		{
			`{"type":"response.completed","response":{"id":"r4","output":[{"type":"function_call","call_id":"c7","name":"f","arguments":"{}"}]}}`,
		},
	}
	for _, events := range completionSequences {
		byIndex := make(map[int64][]byte)
		var fallback [][]byte
		pending := make(map[string]struct{})
		v := rsfixWSVector{Fn: "completion", Events: events}
		for _, event := range events {
			payload := []byte(event)
			collectResponsesWebsocketOutputItem(payload, byIndex, &fallback)
			kind := gjson.GetBytes(payload, "type").String()
			if isResponsesWebsocketCompletionEvent(kind) {
				payload = restoreResponsesWebsocketCompletionOutput(payload, byIndex, fallback)
				v.List = append(v.List, string(payload), string(responseCompletedOutputFromPayload(payload, byIndex, fallback)))
			}
			recordPendingToolCallIDsFromPayload(pending, payload)
		}
		v.Out = strings.Join(sortedStringSet(pending), ",")
		vectors = append(vectors, v)
	}

	// Tool-call repair against the shared caches, in order: Bypass marks a committed turn.
	key := "rsfix-repair-session"
	retainResponsesWebsocketToolCaches(key)
	for _, c := range []rsfixWSVector{
		{Raw: `{"input":[{"type":"message","role":"user"},{"type":"function_call","call_id":"c1","name":"f","arguments":"{}"}]}`},
		{Raw: `{"input":[{"type":"function_call_output","call_id":"c1","output":"ok"}],"previous_response_id":"r"}`, Bypass: true},
		{Raw: `{"input":[{"type":"message","role":"user"},{"type":"function_call","call_id":"c1","name":"f","arguments":"{}"}]}`},
		{Raw: `{"input":[{"type":"function_call_output","call_id":"c2","output":"orphan"}]}`},
		{Raw: `{"input":[{"type":"message","role":"user"}]}`, Events: []string{`{"type":"response.output_item.done","item":{"type":"function_call","call_id":"c2","name":"g","arguments":"{}"}}`}, Bypass: true},
		{Raw: `{"input":[{"type":"function_call_output","call_id":"c2","output":"late"},{"type":"function_call_output","call_id":"c2","output":"again"}]}`},
		{Raw: `{"input":[{"type":"function_call_output","output":"heartbeat","name":"hb"},{"type":"function_call_output","output":"nameless"},{"type":"function_call","name":"f","arguments":"{}"}]}`},
		{Raw: `{"input":"text"}`},
	} {
		c.Fn, c.Key = "repair", key
		out, turn := prepareResponsesWebsocketFallbackTurn(key, []byte(c.Raw))
		for _, event := range c.Events {
			turn.recordResponse([]byte(event))
		}
		if c.Bypass {
			turn.commit()
		}
		c.Out = string(out)
		vectors = append(vectors, c)
	}
	releaseResponsesWebsocketToolCaches(key)
	noKey, _ := prepareResponsesWebsocketFallbackTurn("", []byte(`{"input":[{"type":"function_call","call_id":"c1","name":"f","arguments":"{}","id":"x"},{"type":"message","id":"x"}]}`))
	vectors = append(vectors, rsfixWSVector{Fn: "repair", Raw: `{"input":[{"type":"function_call","call_id":"c1","name":"f","arguments":"{}","id":"x"},{"type":"message","id":"x"}]}`, Out: string(noKey)})

	for _, c := range []rsfixWSVector{
		{Raw: "upstream requires HTTP replay", Max: 123},
		{Raw: strings.Repeat("é", 70), Max: 123},
		{Raw: "abc", Max: 0},
	} {
		c.Fn = "truncate"
		c.Out = truncateWebsocketCloseReason(c.Raw, c.Max)
		vectors = append(vectors, c)
	}

	prewarm, err := syntheticResponsesWebsocketPrewarmPayloads([]byte(`{"model":"gpt-fixture","input":[]}`))
	if err != nil {
		t.Fatal(err)
	}
	vectors = append(vectors, rsfixWSVector{Fn: "prewarm_payloads", Raw: `{"model":"gpt-fixture","input":[]}`, List: rsfixStrings(prewarm)})

	rsfixWSWrite(t, dir, "ws_vectors.json", vectors)
}

func rsfixStrings(in [][]byte) []string {
	out := make([]string, 0, len(in))
	for _, b := range in {
		out = append(out, string(b))
	}
	return out
}

// ---------------------------------------------------------------- end to end

type rsfixWSCred struct {
	ID     string            `json:"id"`
	Attrs  map[string]string `json:"attributes"`
	Models []string          `json:"models"`
}

// One scripted upstream answer. WebSocket turns send Events as frames and then apply
// Then ("" keeps the socket, "close" sends close 1000, "close1009", "drop" closes TCP).
// Closes wait 300ms so the proxy forwards the events first: Go's disconnect notifier
// otherwise races the turn and may close the client before those frames are written.
// HTTP requests answer Status/Body, or Events as SSE when Status is 0.
type rsfixWSReply struct {
	Status int      `json:"status,omitempty"`
	Body   string   `json:"body,omitempty"`
	Events []string `json:"events,omitempty"`
	Then   string   `json:"then,omitempty"`
}

type rsfixWSFrame struct {
	Text   string `json:"text,omitempty"`
	Close  int    `json:"close,omitempty"`
	Reason string `json:"reason,omitempty"`
}

type rsfixWSStep struct {
	// Send is written as a text frame; "{{last_response_id}}" is replaced by the id of
	// the last completed response the client saw. Empty sends nothing.
	Send string `json:"send,omitempty"`
	// Read: "completed" (until a completion event or close), "one", or "close".
	Read     string         `json:"read"`
	Upstream []rsfixWSReply `json:"upstream,omitempty"`
	Frames   []rsfixWSFrame `json:"frames"`
}

type rsfixWSCaptured struct {
	Kind    string            `json:"kind"`
	Headers map[string]string `json:"headers,omitempty"`
	Body    string            `json:"body,omitempty"`
}

type rsfixWSScenario struct {
	Name          string            `json:"name"`
	Config        string            `json:"config"`
	ClientHeaders map[string]string `json:"client_headers,omitempty"`
	Creds         []rsfixWSCred     `json:"credentials"`
	Steps         []rsfixWSStep     `json:"steps"`
	// Outputs.
	UpgradeTurnState string            `json:"upgrade_turn_state,omitempty"`
	Upstream         []rsfixWSCaptured `json:"upstream"`
}

type rsfixWSUpstream struct {
	mu       sync.Mutex
	script   []rsfixWSReply
	captured []rsfixWSCaptured
}

func (u *rsfixWSUpstream) next() (rsfixWSReply, bool) {
	u.mu.Lock()
	defer u.mu.Unlock()
	if len(u.script) == 0 {
		return rsfixWSReply{}, false
	}
	reply := u.script[0]
	u.script = u.script[1:]
	return reply, true
}

func (u *rsfixWSUpstream) record(c rsfixWSCaptured) {
	u.mu.Lock()
	defer u.mu.Unlock()
	u.captured = append(u.captured, c)
}

func rsfixHeaders(h http.Header) map[string]string {
	out := map[string]string{}
	for k, v := range h {
		out[strings.ToLower(k)] = strings.Join(v, ", ")
	}
	return out
}

func (u *rsfixWSUpstream) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	if websocket.IsWebSocketUpgrade(r) {
		u.record(rsfixWSCaptured{Kind: "ws_dial", Headers: rsfixHeaders(r.Header)})
		conn, err := (&websocket.Upgrader{}).Upgrade(w, r, nil)
		if err != nil {
			return
		}
		defer func() { _ = conn.Close() }()
		for {
			_, msg, errRead := conn.ReadMessage()
			if errRead != nil {
				return
			}
			u.record(rsfixWSCaptured{Kind: "ws_frame", Body: string(msg)})
			reply, ok := u.next()
			if !ok {
				continue
			}
			for _, event := range reply.Events {
				_ = conn.WriteMessage(websocket.TextMessage, []byte(event))
			}
			if reply.Then != "" {
				time.Sleep(300 * time.Millisecond)
			}
			switch reply.Then {
			case "close":
				_ = conn.WriteControl(websocket.CloseMessage, websocket.FormatCloseMessage(websocket.CloseNormalClosure, "bye"), time.Now().Add(time.Second))
				return
			case "close1009":
				_ = conn.WriteControl(websocket.CloseMessage, websocket.FormatCloseMessage(websocket.CloseMessageTooBig, "too big"), time.Now().Add(time.Second))
				return
			case "drop":
				return
			}
		}
	}
	var body []byte
	if r.Body != nil {
		body, _ = io.ReadAll(r.Body)
	}
	u.record(rsfixWSCaptured{Kind: "http", Headers: rsfixHeaders(r.Header), Body: string(body)})
	reply, ok := u.next()
	if !ok {
		w.WriteHeader(http.StatusInternalServerError)
		return
	}
	if reply.Status != 0 {
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(reply.Status)
		_, _ = w.Write([]byte(reply.Body))
		return
	}
	w.Header().Set("Content-Type", "text/event-stream")
	w.WriteHeader(http.StatusOK)
	for _, event := range reply.Events {
		_, _ = w.Write([]byte("event: " + gjson.Get(event, "type").String() + "\ndata: " + event + "\n\n"))
	}
	if f, ok := w.(http.Flusher); ok {
		f.Flush()
	}
}

func rsfixWSScenarios() []rsfixWSScenario {
	created := func(id string) string {
		return `{"type":"response.created","response":{"id":"` + id + `","status":"in_progress","output":[]}}`
	}
	message := func(text string) string {
		return `{"type":"response.output_item.done","output_index":0,"item":{"type":"message","role":"assistant","id":"msg_` + text + `","content":[{"type":"output_text","text":"` + text + `"}]}}`
	}
	delta := func(text string) string {
		return `{"type":"response.output_text.delta","output_index":0,"delta":"` + text + `"}`
	}
	completed := func(id string) string {
		return `{"type":"response.completed","response":{"id":"` + id + `","status":"completed","output":[],"usage":{"input_tokens":3,"output_tokens":2,"total_tokens":5}}}`
	}
	toolCall := `{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"lookup","arguments":"{\"q\":1}"}}`
	user := func(text string) string {
		return `{"type":"message","role":"user","content":[{"type":"input_text","text":"` + text + `"}]}`
	}
	ws := rsfixWSCred{ID: "codex-ws.json", Attrs: map[string]string{"api_key": "sk-fake-ws", "websockets": "true"}, Models: []string{"gpt-fixture"}}
	httpCred := rsfixWSCred{ID: "codex-http.json", Attrs: map[string]string{"api_key": "sk-fake-http"}, Models: []string{"gpt-fixture"}}
	cfg := "host: \"\"\n"
	return []rsfixWSScenario{
		{
			Name: "ws_two_turns", Config: cfg, Creds: []rsfixWSCred{ws},
			ClientHeaders: map[string]string{"x-codex-turn-state": "ts-1", "session_id": "sess-ws"},
			Steps: []rsfixWSStep{
				{Send: `{"type":"response.create","model":"gpt-fixture","instructions":"be brief","input":[` + user("hi") + `]}`, Read: "completed",
					Upstream: []rsfixWSReply{{Events: []string{created("r1"), delta("hel"), message("hello"), completed("r1")}}}},
				{Send: `{"type":"response.create","previous_response_id":"{{last_response_id}}","input":[` + user("more") + `]}`, Read: "completed",
					Upstream: []rsfixWSReply{{Events: []string{created("r2"), message("more"), completed("r2")}}}},
			},
		},
		{
			Name: "http_incremental", Config: cfg, Creds: []rsfixWSCred{httpCred},
			ClientHeaders: map[string]string{"session_id": "sess-http"},
			Steps: []rsfixWSStep{
				{Send: `{"type":"response.create","model":"gpt-fixture","instructions":"be brief","input":[` + user("hi") + `]}`, Read: "completed",
					Upstream: []rsfixWSReply{{Events: []string{created("r1"), message("hello"), completed("r1")}}}},
				{Send: `{"type":"response.create","input":[` + user("call a tool") + `]}`, Read: "completed",
					Upstream: []rsfixWSReply{{Events: []string{created("r2"), toolCall, completed("r2")}}}},
				{Send: `{"type":"response.append","input":[{"type":"function_call_output","call_id":"call_1","output":"42"}]}`, Read: "completed",
					Upstream: []rsfixWSReply{{Events: []string{created("r3"), message("done"), completed("r3")}}}},
			},
		},
		{
			Name: "previous_not_found_then_create", Config: cfg, Creds: []rsfixWSCred{httpCred},
			Steps: []rsfixWSStep{
				{Send: `{"type":"response.create","model":"gpt-fixture","previous_response_id":"r0","input":[]}`, Read: "one"},
				{Send: `{"type":"response.bogus"}`, Read: "one"},
				{Send: `{"type":"response.create","model":"gpt-fixture","input":"text"}`, Read: "one"},
				{Send: `{"type":"response.create","model":"gpt-fixture","input":[` + user("ok") + `]}`, Read: "completed",
					Upstream: []rsfixWSReply{{Events: []string{created("r1"), message("ok"), completed("r1")}}}},
			},
		},
		{
			Name: "prewarm_then_followup", Config: cfg, Creds: []rsfixWSCred{httpCred},
			Steps: []rsfixWSStep{
				{Send: `{"type":"response.create","model":"gpt-fixture","generate":false,"instructions":"warm","input":[` + user("setup") + `]}`, Read: "completed"},
				{Send: `{"type":"response.create","previous_response_id":"{{last_response_id}}","input":[` + user("go") + `]}`, Read: "completed",
					Upstream: []rsfixWSReply{{Events: []string{created("r1"), message("went"), completed("r1")}}}},
			},
		},
		{
			Name: "ws_request_fault_exposed", Config: cfg, Creds: []rsfixWSCred{ws},
			Steps: []rsfixWSStep{
				{Send: `{"type":"response.create","model":"gpt-fixture","input":[` + user("huge") + `]}`, Read: "close",
					Upstream: []rsfixWSReply{{Events: []string{`{"type":"error","status":400,"error":{"type":"invalid_request_error","code":"context_length_exceeded","message":"too long"}}`}}}},
			},
		},
		{
			Name: "ws_quota_silent_close", Config: cfg, Creds: []rsfixWSCred{ws},
			Steps: []rsfixWSStep{
				{Send: `{"type":"response.create","model":"gpt-fixture","input":[` + user("hi") + `]}`, Read: "close",
					Upstream: []rsfixWSReply{{Events: []string{`{"type":"error","status":429,"error":{"type":"usage_limit_reached","message":"limit","resets_in_seconds":60}}`}}}},
			},
		},
		{
			Name: "ws_continuation_needs_replay", Config: cfg,
			Creds: []rsfixWSCred{
				{ID: "codex-ws.json", Attrs: map[string]string{"api_key": "sk-fake-ws", "websockets": "true"}, Models: []string{"gpt-fixture"}},
				{ID: "codex-http.json", Attrs: map[string]string{"api_key": "sk-fake-http"}, Models: []string{"gpt-other"}},
			},
			Steps: []rsfixWSStep{
				{Send: `{"type":"response.create","model":"gpt-fixture","input":[` + user("hi") + `]}`, Read: "completed",
					Upstream: []rsfixWSReply{{Events: []string{created("r1"), message("hello"), completed("r1")}}}},
				{Send: `{"type":"response.create","model":"gpt-other","previous_response_id":"{{last_response_id}}","input":[` + user("switch") + `]}`, Read: "close"},
			},
		},
		{
			Name: "ws_upstream_closes_between_turns", Config: cfg, Creds: []rsfixWSCred{ws},
			Steps: []rsfixWSStep{
				{Send: `{"type":"response.create","model":"gpt-fixture","input":[` + user("hi") + `]}`, Read: "completed",
					Upstream: []rsfixWSReply{{Events: []string{created("r1"), message("hello"), completed("r1")}, Then: "close"}}},
				{Read: "close"},
			},
		},
		{
			Name: "ws_upstream_message_too_big", Config: cfg, Creds: []rsfixWSCred{ws},
			Steps: []rsfixWSStep{
				{Send: `{"type":"response.create","model":"gpt-fixture","input":[` + user("hi") + `]}`, Read: "close",
					Upstream: []rsfixWSReply{{Events: []string{created("r1")}, Then: "close1009"}}},
			},
		},
		{
			Name: "http_stream_ends_early", Config: cfg, Creds: []rsfixWSCred{httpCred},
			Steps: []rsfixWSStep{
				{Send: `{"type":"response.create","model":"gpt-fixture","input":[` + user("hi") + `]}`, Read: "close",
					Upstream: []rsfixWSReply{{Events: []string{created("r1"), message("partial")}}}},
			},
		},
		{
			Name: "http_upstream_400_exposed", Config: cfg, Creds: []rsfixWSCred{httpCred},
			Steps: []rsfixWSStep{
				{Send: `{"type":"response.create","model":"gpt-fixture","input":[` + user("hi") + `]}`, Read: "close",
					Upstream: []rsfixWSReply{{Status: 400, Body: `{"error":{"message":"bad input","type":"invalid_request_error","code":"invalid_value"}}`}}},
			},
		},
	}
}

func rsfixRunWSScenario(t *testing.T, sc *rsfixWSScenario) {
	upstream := &rsfixWSUpstream{}
	for _, step := range sc.Steps {
		upstream.script = append(upstream.script, step.Upstream...)
	}
	server := httptest.NewServer(upstream)
	defer server.Close()

	cfg, err := config.ParseConfigBytes([]byte(sc.Config))
	if err != nil {
		t.Fatal(err)
	}
	manager := coreauth.NewManager(nil, nil, nil)
	manager.SetConfig(cfg)
	manager.RegisterExecutor(runtimeexecutor.NewCodexAutoExecutor(cfg))
	for _, cred := range sc.Creds {
		attrs := map[string]string{"base_url": server.URL}
		for k, v := range cred.Attrs {
			attrs[k] = v
		}
		if _, err := manager.Register(context.Background(), &coreauth.Auth{ID: cred.ID, Provider: "codex", Status: coreauth.StatusActive, Attributes: attrs}); err != nil {
			t.Fatal(err)
		}
		var models []*registry.ModelInfo
		for _, m := range cred.Models {
			models = append(models, &registry.ModelInfo{ID: m})
		}
		registry.GetGlobalRegistry().RegisterClient(cred.ID, "codex", models)
		defer registry.GetGlobalRegistry().UnregisterClient(cred.ID)
	}
	h := NewOpenAIResponsesAPIHandler(handlers.NewBaseAPIHandlers(&cfg.SDKConfig, manager))
	router := gin.New()
	router.GET("/v1/responses", h.ResponsesWebsocket)
	downstream := httptest.NewServer(router)
	defer downstream.Close()

	header := http.Header{}
	for k, v := range sc.ClientHeaders {
		header.Set(k, v)
	}
	conn, resp, err := websocket.DefaultDialer.Dial("ws"+strings.TrimPrefix(downstream.URL, "http")+"/v1/responses", header)
	if err != nil {
		t.Fatal(err)
	}
	defer func() { _ = conn.Close() }()
	sc.UpgradeTurnState = resp.Header.Get("X-Codex-Turn-State")

	lastResponseID := ""
	closed := false
	for i := range sc.Steps {
		step := &sc.Steps[i]
		step.Frames = []rsfixWSFrame{}
		if closed {
			break
		}
		if step.Send != "" {
			send := strings.ReplaceAll(step.Send, "{{last_response_id}}", lastResponseID)
			if err := conn.WriteMessage(websocket.TextMessage, []byte(send)); err != nil {
				t.Fatalf("%s: write: %v", sc.Name, err)
			}
		}
		for {
			_ = conn.SetReadDeadline(time.Now().Add(5 * time.Second))
			_, msg, err := conn.ReadMessage()
			if err != nil {
				var netErr interface{ Timeout() bool }
				if errors.As(err, &netErr) && netErr.Timeout() {
					t.Fatalf("%s step %d: read timed out", sc.Name, i)
				}
				frame := rsfixWSFrame{Close: 1006}
				var closeErr *websocket.CloseError
				if errors.As(err, &closeErr) {
					frame = rsfixWSFrame{Close: closeErr.Code, Reason: closeErr.Text}
				}
				step.Frames = append(step.Frames, frame)
				closed = true
				break
			}
			step.Frames = append(step.Frames, rsfixWSFrame{Text: string(msg)})
			kind := gjson.GetBytes(msg, "type").String()
			if kind == "response.completed" || kind == "response.done" {
				lastResponseID = gjson.GetBytes(msg, "response.id").String()
			}
			if step.Read == "one" {
				break
			}
			if step.Read == "completed" && (kind == "response.completed" || kind == "response.done" || kind == "response.incomplete") {
				break
			}
		}
	}
	// Let the server side settle before reading what upstream saw.
	time.Sleep(200 * time.Millisecond)
	upstream.mu.Lock()
	sc.Upstream = append([]rsfixWSCaptured(nil), upstream.captured...)
	upstream.mu.Unlock()
}

func TestRSFixWSE2E(t *testing.T) {
	dir := rsfixWSOut(t)
	gin.SetMode(gin.TestMode)
	scenarios := rsfixWSScenarios()
	for i := range scenarios {
		rsfixRunWSScenario(t, &scenarios[i])
	}
	rsfixWSWrite(t, dir, "ws_e2e.json", scenarios)
}
