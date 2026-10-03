package executor

// Recorder for cliproxy-rs. record.py copies Go's own Claude test files into a scratch
// copy of CLIProxyAPI 6fecc6e and rewrites selected call sites to the rsfix* wrappers
// below. Each wrapper calls the real function and records its inputs and outputs,
// tagged with the Go test that made the call, so the Rust port can replay every case
// against Go's results. Go's own assertions still run unchanged.
//
// TestZZZRSFixWrite (last, by file order) writes the records to $RSFIX_OUT.

import (
	"encoding/base64"
	"encoding/json"
	"errors"
	"os"
	"runtime"
	"strings"
	"sync"
	"testing"
	"unicode/utf8"

	cliproxyexecutor "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/executor"
)

var (
	rsfixMu      sync.Mutex
	rsfixRecords []map[string]any
)

// rsfixTest names the Go test (or fuzz seed run) on the calling goroutine's stack.
func rsfixTest() string {
	pcs := make([]uintptr, 128)
	frames := runtime.CallersFrames(pcs[:runtime.Callers(2, pcs)])
	for {
		frame, more := frames.Next()
		name := frame.Function[strings.LastIndex(frame.Function, "/")+1:]
		if parts := strings.Split(name, "."); len(parts) > 1 && parts[0] == "executor" &&
			(strings.HasPrefix(parts[1], "Test") || strings.HasPrefix(parts[1], "Fuzz")) {
			return parts[1]
		}
		if !more {
			return ""
		}
	}
}

func rsfixRecord(fn string, fields map[string]any) {
	fields["fn"] = fn
	fields["test"] = rsfixTest()
	rsfixMu.Lock()
	rsfixRecords = append(rsfixRecords, fields)
	rsfixMu.Unlock()
}

// rsfixBytes keeps invalid UTF-8 exactly (encoding/json would replace it).
func rsfixBytes(b []byte) any {
	if b == nil {
		return nil
	}
	if utf8.Valid(b) {
		return string(b)
	}
	return map[string]string{"b64": base64.StdEncoding.EncodeToString(b)}
}

func rsfixError(fields map[string]any, err error) map[string]any {
	if err != nil {
		fields["error"] = err.Error()
		var scoped cliproxyexecutor.RequestScopedError
		fields["request_scoped"] = errors.As(err, &scoped) && scoped.IsRequestScoped()
	}
	return fields
}

func rsfixClone(b []byte) []byte { return append([]byte(nil), b...) }

// --- MCP tool aliasing (claude_executor_request.go) ---

func rsfixRemapWithOptions(body []byte, options claudeMCPAliasOptions) ([]byte, map[string]string) {
	in := rsfixClone(body)
	out, reverse := remapOAuthToolNamesWithOptions(body, options)
	rsfixRecord("remap", map[string]any{"body": rsfixBytes(in), "secret": options.secret, "out": rsfixBytes(out), "reverse": reverse})
	return out, reverse
}

func rsfixRemap(body []byte) ([]byte, map[string]string) {
	return rsfixRemapWithOptions(body, claudeMCPAliasOptions{secret: "cpa-claude-mcp-default-caller"})
}

func rsfixRemapLegacy(body []byte, options claudeMCPAliasOptions) ([]byte, map[string]string) {
	in := rsfixClone(body)
	out, reverse := remapOAuthToolNamesWithOptionsLegacy(body, options)
	rsfixRecord("remap", map[string]any{"body": rsfixBytes(in), "secret": options.secret, "out": rsfixBytes(out), "reverse": reverse, "legacy": true})
	return out, reverse
}

func rsfixRemapBatched(body []byte, options claudeMCPAliasOptions) ([]byte, map[string]string, bool) {
	in := rsfixClone(body)
	out, reverse, ok := remapOAuthToolNamesWithBatchedEdits(body, options)
	rsfixRecord("remap_batched", map[string]any{"body": rsfixBytes(in), "secret": options.secret, "out": rsfixBytes(out), "reverse": reverse, "ok": ok})
	return out, reverse, ok
}

func rsfixRestore(body []byte, reverse map[string]string) ([]byte, error) {
	in := rsfixClone(body)
	out, err := reverseRemapOAuthToolNames(body, reverse)
	rsfixRecord("restore", rsfixError(map[string]any{"body": rsfixBytes(in), "reverse": reverse, "out": rsfixBytes(out)}, err))
	return out, err
}

func rsfixRestoreLine(line []byte, reverse map[string]string) ([]byte, error) {
	in := rsfixClone(line)
	out, err := reverseRemapOAuthToolNamesFromStreamLine(line, reverse)
	rsfixRecord("restore_line", rsfixError(map[string]any{"line": rsfixBytes(in), "reverse": reverse, "out": rsfixBytes(out)}, err))
	return out, err
}

func rsfixParseAlias(name string) (claudeMCPAliasParts, bool) {
	parts, ok := parseClaudeMCPAlias(name)
	rsfixRecord("parse_alias", map[string]any{"name": name, "ok": ok, "server": parts.server, "tool_id": parts.toolID, "semantic": parts.semantic})
	return parts, ok
}

func TestZZZRSFixWrite(t *testing.T) {
	out := os.Getenv("RSFIX_OUT")
	if out == "" {
		t.Skip("RSFIX_OUT not set")
	}
	rsfixMu.Lock()
	defer rsfixMu.Unlock()
	data, err := json.MarshalIndent(rsfixRecords, "", " ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(out, append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
