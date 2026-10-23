// Command main writes goldens for cpa_common::json::parse_query and Query::matches from
// tidwall/gjson v1.18.0 as CLIProxyAPI v8.0.35 (ae3ee3be) resolves it. Each result is
// payloadQueryTermMatches' expression: gjson.GetBytes("[item]", "#("+term+")").Exists().
package main

import (
	"encoding/json"
	"os"
	"strings"

	"github.com/tidwall/gjson"
)

func main() {
	values := []string{`"a"`, `""`, `"é"`, `"b"`, `"A"`, `"true"`, `"T"`, `"0"`, `1`, `0`, `2.5`, `-1`, `true`, `false`, `null`, `{"y":1}`, `[1]`}
	items := append([]string{}, values...)
	for _, v := range values {
		items = append(items, `{"x":`+v+`}`)
	}
	items = append(items, `{}`, `{"":1}`, `{"x":"a","x":"b"}`)
	ops := []string{"==", "=", "!=", "<", "<=", ">", ">=", "%", "!%", "!", ""}
	rhs := []string{`"a"`, `""`, `"?"`, `"*"`, `"a*"`, `"\u00e9"`, `"\"a"`, `a`, `1`, `0`, `2.5`, `1e0`, `true`, `false`, `null`,
		`~true`, `~false`, `~null`, `~*`, `~x`, ``}
	var terms []string
	for _, path := range []string{"", "x", " x "} {
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
	}, "", " ")
	if err := os.WriteFile(os.Args[1], append(out, '\n'), 0o644); err != nil {
		panic(err)
	}
}
