// Run inside the pinned Go module, with external networking denied.
package main

import (
	"bytes"
	"compress/flate"
	"compress/gzip"
	"encoding/json"
	"fmt"
	"io"
	"net/http/httptest"
	"os"
	"path/filepath"
	"regexp"
	"sort"
	"strings"
	"time"

	"github.com/andybalholm/brotli"
	"github.com/gin-gonic/gin"
	"github.com/klauspost/compress/zstd"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/api/middleware"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/buildinfo"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/logging"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/util"
)

func canonical(raw string) string {
	lines := strings.Split(raw, "\n")
	for i := 0; i < len(lines); i++ {
		if strings.HasPrefix(lines[i], "Timestamp: ") {
			lines[i] = "Timestamp: <time>"
		}
		if lines[i] == "=== HEADERS ===" || lines[i] == "=== RESPONSE ===" {
			start := i + 1
			if start < len(lines) && strings.HasPrefix(lines[start], "Status: ") {
				start++
			}
			end := start
			for end < len(lines) && lines[end] != "" {
				end++
			}
			sort.Strings(lines[start:end])
		}
	}
	return strings.Join(lines, "\n")
}

func main() {
	gin.SetMode(gin.TestMode)
	buildinfo.Version = "cliproxy-rs-0.1.0"
	root, err := os.MkdirTemp("", "cpa-capture-go-")
	if err != nil {
		panic(err)
	}
	defer os.RemoveAll(root)
	output := map[string]any{}
	masks := []map[string]string{}
	for _, name := range []string{"Authorization", "Proxy-Authorization", "X-API-Key", "X-Apikey", "X-Token", "X-Secret", "Api_Key", "Cookie"} {
		for _, value := range []string{"Bearer 1234567890", "  Bearer   abcdefghi  ", "ab", "abc", "abcdefgh", "abcdefghi"} {
			masks = append(masks, map[string]string{"name": name, "value": value, "out": util.MaskSensitiveHeaderValue(name, value)})
		}
	}
	output["masks"] = masks
	cases := []map[string]any{}
	for index, test := range []struct {
		enabled      bool
		method, path string
		status       int
		consume      int
		stream       bool
		body         string
	}{
		{true, "POST", "/v1/responses?key=1234567890", 200, -1, false, "inbound-secret\n"},
		{false, "POST", "/v1/responses", 200, -1, false, "ok"},
		{false, "POST", "/v1/responses", 400, -1, false, "client-error"},
		{false, "POST", "/v1/responses", 499, -1, false, "cancel"},
		{false, "HEAD", "/v1/responses", 500, -1, false, "head"},
		{true, "GET", "/v1/responses", 500, -1, false, "get"},
		{true, "POST", "/v8/management/config", 500, -1, false, "management-secret"},
		{false, "POST", "/v1/responses", 500, 5, false, strings.Repeat("x", (1<<20)+1)},
		{true, "POST", "/v1/responses", 200, -1, true, `{"stream":true}`},
	} {
		dir := filepath.Join(root, fmt.Sprint(index))
		logger := logging.NewFileRequestLogger(test.enabled, dir, "", 10)
		engine := gin.New()
		engine.Use(func(c *gin.Context) { logging.SetGinRequestID(c, "0198-aaaa-a1b2c3d4") })
		engine.Use(middleware.RequestLoggingMiddleware(logger))
		engine.Handle(test.method, strings.Split(test.path, "?")[0], func(c *gin.Context) {
			if test.consume < 0 {
				_, _ = io.Copy(io.Discard, c.Request.Body)
			} else {
				_, _ = io.CopyN(io.Discard, c.Request.Body, int64(test.consume))
			}
			if test.stream {
				c.Header("Content-Type", "text/event-stream")
			} else {
				c.Header("Content-Type", "application/json")
			}
			c.Status(test.status)
			_, _ = c.Writer.Write([]byte("outbound\n"))
			if test.stream {
				// Let Go's async chunk consumer start before Finalize clears its
				// channel field; otherwise this synthetic zero-duration stream
				// exposes an unrelated scheduling race in the reference wrapper.
				time.Sleep(20 * time.Millisecond)
			}
		})
		request := httptest.NewRequest(test.method, test.path, strings.NewReader(test.body))
		request.Header.Set("Authorization", "Bearer 1234567890")
		request.Header.Set("Content-Length", fmt.Sprint(len(test.body)))
		engine.ServeHTTP(httptest.NewRecorder(), request)
		files, _ := filepath.Glob(filepath.Join(dir, "*.log"))
		content := ""
		filename := ""
		if len(files) > 0 {
			raw, err := os.ReadFile(files[0])
			if err != nil {
				panic(err)
			}
			content = canonical(string(raw))
			filename = regexp.MustCompile(`\d{4}-\d\d-\d\dT\d{6}`).ReplaceAllString(filepath.Base(files[0]), "<date>")
		}
		body := test.body
		if len(body) > 100 {
			body = ""
		}
		cases = append(cases, map[string]any{"enabled": test.enabled, "method": test.method, "path": test.path, "status": test.status, "consume": test.consume, "stream": test.stream, "body": body, "body_length": len(test.body), "content": content, "filename": filename})
	}
	output["cases"] = cases
	reloads := []map[string]any{}
	for _, beforeFirst := range []bool{false, true} {
		dir := filepath.Join(root, fmt.Sprintf("reload-%t", beforeFirst))
		logger := logging.NewFileRequestLogger(false, dir, "", 10)
		engine := gin.New()
		engine.Use(middleware.RequestLoggingMiddleware(logger))
		engine.POST("/v1/responses", func(c *gin.Context) {
			c.Header("Content-Type", "application/json")
			if beforeFirst {
				logger.SetEnabled(true)
			}
			_, _ = c.Writer.Write([]byte("first\n"))
			logger.SetEnabled(true)
			_, _ = c.Writer.Write([]byte("second\n"))
		})
		engine.ServeHTTP(httptest.NewRecorder(), httptest.NewRequest("POST", "/v1/responses", nil))
		files, _ := filepath.Glob(filepath.Join(dir, "*.log"))
		raw, _ := os.ReadFile(files[0])
		reloads = append(reloads, map[string]any{"before_first": beforeFirst, "content": canonical(string(raw))})
	}
	output["reloads"] = reloads
	// Real file logger: one part per event, blank-line joins, no generic HTTP
	// sections in websocket transcripts. Formatting inputs match the private
	// formatWebsocketTimelineEvent implementation read at the pinned revision.
	at, _ := time.Parse(time.RFC3339Nano, "2026-10-03T04:05:06.12001Z")
	parts := [][]byte{}
	for _, event := range []string{"request", "response", "disconnect"} {
		parts = append(parts, []byte(fmt.Sprintf("Timestamp: %s\nEvent: websocket.%s\n%s-payload\n", at.Format(time.RFC3339Nano), event, event)))
	}
	timeline := bytes.Join(parts, []byte("\n"))
	dir := filepath.Join(root, "websocket")
	logger := logging.NewFileRequestLogger(true, dir, "", 10)
	err = logger.LogRequest("/v1/responses", "GET", map[string][]string{"Upgrade": {"websocket"}}, nil, 101, nil, nil, timeline, nil, nil, nil, nil, "0198-aaaa-a1b2c3d4", at, time.Time{})
	if err != nil {
		panic(err)
	}
	files, _ := filepath.Glob(filepath.Join(dir, "*.log"))
	raw, _ := os.ReadFile(files[0])
	output["websocket"] = map[string]any{"parts": parts, "content": canonical(string(raw))}
	compressed := []map[string]any{}
	for _, encoding := range []string{"gzip", "deflate", "br", "zstd"} {
		var buffer bytes.Buffer
		var writer io.WriteCloser
		switch encoding {
		case "gzip":
			writer = gzip.NewWriter(&buffer)
		case "deflate":
			writer, _ = flate.NewWriter(&buffer, flate.DefaultCompression)
		case "br":
			writer = brotli.NewWriter(&buffer)
		case "zstd":
			writer, _ = zstd.NewWriter(&buffer)
		}
		_, _ = writer.Write([]byte("decoded payload\n"))
		_ = writer.Close()
		for _, invalid := range []bool{false, true} {
			payload := buffer.Bytes()
			if invalid {
				payload = payload[:len(payload)/2]
			}
			dir := filepath.Join(root, fmt.Sprintf("compression-%s-%t", encoding, invalid))
			logger := logging.NewFileRequestLogger(true, dir, "", 10)
			err = logger.LogRequest("/v1/responses", "POST", nil, nil, 200, map[string][]string{"Content-Encoding": {encoding}}, payload, nil, nil, nil, nil, nil, "a1b2c3d4", at, time.Time{})
			if err != nil {
				panic(err)
			}
			files, _ := filepath.Glob(filepath.Join(dir, "*.log"))
			raw, _ := os.ReadFile(files[0])
			compressed = append(compressed, map[string]any{"encoding": encoding, "invalid": invalid, "input": payload, "raw": raw, "content": canonical(string(raw))})
		}
	}
	output["compression"] = compressed
	var brotliBuffer bytes.Buffer
	writer := brotli.NewWriter(&brotliBuffer)
	_, _ = writer.Write([]byte("decoded payload\n"))
	_ = writer.Close()
	prefixes := []map[string]any{}
	for n := 1; n <= brotliBuffer.Len(); n++ {
		input := append([]byte(nil), brotliBuffer.Bytes()[:n]...)
		decoded, errDecode := io.ReadAll(brotli.NewReader(bytes.NewReader(input)))
		prefixes = append(prefixes, map[string]any{"input": input, "output": decoded, "error": errDecode != nil})
	}
	brotliBuffer.Reset()
	writer = brotli.NewWriter(&brotliBuffer)
	_, _ = writer.Write(bytes.Repeat([]byte("x"), 65536))
	_ = writer.Flush()
	input := append([]byte(nil), brotliBuffer.Bytes()...)
	decoded, errDecode := io.ReadAll(brotli.NewReader(bytes.NewReader(input)))
	prefixes = append(prefixes, map[string]any{"input": input, "output": decoded, "error": errDecode != nil})
	_ = writer.Close()
	output["brotli_prefixes"] = prefixes
	if err = json.NewEncoder(os.Stdout).Encode(output); err != nil {
		panic(err)
	}
}
