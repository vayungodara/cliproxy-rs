// Claude Messages soak test for docs/BENCHMARKS.md. Standard library only.
//
//	messages upstream -addr 127.0.0.1:9202
//	messages load -url http://127.0.0.1:8340/v1/messages -n 3000 -c 8
//	messages load -url http://127.0.0.1:8340/v1/messages -n 300 -c 4 \
//	    -codex 2 -codex-model gpt-5.5 -count 4 -min 100000 -max 2000000 -step 50000
//
// upstream is a fake Anthropic Messages endpoint. POST /v1/messages reads the whole
// request and streams a Claude SSE reply: message_start, a thinking block with a
// signature, a text block, a tool_use block naming the request's first tool, then
// message_delta and message_stop. -events sets the number of delta events and -delay
// the pause between them. It is also a fake Codex upstream: POST /responses reads the
// whole request and streams an OpenAI Responses reply with the same number of events (a
// reasoning item with a summary and encrypted content, a message, a function call naming
// the request's first tool) ending in response.completed with the full output and usage.
//
// load models a coding agent. Each of -c workers is one session whose conversation
// grows turn by turn from -min to -max bytes (assistant turns with signed thinking and
// tool_use, user turns with large tool_result text), then starts a new session. Every
// request is streamed and counts as ok only with status 200, a message_stop event and a
// tool_use block whose input_json_delta fragments join into valid JSON. It prints one
// JSON line with throughput and latency percentiles: ttfb is the time to the first
// response byte, total the time to the end of the stream.
//
// -codex N makes the last N workers Codex sessions instead: Responses requests to the
// /v1/responses route next to -url (reasoning items with encrypted content, function calls
// and large function_call_output text), counted ok with status 200, a response.completed
// event and a function call whose argument deltas join into exactly the arguments of its
// done event and of response.completed, and parse as JSON. With -count K, every Kth turn
// of a Claude session first sends the same body to /v1/messages/count_tokens, alternating
// between the Claude model and -codex-model (a Claude client counting tokens for a Codex
// model), counted ok with status 200 and input_tokens in the body. The JSON line then
// also breaks the counts down by kind.
package main

import (
	"bufio"
	"bytes"
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"log"
	"math/rand"
	"net/http"
	"os"
	"sort"
	"strings"
	"sync"
	"sync/atomic"
	"time"
)

func main() {
	if len(os.Args) < 2 {
		fmt.Fprintln(os.Stderr, "usage: messages upstream|load [flags]")
		os.Exit(2)
	}
	switch os.Args[1] {
	case "upstream":
		upstream(os.Args[2:])
	case "load":
		load(os.Args[2:])
	default:
		fmt.Fprintln(os.Stderr, "usage: messages upstream|load [flags]")
		os.Exit(2)
	}
}

const words = "the function returns early when the buffer is empty so callers must check the length " +
	"before reading and the parser keeps a cursor into the source while the tokenizer emits spans " +
	"for identifiers numbers strings and punctuation which the resolver later binds to declarations "

// text returns n bytes of JSON-safe prose, starting at a pseudo-random offset.
func text(r *rand.Rand, n int) string {
	var b strings.Builder
	b.Grow(n + len(words))
	for b.Len() < n {
		i := r.Intn(len(words) - 40)
		b.WriteString(words[i:])
		fmt.Fprintf(&b, " %d ", r.Int63())
	}
	return b.String()[:n]
}

func upstream(args []string) {
	fs := flag.NewFlagSet("upstream", flag.ExitOnError)
	addr := fs.String("addr", "127.0.0.1:9202", "listen address")
	events := fs.Int("events", 150, "delta events per streamed response")
	delay := fs.Duration("delay", 2*time.Millisecond, "pause between delta events")
	fs.Parse(args)

	r := rand.New(rand.NewSource(1))
	thinking := make([]string, *events)
	for i := range thinking {
		thinking[i] = text(r, 120+r.Intn(200))
	}
	signature := strings.Repeat("EqQBCkgIBRABGAIiQL", 30)

	http.HandleFunc("POST /v1/messages", func(w http.ResponseWriter, req *http.Request) {
		var body struct {
			Model string `json:"model"`
			Tools []struct {
				Name string `json:"name"`
			} `json:"tools"`
		}
		data, err := io.ReadAll(req.Body)
		if err != nil || json.Unmarshal(data, &body) != nil {
			http.Error(w, "bad request", 400)
			return
		}
		tool := "Read"
		if len(body.Tools) > 0 {
			tool = body.Tools[0].Name
		}
		w.Header().Set("Content-Type", "text/event-stream")
		w.Header().Set("Cache-Control", "no-cache")
		w.Header().Set("request-id", "req_bench")
		f := w.(http.Flusher)
		send := func(event, payload string) {
			fmt.Fprintf(w, "event: %s\ndata: %s\n\n", event, payload)
			f.Flush()
		}
		pause := func() {
			if *delay > 0 {
				time.Sleep(*delay)
			}
		}
		id := fmt.Sprintf("msg_bench%d", time.Now().UnixNano())
		send("message_start", `{"type":"message_start","message":{"id":"`+id+`","type":"message","role":"assistant","model":"`+body.Model+`","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":`+fmt.Sprint(len(data)/4)+`,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":1}}}`)
		send("content_block_start", `{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}`)
		third := *events / 3
		for i := 0; i < third; i++ {
			pause()
			b, _ := json.Marshal(thinking[i])
			send("content_block_delta", `{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":`+string(b)+`}}`)
		}
		send("content_block_delta", `{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"`+signature+`"}}`)
		send("content_block_stop", `{"type":"content_block_stop","index":0}`)
		send("content_block_start", `{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}`)
		for i := third; i < 2*third; i++ {
			pause()
			b, _ := json.Marshal(thinking[i])
			send("content_block_delta", `{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":`+string(b)+`}}`)
		}
		send("content_block_stop", `{"type":"content_block_stop","index":1}`)
		send("content_block_start", `{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_bench","name":"`+tool+`","input":{}}}`)
		for i, piece := range argumentDeltas(thinking[2*third:]) {
			if i > 0 {
				pause()
			}
			b, _ := json.Marshal(piece)
			send("content_block_delta", `{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":`+string(b)+`}}`)
		}
		send("content_block_stop", `{"type":"content_block_stop","index":2}`)
		send("message_delta", `{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":`+fmt.Sprint(*events*30)+`}}`)
		send("message_stop", `{"type":"message_stop"}`)
	})

	encrypted := strings.Repeat("gAAAAABo3x9kZ2VuY3J5cHRlZC1yZWFzb25pbmc", 50)
	http.HandleFunc("POST /responses", func(w http.ResponseWriter, req *http.Request) {
		var body struct {
			Model string `json:"model"`
			Tools []struct {
				Name string `json:"name"`
			} `json:"tools"`
		}
		data, err := io.ReadAll(req.Body)
		if err != nil || json.Unmarshal(data, &body) != nil {
			http.Error(w, "bad request", 400)
			return
		}
		tool := "shell"
		if len(body.Tools) > 0 {
			tool = body.Tools[0].Name
		}
		w.Header().Set("Content-Type", "text/event-stream")
		w.Header().Set("Cache-Control", "no-cache")
		w.Header().Set("x-request-id", "req_bench")
		f := w.(http.Flusher)
		seq := 0
		send := func(event string, payload map[string]any) {
			payload["type"] = event
			payload["sequence_number"] = seq
			seq++
			b, _ := json.Marshal(payload)
			fmt.Fprintf(w, "event: %s\ndata: %s\n\n", event, b)
			f.Flush()
		}
		pause := func() {
			if *delay > 0 {
				time.Sleep(*delay)
			}
		}
		now := time.Now()
		id := fmt.Sprintf("resp_bench%d", now.UnixNano())
		response := func(status string, output []any, usage any) map[string]any {
			return map[string]any{"id": id, "object": "response", "created_at": now.Unix(), "status": status,
				"model": body.Model, "output": output, "usage": usage, "parallel_tool_calls": false,
				"reasoning": map[string]any{"effort": "medium", "summary": "auto"}, "store": false}
		}
		send("response.created", map[string]any{"response": response("in_progress", []any{}, nil)})
		send("response.in_progress", map[string]any{"response": response("in_progress", []any{}, nil)})

		third := *events / 3
		var summary strings.Builder
		reasoning := map[string]any{"id": "rs_" + id, "type": "reasoning", "summary": []any{}}
		send("response.output_item.added", map[string]any{"output_index": 0, "item": reasoning})
		part := map[string]any{"type": "summary_text", "text": ""}
		send("response.reasoning_summary_part.added", map[string]any{"item_id": "rs_" + id, "output_index": 0, "summary_index": 0, "part": part})
		for i := 0; i < third; i++ {
			pause()
			summary.WriteString(thinking[i])
			send("response.reasoning_summary_text.delta", map[string]any{"item_id": "rs_" + id, "output_index": 0, "summary_index": 0, "delta": thinking[i]})
		}
		part["text"] = summary.String()
		send("response.reasoning_summary_text.done", map[string]any{"item_id": "rs_" + id, "output_index": 0, "summary_index": 0, "text": summary.String()})
		send("response.reasoning_summary_part.done", map[string]any{"item_id": "rs_" + id, "output_index": 0, "summary_index": 0, "part": part})
		reasoning["summary"] = []any{part}
		reasoning["encrypted_content"] = encrypted
		send("response.output_item.done", map[string]any{"output_index": 0, "item": reasoning})

		var text strings.Builder
		message := map[string]any{"id": "msg_" + id, "type": "message", "status": "in_progress", "role": "assistant", "content": []any{}}
		send("response.output_item.added", map[string]any{"output_index": 1, "item": message})
		content := map[string]any{"type": "output_text", "text": "", "annotations": []any{}}
		send("response.content_part.added", map[string]any{"item_id": "msg_" + id, "output_index": 1, "content_index": 0, "part": content})
		for i := third; i < 2*third; i++ {
			pause()
			text.WriteString(thinking[i])
			send("response.output_text.delta", map[string]any{"item_id": "msg_" + id, "output_index": 1, "content_index": 0, "delta": thinking[i]})
		}
		content["text"] = text.String()
		send("response.output_text.done", map[string]any{"item_id": "msg_" + id, "output_index": 1, "content_index": 0, "text": text.String()})
		send("response.content_part.done", map[string]any{"item_id": "msg_" + id, "output_index": 1, "content_index": 0, "part": content})
		message["status"], message["content"] = "completed", []any{content}
		send("response.output_item.done", map[string]any{"output_index": 1, "item": message})

		var args strings.Builder
		call := map[string]any{"id": "fc_" + id, "type": "function_call", "status": "in_progress", "name": tool, "call_id": "call_" + id, "arguments": ""}
		send("response.output_item.added", map[string]any{"output_index": 2, "item": call})
		for i, delta := range argumentDeltas(thinking[2*third:]) {
			if i > 0 {
				pause()
			}
			args.WriteString(delta)
			send("response.function_call_arguments.delta", map[string]any{"item_id": "fc_" + id, "output_index": 2, "delta": delta})
		}
		send("response.function_call_arguments.done", map[string]any{"item_id": "fc_" + id, "output_index": 2, "arguments": args.String()})
		call["status"], call["arguments"] = "completed", args.String()
		send("response.output_item.done", map[string]any{"output_index": 2, "item": call})

		input := len(data) / 4
		usage := map[string]any{"input_tokens": input, "input_tokens_details": map[string]any{"cached_tokens": input * 9 / 10},
			"output_tokens": *events * 30, "output_tokens_details": map[string]any{"reasoning_tokens": *events * 10},
			"total_tokens": input + *events*30}
		send("response.completed", map[string]any{"response": response("completed", []any{reasoning, message, call}, usage)})
	})
	log.Printf("fake Claude and Codex upstream on %s (%d events, %s apart)", *addr, *events, *delay)
	log.Fatal(http.ListenAndServe(*addr, nil))
}

// argumentDeltas splits a tool call's arguments, {"path":"..."}, into one fragment per
// text: the first opens the object, the last closes it, so the fragments join into JSON.
func argumentDeltas(texts []string) []string {
	deltas := make([]string, 0, len(texts))
	for _, t := range texts {
		deltas = append(deltas, strings.ReplaceAll(t[:40], " ", "_"))
	}
	if len(deltas) == 0 {
		return []string{`{"path":""}`}
	}
	deltas[0] = `{"path":"` + deltas[0]
	deltas[len(deltas)-1] += `"}`
	return deltas
}

// sseData calls f with the JSON payload of every data line of an SSE body.
func sseData(body []byte, f func([]byte) error) error {
	for _, line := range bytes.Split(body, []byte("\n")) {
		payload, ok := bytes.CutPrefix(bytes.TrimSpace(line), []byte("data:"))
		if !ok {
			continue
		}
		if err := f(bytes.TrimSpace(payload)); err != nil {
			return err
		}
	}
	return nil
}

// checkClaudeStream: message_stop arrived, and every tool_use block's input_json_delta
// fragments join into valid JSON.
func checkClaudeStream(body []byte) error {
	var stopped bool
	inputs := map[int]*strings.Builder{}
	err := sseData(body, func(payload []byte) error {
		var event struct {
			Type         string `json:"type"`
			Index        int    `json:"index"`
			ContentBlock struct {
				Type string `json:"type"`
			} `json:"content_block"`
			Delta struct {
				Type        string `json:"type"`
				PartialJSON string `json:"partial_json"`
			} `json:"delta"`
		}
		if err := json.Unmarshal(payload, &event); err != nil {
			return fmt.Errorf("event is not JSON: %w", err)
		}
		switch {
		case event.Type == "content_block_start" && event.ContentBlock.Type == "tool_use":
			inputs[event.Index] = &strings.Builder{}
		case event.Type == "content_block_delta" && event.Delta.Type == "input_json_delta":
			input := inputs[event.Index]
			if input == nil {
				return fmt.Errorf("input_json_delta for block %d, which is not a tool_use block", event.Index)
			}
			input.WriteString(event.Delta.PartialJSON)
		case event.Type == "message_stop":
			stopped = true
		}
		return nil
	})
	if err != nil {
		return err
	}
	if !stopped {
		return fmt.Errorf("no message_stop")
	}
	if len(inputs) == 0 {
		return fmt.Errorf("no tool_use block")
	}
	for index, input := range inputs {
		if !json.Valid([]byte(input.String())) {
			return fmt.Errorf("tool_use block %d: input_json_delta fragments join into invalid JSON: %.200s", index, input.String())
		}
	}
	return nil
}

// checkResponsesStream: response.completed arrived, and every function call in it had a
// done event, and its argument deltas join into exactly the arguments of that done event
// and of response.completed, which parse as JSON.
func checkResponsesStream(body []byte) error {
	type item struct {
		ID        string `json:"id"`
		Type      string `json:"type"`
		Arguments string `json:"arguments"`
	}
	var output []item
	completed := false
	deltas := map[string]*strings.Builder{}
	done := map[string]bool{}
	err := sseData(body, func(payload []byte) error {
		var event struct {
			Type      string `json:"type"`
			ItemID    string `json:"item_id"`
			Delta     string `json:"delta"`
			Arguments string `json:"arguments"`
			Response  struct {
				Output []item `json:"output"`
			} `json:"response"`
		}
		if err := json.Unmarshal(payload, &event); err != nil {
			return fmt.Errorf("event is not JSON: %w", err)
		}
		switch event.Type {
		case "response.function_call_arguments.delta":
			if deltas[event.ItemID] == nil {
				deltas[event.ItemID] = &strings.Builder{}
			}
			deltas[event.ItemID].WriteString(event.Delta)
		case "response.function_call_arguments.done":
			joined := deltas[event.ItemID]
			if joined == nil || joined.String() != event.Arguments {
				return fmt.Errorf("call %s: argument deltas do not join into its done arguments", event.ItemID)
			}
			done[event.ItemID] = true
		case "response.completed":
			completed, output = true, event.Response.Output
		}
		return nil
	})
	if err != nil {
		return err
	}
	if !completed {
		return fmt.Errorf("no response.completed")
	}
	calls := 0
	for _, it := range output {
		if it.Type != "function_call" {
			continue
		}
		calls++
		if !done[it.ID] {
			return fmt.Errorf("call %s: no response.function_call_arguments.done", it.ID)
		}
		joined := deltas[it.ID]
		if joined == nil || joined.String() != it.Arguments {
			return fmt.Errorf("call %s: argument deltas do not join into its completed arguments", it.ID)
		}
		if !json.Valid([]byte(it.Arguments)) {
			return fmt.Errorf("call %s: arguments are not JSON: %.200s", it.ID, it.Arguments)
		}
	}
	if calls == 0 {
		return fmt.Errorf("no function call in response.completed")
	}
	return nil
}

type msg = map[string]any

// session is one growing conversation.
type session struct {
	r        *rand.Rand
	id       string
	system   []msg
	tools    []msg
	messages []msg
	size     int
}

func newSession(r *rand.Rand, n int) *session {
	s := &session{r: r, id: fmt.Sprintf("%016x", r.Int63())}
	s.system = []msg{
		{"type": "text", "text": "You are a coding agent working in the user's repository. " + text(r, 6000)},
		{"type": "text", "text": text(r, 12000), "cache_control": msg{"type": "ephemeral"}},
	}
	for i := 0; i < 24; i++ {
		s.tools = append(s.tools, msg{
			"name":        fmt.Sprintf("tool_%02d", i),
			"description": text(r, 900),
			"input_schema": msg{"type": "object", "properties": msg{
				"path":    msg{"type": "string", "description": text(r, 120)},
				"content": msg{"type": "string", "description": text(r, 120)},
				"limit":   msg{"type": "integer"},
			}, "required": []string{"path"}},
		})
	}
	s.messages = []msg{{"role": "user", "content": []msg{{"type": "text", "text": text(r, 2000)}}}}
	s.size = 40000 + 2000
	return s
}

// turn appends one assistant tool call and its result, about step bytes.
func (s *session) turn(step int) {
	n := len(s.messages)
	s.messages = append(s.messages,
		msg{"role": "assistant", "content": []msg{
			{"type": "thinking", "thinking": text(s.r, 1500), "signature": strings.Repeat("EqQBCkgIBRABGAIiQL", 20)},
			{"type": "text", "text": text(s.r, 400)},
			{"type": "tool_use", "id": fmt.Sprintf("toolu_%s_%d", s.id, n), "name": fmt.Sprintf("tool_%02d", n%24), "input": msg{"path": "src/lib.rs"}},
		}},
		msg{"role": "user", "content": []msg{
			{"type": "tool_result", "tool_use_id": fmt.Sprintf("toolu_%s_%d", s.id, n), "content": text(s.r, step-2500)},
		}},
	)
	s.size += step
}

func (s *session) body(model string) []byte {
	b, _ := json.Marshal(msg{
		"model":      model,
		"max_tokens": 32000,
		"stream":     true,
		"thinking":   msg{"type": "enabled", "budget_tokens": 16000},
		"metadata":   msg{"user_id": "user_bench_account__session_" + s.id},
		"system":     s.system,
		"tools":      s.tools,
		"messages":   s.messages,
	})
	return b
}

// codexSession is one growing Codex conversation in the Responses format.
type codexSession struct {
	r            *rand.Rand
	id           string
	instructions string
	tools        []msg
	input        []msg
	size         int
}

func newCodexSession(r *rand.Rand) *codexSession {
	s := &codexSession{r: r, id: fmt.Sprintf("%016x", r.Int63())}
	s.instructions = "You are a coding agent working in the user's repository. " + text(r, 18000)
	for i := 0; i < 24; i++ {
		s.tools = append(s.tools, msg{
			"type":        "function",
			"name":        fmt.Sprintf("tool_%02d", i),
			"description": text(r, 900),
			"strict":      false,
			"parameters": msg{"type": "object", "properties": msg{
				"path":    msg{"type": "string", "description": text(r, 120)},
				"content": msg{"type": "string", "description": text(r, 120)},
				"limit":   msg{"type": "integer"},
			}, "required": []string{"path"}},
		})
	}
	s.input = []msg{{"type": "message", "role": "user", "content": []msg{{"type": "input_text", "text": text(r, 2000)}}}}
	s.size = 40000 + 2000
	return s
}

// turn appends one reasoning item, a function call and its output, about step bytes.
func (s *codexSession) turn(step int) {
	n := len(s.input)
	call := fmt.Sprintf("call_%s_%d", s.id, n)
	s.input = append(s.input,
		msg{"type": "reasoning", "id": fmt.Sprintf("rs_%s_%d", s.id, n),
			"summary":           []msg{{"type": "summary_text", "text": text(s.r, 600)}},
			"encrypted_content": strings.Repeat("gAAAAABo3x9kZ2VuY3J5cHRlZC1yZWFzb25pbmc", 30)},
		msg{"type": "function_call", "id": fmt.Sprintf("fc_%s_%d", s.id, n), "call_id": call,
			"name": fmt.Sprintf("tool_%02d", n%24), "arguments": `{"path":"src/lib.rs"}`},
		msg{"type": "function_call_output", "call_id": call, "output": text(s.r, step-2500)},
	)
	s.size += step
}

func (s *codexSession) body(model string) []byte {
	b, _ := json.Marshal(msg{
		"model":               model,
		"instructions":        s.instructions,
		"input":               s.input,
		"tools":               s.tools,
		"tool_choice":         "auto",
		"parallel_tool_calls": false,
		"reasoning":           msg{"effort": "medium", "summary": "auto"},
		"store":               false,
		"stream":              true,
		"include":             []string{"reasoning.encrypted_content"},
		"prompt_cache_key":    s.id,
	})
	return b
}

func load(args []string) {
	fs := flag.NewFlagSet("load", flag.ExitOnError)
	url := fs.String("url", "", "request URL")
	model := fs.String("model", "claude-sonnet-4-5-20250929", "model")
	key := fs.String("key", "", "client API key (x-api-key)")
	total := fs.Int("n", 3000, "requests in total")
	workers := fs.Int("c", 8, "concurrent sessions")
	minSize := fs.Int("min", 100_000, "first request size in bytes")
	maxSize := fs.Int("max", 500_000, "largest request size in bytes before a new session")
	step := fs.Int("step", 25_000, "bytes each turn adds")
	seed := fs.Int64("seed", 1, "random seed")
	codexWorkers := fs.Int("codex", 0, "how many of the -c sessions are Codex Responses sessions")
	codexModel := fs.String("codex-model", "gpt-5.5", "model of the Codex sessions and of every other count_tokens request")
	countEvery := fs.Int("count", 0, "send count_tokens before every Nth Claude turn (0: never)")
	fs.Parse(args)
	base := strings.TrimSuffix(*url, "/v1/messages")

	client := &http.Client{Transport: &http.Transport{
		MaxIdleConns:        *workers,
		MaxIdleConnsPerHost: *workers,
		DisableCompression:  true,
	}}
	type kind struct{ ok, bad int }
	type result struct {
		ttfb, total []time.Duration
		ok, bad     int
		bytesIn     int64
		bytesOut    int64
		kinds       map[string]*kind
	}
	results := make([]result, *workers)
	var issued atomic.Int64
	start := time.Now()
	var wg sync.WaitGroup
	for w := 0; w < *workers; w++ {
		wg.Add(1)
		go func(w int, res *result) {
			defer wg.Done()
			res.kinds = map[string]*kind{}
			r := rand.New(rand.NewSource(*seed*1000 + int64(w)))
			codex := w >= *workers-*codexWorkers
			// send posts one request; check returns why a reply is incomplete or invalid.
			send := func(name, url string, body []byte, stream bool, check func([]byte) error) {
				k := res.kinds[name]
				if k == nil {
					k = &kind{}
					res.kinds[name] = k
				}
				req, _ := http.NewRequest("POST", url, bytes.NewReader(body))
				req.Header.Set("Content-Type", "application/json")
				if codex {
					req.Header.Set("Authorization", "Bearer "+*key)
				} else {
					req.Header.Set("anthropic-version", "2023-06-01")
					req.Header.Set("anthropic-beta", "interleaved-thinking-2025-05-14")
					if *key != "" {
						req.Header.Set("x-api-key", *key)
					}
				}
				t0 := time.Now()
				resp, err := client.Do(req)
				if err != nil {
					res.bad++
					k.bad++
					return
				}
				br := bufio.NewReader(resp.Body)
				_, err = br.Peek(1)
				ttfb := time.Since(t0)
				data, err2 := io.ReadAll(br)
				resp.Body.Close()
				var invalid error
				if err == nil && err2 == nil && resp.StatusCode == 200 {
					invalid = check(data)
				}
				if err != nil || err2 != nil || resp.StatusCode != 200 || invalid != nil {
					res.bad++
					k.bad++
					if res.bad <= 3 {
						log.Printf("bad %s response: status %d err %v %v %v: %.300s", name, resp.StatusCode, err, err2, invalid, data)
					}
					return
				}
				res.ok++
				k.ok++
				if stream {
					res.ttfb = append(res.ttfb, ttfb)
					res.total = append(res.total, time.Since(t0))
				}
				res.bytesOut += int64(len(body))
				res.bytesIn += int64(len(data))
			}
			counted := func(data []byte) error {
				if !bytes.Contains(data, []byte(`"input_tokens"`)) {
					return fmt.Errorf("no input_tokens")
				}
				return nil
			}
			var s *session
			var cs *codexSession
			turns := 0
			for issued.Add(1) <= int64(*total) {
				turns++
				if codex {
					if cs == nil || cs.size+*step > *maxSize {
						cs = newCodexSession(r)
						for cs.size < *minSize {
							cs.turn(*step)
						}
					} else {
						cs.turn(*step)
					}
					send("responses", base+"/v1/responses", cs.body(*codexModel), true, checkResponsesStream)
					continue
				}
				if s == nil || s.size+*step > *maxSize {
					s = newSession(r, *minSize)
					for s.size < *minSize {
						s.turn(*step)
					}
				} else {
					s.turn(*step)
				}
				if *countEvery > 0 && turns%*countEvery == 0 {
					countModel := *model
					if (turns / *countEvery)%2 == 0 {
						countModel = *codexModel
					}
					send("count_tokens", base+"/v1/messages/count_tokens", s.body(countModel), false, counted)
				}
				send("messages", *url, s.body(*model), true, checkClaudeStream)
			}
		}(w, &results[w])
	}
	wg.Wait()
	elapsed := time.Since(start)

	var ttfb, tot []time.Duration
	out := map[string]any{"seconds": elapsed.Seconds()}
	ok, bad := 0, 0
	var in, sent int64
	kinds := map[string]*kind{}
	for _, r := range results {
		ttfb = append(ttfb, r.ttfb...)
		tot = append(tot, r.total...)
		ok += r.ok
		bad += r.bad
		in += r.bytesIn
		sent += r.bytesOut
		for name, k := range r.kinds {
			if kinds[name] == nil {
				kinds[name] = &kind{}
			}
			kinds[name].ok += k.ok
			kinds[name].bad += k.bad
		}
	}
	pct := func(lat []time.Duration, p float64) float64 {
		if len(lat) == 0 {
			return 0
		}
		sort.Slice(lat, func(i, j int) bool { return lat[i] < lat[j] })
		return float64(lat[int(p*float64(len(lat)-1))].Microseconds()) / 1000
	}
	out["ok"], out["bad"] = ok, bad
	out["rps"] = float64(ok) / elapsed.Seconds()
	out["ttfb_p50_ms"], out["ttfb_p99_ms"] = pct(ttfb, 0.5), pct(ttfb, 0.99)
	out["total_p50_ms"], out["total_p99_ms"] = pct(tot, 0.5), pct(tot, 0.99)
	if ok > 0 {
		out["avg_request_kb"] = sent / int64(ok) / 1024
		out["avg_response_kb"] = in / int64(ok) / 1024
	}
	if *codexWorkers > 0 || *countEvery > 0 {
		for name, k := range kinds {
			out[name+"_ok"], out[name+"_bad"] = k.ok, k.bad
		}
	}
	json.NewEncoder(os.Stdout).Encode(out)
}
