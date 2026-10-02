// Records tidwall/sjson v1.2.5 edits and gjson v1.18.0 reads (the versions pinned by
// CLIProxyAPI 6fecc6e) over a cross product of documents, paths and values.
// Usage: go run . > ../../fixtures/sjson_go.json
package main

import (
	"encoding/json"
	"os"

	"github.com/tidwall/gjson"
	"github.com/tidwall/sjson"
)

func main() {
	docs := []string{
		``, `   `, `{}`, `[]`, `null`, `"str"`, `12`,
		`{"a":1}`, `{ "a" : 1 , "b" : [ 1 , 2 , 3 ] }`, `{"a":{"b":{"c":true}},"d":[{"e":1},{"e":2}]}`,
		`[1,2,3]`, `[ ]`, `[{"a":1},{"a":2}]`, `{"a":1,"a":2}`, `{"x.y":1,"a":{"":2}}`,
		`{"a":[],"b":{}}`, "{\n  \"a\": 1,\n  \"b\": 2\n}", `{"k":"v"}garbage}`, `{"esc\"key":1,"b":2}`,
		`{"contents":[{"role":"model","parts":[{"text":"hi","thoughtSignature":"s"}]}]}`,
		`{"+0":1,"00":2}`, ` [ 1 , [2] ]`,
	}
	paths := []string{
		"a", "b", "a.b", "a.b.c", "d.1.e", "d.5", "b.0", "b.1", "b.2", "b.-1", "0", "1", "3", "-1", "a.0", "a.-1",
		"new", "new.deep.path", "new.0.x", "new.2", "x\\.y", "a.", "contents.0.parts.0.thoughtSignature",
		"contents.0.parts.-1", "esc\"key", ":0", "a.:1",
		"+0", "-0", "00", "18446744073709551616", "b.+1", "b.18446744073709551617",
	}
	values := []string{"v", "<a>&\u2028\n\t\"\\é", "", "x\x01y"}
	raws := []string{`{"z":1}`, `[1, 2]`, `null`, `"s"`, `true`}
	var edits []map[string]string
	for _, doc := range docs {
		for _, path := range paths {
			out, _ := sjson.Delete(doc, path)
			edits = append(edits, map[string]string{"op": "delete", "json": doc, "path": path, "out": out})
			for _, v := range values {
				out, _ := sjson.Set(doc, path, v)
				edits = append(edits, map[string]string{"op": "set_str", "json": doc, "path": path, "value": v, "out": out})
			}
			for _, v := range raws {
				out, _ := sjson.SetRaw(doc, path, v)
				edits = append(edits, map[string]string{"op": "set_raw", "json": doc, "path": path, "value": v, "out": out})
			}
		}
	}
	// A wrapped-negative index only returns a body for empty arrays: for a non-empty one
	// sjson computes n-len(items), which wraps to ~2^63 and exhausts memory.
	for _, doc := range []string{``, `[]`, `{}`} {
		for _, path := range []string{"9223372036854775808", "a.9223372036854775808"} {
			out, _ := sjson.SetRaw(doc, path, "1")
			edits = append(edits, map[string]string{"op": "set_raw", "json": doc, "path": path, "value": "1", "out": out})
		}
	}
	readDocs := []string{
		`{"n":1.5,"m":-0,"e":1e3,"big":9007199254740993,"neg":-12,"s":"123","sf":"1.5","t":true,"f":false,"z":null,"o":{"a":1},"arr":[1,"2"],"esc":"a\"b\u00e9","sp":" 42 ","huge":1e300,"ts":"TRUE","one":"1","ff":"f","inf":1e999,"ninf":-1e999,"tiny":1e-7}`,
	}
	var reads []map[string]any
	for _, doc := range readDocs {
		for _, path := range []string{"n", "m", "e", "big", "neg", "s", "sf", "t", "f", "z", "o", "arr", "esc", "sp", "huge", "ts", "one", "ff", "missing", "arr.1", "o.a", "inf", "ninf", "tiny"} {
			r := gjson.Get(doc, path)
			reads = append(reads, map[string]any{"json": doc, "path": path, "string": r.String(), "int": r.Int(), "bool": r.Bool(), "exists": r.Exists()})
		}
	}
	enc := json.NewEncoder(os.Stdout)
	enc.SetEscapeHTML(false)
	if err := enc.Encode(map[string]any{"edits": edits, "reads": reads}); err != nil {
		panic(err)
	}
}
