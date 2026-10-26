package executor

// Go's url.Parse then URL.String(), and the xAI and Codex WebSocket URL builders that
// end in it, for the same inputs. Copy into internal/runtime/executor/ of CLIProxyAPI
// v8.0.35 (ae3ee3be) and run with RSFIX_OUT=<dir>; it writes go_url_string.json.
// Nothing is dialed.

import (
	"encoding/json"
	"net/url"
	"os"
	"path/filepath"
	"testing"
)

type rsfixURLResult struct {
	Out string `json:"out,omitempty"`
	Err string `json:"err,omitempty"`
}

type rsfixURLCase struct {
	In     string         `json:"in"`
	String rsfixURLResult `json:"string"`
	XAI    rsfixURLResult `json:"xai_ws"`
	Codex  rsfixURLResult `json:"codex_ws"`
}

func rsfixURLResultOf(out string, err error) rsfixURLResult {
	if err != nil {
		return rsfixURLResult{Err: err.Error()}
	}
	return rsfixURLResult{Out: out}
}

func TestRSFixURLString(t *testing.T) {
	inputs := []string{
		"https://example.com/backend/responses",
		" HTTP://127.0.0.1:9/v1/responses ",
		"HTTPS://Example.COM/a%2Fb/responses",
		"https://h/a%2fb",
		// Spaces.
		"https://h/a b/responses",
		"https://h/x?q=a b",
		"https://h/x#a b",
		// Non-ASCII path bytes, raw and escaped, and one that is not UTF-8.
		"https://h/café/responses",
		"https://h/caf%C3%A9/responses",
		"https://h/caf%e9/responses",
		// An empty fragment, and fragments Go keeps or escapes.
		"https://h/x#",
		"https://h/x?#",
		"https://h/x#a%2Fb",
		"https://h/x#a'b!(c)*",
		"https://h/x#a\"b",
		"https://h/x#a?b/c",
		// Escaped user info.
		"https://us%65r:p%40ss@h/x",
		"https://a@b:c:d@h/x",
		"https://user@h/x",
		"https://u:@h/x",
		"https://%3A:%2F@h/x",
		// Path bytes validEncoded keeps or rejects.
		"https://h/a'b!c(d)*e",
		"https://h/a\"b",
		"https://h/a b!c(d)*e'f",
		"https://h/a;b,c=d&e+f$g:h@i",
		"https://h/a[b]",
		"https://h/a%5Bb%5D",
		"https://h/*",
		"https://h",
		"https://h/x?",
		// Hosts.
		"https://例.x.ai/r",
		"https://%C3%A9.x.ai/r",
		"https://%ff.x.ai/r",
		"https://[fe80::1%25en0]:8443/r",
		"https://[fe80::1%25%65n0]/r",
		"https://:443/responses",
		"https://h:/r",
		// No host, omitted host, opaque.
		"https:///responses",
		"https:/responses",
		"https:example.com/responses",
		"https:example.com/responses?q#f",
		"wss://h/x",
		"ws://H/x",
		// Refusals.
		"https://h/a%zz",
		"https://%61pi.example/responses",
		"ftp://h/x",
		"FTP://h/x",
		"relay.example/responses",
	}
	cases := make([]rsfixURLCase, 0, len(inputs))
	for _, in := range inputs {
		c := rsfixURLCase{In: in}
		if u, err := url.Parse(in); err != nil {
			c.String = rsfixURLResult{Err: err.Error()}
		} else {
			c.String = rsfixURLResult{Out: u.String()}
		}
		c.XAI = rsfixURLResultOf(buildXAIResponsesWebsocketURL(in))
		c.Codex = rsfixURLResultOf(buildCodexResponsesWebsocketURL(in))
		cases = append(cases, c)
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
	if err := os.WriteFile(filepath.Join(dir, "go_url_string.json"), append(out, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
