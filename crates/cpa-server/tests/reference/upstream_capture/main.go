// Run inside the pinned Go module, with external networking denied.
package main

import (
	"context"
	"encoding/json"
	"errors"
	"net/http"
	"net/http/httptest"
	"os"
	"strings"

	"github.com/gin-gonic/gin"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/logging"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/runtime/executor/helps"
)

func canonical(raw []byte) string {
	lines := strings.Split(string(raw), "\n")
	for i, line := range lines {
		if strings.HasPrefix(line, "Timestamp: ") {
			lines[i] = "Timestamp: <time>"
		}
	}
	return strings.Join(lines, "\n")
}

func main() {
	gin.SetMode(gin.TestMode)
	root, err := os.MkdirTemp("", "cpa-upstream-go-")
	if err != nil {
		panic(err)
	}
	defer os.RemoveAll(root)
	cases := []map[string]any{}
	for _, kind := range []string{"http", "missing", "websocket", "disabled", "oauth", "reload"} {
		c, _ := gin.CreateTestContext(httptest.NewRecorder())
		ctx := context.WithValue(context.Background(), "gin", c)
		cfg := &config.Config{}
		cfg.RequestLog = kind != "disabled" && kind != "reload"
		sources := map[string]*logging.FileBodySource{}
		for _, key := range []string{logging.APIRequestSourceContextKey, logging.APIResponseSourceContextKey, logging.APIWebsocketTimelineSourceContextKey} {
			source, err := logging.NewFileBodySourceInDir(root, "capture")
			if err != nil {
				panic(err)
			}
			sources[key] = source
			c.Set(key, source)
		}
		info := helps.UpstreamRequestLog{URL: "https://fixture.invalid/private", Method: "POST", Headers: http.Header{"Authorization": {"Bearer 1234567890"}, "X-Token": {"abcdefghi"}, "Z-Header": {"first", "second"}}, Body: []byte("request body"), Provider: " codex ", AuthID: " fixture-id ", AuthLabel: " fixture-label ", AuthType: " API_KEY ", AuthValue: " 1234567890 "}
		if kind == "oauth" {
			info.AuthType = "oauth"
			info.AuthValue = "never-print-oauth"
		}
		if kind != "missing" && kind != "websocket" {
			helps.RecordAPIRequest(ctx, cfg, info)
		}
		if kind == "websocket" {
			helps.RecordAPIWebsocketRequest(ctx, cfg, info)
			helps.RecordAPIWebsocketHandshake(ctx, cfg, 101, http.Header{"X-Secret": {"1234567890"}})
			helps.AppendAPIWebsocketResponse(ctx, cfg, []byte("  {\"type\":\"response.created\"} \n"))
			helps.RecordAPIWebsocketError(ctx, cfg, " read ", errors.New("fixture disconnect"))
		} else {
			if kind == "reload" {
				cfg.RequestLog = true
			}
			helps.RecordAPIResponseMetadata(ctx, cfg, 201, http.Header{"X-Secret": {"1234567890"}})
			helps.RecordAPIResponseMetadata(ctx, cfg, 202, http.Header{"Ignored": {"yes"}})
			helps.AppendAPIResponseChunk(ctx, cfg, []byte("  event: first\n"))
			helps.AppendAPIResponseChunk(ctx, cfg, []byte("data: {\"a\":1}\n"))
			helps.AppendAPIResponseChunk(ctx, cfg, []byte("\n data: second \n"))
			helps.RecordAPIResponseError(ctx, cfg, errors.New("fixture error"))
			helps.RecordAPIResponseError(ctx, cfg, errors.New("second error"))
			if kind == "http" {
				info.Body = nil
				info.AuthType = "oauth"
				info.AuthValue = "never-print-oauth"
				helps.RecordAPIRequest(ctx, cfg, info)
				helps.RecordAPIResponseError(ctx, cfg, errors.New("failed before response"))
			}
		}
		item := map[string]any{"kind": kind}
		for field, key := range map[string]string{"request": logging.APIRequestSourceContextKey, "response": logging.APIResponseSourceContextKey, "timeline": logging.APIWebsocketTimelineSourceContextKey} {
			raw, err := sources[key].Bytes()
			if err != nil {
				panic(err)
			}
			item[field] = canonical(raw)
		}
		if kind == "disabled" {
			if value, ok := c.Get(logging.DeferredAPIRequestContextKey); ok {
				for _, request := range value.([]logging.DeferredAPIRequest) {
					item["request"] = canonical(request())
				}
			}
		}
		cases = append(cases, item)
	}
	if err := json.NewEncoder(os.Stdout).Encode(cases); err != nil {
		panic(err)
	}
}
