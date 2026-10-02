// Generates differential fixtures for the Rust gjson/sjson port by running the pinned
// tidwall libraries (gjson v1.18.0, sjson v1.2.5) and encoding/json over a cross product
// of awkward documents, paths and values. Byte fields map each byte to the rune of the same value.
package main

import (
	"encoding/json"
	"fmt"
	"math"
	"os"
	"sort"
	"strconv"
	"strings"

	"github.com/tidwall/gjson"
	"github.com/tidwall/sjson"
)

type getCase struct {
	JSON    string   `json:"json"`
	Path    string   `json:"path"`
	Exists  bool     `json:"exists"`
	Type    int      `json:"type"`
	Raw     string   `json:"raw"`
	Str     string   `json:"str"`
	String  string   `json:"string"`
	Int     int64    `json:"int"`
	Uint    uint64   `json:"uint"`
	Float   string   `json:"float"`
	Bool    bool     `json:"bool"`
	Index   int      `json:"index"`
	Indexes []int    `json:"indexes"`
	Array   []string `json:"array"`
	Each    []string `json:"each"`
	Map     []string `json:"map"`
}

type setCase struct {
	JSON   string `json:"json"`
	Path   string `json:"path"`
	Kind   string `json:"kind"`
	Value  string `json:"value"`
	Output string `json:"output"`
	Err    bool   `json:"err"`
}

type encodeCase struct {
	Input string `json:"input"`
	HTML  string `json:"html"`
	Plain string `json:"plain"`
}

type floatCase struct {
	Bits uint64 `json:"bits"`
	F    string `json:"f"`
	JSON string `json:"json"`
}

// h maps each byte to the rune with the same value, so any byte string survives JSON.
func h(s string) string {
	r := make([]rune, len(s))
	for i := 0; i < len(s); i++ {
		r[i] = rune(s[i])
	}
	return string(r)
}

func describe(r gjson.Result) []string {
	return []string{strconv.Itoa(int(r.Type)), h(r.Raw), h(r.Str), h(r.String()), strconv.Itoa(r.Index)}
}

func main() {
	docs := []string{
		`{"a":1,"b":"two","c":[1,"x",{"d":true}],"e":{"f":{"g":null}},"h":[]}`,
		` {"model" : "old", "model":"dup", "n":1e+09, "s":"\u0061\ud83d\ude00\ud800x\\\"q", "esc\"key":5} trailing`,
		`{"max_tokens":"12.75","top_p":"1e3","stop":[1e3,-0,1.5,9007199254740993,"5",true,null],"neg":"-42","big":1e400,"nan":NaN,"inf":Infinity,"plus":+5}`,
		`{"x":"bad` + "\xff\xfe" + `","y":"ok\u2028<&>","z":` + "\"\xe2\x80\xa8\"" + `}`,
		`[1,{"a":[{"b":1},{"b":2},{"c":3},{"b":"s"}]},[2,3],"four",,5]`,
		`{"a.b":1,"a":{"b":2},"w*c":3,"wxc":4,"q?":5,"pipe|k":6,"@this":7,"#":8,"": 9}`,
		`{"a":{"b":`,
		`{"unterminated":"abc`,
		`{"bs":"\\\\","bs2":"a\\\\\"b","u":"\u00","uu":"\uZZZZ","ctl":"a` + "\x01" + `b"}`,
		`invalid`, `null`, `"str"`, `123`, ``, `   `, `[`, `{}`, `[]`, `{"a":(1),"b":2}`,
		`{"arr":[{"t":"x","v":1},{"t":"y"},{"v":[1,2]}],"num":[0,1,2]}`,
		`{"t":true,"f":false,"tt":"TRUE","t1":"1","tf":"t","n0":0,"n1":0.5,"str0":"0"}`,
		"{\"a\":1}\n{\"a\":2}",
		`["a""b"]`, `{"a":[{"x":1}]}`, `["x"]`, `1e19`, `-1.5e0`, `2e19`, `{"f":"1_0","g":"1__0","h":"1e1_0","i":"_1","j":"1_","k":"0x1p-2"}`,
	}
	paths := []string{"a", "b", "c", "c.0", "c.1", "c.2.d", "c.#", "c.#.d", "e.f.g", "e.f", "h", "h.#", "model", "n", "s", "esc\\\"key", "esc\"key",
		"max_tokens", "top_p", "stop", "stop.0", "stop.1", "stop.3", "stop.#", "neg", "big", "nan", "inf", "plus", "x", "y", "z",
		"0", "1", "1.a", "1.a.#", "1.a.#.b", "1.a.1.b", "2.1", "3", "4", "5", "-1", "a\\.b", "a.b", "w*c", "w?c", "q\\?", "pipe|k", "pipe\\|k",
		"@this", "#", "", "arr.#.t", "arr.#.v", "arr.1.t", "num.#", "unterminated", "bs", "bs2", "u", "uu", "ctl",
		"t", "f", "tt", "t1", "tf", "n0", "n1", "str0", "a|b", "e|f", "e.f|g", "c.#|0", "*", "e.*", "e.f.?",
		"a|#.x", "#.@this", "g", "h", "i", "j"}
	var gets []getCase
	for _, doc := range docs {
		for _, path := range paths {
			r := gjson.Get(doc, path)
			g := getCase{JSON: h(doc), Path: path, Exists: r.Exists(), Type: int(r.Type), Raw: h(r.Raw), Str: h(r.Str), String: h(r.String()),
				Int: r.Int(), Uint: r.Uint(), Float: strconv.FormatUint(math.Float64bits(r.Float()), 10), Bool: r.Bool(), Index: r.Index, Indexes: r.Indexes}
			g.Array = []string{}
			for _, item := range r.Array() {
				g.Array = append(g.Array, describe(item)...)
			}
			g.Each = []string{}
			r.ForEach(func(k, v gjson.Result) bool {
				g.Each = append(g.Each, describe(k)...)
				g.Each = append(g.Each, describe(v)...)
				return true
			})
			g.Map = []string{}
			for k, v := range r.Map() {
				g.Map = append(g.Map, h(k)+"="+h(v.Raw)+"@"+strconv.Itoa(v.Index))
			}
			sort.Strings(g.Map)
			gets = append(gets, g)
		}
		// Parse as well, recorded with an empty sentinel path marker.
		r := gjson.Parse(doc)
		g := getCase{JSON: h(doc), Path: "\x00parse", Exists: r.Exists(), Type: int(r.Type), Raw: h(r.Raw), Str: h(r.Str), String: h(r.String()),
			Int: r.Int(), Uint: r.Uint(), Float: strconv.FormatUint(math.Float64bits(r.Float()), 10), Bool: r.Bool(), Index: r.Index}
		g.Array = []string{}
		for _, item := range r.Array() {
			g.Array = append(g.Array, describe(item)...)
		}
		g.Each = []string{}
		r.ForEach(func(k, v gjson.Result) bool {
			g.Each = append(g.Each, describe(k)...)
			g.Each = append(g.Each, describe(v)...)
			return true
		})
		g.Map = []string{}
		gets = append(gets, g)
		_ = gjson.Valid(doc)
	}

	setDocs := []string{`{"a":[{"x":1}]}`, `["x"]`, `{}`, ``, ` `, `[]`, `[1,2]`, `{"a":1}`, `{"a":{"b":[1,{"c":2}]}}`, `{ "x" : 1 , "y" : [ ] }  tail}`, `invalid`, `"str"`, `{`, `{"a":1,}`,
		`{"msgs":[{"role":"user"},{"role":"assistant"}],"tools":[],"meta":{}}`, `{"k":"v"} trailing {}`, `[[]]`, `{"e\"k":1}`, `{"a":1,"b":2,"c":3}`}
	setPaths := []string{"a", "a.b", "a.b.1.c", "a.b.-1", "x", "y.0", "y.-1", "y.3", "0", "-1", "2", "5", "msgs.1.role", "msgs.-1", "msgs.2.content.0.text", "tools.-1",
		"meta.user_id", "new.0.x", "new.-1", ":0", "a.:1", "a\\.b", "e\\\"k", "k", "b", "c", "a*", "a?", "#", "msgs.#.role", "", "a..b", "a.", ".a", "a|#.x"}
	values := map[string][]string{
		"str": {"a<b>&c", "é<", "q\"uote\n\x08\xff"},
		"raw": {`{"z":1}`, ``},
		"del": {""},
	}
	var sets []setCase
	for _, doc := range setDocs {
		for _, path := range setPaths {
			for _, kind := range []string{"str", "raw", "del"} {
				for _, value := range values[kind] {
					var out string
					var err error
					switch kind {
					case "str":
						out, err = sjson.Set(doc, path, value)
					case "raw":
						out, err = sjson.SetRaw(doc, path, value)
					case "del":
						out, err = sjson.Delete(doc, path)
					}
					sets = append(sets, setCase{JSON: h(doc), Path: path, Kind: kind, Value: h(value), Output: h(out), Err: err != nil})
				}
			}
		}
	}
	// sjson's signed index arithmetic: a wrapped-negative index pads nothing. (On a
	// non-empty array Go's n-len(items) wraps again and exhausts memory, in Go too.)
	for _, doc := range []string{`[]`, `{}`, `{"a":[]}`} {
		for _, path := range []string{"9223372036854775808", "a.18446744073709551617.b", "a.9223372036854775808"} {
			out, err := sjson.SetRaw(doc, path, "1")
			sets = append(sets, setCase{JSON: h(doc), Path: path, Kind: "raw", Value: h("1"), Output: h(out), Err: err != nil})
		}
	}
	strs := []string{"plain", "a<b>&c", "é<", "\u2028\u2029", "bad\xff\xfe", "\x00\x01\x08\x09\x0a\x0c\x0d\x1f\x7f", "\"\\/", "\xe2\x80", "\xed\xa0\x80", "\xf0\x9f\x98\x80", "\xc0\x80"}
	var encs []encodeCase
	for _, s := range strs {
		b, _ := json.Marshal(s)
		var plain []byte
		{
			buf := &jsonBuffer{}
			enc := json.NewEncoder(buf)
			enc.SetEscapeHTML(false)
			_ = enc.Encode(s)
			plain = buf.b[:len(buf.b)-1]
		}
		encs = append(encs, encodeCase{Input: h(s), HTML: h(string(b)), Plain: h(string(plain))})
	}
	var floats []floatCase
	for _, f := range []float64{0, math.Copysign(0, -1), 1, -1.5, 0.1 + 0.2, 1e-7, 1e-6, 9.99e-7, 1e20, 1e21, 1.5e300, 5e-324, 123456789012345678, 1000, 1e3, 2.5e-10, -3e25, math.MaxFloat64} {
		j, _ := json.Marshal(f)
		floats = append(floats, floatCase{Bits: math.Float64bits(f), F: strconv.FormatFloat(f, 'f', -1, 64), JSON: string(j)})
	}
	type decodeCase struct {
		Input   string `json:"input"`
		Float   string `json:"float"`
		Number  string `json:"number"`
	}
	var decodes []decodeCase
	for _, in := range []string{
		`{"z":1.50,"a":{"y":"<&>","b":[true,null]},"a":2}`, `{"\ud800\u0061":1,"\ud83d\ude00":"\udc00x"}`, "{\"k\":\"\xe2\x82\",\"\xff\":1}",
		`[1e3,1.50,9007199254740993,1e-7,1e21,-0,0.1]`, `[1e400]`, `{"a":"\u2028\u2029<>&\u0000"}`, `"\/\'"`, `{"bad`, ` [ 1 , "x" ] `,
	} {
		d := decodeCase{Input: h(in)}
		var v any
		if err := json.Unmarshal([]byte(in), &v); err == nil {
			b, errM := json.Marshal(v)
			if errM == nil {
				d.Float = h(string(b))
			}
		}
		dec := json.NewDecoder(strings.NewReader(in))
		dec.UseNumber()
		var n any
		if err := dec.Decode(&n); err == nil && !dec.More() {
			b, errM := json.Marshal(n)
			if errM == nil {
				d.Number = h(string(b))
			}
		}
		decodes = append(decodes, d)
	}
	out := map[string]any{"get": gets, "set": sets, "encode": encs, "float": floats, "decode": decodes}
	raw, err := json.Marshal(out)
	if err != nil {
		panic(err)
	}
	if err := os.WriteFile(os.Args[1], append(raw, '\n'), 0o644); err != nil {
		panic(err)
	}
	fmt.Printf("wrote %d get, %d set, %d encode, %d float cases\n", len(gets), len(sets), len(encs), len(floats))
}

type jsonBuffer struct{ b []byte }

func (w *jsonBuffer) Write(p []byte) (int, error) {
	w.b = append(w.b, p...)
	return len(p), nil
}
