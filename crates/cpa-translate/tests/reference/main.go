// Generates byte-level translator goldens by running the pinned Go translators through
// sdk/translator's registry (the path executors use). It never calls a provider.
//
// For every requested pair it reads the pair's init.go registration, mines converter
// calls and JSON/SSE literals from the pair's Go tests, adds the shared edge matrices
// below, and records Go's output. Each case runs twice; JSON leaves that differ between
// runs (random IDs) or hold the current time are recorded as dynamic so the Rust test can
// check their shape instead of their value.
package main

import (
	"context"
	"encoding/json"
	"fmt"
	"go/ast"
	"go/parser"
	"go/token"
	"os"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"time"
	"unicode/utf8"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/registry"
	_ "github.com/router-for-me/CLIProxyAPI/v8/internal/translator"
	claudechat "github.com/router-for-me/CLIProxyAPI/v8/internal/translator/claude/openai/chat-completions"
	codexclaude "github.com/router-for-me/CLIProxyAPI/v8/internal/translator/codex/claude"
	geminiclaude "github.com/router-for-me/CLIProxyAPI/v8/internal/translator/gemini/claude"
	interactionsclaude "github.com/router-for-me/CLIProxyAPI/v8/internal/translator/interactions/claude"
	openaiclaude "github.com/router-for-me/CLIProxyAPI/v8/internal/translator/openai/claude"
	sdk "github.com/router-for-me/CLIProxyAPI/v8/sdk/translator"
	"github.com/tidwall/gjson"
)

type dynamic struct {
	Out    int    `json:"out"`
	Data   int    `json:"data"`
	Path   string `json:"path"`
	Prefix string `json:"prefix,omitempty"`
	Time   bool   `json:"time,omitempty"`
}

type fixture struct {
	Name       string     `json:"name"`
	Path       string     `json:"path"`
	Model      string     `json:"model"`
	Stream     bool       `json:"stream,omitempty"`
	Bytes      bool       `json:"bytes,omitempty"`
	Input      string     `json:"input,omitempty"`
	Original   string     `json:"original,omitempty"`
	Translated string     `json:"translated,omitempty"`
	Lines      []string   `json:"lines,omitempty"`
	Count      int64      `json:"count,omitempty"`
	Outputs    [][]string `json:"outputs"`
	Finalize   bool       `json:"finalize,omitempty"`
	ToolError  bool       `json:"tool_error,omitempty"`
	// Variants are every distinct output of a request whose Go output is not stable
	// (map iteration order); Rust must match one of them modulo object key order.
	Variants []string `json:"variants,omitempty"`
	// Unordered streams emit in Go map order; every distinct output over 25 runs is
	// recorded in StreamVariants (JSON-encoded output lists).
	Unordered      bool      `json:"unordered,omitempty"`
	StreamVariants []string  `json:"stream_variants,omitempty"`
	Dynamic        []dynamic `json:"dynamic,omitempty"`
	key            string
}

type registration struct {
	dir, client, upstream                  string
	request, stream, nonStream, tokenCount string
}

var formats = map[string]string{"OpenAI": "openai", "OpenaiResponse": "openai-response", "Claude": "claude", "Gemini": "gemini",
	"Codex": "codex", "Antigravity": "antigravity", "Interactions": "interactions"}

func registrations(root string) []registration {
	var out []registration
	_ = filepath.Walk(filepath.Join(root, "internal/translator"), func(path string, info os.FileInfo, err error) error {
		if err != nil || info.Name() != "init.go" {
			return nil
		}
		file, errParse := parser.ParseFile(token.NewFileSet(), path, nil, 0)
		if errParse != nil {
			panic(errParse)
		}
		ast.Inspect(file, func(n ast.Node) bool {
			call, ok := n.(*ast.CallExpr)
			if !ok || len(call.Args) != 4 {
				return true
			}
			sel, ok := call.Fun.(*ast.SelectorExpr)
			if !ok || sel.Sel.Name != "Register" {
				return true
			}
			r := registration{dir: filepath.Dir(path)}
			r.client = formats[call.Args[0].(*ast.Ident).Name]
			r.upstream = formats[call.Args[1].(*ast.Ident).Name]
			r.request = call.Args[2].(*ast.Ident).Name
			for _, el := range call.Args[3].(*ast.CompositeLit).Elts {
				kv := el.(*ast.KeyValueExpr)
				name := kv.Value.(*ast.Ident).Name
				switch kv.Key.(*ast.Ident).Name {
				case "Stream":
					r.stream = name
				case "NonStream":
					r.nonStream = name
				case "TokenCount":
					r.tokenCount = name
				}
			}
			out = append(out, r)
			return true
		})
		return nil
	})
	return out
}

// ---------------------------------------------------------------------------------------
// Literal evaluation over Go test sources.

func eval(expr ast.Expr, env map[string]any) any {
	switch e := expr.(type) {
	case *ast.BasicLit:
		switch e.Kind {
		case token.STRING:
			s, _ := strconv.Unquote(e.Value)
			return s
		case token.INT:
			n, _ := strconv.ParseInt(e.Value, 0, 64)
			return n
		}
	case *ast.Ident:
		switch e.Name {
		case "true":
			return true
		case "false":
			return false
		case "nil":
			return nil
		}
		return env[e.Name]
	case *ast.ParenExpr:
		return eval(e.X, env)
	case *ast.CallExpr:
		// []byte("..."), string(x)
		if len(e.Args) == 1 {
			switch f := e.Fun.(type) {
			case *ast.ArrayType, *ast.Ident:
				_ = f
				return eval(e.Args[0], env)
			}
		}
	case *ast.BinaryExpr:
		if e.Op == token.ADD {
			a, okA := eval(e.X, env).(string)
			b, okB := eval(e.Y, env).(string)
			if okA && okB {
				return a + b
			}
		}
	case *ast.CompositeLit:
		values := []string{}
		for _, v := range e.Elts {
			s, ok := eval(v, env).(string)
			if !ok {
				return nil
			}
			values = append(values, s)
		}
		return values
	}
	return nil
}

type mined struct {
	requests  []fixture
	streams   []fixture
	nonStream []fixture
}

func mentions(body ast.Node, name string) bool {
	found := false
	ast.Inspect(body, func(n ast.Node) bool {
		if id, ok := n.(*ast.Ident); ok && id.Name == name {
			found = true
		}
		return !found
	})
	return name != "" && found
}

func isSSE(s string) bool {
	t := strings.TrimSpace(s)
	return strings.HasPrefix(t, "data:") || strings.HasPrefix(t, "event:")
}

func mine(r registration, defaultModel string) mined {
	var m mined
	files, _ := filepath.Glob(filepath.Join(r.dir, "*_test.go"))
	sort.Strings(files)
	global := map[string]any{}
	parsed := map[string]*ast.File{}
	fset := token.NewFileSet()
	for _, f := range files {
		file, err := parser.ParseFile(fset, f, nil, 0)
		if err != nil {
			panic(err)
		}
		parsed[f] = file
		for _, decl := range file.Decls {
			gen, ok := decl.(*ast.GenDecl)
			if !ok {
				continue
			}
			for _, spec := range gen.Specs {
				vs, ok := spec.(*ast.ValueSpec)
				if !ok {
					continue
				}
				for i, name := range vs.Names {
					if i < len(vs.Values) {
						global[name.Name] = eval(vs.Values[i], global)
					}
				}
			}
		}
	}
	seen := map[string]bool{}
	for _, f := range files {
		for _, decl := range parsed[f].Decls {
			fn, ok := decl.(*ast.FuncDecl)
			if !ok || fn.Body == nil || !strings.HasPrefix(fn.Name.Name, "Test") {
				continue
			}
			env := map[string]any{}
			for k, v := range global {
				env[k] = v
			}
			model := defaultModel
			streams := map[string][]string{}
			var streamOrder []string
			usesRequest := mentions(fn.Body, r.request)
			usesStream := mentions(fn.Body, r.stream)
			usesNonStream := mentions(fn.Body, r.nonStream)
			var literals []string
			ast.Inspect(fn.Body, func(n ast.Node) bool {
				switch node := n.(type) {
				case *ast.AssignStmt:
					for i, right := range node.Rhs {
						if i < len(node.Lhs) {
							if id, ok := node.Lhs[i].(*ast.Ident); ok {
								env[id.Name] = eval(right, env)
							}
						}
					}
				case *ast.ValueSpec:
					for i, name := range node.Names {
						if i < len(node.Values) {
							env[name.Name] = eval(node.Values[i], env)
						}
					}
				case *ast.CallExpr:
					id, ok := node.Fun.(*ast.Ident)
					if !ok {
						break
					}
					switch {
					case id.Name == r.request && len(node.Args) == 3:
						mdl, okM := eval(node.Args[0], env).(string)
						input, okI := eval(node.Args[1], env).(string)
						stream, _ := eval(node.Args[2], env).(bool)
						if okM {
							model = mdl
						}
						if okI {
							key := "req|" + mdl + "|" + input + "|" + strconv.FormatBool(stream)
							if !seen[key] {
								seen[key] = true
								m.requests = append(m.requests, fixture{Name: fmt.Sprintf("%s:%d", fn.Name.Name, fset.Position(node.Pos()).Line), Path: "request", Model: firstNonEmpty(mdl, model), Stream: stream, Input: input})
							}
						}
					case id.Name == r.stream && len(node.Args) == 6:
						raw, ok := eval(node.Args[4], env).(string)
						if !ok {
							break
						}
						key := exprKey(node.Args[5])
						if _, exists := streams[key]; !exists {
							streamOrder = append(streamOrder, key)
						}
						streams[key] = append(streams[key], raw)
					case id.Name == r.nonStream && len(node.Args) == 6:
						raw, ok := eval(node.Args[4], env).(string)
						if !ok {
							break
						}
						orig, _ := eval(node.Args[2], env).(string)
						req, _ := eval(node.Args[3], env).(string)
						key := "ns|" + raw + "|" + orig
						if !seen[key] {
							seen[key] = true
							m.nonStream = append(m.nonStream, fixture{Name: fmt.Sprintf("%s:%d", fn.Name.Name, fset.Position(node.Pos()).Line), Path: "non_stream", Model: model, Input: raw, Original: orig, Translated: req})
						}
					}
				case *ast.BasicLit, *ast.BinaryExpr, *ast.CompositeLit:
					switch v := eval(node.(ast.Expr), env).(type) {
					case string:
						literals = append(literals, v)
					case []string:
						if len(v) > 1 && isSSE(v[0]) && usesStream {
							key := "lit|" + strings.Join(v, "\x00")
							if !seen[key] {
								seen[key] = true
								m.streams = append(m.streams, fixture{Name: fmt.Sprintf("%s:%d:events", fn.Name.Name, fset.Position(node.Pos()).Line), Path: "stream", Model: model, Lines: v})
							}
						}
					}
				}
				return true
			})
			for _, key := range streamOrder {
				lines := streams[key]
				sk := "stream|" + strings.Join(lines, "\x00")
				if !seen[sk] {
					seen[sk] = true
					m.streams = append(m.streams, fixture{Name: fn.Name.Name + ":calls", Path: "stream", Model: model, Lines: lines})
				}
			}
			for _, lit := range literals {
				trimmed := strings.TrimSpace(lit)
				if usesRequest && strings.HasPrefix(trimmed, "{") && gjson.Valid(lit) {
					key := "req|" + model + "|" + lit + "|false"
					if !seen[key] {
						seen[key] = true
						m.requests = append(m.requests, fixture{Name: fn.Name.Name + ":literal", Path: "request", Model: model, Input: lit})
					}
				}
				if usesNonStream && (strings.Contains(lit, "\ndata:") || strings.HasPrefix(trimmed, "data:") || (strings.HasPrefix(trimmed, "{") && gjson.Valid(lit))) {
					key := "ns|" + lit + "|"
					if !seen[key] {
						seen[key] = true
						m.nonStream = append(m.nonStream, fixture{Name: fn.Name.Name + ":literal", Path: "non_stream", Model: model, Input: lit})
					}
				}
			}
		}
	}
	return m
}

func exprKey(e ast.Expr) string {
	switch v := e.(type) {
	case *ast.UnaryExpr:
		return exprKey(v.X)
	case *ast.Ident:
		return v.Name
	case *ast.SelectorExpr:
		return exprKey(v.X) + "." + v.Sel.Name
	}
	return "?"
}

func firstNonEmpty(values ...string) string {
	for _, v := range values {
		if v != "" {
			return v
		}
	}
	return ""
}

// ---------------------------------------------------------------------------------------
// Running and dynamic-value detection.

// lastToolError is the ToolInputError state run left behind (Go's apply_patch contract).
var lastToolError bool

func toolInputFailed(param any) bool {
	state, ok := param.(interface{ ToolInputError() error })
	return ok && state.ToolInputError() != nil
}

func run(r registration, f fixture) [][]string {
	lastToolError = false
	ctx := context.Background()
	if r.upstream == "antigravity" {
		// The Antigravity executor streams with an empty `alt` (antigravity_executor_stream.go).
		ctx = context.WithValue(ctx, "alt", "")
	}
	from, to := sdk.FromString(r.client), sdk.FromString(r.upstream)
	var orig, req []byte
	if f.Original != "" {
		orig = []byte(f.Original)
	}
	if f.Translated != "" {
		req = []byte(f.Translated)
	}
	switch f.Path {
	case "request":
		return [][]string{{string(sdk.TranslateRequest(from, to, f.Model, []byte(f.Input), f.Stream))}}
	case "request_compat":
		switch r.client + ":" + r.upstream {
		case "openai:claude":
			return [][]string{{string(claudechat.ConvertOpenAIRequestToClaudeWithCompat(f.Model, []byte(f.Input), f.Stream))}}
		case "claude:openai":
			return [][]string{{string(openaiclaude.ConvertClaudeRequestToOpenAIWithCompat(f.Model, []byte(f.Input), f.Stream))}}
		case "claude:gemini":
			return [][]string{{string(geminiclaude.ConvertClaudeRequestToGeminiWithCompat(f.Model, []byte(f.Input), f.Stream))}}
		case "claude:codex":
			return [][]string{{string(codexclaude.ConvertClaudeRequestToCodexWithCompat(f.Model, []byte(f.Input), f.Stream))}}
		case "claude:interactions":
			return [][]string{{string(interactionsclaude.ConvertClaudeRequestToInteractionsWithCompat(f.Model, []byte(f.Input), f.Stream))}}
		}
		panic("no compat request for " + r.client + ":" + r.upstream)
	case "request_envelope":
		// The executor's ResolvedModelInfo with native web search on.
		webSearch := true
		info := &registry.ModelInfo{ID: f.Model, NativeCapabilities: &registry.NativeCapabilities{WebSearch: &webSearch}}
		env := sdk.TranslateRequestEnvelope(ctx, from, to, sdk.RequestEnvelope{Format: from, Model: f.Model, Stream: f.Stream, Body: []byte(f.Input), ModelInfo: info})
		return [][]string{{string(env.Body)}}
	case "non_stream":
		var param any
		out := sdk.TranslateNonStream(ctx, to, from, f.Model, orig, req, []byte(f.Input), &param)
		lastToolError = toolInputFailed(param)
		return [][]string{{string(out)}}
	case "token_count":
		return [][]string{{string(sdk.TranslateTokenCount(ctx, to, from, f.Count, []byte(f.Input)))}}
	case "stream":
		var param any
		out := [][]string{}
		for _, line := range f.Lines {
			chunks := sdk.TranslateStream(ctx, to, from, f.Model, orig, req, []byte(line), &param)
			strs := []string{}
			for _, c := range chunks {
				strs = append(strs, string(c))
			}
			out = append(out, strs)
		}
		if f.Finalize {
			// helps.FinalizeApplyPatchStream at transport EOF.
			strs := []string{}
			if state, ok := param.(interface{ FinalizeToolInput() [][]byte }); ok {
				for _, c := range state.FinalizeToolInput() {
					strs = append(strs, string(c))
				}
			}
			out = append(out, strs)
		}
		lastToolError = toolInputFailed(param)
		return out
	}
	panic("unknown path " + f.Path)
}

// docs splits one output into its JSON documents: the output itself, or each `data:`
// payload of an SSE chunk. The index is the line number, or -1 for a bare document.
func docs(s string) map[int]string {
	out := map[int]string{}
	if gjson.Valid(s) {
		out[-1] = s
		return out
	}
	for i, line := range strings.Split(s, "\n") {
		if strings.HasPrefix(line, "data:") {
			payload := strings.TrimSpace(line[5:])
			if gjson.Valid(payload) {
				out[i] = payload
			}
		}
	}
	return out
}

func escape(key string) string { return gjson.Escape(key) }

func join(path, key string) string {
	if path == "" {
		return key
	}
	return path + "." + key
}

func digitFreePrefix(a, b string) string {
	i := 0
	for i < len(a) && i < len(b) && a[i] == b[i] && !(a[i] >= '0' && a[i] <= '9') {
		i++
	}
	return a[:i]
}

func walk(a, b gjson.Result, path string, start, end int64, add func(path, prefix string, isTime bool)) {
	if a.IsObject() && b.IsObject() {
		a.ForEach(func(k, v gjson.Result) bool {
			walk(v, b.Get(escape(k.String())), join(path, escape(k.String())), start, end, add)
			return true
		})
		return
	}
	if a.IsArray() && b.IsArray() {
		aa, bb := a.Array(), b.Array()
		for i := range aa {
			if i < len(bb) {
				walk(aa[i], bb[i], join(path, strconv.Itoa(i)), start, end, add)
			}
		}
		return
	}
	if a.Type == gjson.Number {
		n := a.Int()
		if (n >= start-2 && n <= end+2) || (n >= (start-2)*1000 && n <= (end+2)*1000) {
			add(path, "", true)
			return
		}
	}
	if a.Type == gjson.String {
		// RFC3339 wall-clock stamps (interaction created/updated).
		if t, err := time.Parse(time.RFC3339Nano, a.String()); err == nil && t.Unix() >= start-2 && t.Unix() <= end+2 {
			add(path, "", true)
			return
		}
	}
	if a.Raw != b.Raw {
		prefix := ""
		if a.Type == gjson.String && b.Type == gjson.String {
			prefix = digitFreePrefix(a.String(), b.String())
		}
		add(path, prefix, false)
	}
}

func record(r registration, f fixture) fixture {
	start := time.Now().Unix()
	a := run(r, f)
	f.ToolError = lastToolError
	if f.Unordered {
		seen := map[string]bool{}
		for i := 0; i < 25; i++ {
			raw, _ := json.Marshal(run(r, f))
			if !seen[string(raw)] {
				seen[string(raw)] = true
				f.StreamVariants = append(f.StreamVariants, string(raw))
			}
		}
		f.Outputs = a
		return f
	}
	if f.Path == "request" || f.Path == "request_compat" || f.Path == "request_envelope" {
		seen := map[string]bool{a[0][0]: true}
		variants := []string{a[0][0]}
		for i := 0; i < 24; i++ {
			if v := run(r, f)[0][0]; !seen[v] {
				seen[v] = true
				variants = append(variants, v)
			}
		}
		if len(variants) > 1 {
			f.Variants = variants
		}
	}
	time.Sleep(time.Millisecond)
	b := run(r, f)
	end := time.Now().Unix()
	f.Outputs = a
	out := 0
	for i := range a {
		for j := range a[i] {
			da, db := docs(a[i][j]), docs("")
			if i < len(b) && j < len(b[i]) {
				db = docs(b[i][j])
			}
			keys := []int{}
			for k := range da {
				keys = append(keys, k)
			}
			sort.Ints(keys)
			for _, k := range keys {
				walk(gjson.Parse(da[k]), gjson.Parse(db[k]), "", start, end, func(path, prefix string, isTime bool) {
					f.Dynamic = append(f.Dynamic, dynamic{Out: out, Data: k, Path: path, Prefix: prefix, Time: isTime})
				})
			}
			out++
		}
	}
	return f
}

// encode switches every byte field to one rune per byte when any field is not UTF-8.
func encode(f fixture) fixture {
	fields := []*string{&f.Input, &f.Original, &f.Translated}
	for i := range f.Lines {
		fields = append(fields, &f.Lines[i])
	}
	for i := range f.Variants {
		fields = append(fields, &f.Variants[i])
	}
	for i := range f.Outputs {
		for j := range f.Outputs[i] {
			fields = append(fields, &f.Outputs[i][j])
		}
	}
	valid := true
	for _, p := range fields {
		valid = valid && utf8.ValidString(*p)
	}
	if valid {
		return f
	}
	f.Bytes = true
	for _, p := range fields {
		r := make([]rune, len(*p))
		for i := 0; i < len(*p); i++ {
			r[i] = rune((*p)[i])
		}
		*p = string(r)
	}
	return f
}

func main() {
	if len(os.Args) < 4 {
		panic("usage: generate REFERENCE_ROOT OUTPUT_DIR client:upstream...")
	}
	if os.Args[3] == "sdk" {
		sdkRegistry(os.Args[1], os.Args[2])
		return
	}
	if os.Args[3] == "helpers" {
		helperFixtures(filepath.Join(os.Args[2], "..", "go_helpers.json"))
		return
	}
	if os.Args[3] == "apply_patch_responses" {
		applyPatchResponses(os.Args[2])
		return
	}
	regs := registrations(os.Args[1])
	for _, want := range os.Args[3:] {
		var r *registration
		for i := range regs {
			if regs[i].client+":"+regs[i].upstream == want {
				r = &regs[i]
			}
		}
		if r == nil {
			panic("unregistered pair " + want)
		}
		model := defaultModels[r.upstream]
		m := mine(*r, model)
		cases := append(append(append([]fixture{}, m.requests...), m.streams...), m.nonStream...)
		cases = append(cases, matrix(*r, model)...)
		harvest := harvested(os.Args[1], *r, cases)
		cases = append(cases, harvest...)
		var out []fixture
		names := map[string]int{}
		for _, f := range cases {
			names[f.Name]++
			if names[f.Name] > 1 {
				f.Name = fmt.Sprintf("%s#%d", f.Name, names[f.Name])
			}
			out = append(out, encode(record(*r, f)))
		}
		raw, err := json.MarshalIndent(map[string]any{"client": r.client, "upstream": r.upstream, "token_count": r.tokenCount != "", "fixtures": out}, "", " ")
		if err != nil {
			panic(err)
		}
		path := filepath.Join(os.Args[2], r.client+"-"+r.upstream+".json")
		if err := os.WriteFile(path, append(raw, '\n'), 0o644); err != nil {
			panic(err)
		}
		fmt.Printf("%s: %d fixtures (%d mined requests, %d mined streams, %d mined non-stream, %d harvested)\n", want, len(out), len(m.requests), len(m.streams), len(m.nonStream), len(harvest))
	}
}
