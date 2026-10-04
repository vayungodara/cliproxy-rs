// Generates the plugin command-line golden by running Go's server binary (CLIProxyAPI
// 6fecc6e, cmd/server) with the recorder plugin: plugin flags registered before the flag
// parse, Go's parse errors for them, and the run handed to the plugins that own them
// (exit code, output, the execute request and the auth files they produce). Nothing
// leaves the machine; every case exits before the server would start.
//
// Usage: go run . <server binary> <recorder.so> <output.json>
package main

import (
	"bytes"
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"sort"
	"strings"
	"time"
)

type testCase struct {
	Name     string            `json:"name"`
	Config   string            `json:"config"`
	Register string            `json:"register,omitempty"`
	Execute  string            `json:"execute,omitempty"`
	Auths    map[string]string `json:"auths,omitempty"`
	Args     []string          `json:"args"`
	Result   *result           `json:"result,omitempty"`
}

type result struct {
	Exit    int               `json:"exit"`
	Stdout  []string          `json:"stdout"`
	Stderr  []string          `json:"stderr"`
	Request json.RawMessage   `json:"request"`
	Files   map[string]string `json:"files"`
}

func check(err error) {
	if err != nil {
		panic(err)
	}
}

const baseConfig = `config-version: 8
port: 0
auth-dir: AUTHDIR
plugins:
  enabled: true
  dir: PLUGINDIR
  configs:
    rec:
      enabled: ENABLED
      record: RECDIR
      label: a
      caps: command_line_plugin
`

func config(enabled bool) string {
	return strings.ReplaceAll(baseConfig, "ENABLED", fmt.Sprint(enabled))
}

func ok(v any) string {
	raw, err := json.Marshal(map[string]any{"ok": true, "result": v})
	check(err)
	return string(raw)
}

var flags = ok(map[string]any{"Flags": []map[string]any{
	{"Name": "mine", "Usage": "run mine", "Type": "bool"},
	{"Name": "count", "Usage": "how many", "Type": "int", "DefaultValue": "2"},
	{"Name": "ratio", "Usage": "a ratio", "Type": "float64", "DefaultValue": "0.25"},
	{"Name": "wait", "Usage": "how long", "Type": "duration"},
	{"Name": "name", "Usage": "a name", "Type": "string", "DefaultValue": "x"},
	{"Name": "config", "Usage": "clashes with a built-in flag", "Type": "string"},
	{"Name": "bad name", "Usage": "invalid", "Type": "bool"},
	{"Name": "odd", "Usage": "unsupported type", "Type": "uint"},
	{"Name": "late", "Usage": "invalid default", "Type": "int", "DefaultValue": "nope"},
}})

func cases() []testCase {
	out := func(stdout, stderr string, code int, auths ...map[string]any) string {
		resp := map[string]any{"Stdout": []byte(stdout), "Stderr": []byte(stderr), "ExitCode": code}
		if len(auths) > 0 {
			resp["Auths"] = auths
		}
		return ok(resp)
	}
	run := out("out-a\n", "err-a\n", 4)
	cfg := config(true)
	authData := map[string]any{"Provider": "Rec", "FileName": "rec-1.json", "Label": "L",
		"StorageJSON": []byte(`{"token":"t","expired":"2030-01-01T00:00:00Z","priority":"3"}`),
		"Metadata":    map[string]any{"email": "a@example.invalid", "base-url": "https://b.example"}}
	disabled := map[string]any{"Provider": "rec", "FileName": "gone.json", "Disabled": true, "StorageJSON": []byte(`{"k":1}`)}
	existing := map[string]any{"Provider": "rec", "FileName": "kept.json", "StorageJSON": []byte(`{"k":2}`)}
	return []testCase{
		{Name: "int and false bool trigger", Config: cfg, Register: flags, Execute: run, Args: []string{"-config", "CONFIG", "-count", "7", "-mine=false"}},
		{Name: "every kind, double dash, last value wins", Config: cfg, Register: flags, Execute: out("done", "", 0),
			Args: []string{"--config=CONFIG", "--count=1", "-count", "-3", "-ratio", "1e3", "-wait=90s", "--name", "-tui", "-mine"}},
		{Name: "flags after an operand are ignored", Config: cfg, Register: flags, Execute: run, Args: []string{"-config", "CONFIG", "-mine", "operand", "-count", "x"}},
		{Name: "invalid int", Config: cfg, Register: flags, Execute: run, Args: []string{"-config", "CONFIG", "-count", "x"}},
		{Name: "invalid bool", Config: cfg, Register: flags, Execute: run, Args: []string{"-config", "CONFIG", "-mine=maybe"}},
		{Name: "invalid duration", Config: cfg, Register: flags, Execute: run, Args: []string{"-config", "CONFIG", "-wait", "5"}},
		{Name: "missing argument", Config: cfg, Register: flags, Execute: run, Args: []string{"-config", "CONFIG", "-count"}},
		{Name: "plugin disabled", Config: config(false), Register: flags, Execute: run, Args: []string{"-config", "CONFIG", "-mine"}},
		{Name: "execute fails", Config: cfg, Register: flags, Execute: `{"ok":false,"error":{"code":"x","message":"boom"}}`, Args: []string{"-config", "CONFIG", "-mine"}},
		{Name: "auths saved", Config: cfg, Register: flags, Execute: out("logged in", "", 0, authData, disabled, existing),
			Auths: map[string]string{"kept.json": `{"k": 2, "type": "rec", "disabled": false}`}, Args: []string{"-config", "CONFIG", "-mine"}},
		{Name: "auths ignored on failure", Config: cfg, Register: flags, Execute: out("", "nope\n", 5, authData), Args: []string{"-config", "CONFIG", "-mine"}},
		{Name: "invalid auth", Config: cfg, Register: flags, Execute: out("partial", "", 0, map[string]any{"Provider": " "}), Args: []string{"-config", "CONFIG", "-mine"}},
	}
}

var logLine = regexp.MustCompile(`^\[\d{4}-\d\d-\d\d \d\d:\d\d:\d\d\] `)

// lines drops log lines and the banner; after a parse error (exit 2) only the error
// itself is kept, since the usage text that follows is the binary's own.
func lines(text string, exit int) []string {
	out := []string{}
	for _, line := range strings.Split(strings.TrimRight(text, "\n"), "\n") {
		if line == "" || logLine.MatchString(line) || strings.HasPrefix(line, "CLIProxyAPI Version:") {
			continue
		}
		out = append(out, line)
		if exit == 2 {
			break
		}
	}
	return out
}

func main() {
	if len(os.Args) != 4 {
		fmt.Fprintln(os.Stderr, "usage: go run . <server binary> <recorder.so> <output.json>")
		os.Exit(2)
	}
	server, recorder := os.Args[1], os.Args[2]
	all := cases()
	for i := range all {
		c := &all[i]
		work, err := os.MkdirTemp("", "cpa-plugin-cli-golden-")
		check(err)
		dirs := map[string]string{"PLUGINDIR": filepath.Join(work, "plugins"), "RECDIR": filepath.Join(work, "rec"), "AUTHDIR": filepath.Join(work, "auths")}
		for _, dir := range dirs {
			check(os.MkdirAll(dir, 0o755))
		}
		data, err := os.ReadFile(recorder)
		check(err)
		check(os.WriteFile(filepath.Join(dirs["PLUGINDIR"], "rec.so"), data, 0o755))
		respond := filepath.Join(dirs["RECDIR"], "respond", "a")
		check(os.MkdirAll(respond, 0o755))
		check(os.WriteFile(filepath.Join(respond, "command_line.register.json"), []byte(c.Register), 0o644))
		check(os.WriteFile(filepath.Join(respond, "command_line.execute.json"), []byte(c.Execute), 0o644))
		for name, body := range c.Auths {
			check(os.WriteFile(filepath.Join(dirs["AUTHDIR"], name), []byte(body), 0o600))
		}
		configPath := filepath.Join(work, "config.yaml")
		text := c.Config
		for k, v := range dirs {
			text = strings.ReplaceAll(text, k, v)
		}
		check(os.WriteFile(configPath, []byte(text), 0o600))
		args := make([]string, len(c.Args))
		for j, a := range c.Args {
			args[j] = strings.ReplaceAll(a, "CONFIG", configPath)
		}
		cmd := exec.Command(server, args...)
		cmd.Dir = work
		var stdout, stderr bytes.Buffer
		cmd.Stdout, cmd.Stderr = &stdout, &stderr
		check(cmd.Start())
		done := make(chan error, 1)
		go func() { done <- cmd.Wait() }()
		select {
		case <-done:
		case <-time.After(60 * time.Second):
			_ = cmd.Process.Kill()
			panic("case " + c.Name + " did not exit")
		}
		normalize := func(s string) string {
			s = strings.ReplaceAll(s, configPath, "CONFIG")
			for k, v := range dirs {
				s = strings.ReplaceAll(s, v, k)
			}
			return strings.ReplaceAll(s, work, "WORK")
		}
		exit := cmd.ProcessState.ExitCode()
		res := &result{Exit: exit, Stdout: lines(normalize(stdout.String()), exit), Stderr: lines(normalize(stderr.String()), exit), Request: json.RawMessage("null"), Files: map[string]string{}}
		if records, errRead := os.ReadFile(filepath.Join(dirs["RECDIR"], "a.jsonl")); errRead == nil {
			for _, line := range strings.Split(strings.TrimSpace(string(records)), "\n") {
				var rec map[string]string
				check(json.Unmarshal([]byte(line), &rec))
				if rec["method"] == "command_line.execute" {
					var req map[string]any
					check(json.Unmarshal([]byte(normalize(rec["request"])), &req))
					req["Program"] = "PROGRAM"
					raw, errMarshal := json.Marshal(req)
					check(errMarshal)
					res.Request = raw
				}
			}
		}
		entries, err := os.ReadDir(dirs["AUTHDIR"])
		check(err)
		names := []string{}
		for _, e := range entries {
			names = append(names, e.Name())
		}
		sort.Strings(names)
		for _, name := range names {
			info, errInfo := os.Stat(filepath.Join(dirs["AUTHDIR"], name))
			check(errInfo)
			body, errBody := os.ReadFile(filepath.Join(dirs["AUTHDIR"], name))
			check(errBody)
			res.Files[name] = fmt.Sprintf("%o %s", info.Mode().Perm(), body)
		}
		c.Result = res
		check(os.RemoveAll(work))
	}
	out, err := json.MarshalIndent(map[string]any{"cases": all}, "", "  ")
	check(err)
	check(os.WriteFile(os.Args[3], append(out, '\n'), 0o644))
}
