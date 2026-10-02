package openai

// Fixture post-processor for cliproxy-rs: adds `downstream.frames` to every executor
// fixture under $RSFIX_OUT/<provider> that recorded stream chunks. Frames are the chunks
// joined exactly as responsesSSEFramer.WriteChunk/Flush join them (Flush also runs
// before a terminal error), before repairFrame
// (route-level repair stays with the Rust Responses route). Run after the executor
// generators.

import (
	"bytes"
	"encoding/json"
	"os"
	"path/filepath"
	"testing"
)

type rsfixJoiner struct {
	pending []byte
	frames  []string
}

func (f *rsfixJoiner) emit(frame []byte) {
	var buf bytes.Buffer
	writeResponsesSSEChunk(&buf, frame)
	if buf.Len() > 0 {
		f.frames = append(f.frames, buf.String())
	}
}

func (f *rsfixJoiner) write(chunk []byte) {
	if len(chunk) == 0 {
		return
	}
	if responsesSSEStartsNewDataFrame(f.pending, chunk) {
		f.emit(append([]byte(nil), f.pending...))
		f.pending = f.pending[:0]
	}
	if responsesSSENeedsLineBreak(f.pending, chunk) {
		f.pending = append(f.pending, '\n')
	}
	f.pending = append(f.pending, chunk...)
	for {
		n := responsesSSEFrameLen(f.pending)
		if n == 0 {
			break
		}
		f.emit(append([]byte(nil), f.pending[:n]...))
		f.pending = append([]byte(nil), f.pending[n:]...)
	}
	if len(bytes.TrimSpace(f.pending)) == 0 {
		f.pending = f.pending[:0]
		return
	}
	if responsesSSECanEmitWithoutDelimiter(f.pending) {
		f.emit(append([]byte(nil), f.pending...))
		f.pending = f.pending[:0]
	}
}

func (f *rsfixJoiner) flush() {
	if len(bytes.TrimSpace(f.pending)) == 0 || !responsesSSECanFlushWithoutDelimiter(f.pending) {
		f.pending = f.pending[:0]
		return
	}
	f.emit(append([]byte(nil), f.pending...))
	f.pending = f.pending[:0]
}

func TestRSFixResponsesFrames(t *testing.T) {
	dir := os.Getenv("RSFIX_OUT")
	if dir == "" {
		t.Skip("RSFIX_OUT not set")
	}
	paths, _ := filepath.Glob(filepath.Join(dir, "*", "*.json"))
	for _, path := range paths {
		raw, err := os.ReadFile(path)
		if err != nil {
			t.Fatal(err)
		}
		var fixture map[string]any
		if err := json.Unmarshal(raw, &fixture); err != nil {
			continue // vector files are arrays
		}
		down, _ := fixture["downstream"].(map[string]any)
		chunks, _ := down["chunks"].([]any)
		if len(chunks) == 0 {
			continue
		}
		request, _ := fixture["request"].(map[string]any)
		if source, _ := request["source"].(string); source != "openai-response" {
			continue
		}
		var j rsfixJoiner
		for _, c := range chunks {
			s, _ := c.(string)
			j.write([]byte(s))
		}
		// The route flushes pending data before writing a terminal error too
		// (forwardResponsesStream writeTerminalError).
		j.flush()
		down["frames"] = j.frames
		out, err := json.MarshalIndent(fixture, "", "  ")
		if err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(path, append(out, '\n'), 0o644); err != nil {
			t.Fatal(err)
		}
	}
}
