// Generates goldens for the xAI upstream Responses WebSocket by running the pinned Go
// XAIAutoExecutor (as production registers it) on downstream-WebSocket turns against a
// scripted local upstream: a gorilla WebSocket endpoint for /v1/responses and plain HTTP
// for /v1/responses/compact. It never contacts a provider; keys are fake.
package main

import (
	"bufio"
	"context"
	"crypto/sha256"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"sort"
	"strings"
	"sync"
	"time"

	"github.com/gorilla/websocket"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/runtime/executor"
	// Production registers every translator through this package (cmd/server/main.go).
	_ "github.com/router-for-me/CLIProxyAPI/v8/internal/translator"
	cliproxyauth "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/auth"
	cliproxyexecutor "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/executor"
	"github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/usage"
	sdktranslator "github.com/router-for-me/CLIProxyAPI/v8/sdk/translator"
	"github.com/tidwall/gjson"
)

// act is what the upstream does after receiving one frame: optionally ping and wait for
// the pong, send frames (a quoted "PREVIOUS_ID" becomes the received frame's
// previous_response_id), then a binary frame, a close frame, or a drop without one.
type act struct {
	Ping      string   `json:"ping,omitempty"`
	Send      []string `json:"send,omitempty"`
	Binary    bool     `json:"binary,omitempty"`
	Close     int      `json:"close,omitempty"`
	CloseText string   `json:"close_text,omitempty"`
	Drop      bool     `json:"drop,omitempty"`
}

// usageOut is the record Go's usage reporter published for the turn.
type usageOut struct {
	Input         int64  `json:"input"`
	Output        int64  `json:"output"`
	Reasoning     int64  `json:"reasoning"`
	Cached        int64  `json:"cached"`
	Total         int64  `json:"total"`
	Effort        string `json:"effort"`
	ResponseModel string `json:"response_model"`
	Failed        bool   `json:"failed"`
}

// records holds the published usage records by trace ID. Each turn runs under its own
// trace ID, so a record the usage dispatcher delivers late still belongs to its turn.
var records = struct {
	sync.Mutex
	byTrace map[string]usage.Record
}{byTrace: map[string]usage.Record{}}

type capturePlugin struct{}

func (capturePlugin) HandleUsage(_ context.Context, r usage.Record) {
	records.Lock()
	defer records.Unlock()
	if _, seen := records.byTrace[r.TraceID]; seen {
		panic("second usage record for turn " + r.TraceID)
	}
	records.byTrace[r.TraceID] = r
}

// usageFor waits up to wait for the turn's record.
func usageFor(traceID string, wait time.Duration) *usageOut {
	deadline := time.Now().Add(wait)
	for {
		records.Lock()
		r, ok := records.byTrace[traceID]
		records.Unlock()
		if ok {
			return &usageOut{Input: r.Detail.InputTokens, Output: r.Detail.OutputTokens, Reasoning: r.Detail.ReasoningTokens,
				Cached: r.Detail.CachedTokens, Total: r.Detail.TotalTokens, Effort: r.ReasoningEffort, ResponseModel: r.ResponseModel, Failed: r.Failed}
		}
		if time.Now().After(deadline) {
			return nil
		}
		time.Sleep(5 * time.Millisecond)
	}
}

type reply struct {
	Status int    `json:"status"`
	Body   string `json:"body"`
}

type errOut struct {
	Status       int    `json:"status"`
	Message      string `json:"message"`
	RetryAfterMS int64  `json:"retry_after_ms"`
}

type turn struct {
	// Close ends the downstream session instead of sending a request.
	Close   bool   `json:"close,omitempty"`
	Auth    string `json:"auth,omitempty"`
	Payload string `json:"payload,omitempty"`
	Model   string `json:"model,omitempty"`
	// Response is opts.ResponseFormat (default openai-response).
	Response     string            `json:"response,omitempty"`
	Headers      map[string]string `json:"headers,omitempty"`
	Continuation bool              `json:"continuation,omitempty"`
	Acts         []act             `json:"acts,omitempty"`
	Reject       *reply            `json:"reject,omitempty"`
	Compact      *reply            `json:"compact,omitempty"`

	Chunks   []string  `json:"chunks"`
	Error    *errOut   `json:"error,omitempty"`
	Upgrades []string  `json:"upgrades"`
	Frames   []string  `json:"frames"`
	HTTP     []string  `json:"http"`
	Pongs    []string  `json:"pongs"`
	Usage    *usageOut `json:"usage,omitempty"`
}

type scenario struct {
	Name    string `json:"name"`
	Session string `json:"session"`
	// Auths maps an auth ID to its attributes; base_url UPSTREAM is the local upstream
	// and an access_token entry is auth metadata instead.
	Auths map[string]map[string]string `json:"auths"`
	Turns []*turn                      `json:"turns"`
}

// upstream is the scripted server; the driver points it at the turn in progress.
type upstream struct {
	mu   sync.Mutex
	turn *turn
	addr string
}

func (u *upstream) current() *turn {
	u.mu.Lock()
	defer u.mu.Unlock()
	return u.turn
}

func (u *upstream) record(f func(t *turn)) {
	u.mu.Lock()
	defer u.mu.Unlock()
	if u.turn != nil {
		f(u.turn)
	}
}

// upgradeHeaders lists the handshake headers except the random key, sorted.
func upgradeHeaders(r *http.Request) string {
	var lines []string
	for name, values := range r.Header {
		if name == "Sec-Websocket-Key" {
			continue
		}
		lines = append(lines, name+": "+strings.Join(values, ", "))
	}
	sort.Strings(lines)
	return r.URL.RequestURI() + "\n" + strings.Join(lines, "\n")
}

func (u *upstream) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	t := u.current()
	if websocket.IsWebSocketUpgrade(r) {
		u.record(func(t *turn) { t.Upgrades = append(t.Upgrades, upgradeHeaders(r)) })
		if t != nil && t.Reject != nil {
			w.Header().Set("Content-Type", "application/json")
			w.WriteHeader(t.Reject.Status)
			_, _ = io.WriteString(w, t.Reject.Body)
			return
		}
		upgrader := websocket.Upgrader{CheckOrigin: func(*http.Request) bool { return true }}
		conn, err := upgrader.Upgrade(w, r, nil)
		if err != nil {
			return
		}
		go u.serveSocket(conn)
		return
	}
	body, _ := io.ReadAll(r.Body)
	u.record(func(t *turn) {
		t.HTTP = append(t.HTTP, r.Method+" "+r.URL.RequestURI()+"\n"+string(body))
	})
	w.Header().Set("Content-Type", "application/json")
	if t == nil || t.Compact == nil {
		w.WriteHeader(599)
		return
	}
	w.WriteHeader(t.Compact.Status)
	_, _ = io.WriteString(w, t.Compact.Body)
}

func (u *upstream) serveSocket(conn *websocket.Conn) {
	defer conn.Close()
	pongs := make(chan string, 4)
	conn.SetPongHandler(func(data string) error {
		pongs <- data
		return nil
	})
	frames := make(chan []byte)
	go func() {
		defer close(frames)
		for {
			_, data, err := conn.ReadMessage()
			if err != nil {
				return
			}
			frames <- data
		}
	}()
	for data := range frames {
		var next *act
		u.record(func(t *turn) {
			t.Frames = append(t.Frames, string(data))
			if len(t.Acts) >= len(t.Frames) {
				next = &t.Acts[len(t.Frames)-1]
			}
		})
		if next == nil {
			continue
		}
		if next.Ping != "" {
			_ = conn.WriteControl(websocket.PingMessage, []byte(next.Ping), time.Now().Add(time.Second))
			select {
			case got := <-pongs:
				u.record(func(t *turn) { t.Pongs = append(t.Pongs, got) })
			case <-time.After(2 * time.Second):
			}
		}
		previous, _ := json.Marshal(gjson.GetBytes(data, "previous_response_id").String())
		for _, frame := range next.Send {
			frame = strings.ReplaceAll(frame, `"PREVIOUS_ID"`, string(previous))
			if err := conn.WriteMessage(websocket.TextMessage, []byte(frame)); err != nil {
				return
			}
		}
		if next.Drop {
			_ = conn.UnderlyingConn().Close()
			return
		}
		if next.Binary {
			_ = conn.WriteMessage(websocket.BinaryMessage, []byte{1, 2, 3})
		}
		if next.Close != 0 {
			msg := websocket.FormatCloseMessage(next.Close, next.CloseText)
			_ = conn.WriteControl(websocket.CloseMessage, msg, time.Now().Add(time.Second))
			return
		}
	}
}

func statusOf(err error) *errOut {
	out := &errOut{Status: 500, Message: err.Error()}
	var sc interface{ StatusCode() int }
	if errors.As(err, &sc) && sc.StatusCode() > 0 {
		out.Status = sc.StatusCode()
	}
	var ra interface{ RetryAfter() *time.Duration }
	if errors.As(err, &ra) {
		if d := ra.RetryAfter(); d != nil {
			out.RetryAfterMS = d.Milliseconds()
		}
	}
	return out
}

func run(s *scenario) {
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		panic(err)
	}
	up := &upstream{addr: ln.Addr().String()}
	server := &http.Server{Handler: up}
	go func() { _ = server.Serve(ln) }()
	defer server.Close()

	cfg := &config.Config{}
	exec := executor.NewXAIAutoExecutor(cfg)
	auths := map[string]*cliproxyauth.Auth{}
	for id, attrs := range s.Auths {
		copied := map[string]string{}
		metadata := map[string]any{}
		for k, v := range attrs {
			if k == "access_token" {
				metadata[k] = v
				continue
			}
			copied[k] = strings.ReplaceAll(v, "UPSTREAM", up.addr)
		}
		auths[id] = &cliproxyauth.Auth{ID: id, Provider: "xai", Attributes: copied, Metadata: metadata}
	}
	normalize := func(text string) string { return strings.ReplaceAll(text, up.addr, "UPSTREAM") }

	traceID := func(i int) string { return fmt.Sprintf("%s#%d", s.Name, i) }
	for i, t := range s.Turns {
		up.mu.Lock()
		up.turn = t
		up.mu.Unlock()
		if t.Close {
			exec.CloseExecutionSession(s.Session)
			time.Sleep(50 * time.Millisecond)
		} else {
			runTurn(exec, auths[t.Auth], s.Session, traceID(i), t)
			t.Usage = usageFor(traceID(i), 300*time.Millisecond)
		}
		up.mu.Lock()
		up.turn = nil
		up.mu.Unlock()
		for i := range t.Upgrades {
			t.Upgrades[i] = normalize(t.Upgrades[i])
		}
		for i := range t.Frames {
			t.Frames[i] = normalize(t.Frames[i])
		}
		for i := range t.HTTP {
			t.HTTP[i] = normalize(t.HTTP[i])
		}
		for i := range t.Chunks {
			t.Chunks[i] = normalize(t.Chunks[i])
		}
		if t.Error != nil {
			t.Error.Message = normalize(t.Error.Message)
		}
		if t.Chunks == nil {
			t.Chunks = []string{}
		}
		if t.Upgrades == nil {
			t.Upgrades = []string{}
		}
		if t.Frames == nil {
			t.Frames = []string{}
		}
		if t.HTTP == nil {
			t.HTTP = []string{}
		}
		if t.Pongs == nil {
			t.Pongs = []string{}
		}
	}
	exec.CloseExecutionSession(s.Session)
	// A record that arrived after its turn's wait still belongs to that turn.
	for i, t := range s.Turns {
		if !t.Close && t.Usage == nil {
			t.Usage = usageFor(traceID(i), 0)
		}
	}
}

func runTurn(exec *executor.XAIAutoExecutor, auth *cliproxyauth.Auth, session, traceID string, t *turn) {
	model := t.Model
	if model == "" {
		model = "grok-4.3"
	}
	headers := http.Header{}
	for k, v := range t.Headers {
		headers.Set(k, v)
	}
	ctx := usage.WithTraceID(cliproxyexecutor.WithDownstreamWebsocket(context.Background()), traceID)
	if t.Continuation {
		ctx = cliproxyexecutor.WithRequiredUpstreamWebsocket(ctx)
	}
	req := cliproxyexecutor.Request{Model: model, Payload: []byte(t.Payload), Metadata: map[string]any{}}
	response := t.Response
	if response == "" {
		response = "openai-response"
	}
	opts := cliproxyexecutor.Options{
		Stream:          true,
		Headers:         headers,
		SourceFormat:    sdktranslator.FromString("openai-response"),
		ResponseFormat:  sdktranslator.FromString(response),
		OriginalRequest: []byte(t.Payload),
		Metadata:        map[string]any{cliproxyexecutor.ExecutionSessionMetadataKey: session},
	}
	ctx, cancel := context.WithTimeout(ctx, 10*time.Second)
	defer cancel()
	result, err := exec.ExecuteStream(ctx, auth, req, opts)
	if err != nil {
		t.Error = statusOf(err)
		return
	}
	for chunk := range result.Chunks {
		if chunk.Err != nil {
			t.Error = statusOf(chunk.Err)
			continue
		}
		t.Chunks = append(t.Chunks, string(chunk.Payload))
	}
}

// grokBlob is a structurally valid Grok encrypted-content value (as in ../xai).
func grokBlob(seed byte) string {
	buf := make([]byte, 0, 256)
	for i := 0; len(buf) < 256; i++ {
		sum := sha256.Sum256([]byte{byte(i), byte(i >> 8), seed, 99})
		buf = append(buf, sum[:]...)
	}
	return base64.RawStdEncoding.EncodeToString(buf[:256])
}

func main() {
	if len(os.Args) != 2 {
		fmt.Fprintln(os.Stderr, "usage: generator OUTPUT.json")
		os.Exit(2)
	}
	usage.RegisterPlugin(capturePlugin{})
	blobs := strings.NewReplacer("GROKENC1", grokBlob(1), "GROKENC2", grokBlob(2))
	all := scenarios()
	// ONLY=<scenario name> regenerates one scenario (for inspection; the fixture needs all).
	if only := os.Getenv("ONLY"); only != "" {
		var kept []*scenario
		for _, s := range all {
			if s.Name == only {
				kept = append(kept, s)
			}
		}
		all = kept
	}
	for _, s := range all {
		for _, t := range s.Turns {
			t.Payload = blobs.Replace(t.Payload)
			if t.Compact != nil {
				t.Compact.Body = blobs.Replace(t.Compact.Body)
			}
			for i := range t.Acts {
				for j := range t.Acts[i].Send {
					t.Acts[i].Send[j] = blobs.Replace(t.Acts[i].Send[j])
				}
			}
		}
		run(s)
	}
	out, err := os.Create(os.Args[1])
	if err != nil {
		panic(err)
	}
	defer out.Close()
	w := bufio.NewWriter(out)
	enc := json.NewEncoder(w)
	enc.SetEscapeHTML(false)
	enc.SetIndent("", "  ")
	if err := enc.Encode(all); err != nil {
		panic(err)
	}
	if err := w.Flush(); err != nil {
		panic(err)
	}
}
