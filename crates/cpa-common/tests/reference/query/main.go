// Command main writes goldens for cpa_common::json::parse_query and Query::matches from
// tidwall/gjson v1.18.0 as CLIProxyAPI v8.0.35 (ae3ee3be) resolves it. Each result is
// payloadQueryTermMatches' expression: gjson.GetBytes("[item]", "#("+term+")").Exists().
package main

import (
	"encoding/hex"
	"encoding/json"
	"os"
	"strings"

	"github.com/tidwall/gjson"
)

func main() {
	values := []string{`"a"`, `""`, `"é"`, `"b"`, `"A"`, `"true"`, `"T"`, `"0"`, `1`, `0`, `2.5`, `-1`, `true`, `false`, `null`, `{"y":1}`, `[1]`,
		// Tokens gjson's array-query loop classifies itself.
		`nan`, `NaN`, `inf`, `-inf`, `Infinity`, `+1`, `nope`, `fish`, `hello`, `9007199254740993`, `-0`, `1e400`}
	items := append([]string{}, values...)
	for _, v := range values {
		items = append(items, `{"x":`+v+`}`)
	}
	items = append(items, `{}`, `{"":1}`, `{"x":"a","x":"b"}`, `x1`, `tx`, `{"x":{"b":1}}`, `{"x":{"b":"a"}}`,
		// Raw fragments the loop reads as several elements or one odd number token, and
		// a match before malformed trailing input.
		`1 2`, `x1,2`, `1-2`, `1e`, `0x1`, `1 "x`, `"a" {`)
	ops := []string{"==", "=", "!=", "<", "<=", ">", ">=", "%", "!%", "!", ""}
	rhs := []string{`"a"`, `""`, `"?"`, `"*"`, `"a*"`, `"\u00e9"`, `"\"a"`, `a`, `1`, `0`, `2.5`, `1e0`, `true`, `false`, `null`,
		`~true`, `~false`, `~null`, `~*`, `~x`, ``,
		`0x10`, `0x1p4`, `1_0`, `0x1_0`, `1e308`, `1e309`, `-0`, `9007199254740992`, `nan`, `inf`}
	var terms []string
	for _, path := range []string{"", "x", " x ", "x.b"} {
		for _, op := range ops {
			for _, v := range rhs {
				if op == "" && v != "" {
					continue
				}
				terms = append(terms, path+op+v)
			}
		}
	}
	terms = append(terms, `x == "a"`, ` x=="a" `, `x=="a\"`, `x=="`, `x>"`, `x!=""`, `x==""`, `x=="b"`, `x%"?"`, `x%"\?"`, `x!%"?"`)
	// Byte-level pairs, hex-encoded: invalid UTF-8 in items and patterns, and patterns
	// that exhaust match.MatchLimit's 10000 calls.
	long := `"` + strings.Repeat("a", 20) + `"`
	pairs := [][2]string{
		{"\"\xff\"", `%"?"`}, {"\"\xff\"", `%"\xff"`}, {"\"\xff\"", "==\"\xff\""}, {"\"\xff\"", "%\"\xff\""}, {"\"\xffa\"", `%"??"`},
		{"\"\xc3\"", `%"?"`}, {"\"\xe9\"", "%\"\xe9\""}, {"\"\xe9\"", `%"\xe9"`}, {`"é"`, "%\"\xc3?\""}, {`"a"`, "%\"\xff\""},
		// 20 bytes, so a 200000-call budget: 8 stars exhaust it, 4 do not.
		{long, `%"*a*a*a*a*a*a*a*ab*"`}, {long, `!%"*a*a*a*a*a*a*a*ab*"`}, {long, `%"*a*a*a*ab*"`},
		{long, `%"*a*a*a*a*a*a*a*a*"`}, {`"` + strings.Repeat("a", 20) + `b"`, `%"*a*a*a*a*a*a*a*ab*"`},
	}
	var pairOut []map[string]any
	for _, p := range pairs {
		pairOut = append(pairOut, map[string]any{
			"item":  hex.EncodeToString([]byte(p[0])),
			"term":  hex.EncodeToString([]byte(p[1])),
			"match": gjson.GetBytes([]byte("["+p[0]+"]"), "#("+p[1]+")").Exists(),
		})
	}
	results := make([]string, len(items))
	for i, item := range items {
		var b strings.Builder
		for _, term := range terms {
			if gjson.GetBytes([]byte("["+item+"]"), "#("+term+")").Exists() {
				b.WriteByte('1')
			} else {
				b.WriteByte('0')
			}
		}
		results[i] = b.String()
	}
	out, _ := json.MarshalIndent(map[string]any{
		"source":  "tidwall/gjson v1.18.0 via CLIProxyAPI v8.0.35 (ae3ee3be303fb07fb8173279923413005bf32177)",
		"terms":   terms,
		"items":   items,
		"results": results,
		"pairs":   pairOut,
	}, "", " ")
	if err := os.WriteFile(os.Args[1], append(out, '\n'), 0o644); err != nil {
		panic(err)
	}
}
