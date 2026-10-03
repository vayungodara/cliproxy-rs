// Harvest instruments a scratch copy of CLIProxyAPI so `go test` records the inputs Go's
// own translator tests pass to the converters, including table-driven and helper-built
// inputs the static miner in ../main.go cannot evaluate.
//
// Usage: go run ./harvest COPY_ROOT
//
// It renames every converter registered in internal/translator/**/init.go (request,
// stream, non-stream), every exported `...WithCompat` request converter, and
// sdk/translator's TranslateRequest/TranslateStream/TranslateNonStream to
// harvestOrig_<Name>, and adds a same-signature wrapper that logs the call before
// forwarding it. Only calls made directly from a `_test.go` file are logged, with the
// enclosing Test function's name and the call line, as JSON lines in $HARVEST_OUT. The
// generator merges them when HARVEST_JSONL points at that file.
package main

import (
	"bytes"
	"fmt"
	"go/ast"
	"go/parser"
	"go/printer"
	"go/token"
	"os"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
)

const module = "github.com/router-for-me/CLIProxyAPI/v8"

const recorder = `// Package harvest logs converter calls made from Go tests (generated; scratch copy only).
package harvest

import (
	"context"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"runtime"
	"strings"
	"sync"
)

var mu sync.Mutex

func testName(function string) string {
	if i := strings.LastIndex(function, "/"); i >= 0 {
		function = function[i+1:]
	}
	parts := strings.Split(function, ".")
	for _, p := range parts[1:] {
		if strings.HasPrefix(p, "Test") && (len(p) == 4 || p[4] == '_' || (p[4] >= 'A' && p[4] <= 'Z')) {
			return p
		}
	}
	return ""
}

func encode(v any) any {
	if ctx, ok := v.(context.Context); ok {
		out := map[string]any{"ctx": true}
		if alt, ok := ctx.Value("alt").(string); ok {
			out["alt"] = alt
		}
		return out
	}
	rv := reflect.ValueOf(v)
	switch rv.Kind() {
	case reflect.String:
		return map[string]any{"s": rv.String()}
	case reflect.Bool:
		return map[string]any{"bool": rv.Bool()}
	case reflect.Int, reflect.Int64, reflect.Int32:
		return map[string]any{"i": rv.Int()}
	case reflect.Slice:
		if rv.Type().Elem().Kind() == reflect.Uint8 {
			return map[string]any{"b": base64.StdEncoding.EncodeToString(rv.Bytes()), "nil": rv.IsNil()}
		}
	case reflect.Ptr:
		return map[string]any{"p": fmt.Sprintf("%p", v)}
	}
	return map[string]any{"other": fmt.Sprintf("%T", v)}
}

// Begin logs one call when the wrapper's caller is a test file.
func Begin(fn string, args ...any) {
	path := os.Getenv("HARVEST_OUT")
	if path == "" {
		return
	}
	_, file, line, ok := runtime.Caller(2)
	if !ok || !strings.HasSuffix(file, "_test.go") {
		return
	}
	pcs := make([]uintptr, 64)
	frames := runtime.CallersFrames(pcs[:runtime.Callers(2, pcs)])
	test := ""
	for {
		frame, more := frames.Next()
		if test = testName(frame.Function); test != "" || !more {
			break
		}
	}
	if test == "" {
		return
	}
	encoded := make([]any, len(args))
	for i, a := range args {
		encoded[i] = encode(a)
	}
	raw, err := json.Marshal(map[string]any{"fn": fn, "file": filepath.Base(file), "line": line, "test": test, "args": encoded})
	if err != nil {
		panic(err)
	}
	mu.Lock()
	defer mu.Unlock()
	f, err := os.OpenFile(path, os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o644)
	if err != nil {
		panic(err)
	}
	defer f.Close()
	if _, err := f.Write(append(raw, '\n')); err != nil {
		panic(err)
	}
}
`

func main() {
	if len(os.Args) != 2 {
		panic("usage: harvest COPY_ROOT")
	}
	root := os.Args[1]
	if err := os.MkdirAll(filepath.Join(root, "internal/harvest"), 0o755); err != nil {
		panic(err)
	}
	if err := os.WriteFile(filepath.Join(root, "internal/harvest/harvest.go"), []byte(recorder), 0o644); err != nil {
		panic(err)
	}
	targets := map[string]map[string]bool{} // dir -> function names
	add := func(dir, name string) {
		if name == "" {
			return
		}
		if targets[dir] == nil {
			targets[dir] = map[string]bool{}
		}
		targets[dir][name] = true
	}
	_ = filepath.Walk(filepath.Join(root, "internal/translator"), func(path string, info os.FileInfo, err error) error {
		if err != nil || info.Name() != "init.go" {
			return nil
		}
		dir := filepath.Dir(path)
		file, errParse := parser.ParseFile(token.NewFileSet(), path, nil, 0)
		if errParse != nil {
			panic(errParse)
		}
		ast.Inspect(file, func(n ast.Node) bool {
			call, ok := n.(*ast.CallExpr)
			if !ok || len(call.Args) != 4 {
				return true
			}
			if sel, ok := call.Fun.(*ast.SelectorExpr); !ok || sel.Sel.Name != "Register" {
				return true
			}
			add(dir, call.Args[2].(*ast.Ident).Name)
			for _, el := range call.Args[3].(*ast.CompositeLit).Elts {
				kv := el.(*ast.KeyValueExpr)
				if key := kv.Key.(*ast.Ident).Name; key == "Stream" || key == "NonStream" {
					add(dir, kv.Value.(*ast.Ident).Name)
				}
			}
			return true
		})
		return nil
	})
	for dir := range targets {
		for _, decl := range funcDecls(dir) {
			if strings.HasSuffix(decl.Name.Name, "WithCompat") && decl.Name.IsExported() {
				add(dir, decl.Name.Name)
			}
		}
	}
	sdkDir := filepath.Join(root, "sdk/translator")
	for _, name := range []string{"TranslateRequest", "TranslateStream", "TranslateNonStream"} {
		add(sdkDir, name)
	}
	dirs := make([]string, 0, len(targets))
	for dir := range targets {
		dirs = append(dirs, dir)
	}
	sort.Strings(dirs)
	total := 0
	for _, dir := range dirs {
		total += instrument(root, dir, targets[dir])
	}
	fmt.Printf("instrumented %d functions in %d packages\n", total, len(dirs))
}

type decl struct {
	*ast.FuncDecl
	path string
	file *ast.File
	fset *token.FileSet
}

func funcDecls(dir string) []decl {
	files, _ := filepath.Glob(filepath.Join(dir, "*.go"))
	sort.Strings(files)
	var out []decl
	for _, path := range files {
		if strings.HasSuffix(path, "_test.go") || strings.HasPrefix(filepath.Base(path), "zz_harvest") {
			continue
		}
		fset := token.NewFileSet()
		file, err := parser.ParseFile(fset, path, nil, 0)
		if err != nil {
			panic(err)
		}
		for _, d := range file.Decls {
			if fn, ok := d.(*ast.FuncDecl); ok && fn.Recv == nil {
				out = append(out, decl{fn, path, file, fset})
			}
		}
	}
	return out
}

func importName(spec *ast.ImportSpec) string {
	if spec.Name != nil {
		return spec.Name.Name
	}
	path, _ := strconv.Unquote(spec.Path.Value)
	parts := strings.Split(path, "/")
	name := parts[len(parts)-1]
	if len(parts) > 1 && len(name) > 1 && name[0] == 'v' && strings.Trim(name[1:], "0123456789") == "" {
		name = parts[len(parts)-2]
	}
	return name
}

// instrument renames each target and writes the wrappers into zz_harvest_wrappers.go.
func instrument(root, dir string, names map[string]bool) int {
	renames := map[string][]int{} // file -> offsets of names to rename
	var wrappers bytes.Buffer
	imports := map[string]string{module + "/internal/harvest": ""}
	pkg := ""
	count := 0
	for _, d := range funcDecls(dir) {
		if !names[d.Name.Name] {
			continue
		}
		pkg = d.file.Name.Name
		renames[d.path] = append(renames[d.path], d.fset.Position(d.Name.Pos()).Offset)
		print := func(e ast.Expr) string {
			var b bytes.Buffer
			if err := printer.Fprint(&b, d.fset, e); err != nil {
				panic(err)
			}
			ast.Inspect(e, func(n ast.Node) bool {
				if sel, ok := n.(*ast.SelectorExpr); ok {
					if id, ok := sel.X.(*ast.Ident); ok {
						for _, spec := range d.file.Imports {
							if importName(spec) == id.Name {
								path, _ := strconv.Unquote(spec.Path.Value)
								alias := ""
								if spec.Name != nil {
									alias = spec.Name.Name
								}
								imports[path] = alias
							}
						}
					}
				}
				return true
			})
			return b.String()
		}
		var params, args []string
		i := 0
		for _, field := range d.Type.Params.List {
			typ := print(field.Type)
			n := len(field.Names)
			if n == 0 {
				n = 1
			}
			for j := 0; j < n; j++ {
				name := fmt.Sprintf("a%d", i)
				params = append(params, name+" "+typ)
				if strings.HasPrefix(typ, "...") {
					name += "..."
				}
				args = append(args, name)
				i++
			}
		}
		var results []string
		if d.Type.Results != nil {
			for _, field := range d.Type.Results.List {
				n := len(field.Names)
				if n == 0 {
					n = 1
				}
				for j := 0; j < n; j++ {
					results = append(results, print(field.Type))
				}
			}
		}
		rel, _ := filepath.Rel(root, dir)
		plain := make([]string, len(args))
		for k, a := range args {
			plain[k] = strings.TrimSuffix(a, "...")
		}
		fmt.Fprintf(&wrappers, "\nfunc %s(%s) (%s) {\n", d.Name.Name, strings.Join(params, ", "), strings.Join(results, ", "))
		fmt.Fprintf(&wrappers, "\tharvest.Begin(%q, %s)\n", filepath.ToSlash(rel)+"."+d.Name.Name, strings.Join(plain, ", "))
		call := fmt.Sprintf("harvestOrig_%s(%s)", d.Name.Name, strings.Join(args, ", "))
		if len(results) == 0 {
			fmt.Fprintf(&wrappers, "\t%s\n}\n", call)
		} else {
			fmt.Fprintf(&wrappers, "\treturn %s\n}\n", call)
		}
		count++
	}
	if count != len(names) {
		panic(fmt.Sprintf("%s: found %d of %d targets", dir, count, len(names)))
	}
	for path, offsets := range renames {
		src, err := os.ReadFile(path)
		if err != nil {
			panic(err)
		}
		sort.Sort(sort.Reverse(sort.IntSlice(offsets)))
		for _, off := range offsets {
			src = append(src[:off], append([]byte("harvestOrig_"), src[off:]...)...)
		}
		if err := os.WriteFile(path, src, 0o644); err != nil {
			panic(err)
		}
	}
	var out bytes.Buffer
	fmt.Fprintf(&out, "// Code generated by harvest; scratch copy only.\n\npackage %s\n\nimport (\n", pkg)
	paths := make([]string, 0, len(imports))
	for p := range imports {
		paths = append(paths, p)
	}
	sort.Strings(paths)
	for _, p := range paths {
		fmt.Fprintf(&out, "\t%s %q\n", imports[p], p)
	}
	out.WriteString(")\n")
	out.Write(wrappers.Bytes())
	if err := os.WriteFile(filepath.Join(dir, "zz_harvest_wrappers.go"), out.Bytes(), 0o644); err != nil {
		panic(err)
	}
	return count
}
