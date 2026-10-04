// Claude Messages soak test for docs/BENCHMARKS.md. Standard library only.
//
//	messages upstream -addr 127.0.0.1:9202
//	messages load -url http://127.0.0.1:8340/v1/messages -n 3000 -c 8
//
// upstream is a fake Anthropic Messages endpoint. POST /v1/messages reads the whole
// request and streams a Claude SSE reply: message_start, a thinking block with a
// signature, a text block, a tool_use block naming the request's first tool, then
// message_delta and message_stop. -events sets the number of delta events and -delay
// the pause between them.
//
// load models a coding agent. Each of -c workers is one session whose conversation
// grows turn by turn from -min to -max bytes (assistant turns with signed thinking and
// tool_use, user turns with large tool_result text), then starts a new session. Every
// request is streamed and counts as ok only with status 200 and a body containing
// message_stop. It prints one JSON line with throughput and latency percentiles: ttfb is
// the time to the first response byte, total the time to the end of the stream.
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
		for i := 2 * third; i < *events; i++ {
			pause()
			b, _ := json.Marshal(`{"path":"` + thinking[i][:40] + `"`)
			send("content_block_delta", `{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":`+string(b)+`}}`)
		}
		send("content_block_stop", `{"type":"content_block_stop","index":2}`)
		send("message_delta", `{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":`+fmt.Sprint(*events*30)+`}}`)
		send("message_stop", `{"type":"message_stop"}`)
	})
	log.Printf("fake Claude upstream on %s (%d events, %s apart)", *addr, *events, *delay)
	log.Fatal(http.ListenAndServe(*addr, nil))
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
	fs.Parse(args)

	client := &http.Client{Transport: &http.Transport{
		MaxIdleConns:        *workers,
		MaxIdleConnsPerHost: *workers,
		DisableCompression:  true,
	}}
	type result struct {
		ttfb, total []time.Duration
		ok, bad     int
		bytesIn     int64
		bytesOut    int64
	}
	results := make([]result, *workers)
	var issued atomic.Int64
	start := time.Now()
	var wg sync.WaitGroup
	for w := 0; w < *workers; w++ {
		wg.Add(1)
		go func(w int, res *result) {
			defer wg.Done()
			r := rand.New(rand.NewSource(*seed*1000 + int64(w)))
			var s *session
			for issued.Add(1) <= int64(*total) {
				if s == nil || s.size+*step > *maxSize {
					s = newSession(r, *minSize)
					for s.size < *minSize {
						s.turn(*step)
					}
				} else {
					s.turn(*step)
				}
				body := s.body(*model)
				req, _ := http.NewRequest("POST", *url, bytes.NewReader(body))
				req.Header.Set("Content-Type", "application/json")
				req.Header.Set("anthropic-version", "2023-06-01")
				req.Header.Set("anthropic-beta", "interleaved-thinking-2025-05-14")
				if *key != "" {
					req.Header.Set("x-api-key", *key)
				}
				t0 := time.Now()
				resp, err := client.Do(req)
				if err != nil {
					res.bad++
					continue
				}
				br := bufio.NewReader(resp.Body)
				_, err = br.Peek(1)
				ttfb := time.Since(t0)
				data, err2 := io.ReadAll(br)
				resp.Body.Close()
				if err != nil || err2 != nil || resp.StatusCode != 200 || !bytes.Contains(data, []byte("message_stop")) {
					res.bad++
					if res.bad <= 3 {
						log.Printf("bad response: status %d err %v %v: %.300s", resp.StatusCode, err, err2, data)
					}
					continue
				}
				res.ok++
				res.ttfb = append(res.ttfb, ttfb)
				res.total = append(res.total, time.Since(t0))
				res.bytesOut += int64(len(body))
				res.bytesIn += int64(len(data))
			}
		}(w, &results[w])
	}
	wg.Wait()
	elapsed := time.Since(start)

	var ttfb, tot []time.Duration
	out := map[string]any{"seconds": elapsed.Seconds()}
	ok, bad := 0, 0
	var in, sent int64
	for _, r := range results {
		ttfb = append(ttfb, r.ttfb...)
		tot = append(tot, r.total...)
		ok += r.ok
		bad += r.bad
		in += r.bytesIn
		sent += r.bytesOut
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
	json.NewEncoder(os.Stdout).Encode(out)
}
