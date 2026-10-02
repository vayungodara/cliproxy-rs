package executor

// Vectors for the Codex upstream WebSocket error-frame classifier
// (parseCodexWebsocketErrorWithCooling). Copy into internal/runtime/executor/ of
// CLIProxyAPI 6fecc6e and run with RSFIX_OUT=<dir>; it writes codex_ws_errors.json.

import (
	"encoding/json"
	"os"
	"path/filepath"
	"testing"
)

func TestRSFixCodexWebsocketErrors(t *testing.T) {
	dir := os.Getenv("RSFIX_OUT")
	if dir == "" {
		t.Skip("RSFIX_OUT not set")
	}
	type vector struct {
		Payload          string            `json:"payload"`
		Cooling          bool              `json:"model_level_cooling,omitempty"`
		Matched          bool              `json:"matched"`
		Status           int               `json:"status,omitempty"`
		Message          string            `json:"message,omitempty"`
		RetryAfterMS     *int64            `json:"retry_after_ms,omitempty"`
		CredentialScoped bool              `json:"credential_scoped,omitempty"`
		Headers          map[string]string `json:"headers,omitempty"`
	}
	var out []vector
	for _, c := range []struct {
		payload string
		cooling bool
	}{
		{payload: `{"type":"error","status":400,"error":{"type":"invalid_request_error","code":"context_length_exceeded","message":"too long"}}`},
		{payload: `{"type":"error","status":429,"error":{"type":"usage_limit_reached","message":"limit","resets_in_seconds":60}}`},
		{payload: `{"type":"error","status":429,"error":{"type":"usage_limit_reached","message":"limit","resets_in_seconds":60}}`, cooling: true},
		{payload: `{"type":"error","status_code":503}`},
		{payload: `{"type":"error","status":418}`},
		{payload: `{"type":"error","status":413}`},
		{payload: `{"type":"error","status":429,"body":{"error":{"code":"rate_limit_exceeded","message":"slow"}},"headers":{"retry-after":"7","x-codex-primary-used-percent":91,"x-flag":true,"x-empty":" ","x-obj":{}}}`},
		{payload: `{"type":"error","status":429,"body":{"detail":"no error node"},"error":{"message":"top"}}`},
		{payload: `{"type":"error","status":429,"error":{"code":"websocket_connection_limit_reached","message":"too many"}}`},
		{payload: `{"type":"error","status":503,"code":"websocket_connection_limit_reached"}`},
		{payload: `{"type":"error","status":1200,"error":{"message":"big status"}}`},
		{payload: `{"type":"error","status":0}`},
		{payload: `{"type":"error","error":{"message":"no status"}}`},
		{payload: `{"type":"response.failed","status":500}`},
		{payload: ` {"type":" error ","status":"502"}`},
	} {
		v := vector{Payload: c.payload, Cooling: c.cooling}
		err, ok := parseCodexWebsocketErrorWithCooling([]byte(c.payload), c.cooling)
		v.Matched = ok
		if ok {
			se := err.(statusErrWithHeaders)
			v.Status = se.StatusCode()
			v.Message = se.Error()
			if ra := se.RetryAfter(); ra != nil {
				ms := ra.Milliseconds()
				v.RetryAfterMS = &ms
			}
			v.CredentialScoped = se.IsCredentialScoped()
			if h := se.Headers(); h != nil {
				v.Headers = map[string]string{}
				for k := range h {
					v.Headers[k] = h.Get(k)
				}
			}
		}
		out = append(out, v)
	}
	data, err := json.MarshalIndent(out, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "codex_ws_errors.json"), append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
