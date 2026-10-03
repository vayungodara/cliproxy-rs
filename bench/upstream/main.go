// Fake OpenAI-compatible upstream for docs/BENCHMARKS.md. Standard library only.
//
// POST /v1/chat/completions answers with a fixed completion. With "stream": true in
// the request it sends -chunks SSE chunks, waiting -delay between them, then [DONE].
// GET /v1/models lists the one model. Nothing else is served.
package main

import (
	"bytes"
	"flag"
	"io"
	"log"
	"net/http"
	"time"
)

func main() {
	addr := flag.String("addr", "127.0.0.1:9201", "listen address")
	chunks := flag.Int("chunks", 20, "SSE chunks per streamed response")
	delay := flag.Duration("delay", 0, "pause between streamed chunks")
	flag.Parse()

	completion := []byte(`{"id":"chatcmpl-bench","object":"chat.completion","created":1760000000,"model":"bench-model",` +
		`"choices":[{"index":0,"message":{"role":"assistant","content":"` + filler(400) + `"},"finish_reason":"stop"}],` +
		`"usage":{"prompt_tokens":512,"completion_tokens":100,"total_tokens":612}}`)
	first := []byte(`data: {"id":"chatcmpl-bench","object":"chat.completion.chunk","created":1760000000,"model":"bench-model","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}` + "\n\n")
	piece := []byte(`data: {"id":"chatcmpl-bench","object":"chat.completion.chunk","created":1760000000,"model":"bench-model","choices":[{"index":0,"delta":{"content":"` + filler(20) + `"},"finish_reason":null}]}` + "\n\n")
	last := []byte(`data: {"id":"chatcmpl-bench","object":"chat.completion.chunk","created":1760000000,"model":"bench-model","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":512,"completion_tokens":100,"total_tokens":612}}` + "\n\n" + "data: [DONE]\n\n")

	mux := http.NewServeMux()
	mux.HandleFunc("GET /v1/models", func(w http.ResponseWriter, _ *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		io.WriteString(w, `{"object":"list","data":[{"id":"bench-model","object":"model","owned_by":"bench"}]}`)
	})
	mux.HandleFunc("POST /v1/chat/completions", func(w http.ResponseWriter, r *http.Request) {
		body, _ := io.ReadAll(r.Body)
		if !bytes.Contains(body, []byte(`"stream":true`)) && !bytes.Contains(body, []byte(`"stream": true`)) {
			w.Header().Set("Content-Type", "application/json")
			w.Write(completion)
			return
		}
		w.Header().Set("Content-Type", "text/event-stream")
		w.Header().Set("Cache-Control", "no-cache")
		flusher := w.(http.Flusher)
		w.Write(first)
		flusher.Flush()
		for i := 0; i < *chunks; i++ {
			if *delay > 0 {
				time.Sleep(*delay)
			}
			w.Write(piece)
			flusher.Flush()
		}
		w.Write(last)
		flusher.Flush()
	})
	log.Printf("fake upstream on %s (chunks %d, delay %s)", *addr, *chunks, *delay)
	log.Fatal(http.ListenAndServe(*addr, mux))
}

func filler(n int) string {
	const words = "lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod tempor "
	out := make([]byte, 0, n)
	for len(out) < n {
		out = append(out, words...)
	}
	return string(out[:n])
}
