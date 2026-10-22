package cmd

// Records what CLIProxyAPI v8.0.35 (ae3ee3be) does for `-siwc-bind` (7b0319fb) before
// and after its OAuth flow: ResolveSIWCBindPath, ensureSIWCDeviceIDFromFile and
// saveSIWCToCodexCredentialFile. See README.md.

import (
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/auth/codex"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/misc"
)

type rsfixBindCase struct {
	Name string `json:"name"`
	// Files are created under the auth-dir ($DIR) before the call; a value starting
	// with "->" makes a symlink to that target ($DIR and $OUT expand), "/" a directory.
	Files map[string]string `json:"files,omitempty"`
	// Outside files live in a second directory ($OUT).
	Outside map[string]string `json:"outside,omitempty"`
	AuthDir string            `json:"auth_dir"`
	Target  string            `json:"target"`
	// Result is the resolved path ($DIR substituted), or the output file.
	Result string `json:"result,omitempty"`
	// Error is the message; Cause marks a wrapped error, cut from the message.
	Error string `json:"error,omitempty"`
	Cause bool   `json:"cause,omitempty"`
	// Device is ensureSIWCDeviceIDFromFile's result, or "generated" for a fresh UUID.
	Device string `json:"device,omitempty"`
	// Listing is the auth-dir's entries after a save (the lock file included).
	Listing []string `json:"listing,omitempty"`
}

func rsfixBindErr(c *rsfixBindCase, err error, dir, out string) {
	msg := err.Error()
	if inner := errors.Unwrap(err); inner != nil {
		msg = strings.TrimSuffix(msg, ": "+inner.Error())
		c.Cause = true
	}
	c.Error = strings.ReplaceAll(strings.ReplaceAll(msg, dir, "$DIR"), out, "$OUT")
}

func rsfixBindSetup(t *testing.T, c *rsfixBindCase) (string, string) {
	root := t.TempDir()
	dir, out := filepath.Join(root, "auth"), filepath.Join(root, "out")
	_ = os.MkdirAll(dir, 0o700)
	_ = os.MkdirAll(out, 0o700)
	write := func(base string, files map[string]string) {
		for name, content := range files {
			path := filepath.Join(base, name)
			switch {
			case strings.HasPrefix(content, "->"):
				target := strings.ReplaceAll(strings.ReplaceAll(content[2:], "$DIR", dir), "$OUT", out)
				if err := os.Symlink(target, path); err != nil {
					t.Fatal(err)
				}
			case content == "/":
				_ = os.MkdirAll(path, 0o700)
			default:
				_ = os.MkdirAll(filepath.Dir(path), 0o700)
				if err := os.WriteFile(path, []byte(content), 0o600); err != nil {
					t.Fatal(err)
				}
			}
		}
	}
	write(out, c.Outside)
	write(dir, c.Files)
	return dir, out
}

func rsfixBindExpand(s, dir, out string) string {
	return strings.ReplaceAll(strings.ReplaceAll(s, "$DIR", dir), "$OUT", out)
}

const rsfixBindCodex = `{"type":"codex","access_token":"at","refresh_token":"rt"}`

func TestRSFixSIWCBind(t *testing.T) {
	var resolve, device, save []rsfixBindCase

	resolveCases := []rsfixBindCase{
		{Name: "bare_name", Files: map[string]string{"c.json": rsfixBindCodex}, AuthDir: "$DIR", Target: " c.json "},
		{Name: "absolute", Files: map[string]string{"c.json": rsfixBindCodex}, AuthDir: "$DIR", Target: "$DIR/c.json"},
		{Name: "auth_dir_dotdot", Files: map[string]string{"c.json": rsfixBindCodex}, AuthDir: "$DIR/../auth", Target: "c.json"},
		{Name: "symlink_inside", Files: map[string]string{"c.json": rsfixBindCodex, "l.json": "->$DIR/c.json"}, AuthDir: "$DIR", Target: "l.json"},
		{Name: "auth_dir_alias_bare", Outside: map[string]string{"alias": "->$DIR"}, Files: map[string]string{"c.json": rsfixBindCodex}, AuthDir: "$OUT/alias", Target: "c.json"},
		{Name: "auth_dir_alias_absolute", Outside: map[string]string{"alias": "->$DIR"}, Files: map[string]string{"c.json": rsfixBindCodex}, AuthDir: "$OUT/alias", Target: "$OUT/alias/c.json"},
		// Go cleans the auth-dir lexically first: link/.. is out, not the parent of link's target.
		{Name: "auth_dir_lexical_dotdot", Outside: map[string]string{"link": "->$DIR/sub", "auth/c.json": rsfixBindCodex}, Files: map[string]string{"sub": "/", "auth/c.json": rsfixBindCodex}, AuthDir: "$OUT/link/../auth", Target: "c.json"},
		{Name: "traversal", Outside: map[string]string{"c.json": rsfixBindCodex}, AuthDir: "$DIR", Target: "../out/c.json"},
		{Name: "no_type", Files: map[string]string{"c.json": `{"hello":"world","access_token":"a","refresh_token":"r"}`}, AuthDir: "$DIR", Target: "c.json"},
		{Name: "empty", AuthDir: "$DIR", Target: "  "},
		{Name: "no_auth_dir", AuthDir: " ", Target: "c.json"},
		{Name: "auth_dir_missing", AuthDir: "$DIR/nope", Target: "c.json"},
		{Name: "relative_path", Files: map[string]string{"s": "/"}, AuthDir: "$DIR", Target: "s/c.json"},
		{Name: "backslash", AuthDir: "$DIR", Target: `s\c.json`},
		{Name: "dot", AuthDir: "$DIR", Target: "."},
		{Name: "dotdot", AuthDir: "$DIR", Target: ".."},
		{Name: "missing", AuthDir: "$DIR", Target: "missing.json"},
		{Name: "outside_absolute", Outside: map[string]string{"c.json": rsfixBindCodex}, AuthDir: "$DIR", Target: "$OUT/c.json"},
		{Name: "symlink_outside", Outside: map[string]string{"c.json": rsfixBindCodex}, Files: map[string]string{"l.json": "->$OUT/c.json"}, AuthDir: "$DIR", Target: "l.json"},
		{Name: "auth_dir_itself", AuthDir: "$DIR", Target: "$DIR"},
		{Name: "directory", Files: map[string]string{"d.json": "/"}, AuthDir: "$DIR", Target: "d.json"},
		{Name: "invalid_json", Files: map[string]string{"c.json": `{"type":`}, AuthDir: "$DIR", Target: "c.json"},
		{Name: "array", Files: map[string]string{"c.json": `[]`}, AuthDir: "$DIR", Target: "c.json"},
		{Name: "null", Files: map[string]string{"c.json": `null`}, AuthDir: "$DIR", Target: "c.json"},
		{Name: "type_spaced_upper", Files: map[string]string{"c.json": `{"type":" CODEX ","access_token":"a","refresh_token":"r"}`}, AuthDir: "$DIR", Target: "c.json"},
		{Name: "type_claude", Files: map[string]string{"c.json": `{"type":"claude","access_token":"a","refresh_token":"r"}`}, AuthDir: "$DIR", Target: "c.json"},
		{Name: "type_number", Files: map[string]string{"c.json": `{"type":5,"access_token":"a","refresh_token":"r"}`}, AuthDir: "$DIR", Target: "c.json"},
		{Name: "no_access", Files: map[string]string{"c.json": `{"type":"codex","refresh_token":"r"}`}, AuthDir: "$DIR", Target: "c.json"},
		{Name: "blank_refresh", Files: map[string]string{"c.json": `{"type":"codex","access_token":"a","refresh_token":" \t"}`}, AuthDir: "$DIR", Target: "c.json"},
		{Name: "numeric_access", Files: map[string]string{"c.json": `{"type":"codex","access_token":1,"refresh_token":"r"}`}, AuthDir: "$DIR", Target: "c.json"},
		{Name: "duplicate_type", Files: map[string]string{"c.json": `{"type":"codex","type":"claude","access_token":"a","refresh_token":"r"}`}, AuthDir: "$DIR", Target: "c.json"},
	}
	for _, c := range resolveCases {
		dir, out := rsfixBindSetup(t, &c)
		cfg := &config.Config{AuthDir: rsfixBindExpand(c.AuthDir, dir, out)}
		path, err := ResolveSIWCBindPath(cfg, rsfixBindExpand(c.Target, dir, out))
		if err != nil {
			rsfixBindErr(&c, err, dir, out)
		} else {
			c.Result = strings.ReplaceAll(strings.ReplaceAll(path, dir, "$DIR"), out, "$OUT")
		}
		resolve = append(resolve, c)
	}

	deviceCases := []rsfixBindCase{
		{Name: "kept_lowered", Files: map[string]string{"c.json": `{"siwc":{"device_id":" E61BBE28-07EF-466D-8E5D-A344F94AB305 "}}`}},
		{Name: "invalid", Files: map[string]string{"c.json": `{"siwc":{"device_id":"e61bbe28-07ef-466d-8e5d-a344f94ab30"}}`}},
		{Name: "number", Files: map[string]string{"c.json": `{"siwc":{"device_id":7}}`}},
		{Name: "siwc_empty", Files: map[string]string{"c.json": `{"siwc":{}}`}},
		{Name: "siwc_string", Files: map[string]string{"c.json": `{"siwc":"x"}`}},
		{Name: "none", Files: map[string]string{"c.json": `{}`}},
		{Name: "invalid_json", Files: map[string]string{"c.json": `{`}},
	}
	for _, c := range deviceCases {
		dir, out := rsfixBindSetup(t, &c)
		id, err := ensureSIWCDeviceIDFromFile(filepath.Join(dir, "c.json"))
		switch {
		case err != nil:
			rsfixBindErr(&c, err, dir, out)
		case strings.Contains(c.Files["c.json"], strings.ToUpper(id)) || strings.Contains(c.Files["c.json"], id):
			c.Device = id
		case codex.IsValidUUID(id) && id == strings.ToLower(id):
			c.Device = "generated"
		default:
			c.Device = "unexpected " + id
		}
		device = append(device, c)
	}

	tokens := &codex.CodexSIWCTokenData{
		AccessToken:  "siwc-at",
		RefreshToken: "siwc-rt",
		IDToken:      "siwc-id",
		ClientID:     "issued-client",
		Scope:        "openid chatgpt.tokens.use.direct",
		Scopes:       []string{"openid", "chatgpt.tokens.use.direct"},
		Expire:       "2026-10-22T18:00:00Z",
		LastRefresh:  "2026-10-22T17:00:00Z",
		DeviceID:     "e61bbe28-07ef-466d-8e5d-a344f94ab305",
	}
	saveCases := []rsfixBindCase{
		{Name: "replaces_siwc", Files: map[string]string{"c.json": `{
  "type": "codex", "access_token": "at", "refresh_token": "rt",
  "use_siwc": false, "siwc": {"access_token": "old", "extra": 1},
  "email": "a<b>&c@x.io", "line": "\u2028",
  "n_big": 12345678901234567890, "n_exp": 1e3, "n_frac": 1.50, "n_neg": -0.0, "n_tiny": 1e-7, "n_huge": 1e21,
  "nested": {"z": 1, "a": [true, null, {"y": "2", "b": 3.0}]},
  "dup": 1, "dup": 2
}`}},
		{Name: "invalid_json", Files: map[string]string{"c.json": `{"type":"codex"`}},
		{Name: "not_codex", Files: map[string]string{"c.json": `{"type":"gemini","access_token":"a","refresh_token":"r"}`}},
		{Name: "null", Files: map[string]string{"c.json": `null`}},
		{Name: "missing", Files: map[string]string{}},
		// canonicalLockPath trims the path: the lock of "c.json " is "c.json.lock".
		{Name: "trailing_space", Target: "c.json ", Files: map[string]string{"c.json ": rsfixBindCodex}},
	}
	for _, c := range saveCases {
		dir, out := rsfixBindSetup(t, &c)
		name := "c.json"
		if c.Target != "" {
			name = c.Target
		}
		path := filepath.Join(dir, name)
		if err := saveSIWCToCodexCredentialFile(path, tokens); err != nil {
			rsfixBindErr(&c, err, dir, out)
		} else {
			data, _ := os.ReadFile(path)
			c.Result = string(data)
		}
		entries, _ := os.ReadDir(dir)
		for _, e := range entries {
			c.Listing = append(c.Listing, e.Name())
		}
		save = append(save, c)
	}

	// misc.ParseOAuthCallback with the issued client ID a pasted SIWC callback carries.
	var callback []map[string]string
	for _, input := range []string{
		"http://127.0.0.1:1455/auth/callback?code=c&state=s&client_id=%20cid%20",
		"?code=c&state=s#client_id=frag",
		"code=c&state=s&client_id=q#client_id=frag",
		"http://x/cb#code=c&state=s&client_id=f",
		"?error=access_denied&error_description=nope&client_id=c",
		"?error_description=only&client_id=c",
		"?code=c%23s&client_id=",
	} {
		parsed, err := misc.ParseOAuthCallback(input)
		if err != nil {
			callback = append(callback, map[string]string{"input": input, "err": err.Error()})
			continue
		}
		callback = append(callback, map[string]string{"input": input, "code": parsed.Code, "state": parsed.State,
			"client_id": parsed.ClientID, "error": parsed.Error, "desc": parsed.ErrorDescription})
	}

	outDir := os.Getenv("RSFIX_OUT")
	if outDir == "" {
		t.Skip("RSFIX_OUT not set")
	}
	data, err := json.MarshalIndent(map[string]any{"resolve": resolve, "device": device, "save": save, "callback": callback}, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(outDir, "codex_siwc_bind_go.json"), append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
