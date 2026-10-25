// Generates differential fixtures for cpa_common::gemini_schema by running the pinned
// internal/util JSON Schema cleaners over every JSON literal mined from
// internal/util/gemini_schema_test.go plus the edge cases below. Strings are stored with
// each byte mapped to the rune of the same value so invalid UTF-8 survives JSON.
//
// Run from a module whose import path sits inside the reference module (see
// crates/cpa-translate/tests/reference/README.md):
//
//	go run . /path/to/CLIProxyAPI /path/to/cliproxy-rs/crates/cpa-common/tests/fixtures/gemini_schema.json.gz
//
// The budget cases from Go's tests expand to its 256 KiB inlining limit, so the fixture is
// gzipped.
//
// An optional third argument names a harvest file: one JSON string per line, each an input
// that Go's own internal/util tests passed to cleanJSONSchema or InlineLocalRefs. It covers
// the inputs those tests build in code rather than as literals. To harvest, in a scratch
// worktree of the reference (never the reference itself), copy zz_rsfix_harvest.go.txt to
// internal/util/zz_rsfix_harvest.go, add `rsfixHarvest(jsonStr)` as the first statement of
// cleanJSONSchema and InlineLocalRefs, and run
//
//	RSFIX_SCHEMA_HARVEST=/tmp/harvest.jsonl go test ./internal/util/
package main

import (
	"bytes"
	"compress/gzip"
	"encoding/json"
	"go/ast"
	"go/parser"
	"go/token"
	"os"
	"path/filepath"
	"sort"
	"strconv"
	"strings"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/util"
	"github.com/tidwall/gjson"
)

type fixture struct {
	Input   string            `json:"input"`
	Outputs map[string]string `json:"outputs"`
}

func h(s string) string {
	r := make([]rune, len(s))
	for i := 0; i < len(s); i++ {
		r[i] = rune(s[i])
	}
	return string(r)
}

var extra = []string{
	``, `true`, `false`, `null`, `5`, `"str"`, `[]`, `{}`, `true x`, `truex`, `{"a":1}} trailing`, `{"type":"object"`,
	`{"schema":{"type":"object","properties":{"a":true}}}`, `{"schema":true}`, `{"schema":{"type":"string"}}`,
	`{"tools":[{"x":1}]}`, `{"request":{"contents":[]}}`,
	`{"type":"object","properties":{"a":{"type":"string","required":true},"b":{"required":false},"":{"required":true},"c":true,"d":false}}`,
	`{"type":"object","foo":{"type":"string"},"bar":{"type":"number","required":true},"x-ext":{"a":1},"properties":{"keep":{}}}`,
	`{"foo":{"type":"string"}}`, `{"type":["string","null"],"foo":{"a":1}}`, `{"type":["integer","object"],"foo":{}}`,
	`{"type":"array"}`, `{"type":["array","null"]}`, `{"items":{"type":"string"}}`, `{"type":"","items":{}}`, `{"type":null,"items":true}`,
	`{"type":"object","properties":{"list":{"type":"array","items":[true,{"type":"array"}]}},"additionalProperties":{"foo":{}},"patternProperties":{"^a":{"required":true}}}`,
	`{"if":true,"then":{"properties":{"t":{"type":"string"}}},"else":{"properties":{"e":{"type":"integer"},"t":{"type":"number"}}},"properties":{"x":{}}}`,
	`{"allOf":[{"properties":{"a":{"type":"string","description":"from allOf"}},"required":["a"]},{"required":["b","a"],"type":"object"},{"if":{"x":1}},5],"properties":{"a":{"type":"string"},"b":{}}}`,
	`{"allOf":[{"required":[]}]}`, `{"allOf":[{"required":"x"}]}`, `{"allOf":{"a":1}}`,
	`{"anyOf":[{"type":"string"},{"type":"object","properties":{"o":{}}},{"type":"null"}],"description":"parent <desc>"}`,
	`{"oneOf":[{"type":"integer"},{"type":"array","items":{"type":"string"}}]}`, `{"anyOf":[]}`, `{"anyOf":[{"type":"null"}]}`,
	`{"type":"object","properties":{"p":{"type":"string"}},"anyOf":[{"properties":{"q":{"type":"integer"}}},{"type":"null"}]}`,
	`{"properties":{"u":{"anyOf":[{"type":"string","description":"child"},{"type":"number"}],"description":"parent"}}}`,
	`{"properties":{"n":{"type":["string","null"]},"m":{"type":["null"]},"k":{"type":["integer","string","null"],"items":{}},"j":{"type":["array","null"],"items":{"type":"string"}}},"required":["n","m","k","z"]}`,
	`{"type":"object","properties":{"n":{"type":["string","null"]}},"required":["n"]}`,
	`{"$ref":"#/$defs/A","$defs":{"A":{"type":"object","properties":{"self":{"$ref":"#/$defs/A"}},"description":"A node"}}}`,
	`{"properties":{"a":{"$ref":"#/definitions/B","description":"sibling"},"b":{"$ref":"https://ext/x.json"},"c":{"$ref":"#/definitions/missing"},"d":{"$ref":"#/definitions/arr/1"}},"definitions":{"B":{"type":"string","enum":["x","y"]},"arr":[{"type":"integer"},{"type":"boolean"}]}}`,
	`{"properties":{"t":{"$ref":"#/definitions/a~1b~0c"}},"definitions":{"a/b~c":{"type":"number"}}}`,
	`{"const":"root"}`, `{"properties":{"c":{"const":5},"d":{"const":{"a":[1,2]},"enum":["x"]},"e":{"const":1e400}}}`,
	`{"properties":{"e":{"enum":[1,2.5,true,null,"s",{"a":1},[1]]},"one":{"enum":["only"]},"none":{"enum":[]},"many":{"enum":[1,2,3,4,5,6,7,8,9,10,11]},"bool":{"type":"boolean","enum":[true]}}}`,
	`{"properties":{"s":{"type":"string","minLength":1,"maxLength":5,"pattern":"^a$","format":"email","default":"x","examples":["a"],"description":"desc"},"n":{"type":"number","minimum":0,"maximum":9,"multipleOf":3,"exclusiveMinimum":1},"arr":{"type":"array","minItems":1,"maxItems":3,"uniqueItems":true,"contains":{"type":"string"},"items":{}}}}`,
	`{"properties":{"pattern":{"type":"string"},"default":{"type":"string"},"properties":{"type":"object","properties":{"propertyNames":{"type":"string"}},"propertyNames":{"pattern":"x"}}}}`,
	`{"additionalProperties":false,"properties":{"o":{"type":"object","additionalProperties":{"type":"string"}},"f":{"type":"object","additionalProperties":false}}}`,
	`{"not":{"type":"string"},"properties":{"not":{"type":"string"}},"title":"T","nullable":true,"$schema":"x","$id":"y","id":"z","$comment":"c","deprecated":true,"x-google-enum":["a"],"properties2":{"x-keep":1}}`,
	`{"type":"object","properties":{"_":{"type":"boolean"},"a":{"type":"string"}},"required":["_","a"]}`,
	`{"type":"object","properties":{"_":{"type":"boolean"}},"required":["_"]}`,
	`{"type":"object","properties":{"reason":{"type":"string","description":"Brief explanation of why you are calling this tool"}},"required":["reason"]}`,
	`{"type":"object","properties":{"reason":{"type":"string","description":"other"}},"required":["reason"]}`,
	`{"type":"object","properties":{"inner":{"type":"object"},"filled":{"type":"object","properties":{"a":{}}},"req":{"type":"object","properties":{"a":{}},"required":["a"]}}}`,
	`{"type":"object","properties":{}}`, `{"type":"OBJECT","properties":{"x":{"type":"STRING","items":{}}}}`,
	`{"properties":{"a.b":{"type":"string"},"c*d":{"type":["string","null"]},"e?f":{"const":"v"}},"required":["a.b","c*d","e?f","missing"]}`,
	`{"properties":{"p|q":{"type":"string"},"#hash":{"enum":["a","b"]},"@at":{"x-ext":1}},"required":["p|q","#hash"]}`,
	`{"properties":{"dup":{"type":"string"},"dup":{"type":"integer"}},"enum":["<&>","\u2028"],"description":"<b>&"}`,
	`{"description":"existing (Allowed: a, b)","enum":["a","b"]}`, `{"description":"Allowed: a, b","enum":["a","b"]}`,
	"{\"properties\":{\"k\xff\":{\"type\":[\"string\",\"null\"],\"description\":\"\xfe\"}},\"required\":[\"k\xff\"]}",
	`{"type":"object","properties":{"q":{"type":"string"}},"required":"q"}`, `{"required":["a"]}`,
	`{"properties":{"big":{"type":"integer","default":9007199254740993,"minimum":1e400}}}`,
	`{"items":[{"type":"string"}],"type":"object"}`, `{"type":"array","items":{"type":"object","properties":{"x":{"type":["null","string"]}}}}`,
	`{"dependencies":{"a":true,"b":{"foo":{}}},"dependentSchemas":{"c":true},"unevaluatedProperties":true,"contentSchema":true,"prefixItems":[true]}`,
	// v8.0.35: reference inlining on every route, literal values, union preservation.
	`{"schema":{"type":"object","properties":{"a":{"$ref":"#/$defs/A"}},"$defs":{"A":{"type":"string","description":"inner"}}}}`,
	`{"type":"object","properties":{"$ref":{"type":"string"},"default":{"type":"object","properties":{"x":{"$ref":"#/$defs/X"}}}},"$defs":{"X":{"type":"integer"}}}`,
	`{"type":"object","properties":{"a":{"type":"object","default":{"$ref":"#/$defs/X","x-y":1,"anyOf":[1],"properties":{"q":{}}},"examples":[{"x-k":{"$ref":"#/z"}}]}},"$defs":{"X":{"type":"integer"}}}`,
	`{"anyOf":[{"type":"string"},{"type":"integer"}],"oneOf":[{"type":"boolean"},{"type":"number"}]}`,
	`{"type":"object","properties":{"a":{},"b":{"type":"string"}},"anyOf":[{"required":["a"]},{"required":["b"]}]}`,
	`{"type":"object","properties":{"a":{"type":"string"}},"oneOf":[{"required":["a"]},{"required":["zz"]}]}`,
	`{"properties":{"u":{"anyOf":[true,{"type":"string"},{"type":"null"}],"nullable":true},"v":{"oneOf":[{"type":"null"},{"type":["null"]}]},"w":{"anyOf":[5,{"type":"null"}]},"z":{"anyOf":[{"type":"integer"},{"type":"object","properties":{"k":{}}},{"type":"null"}],"description":"pick"}}}`,
	`{"type":"array","$ref":"#/$defs/L","$defs":{"L":{"type":"array","items":{"type":"integer"}}}}`,
	`{"properties":{"l":{"$ref":"#/$defs/L"},"m":{"type":"array"}},"$defs":{"L":{"type":"array"}}}`,
	`{"dependencies":{"a":{"properties":{"x-k":{"type":"string"}}},"b":["a"]},"properties":{"p":{"discriminator":{"propertyName":"k"},"readOnly":true,"writeOnly":false,"contentMediaType":"text/plain","contentEncoding":"base64","type":"string"}}}`,
	`{"$defs":{"A":{"type":"object","description":"` + strings.Repeat("d", 600) + `","properties":{"s":{"$ref":"#/$defs/A"}}}},"$ref":"#/$defs/A","title":"root"}`,
	`{"properties":{"a":{"$ref":"#/$defs/A","description":"own"},"b":{"$ref":"#/$defs/A"}},"$defs":{"A":{"type":"object","description":"<&>\"q\\b\u2028\n\t\u0001\u001f` + "\u2028\u00e9\u4e2d" + `","properties":{"s":{"$ref":"#/$defs/A"}}}}}`,
	// The claude->antigravity tools/schema/11 tool schema (golden.rs SUPERSEDED).
	`{"type":"object","properties":{"list":{"type":"array","items":true,"prefixItems":[true,false,{"type":"object"}],"contains":{"type":"object"}}},"additionalProperties":true,"propertyNames":true,"unevaluatedProperties":false,"anyOf":[true,{"type":"object","properties":{"z":{"type":"object"}}}],"not":{"type":"object"},"if":true,"then":{"type":"object"},"else":false,"$defs":{"d":{"type":"object"},"e":true},"definitions":{"f":{"type":"object"}},"dependentSchemas":{"g":true},"dependencies":{"h":["x"],"i":{"type":"object"}}}`,
}

func mine(path string) []string {
	fset := token.NewFileSet()
	file, err := parser.ParseFile(fset, path, nil, 0)
	if err != nil {
		panic(err)
	}
	seen := map[string]bool{}
	var out []string
	ast.Inspect(file, func(n ast.Node) bool {
		lit, ok := n.(*ast.BasicLit)
		if !ok || lit.Kind != token.STRING {
			return true
		}
		s, err := strconv.Unquote(lit.Value)
		if err != nil || !gjson.Valid(s) || !gjson.Parse(s).IsObject() || seen[s] {
			return true
		}
		seen[s] = true
		out = append(out, s)
		return true
	})
	return out
}

func harvested(path string) []string {
	raw, err := os.ReadFile(path)
	if err != nil {
		panic(err)
	}
	var out []string
	for _, line := range strings.Split(string(raw), "\n") {
		if line == "" {
			continue
		}
		var s string
		if err := json.Unmarshal([]byte(line), &s); err != nil {
			panic(err)
		}
		out = append(out, s)
	}
	return out
}

func dedupe(inputs []string) []string {
	seen := map[string]bool{}
	out := inputs[:0]
	for _, s := range inputs {
		if !seen[s] {
			seen[s] = true
			out = append(out, s)
		}
	}
	return out
}

func main() {
	inputs := append(mine(filepath.Join(os.Args[1], "internal/util/gemini_schema_test.go")), extra...)
	if len(os.Args) > 3 {
		inputs = append(inputs, harvested(os.Args[3])...)
	}
	inputs = dedupe(inputs)
	fns := map[string]func(string) string{
		"gemini":             util.CleanJSONSchemaForGemini,
		"gemini_json_schema": util.CleanJSONSchemaForGeminiJSONSchema,
		"antigravity":        util.CleanJSONSchemaForAntigravity,
		"antigravity_tool":   func(s string) string { return util.CleanJSONSchemaForAntigravityTool(s, false) },
		"antigravity_claude_tool": func(s string) string {
			return util.CleanJSONSchemaForAntigravityClaudeTool(s, false)
		},
		"antigravity_response": util.CleanJSONSchemaForAntigravityResponse,
		"inline_local_refs":    util.InlineLocalRefs,
	}
	names := make([]string, 0, len(fns))
	for name := range fns {
		names = append(names, name)
	}
	sort.Strings(names)
	var out []fixture
	for _, input := range inputs {
		f := fixture{Input: h(input), Outputs: map[string]string{}}
		for _, name := range names {
			f.Outputs[name] = h(fns[name](input))
		}
		out = append(out, f)
	}
	raw, err := json.MarshalIndent(out, "", " ")
	if err != nil {
		panic(err)
	}
	raw = append(raw, '\n')
	if strings.HasSuffix(os.Args[2], ".gz") {
		// gzip.Writer leaves the header's mtime zero, so the file is reproducible.
		var buf bytes.Buffer
		zw, _ := gzip.NewWriterLevel(&buf, gzip.BestCompression)
		zw.Write(raw)
		zw.Close()
		raw = buf.Bytes()
	}
	if err := os.WriteFile(os.Args[2], raw, 0o644); err != nil {
		panic(err)
	}
	println(len(out), "schemas")
}
