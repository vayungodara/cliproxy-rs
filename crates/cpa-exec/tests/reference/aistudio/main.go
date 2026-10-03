// Generates goldens for the AI Studio executor and the /v1/ws relay by running the
// pinned Go AIStudioExecutor against a real wsrelay.Manager, with a scripted browser
// connected over a local websocket. Nothing contacts Google; no credential exists.
package main

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"os"
	"strings"
	"time"

	"github.com/gorilla/websocket"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/runtime/executor"
	// Production registers every translator through this package (cmd/server/main.go).
	_ "github.com/router-for-me/CLIProxyAPI/v8/internal/translator"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/wsrelay"
	cliproxyauth "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/auth"
	cliproxyexecutor "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/executor"
	coreusage "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/usage"
	sdktranslator "github.com/router-for-me/CLIProxyAPI/v8/sdk/translator"
)

// reply is one message the browser sends back for a request; its id is the request's.
type reply struct {
	Type    string         `json:"type"`
	Payload map[string]any `json:"payload,omitempty"`
}

type errOut struct {
	Status  int    `json:"status"`
	Message string `json:"message"`
}

type usageOut struct {
	ReasoningEffort string `json:"reasoning_effort"`
	ResponseModel   string `json:"response_model"`
	InputTokens     int64  `json:"input_tokens"`
	OutputTokens    int64  `json:"output_tokens"`
	TotalTokens     int64  `json:"total_tokens"`
}

type scenario struct {
	Name       string            `json:"name"`
	Model      string            `json:"model"`
	Payload    string            `json:"payload"`
	Source     string            `json:"source"`
	Op         string            `json:"op"`
	Alt        string            `json:"alt,omitempty"`
	Headers    map[string]string `json:"headers,omitempty"`
	Attributes map[string]string `json:"attributes,omitempty"`
	// Config is YAML for payload rules.
	Config string `json:"config,omitempty"`
	// Disconnected sends the request for a provider with no session.
	Disconnected bool    `json:"disconnected,omitempty"`
	Replies      []reply `json:"replies,omitempty"`

	// Frames are the relay messages the browser received, with the message id as ID
	// and sent_at as SENT_AT.
	Frames []string  `json:"frames,omitempty"`
	Output string    `json:"output,omitempty"`
	Chunks []string  `json:"chunks,omitempty"`
	Error  *errOut   `json:"error,omitempty"`
	Usage  *usageOut `json:"usage,omitempty"`
}

type usageCapture chan coreusage.Record

func (c usageCapture) HandleUsage(_ context.Context, record coreusage.Record) { c <- record }

var captured = make(usageCapture, 16)

func statusOf(err error) *errOut {
	out := &errOut{Message: err.Error()}
	if s, ok := err.(interface{ StatusCode() int }); ok {
		out.Status = s.StatusCode()
	}
	return out
}

// browser is the scripted AI Studio page: it records each request frame and answers
// with the current scenario's replies.
type browser struct {
	conn    *websocket.Conn
	replies chan []reply
	frames  chan string
}

func (b *browser) run() {
	for {
		_, data, err := b.conn.ReadMessage()
		if err != nil {
			return
		}
		var msg wsrelay.Message
		if err := json.Unmarshal(data, &msg); err != nil {
			panic(err)
		}
		if msg.Type != wsrelay.MessageTypeHTTPReq {
			continue
		}
		text := strings.Replace(string(data), msg.ID, "ID", 1)
		if sent, ok := msg.Payload["sent_at"].(string); ok {
			text = strings.Replace(text, sent, "SENT_AT", 1)
		}
		b.frames <- text
		for _, r := range <-b.replies {
			if err := b.conn.WriteJSON(wsrelay.Message{ID: msg.ID, Type: r.Type, Payload: r.Payload}); err != nil {
				panic(err)
			}
		}
	}
}

func run(s *scenario, manager *wsrelay.Manager, provider string, b *browser) {
	cfg := &config.Config{}
	if s.Config != "" {
		var err error
		cfg, err = config.ParseConfigBytes([]byte(s.Config))
		if err != nil {
			panic(err)
		}
	}
	id := provider
	if s.Disconnected {
		id = "aistudio-missing"
	}
	exec := executor.NewAIStudioExecutor(cfg, id, manager)
	auth := &cliproxyauth.Auth{ID: id, Provider: "aistudio", Attributes: map[string]string{"runtime_only": "true"}, Metadata: map[string]any{"email": id}}
	for k, v := range s.Attributes {
		auth.Attributes[k] = v
	}
	headers := http.Header{}
	for k, v := range s.Headers {
		headers.Set(k, v)
	}
	req := cliproxyexecutor.Request{Model: s.Model, Payload: []byte(s.Payload), Metadata: map[string]any{}}
	opts := cliproxyexecutor.Options{
		Stream:       s.Op == "stream",
		Alt:          s.Alt,
		Headers:      headers,
		SourceFormat: sdktranslator.FromString(s.Source),
		Metadata:     map[string]any{},
	}
	ctx := coreusage.WithTraceID(context.Background(), s.Name)
	go func() { b.replies <- s.Replies }()
	switch s.Op {
	case "execute":
		resp, err := exec.Execute(ctx, auth, req, opts)
		if err != nil {
			s.Error = statusOf(err)
		} else {
			s.Output = string(resp.Payload)
		}
	case "stream":
		result, err := exec.ExecuteStream(ctx, auth, req, opts)
		if err != nil {
			s.Error = statusOf(err)
			break
		}
		for chunk := range result.Chunks {
			if chunk.Err != nil {
				s.Error = statusOf(chunk.Err)
				continue
			}
			s.Chunks = append(s.Chunks, string(chunk.Payload))
		}
	case "count":
		resp, err := exec.CountTokens(ctx, auth, req, opts)
		if err != nil {
			s.Error = statusOf(err)
		} else {
			s.Output = string(resp.Payload)
		}
	default:
		panic(s.Op)
	}
	select {
	case frame := <-b.frames:
		s.Frames = append(s.Frames, frame)
	case <-time.After(200 * time.Millisecond):
		// The request never reached the browser; drop the unused replies.
		<-b.replies
	}
	select {
	case r := <-captured:
		if r.TraceID != s.Name {
			panic(fmt.Sprintf("%s: usage record of %q", s.Name, r.TraceID))
		}
		s.Usage = &usageOut{ReasoningEffort: r.ReasoningEffort, ResponseModel: r.ResponseModel, InputTokens: r.Detail.InputTokens, OutputTokens: r.Detail.OutputTokens, TotalTokens: r.Detail.TotalTokens}
	case <-time.After(300 * time.Millisecond):
	}
}

// decoded is what gorilla's ReadJSON (json.NewDecoder(r).Decode) makes of one frame.
type decoded struct {
	Frame   string `json:"frame"`
	Error   bool   `json:"error"`
	ID      string `json:"id,omitempty"`
	Type    string `json:"type,omitempty"`
	Payload string `json:"payload,omitempty"`
}

func decodeVectors() []decoded {
	frames := []string{
		`{"id":"a","type":"stream_chunk","payload":{"data":"x"}}`,
		`  {"ID":"b","TYPE":"ping"} trailing garbage`,
		`{"id":"c","type":"x","id":"d","Id":null}`,
		`{"id":"e","payload":{"a":1},"PAYLOAD":{"b":2.50},"payload":null}`,
		`{"payload":{"status":1e400}}`,
		`{"id":5}`,
		`{"type":["x"]}`,
		`{"payload":"text"}`,
		`{"payload":[]}`,
		`null`,
		`nullx`,
		`[1]`,
		`"s"`,
		``,
		`   `,
		`{"id":"f"`,
		`{"id":"\u00e9\ud83d\ude00"}`,
		`{"ſd":"long-s","ıd":"dotless"}`,
		"{\"id\":\"g\xff\"}",
		`{"id":"h"}{"id":"i"}`,
		`{"a":{"b":[1,{"c":"}"}]},"id":"j"}`,
	}
	var out []decoded
	for _, f := range frames {
		var msg wsrelay.Message
		err := json.NewDecoder(bytes.NewReader([]byte(f))).Decode(&msg)
		d := decoded{Frame: f, Error: err != nil}
		if err == nil {
			d.ID, d.Type = msg.ID, msg.Type
			if msg.Payload != nil {
				raw, _ := json.Marshal(msg.Payload)
				d.Payload = string(raw)
			}
		}
		out = append(out, d)
	}
	return out
}

// encoded is gorilla's WriteJSON (json.NewEncoder(w).Encode) of one message.
type encoded struct {
	ID      string         `json:"id"`
	Type    string         `json:"type"`
	Payload map[string]any `json:"payload,omitempty"`
	Text    string         `json:"text"`
}

func encodeVectors() []encoded {
	cases := []encoded{
		{ID: "p1", Type: "pong"},
		{ID: "p2", Type: "pong", Payload: map[string]any{}},
		{ID: "<&>", Type: "http_request", Payload: map[string]any{"body": "a<b>&\u2028", "headers": map[string]any{"X-B": []any{"2"}, "Content-Type": []any{"application/json"}}, "method": "POST"}},
	}
	for i := range cases {
		var buf bytes.Buffer
		if err := json.NewEncoder(&buf).Encode(wsrelay.Message{ID: cases[i].ID, Type: cases[i].Type, Payload: cases[i].Payload}); err != nil {
			panic(err)
		}
		cases[i].Text = buf.String()
	}
	return cases
}

func main() {
	if len(os.Args) != 2 {
		fmt.Fprintln(os.Stderr, "usage: generator OUTPUT.json")
		os.Exit(2)
	}
	coreusage.RegisterPlugin(captured)
	connected := make(chan string, 1)
	manager := wsrelay.NewManager(wsrelay.Options{Path: "/v1/ws", OnConnected: func(id string) { connected <- id }})
	server := httptest.NewServer(manager.Handler())
	defer server.Close()
	conn, _, err := websocket.DefaultDialer.Dial("ws"+strings.TrimPrefix(server.URL, "http")+"/v1/ws", nil)
	if err != nil {
		panic(err)
	}
	provider := <-connected
	b := &browser{conn: conn, replies: make(chan []reply), frames: make(chan string, 1)}
	go b.run()
	all := scenarios()
	for i := range all {
		run(&all[i], manager, provider, b)
	}
	data, err := json.MarshalIndent(map[string]any{"scenarios": all, "decode": decodeVectors(), "encode": encodeVectors()}, "", "  ")
	if err != nil {
		panic(err)
	}
	if err := os.WriteFile(os.Args[1], append(data, '\n'), 0o644); err != nil {
		panic(err)
	}
}
