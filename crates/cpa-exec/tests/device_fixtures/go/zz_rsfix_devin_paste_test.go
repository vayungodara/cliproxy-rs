package auth

// cliproxy-rs fixture generator: parseDevinManualPaste vectors (RSFIX_OUT=<dir>).

import (
	"encoding/json"
	"os"
	"path/filepath"
	"testing"
)

func TestRSFixDevinPaste(t *testing.T) {
	dir := os.Getenv("RSFIX_OUT")
	if dir == "" {
		t.Skip("RSFIX_OUT not set")
	}
	var pastes []map[string]string
	for _, s := range []string{"", "  \"eyJtoken\"  ", "devin-session-token$abc", "http://127.0.0.1:1/callback?code=c1&state=st", "http://127.0.0.1:1/callback?code=c1&state=other", "http://127.0.0.1:1/callback?code=c1", "?error=access_denied&error_description=nope", "http://h/cb?error=denied", "code=c2&state=st", "barecode_123", "has space", "a/b", "'quoted'", "x#y", "http://h/cb#code=frag&state=st", "localhost:1/cb?state=st"} {
		code, token, err := parseDevinManualPaste(s, "st")
		entry := map[string]string{"input": s, "code": code, "token": token}
		if err != nil {
			entry["error"] = err.Error()
		}
		pastes = append(pastes, entry)
	}
	raw, _ := json.MarshalIndent(pastes, "", "  ")
	if err := os.MkdirAll(filepath.Join(dir, "devin"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "devin", "paste_vectors.json"), append(raw, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
