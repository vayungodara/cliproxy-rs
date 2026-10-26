package openai

// Chat Completions and legacy Completions streams that close with or without a
// finish_reason (Go 97dd9eec), through Go's own handlers and an executor that replays
// fixed chunks. Copy into sdk/api/handlers/openai/ of CLIProxyAPI v8.0.35 (ae3ee3be) and
// run with RSFIX_OUT=<dir>; it writes stream_truncation_go.json. Nothing is sent upstream.
// The first nine scenarios are the cases of openai_handlers_stream_truncation_test.go.

import (
	"context"
	"encoding/json"
	"errors"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/gin-gonic/gin"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/registry"
	"github.com/router-for-me/CLIProxyAPI/v8/sdk/api/handlers"
	coreauth "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/auth"
	coreexecutor "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/executor"
	sdkconfig "github.com/router-for-me/CLIProxyAPI/v8/sdk/config"
)

type rsfixChunksExecutor struct{ chunks []string }

func (*rsfixChunksExecutor) Identifier() string { return "rsfix-chunks" }

func (*rsfixChunksExecutor) Execute(context.Context, *coreauth.Auth, coreexecutor.Request, coreexecutor.Options) (coreexecutor.Response, error) {
	return coreexecutor.Response{}, errors.New("not implemented")
}

func (e *rsfixChunksExecutor) ExecuteStream(context.Context, *coreauth.Auth, coreexecutor.Request, coreexecutor.Options) (*coreexecutor.StreamResult, error) {
	chunks := make(chan coreexecutor.StreamChunk, len(e.chunks))
	for _, c := range e.chunks {
		chunks <- coreexecutor.StreamChunk{Payload: []byte(c)}
	}
	close(chunks)
	return &coreexecutor.StreamResult{Chunks: chunks}, nil
}

func (*rsfixChunksExecutor) Refresh(_ context.Context, auth *coreauth.Auth) (*coreauth.Auth, error) {
	return auth, nil
}

func (*rsfixChunksExecutor) CountTokens(context.Context, *coreauth.Auth, coreexecutor.Request, coreexecutor.Options) (coreexecutor.Response, error) {
	return coreexecutor.Response{}, errors.New("not implemented")
}

func (*rsfixChunksExecutor) HttpRequest(context.Context, *coreauth.Auth, *http.Request) (*http.Response, error) {
	return nil, errors.New("not implemented")
}

type rsfixTruncationCase struct {
	Name   string   `json:"name"`
	Path   string   `json:"path"`
	Chunks []string `json:"chunks"`
	Out    string   `json:"out"`
}

func TestRSFixStreamTruncation(t *testing.T) {
	gin.SetMode(gin.TestMode)
	const chat, legacy = "/v1/chat/completions", "/v1/completions"
	cases := []rsfixTruncationCase{
		{Name: "multi_chunk_missing", Path: chat, Chunks: []string{`{"choices":[{"index":0,"delta":{"role":"assistant"}}]}`, `{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"do_thing","arguments":"{\"a\":1"}}]}}]}`}},
		{Name: "null_or_empty", Path: chat, Chunks: []string{`{"choices":[{"index":0,"delta":{"role":"assistant","content":"part1"},"finish_reason":null}]}`, `{"choices":[{"index":0,"delta":{"content":"part2"},"finish_reason":""}]}`}},
		{Name: "single_chunk_missing", Path: chat, Chunks: []string{`{"choices":[{"index":0,"delta":{"role":"assistant","content":"hello"}}]}`}},
		{Name: "multi_chunk_present", Path: chat, Chunks: []string{`{"choices":[{"index":0,"delta":{"role":"assistant"}}]}`, `{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"do_thing","arguments":"{\"a\":1}"}}]}}]}`, `{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}`}},
		{Name: "single_chunk_present", Path: chat, Chunks: []string{`{"choices":[{"index":0,"delta":{"content":"instant"},"finish_reason":"stop"}]}`}},
		{Name: "usage_after_finish", Path: chat, Chunks: []string{`{"choices":[{"index":0,"delta":{"content":"hi"}}]}`, `{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}`, `{"choices":[],"usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}`}},
		{Name: "legacy_missing", Path: legacy, Chunks: []string{`{"choices":[{"index":0,"delta":{"role":"assistant"}}]}`, `{"choices":[{"index":0,"delta":{"content":"trunc"}}]}`}},
		{Name: "legacy_present", Path: legacy, Chunks: []string{`{"choices":[{"index":0,"delta":{"role":"assistant"}}]}`, `{"choices":[{"index":0,"delta":{"content":"hello"},"finish_reason":"stop"}]}`}},
		{Name: "legacy_single_present", Path: legacy, Chunks: []string{`{"choices":[{"index":0,"delta":{"content":"single"},"finish_reason":"stop"}]}`}},
		// gjson String() of a number or false is not empty.
		{Name: "finish_reason_zero", Path: chat, Chunks: []string{`{"choices":[{"index":0,"delta":{"content":"x"},"finish_reason":0}]}`}},
		{Name: "finish_reason_false", Path: chat, Chunks: []string{`{"choices":[{"index":0,"delta":{"content":"x"},"finish_reason":false}]}`}},
		// gjson Array() of an object is a one-element list.
		{Name: "choices_object", Path: chat, Chunks: []string{`{"choices":{"index":0,"finish_reason":"stop"}}`}},
		{Name: "second_choice_finishes", Path: chat, Chunks: []string{`{"choices":[{"index":0,"delta":{"content":"x"}},{"index":1,"finish_reason":"length"}]}`}},
		// The legacy route checks the converted chunk, where null becomes "".
		{Name: "legacy_null_finish", Path: legacy, Chunks: []string{`{"choices":[{"index":0,"delta":{"content":"x"},"finish_reason":null}]}`}},
		// The conversion drops the string "null", so it is no finish reason here.
		{Name: "legacy_null_string_finish", Path: legacy, Chunks: []string{`{"choices":[{"index":0,"delta":{"content":"x"},"finish_reason":"null"}]}`}},
		// A first chunk the conversion drops still starts the stream.
		{Name: "legacy_dropped_first", Path: legacy, Chunks: []string{`{"choices":[{"index":0,"delta":{"role":"assistant"}}]}`}},
	}
	for i := range cases {
		tc := &cases[i]
		executor := &rsfixChunksExecutor{chunks: tc.Chunks}
		manager := coreauth.NewManager(nil, nil, nil)
		manager.RegisterExecutor(executor)
		auth := &coreauth.Auth{ID: "rsfix-trunc-" + tc.Name, Provider: executor.Identifier(), Status: coreauth.StatusActive}
		if _, err := manager.Register(context.Background(), auth); err != nil {
			t.Fatal(err)
		}
		model := "rsfix-trunc-" + tc.Name
		registry.GetGlobalRegistry().RegisterClient(auth.ID, auth.Provider, []*registry.ModelInfo{{ID: model}})
		h := NewOpenAIAPIHandler(handlers.NewBaseAPIHandlers(&sdkconfig.SDKConfig{}, manager))
		router := gin.New()
		router.POST(chat, h.ChatCompletions)
		router.POST(legacy, h.Completions)
		body := `{"model":"` + model + `","messages":[{"role":"user","content":"hi"}],"stream":true}`
		if tc.Path == legacy {
			body = `{"model":"` + model + `","prompt":"hi","stream":true}`
		}
		req := httptest.NewRequest(http.MethodPost, tc.Path, strings.NewReader(body))
		req.Header.Set("Content-Type", "application/json")
		rec := httptest.NewRecorder()
		router.ServeHTTP(rec, req)
		registry.GetGlobalRegistry().UnregisterClient(auth.ID)
		tc.Out = rec.Body.String()
	}
	out, err := json.MarshalIndent(cases, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	dir := os.Getenv("RSFIX_OUT")
	if dir == "" {
		t.Skip("RSFIX_OUT not set")
	}
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "stream_truncation_go.json"), append(out, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
