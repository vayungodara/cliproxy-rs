package main

import (
	"bufio"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
)

// Converter calls recorded by ./harvest while Go's own tests ran (HARVEST_JSONL). They
// add the inputs of table-driven and helper-built tests that mine() cannot evaluate.
// Each becomes an ordinary fixture: run() records Go's output through the registry, so
// harvested fixtures differ from mined ones only in where the input came from.

type harvestArg struct {
	S    *string `json:"s"`
	SB   *string `json:"sb"`
	B    *string `json:"b"`
	Bool *bool   `json:"bool"`
	P    string  `json:"p"`
}

type harvestCall struct {
	Fn   string       `json:"fn"`
	Line int          `json:"line"`
	Test string       `json:"test"`
	Args []harvestArg `json:"args"`
}

const (
	harvestMaxInput   = 256 << 10 // the allocation-bound tests' multi-MiB payloads stay out
	harvestMaxPerTest = 24        // per test and path; loops would otherwise flood a pair
	harvestMaxLines   = 200
)

var harvestCalls []harvestCall

func loadHarvest() []harvestCall {
	path := os.Getenv("HARVEST_JSONL")
	if path == "" || harvestCalls != nil {
		return harvestCalls
	}
	f, err := os.Open(path)
	if err != nil {
		panic(err)
	}
	defer f.Close()
	sc := bufio.NewScanner(f)
	sc.Buffer(make([]byte, 1<<20), 64<<20)
	harvestCalls = []harvestCall{}
	for sc.Scan() {
		if len(sc.Bytes()) > 4*harvestMaxInput {
			continue
		}
		var c harvestCall
		if err := json.Unmarshal(sc.Bytes(), &c); err != nil {
			panic(err)
		}
		harvestCalls = append(harvestCalls, c)
	}
	if err := sc.Err(); err != nil {
		panic(err)
	}
	return harvestCalls
}

func (a harvestArg) str() string {
	switch {
	case a.S != nil:
		return *a.S
	case a.SB != nil || a.B != nil:
		enc := a.SB
		if enc == nil {
			enc = a.B
		}
		raw, err := base64.StdEncoding.DecodeString(*enc)
		if err != nil {
			panic(err)
		}
		return string(raw)
	}
	return ""
}

// The request_compat converters run() can call, by pair.
var compatPairs = map[string]bool{"openai:claude": true, "claude:openai": true, "claude:gemini": true, "claude:codex": true, "claude:interactions": true}

func fixtureKey(f fixture) string {
	return strings.Join([]string{f.Path, f.Model, f.Input, fmt.Sprint(f.Stream), f.Original, f.Translated, strings.Join(f.Lines, "\x00")}, "\x01")
}

func testOf(name string) string {
	if i := strings.IndexByte(name, ':'); i >= 0 {
		return name[:i]
	}
	return name
}

// harvested returns the recorded calls for pair r that existing cases do not already
// cover, keeping a duplicate input when it is the only case carrying its test's name.
func harvested(root string, r registration, existing []fixture) []fixture {
	calls := loadHarvest()
	if len(calls) == 0 {
		return nil
	}
	rel, _ := filepath.Rel(root, r.dir)
	pkg := filepath.ToSlash(rel) + "."
	pair := r.client + ":" + r.upstream
	keys, tests := map[string]bool{}, map[string]bool{}
	for _, f := range existing {
		keys[fixtureKey(f)] = true
		tests[testOf(f.Name)] = true
	}
	var out []fixture
	perTest := map[string]int{}
	add := func(test string, f fixture) {
		for _, v := range []string{f.Input, f.Original, f.Translated} {
			if len(v) > harvestMaxInput {
				return
			}
		}
		k := fixtureKey(f)
		if keys[k] && tests[test] {
			return
		}
		if perTest[test+"|"+f.Path]++; perTest[test+"|"+f.Path] > harvestMaxPerTest {
			return
		}
		keys[k], tests[test] = true, true
		out = append(out, f)
	}
	type group struct {
		test string
		f    fixture
	}
	var order []string
	streams := map[string]*group{}
	stream := func(c harvestCall, model, orig, req, raw, param string) {
		key := c.Test + "|" + param
		g := streams[key]
		if g == nil {
			g = &group{c.Test, fixture{Name: fmt.Sprintf("%s:%d:stream", c.Test, c.Line), Path: "stream", Model: model, Original: orig, Translated: req}}
			streams[key] = g
			order = append(order, key)
		}
		if len(g.f.Lines) < harvestMaxLines {
			g.f.Lines = append(g.f.Lines, raw)
		}
	}
	for _, c := range calls {
		name := fmt.Sprintf("%s:%d", c.Test, c.Line)
		a := c.Args
		switch {
		case c.Fn == pkg+r.request && len(a) == 3:
			add(c.Test, fixture{Name: name, Path: "request", Model: a[0].str(), Input: a[1].str(), Stream: a[2].Bool != nil && *a[2].Bool})
		case strings.HasPrefix(c.Fn, pkg) && strings.HasSuffix(c.Fn, "WithCompat") && compatPairs[pair] && len(a) == 3:
			add(c.Test, fixture{Name: name + ":compat", Path: "request_compat", Model: a[0].str(), Input: a[1].str(), Stream: a[2].Bool != nil && *a[2].Bool})
		case c.Fn == pkg+r.stream && len(a) == 6:
			stream(c, a[1].str(), a[2].str(), a[3].str(), a[4].str(), a[5].P)
		case c.Fn == pkg+r.nonStream && len(a) == 6:
			add(c.Test, fixture{Name: name, Path: "non_stream", Model: a[1].str(), Original: a[2].str(), Translated: a[3].str(), Input: a[4].str()})
		case c.Fn == "sdk/translator.TranslateRequest" && len(a) == 5 && a[0].str() == r.client && a[1].str() == r.upstream:
			add(c.Test, fixture{Name: name, Path: "request", Model: a[2].str(), Input: a[3].str(), Stream: a[4].Bool != nil && *a[4].Bool})
		case c.Fn == "sdk/translator.TranslateStream" && len(a) == 8 && a[1].str() == r.upstream && a[2].str() == r.client:
			stream(c, a[3].str(), a[4].str(), a[5].str(), a[6].str(), a[7].P)
		case c.Fn == "sdk/translator.TranslateNonStream" && len(a) == 8 && a[1].str() == r.upstream && a[2].str() == r.client:
			add(c.Test, fixture{Name: name, Path: "non_stream", Model: a[3].str(), Original: a[4].str(), Translated: a[5].str(), Input: a[6].str()})
		}
	}
	for _, key := range order {
		add(streams[key].test, streams[key].f)
	}
	return out
}

// helperFixtures writes the recorded helper calls (records with results) to path:
// src/go_helper_tests.rs replays each against its Rust counterpart. A test's repeated calls
// with the same arguments are kept once.
func helperFixtures(path string) {
	in := os.Getenv("HARVEST_JSONL")
	if in == "" {
		panic("helpers needs HARVEST_JSONL")
	}
	f, err := os.Open(in)
	if err != nil {
		panic(err)
	}
	defer f.Close()
	sc := bufio.NewScanner(f)
	sc.Buffer(make([]byte, 1<<20), 64<<20)
	type call struct {
		Name    string            `json:"name"`
		Fn      string            `json:"fn"`
		Args    []json.RawMessage `json:"args"`
		Results []json.RawMessage `json:"results"`
	}
	var out []call
	seen := map[string]bool{}
	for sc.Scan() {
		if len(sc.Bytes()) > 4*harvestMaxInput {
			continue
		}
		var rec struct {
			Fn      string            `json:"fn"`
			Line    int               `json:"line"`
			Test    string            `json:"test"`
			Args    []json.RawMessage `json:"args"`
			Results []json.RawMessage `json:"results"`
		}
		if err := json.Unmarshal(sc.Bytes(), &rec); err != nil {
			panic(err)
		}
		if rec.Results == nil {
			continue
		}
		key, _ := json.Marshal([]any{rec.Fn, rec.Args, rec.Test})
		if seen[string(key)] {
			continue
		}
		seen[string(key)] = true
		out = append(out, call{Name: fmt.Sprintf("%s:%d", rec.Test, rec.Line), Fn: rec.Fn, Args: rec.Args, Results: rec.Results})
	}
	if err := sc.Err(); err != nil {
		panic(err)
	}
	raw, err := json.MarshalIndent(map[string]any{"calls": out}, "", " ")
	if err != nil {
		panic(err)
	}
	if err := os.WriteFile(path, append(raw, '\n'), 0o644); err != nil {
		panic(err)
	}
	fmt.Printf("helpers: %d calls\n", len(out))
}
