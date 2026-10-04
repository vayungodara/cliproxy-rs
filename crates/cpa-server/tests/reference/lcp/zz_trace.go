//go:build ignore

package session

// Golden trace for cliproxy-rs: every outermost call of the LCP public surface, with
// its inputs, the matcher clock relative to the matcher's creation, and its result.

import (
	"encoding/json"
	"os"
	"runtime"
	"strconv"
	"strings"
	"sync"
	"time"

	sdktranslator "github.com/router-for-me/CLIProxyAPI/v8/sdk/translator"
)

type tracePart struct {
	Kind         string `json:"kind"`
	MIME         string `json:"mime"`
	Value        []byte `json:"value"`
	Digest       string `json:"digest"`
	OriginalSize int    `json:"original_size"`
	Sampled      bool   `json:"sampled"`
}

type traceTurn struct {
	Role  string      `json:"role"`
	Parts []tracePart `json:"parts"`
}

func traceTurns(turns []CanonicalTurn) []traceTurn {
	out := make([]traceTurn, 0, len(turns))
	for _, turn := range turns {
		t := traceTurn{Role: turn.Role, Parts: make([]tracePart, 0, len(turn.Parts))}
		for _, p := range turn.Parts {
			t.Parts = append(t.Parts, tracePart{p.Kind, p.MIME, []byte(p.Value), p.Digest, p.OriginalSize, p.Sampled})
		}
		out = append(out, t)
	}
	return out
}

type traceInfo struct {
	id   string
	test string
	base time.Time
	// realClock: the matcher reads the wall clock (no NowFunc). Its offsets are just
	// execution time and are recorded as 0, so recordings repeat exactly.
	realClock bool
}

var (
	traceMu    sync.Mutex
	traceDepth int
	traceIDs   = map[*MerklePrefixMatcher]traceInfo{}
	// Matchers created so far per test, for ids that do not depend on test order.
	traceCount = map[string]int{}
)

func traceEnter() bool {
	traceMu.Lock()
	defer traceMu.Unlock()
	traceDepth++
	return traceDepth == 1 && os.Getenv("LCP_TRACE") != ""
}

func traceLeave() {
	traceMu.Lock()
	traceDepth--
	traceMu.Unlock()
}

// traceTest names the running top-level test (package.TestName); subtest closures
// (package.TestName.func1) belong to their parent.
func traceTest() string {
	pcs := make([]uintptr, 64)
	n := runtime.Callers(2, pcs)
	frames := runtime.CallersFrames(pcs[:n])
	for {
		frame, more := frames.Next()
		parts := strings.Split(frame.Function[strings.LastIndex(frame.Function, "/")+1:], ".")
		if len(parts) >= 2 && strings.HasPrefix(parts[1], "Test") {
			return parts[0] + "." + parts[1]
		}
		if !more {
			return ""
		}
	}
}

// traceWrite appends one record, prefixed with its test name and a tab; record.sh
// sorts on that prefix (stably) and strips it, so test order never shows.
func traceWrite(test string, rec map[string]any) {
	data, err := json.Marshal(rec)
	if err != nil {
		panic(err)
	}
	f, err := os.OpenFile(os.Getenv("LCP_TRACE"), os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o644)
	if err != nil {
		panic(err)
	}
	defer f.Close()
	_, _ = f.Write(append(append([]byte(test+"\t"), data...), '\n'))
}

// traced reports whether m's calls are recorded.
func (m *MerklePrefixMatcher) traced() bool {
	if m == nil {
		return false
	}
	traceMu.Lock()
	defer traceMu.Unlock()
	_, ok := traceIDs[m]
	return ok
}

func (m *MerklePrefixMatcher) traceRec(op string) map[string]any {
	traceMu.Lock()
	info := traceIDs[m]
	traceMu.Unlock()
	at := int64(0)
	if !info.realClock {
		at = m.now().Sub(info.base).Nanoseconds()
	}
	return map[string]any{"op": op, "m": info.id, "test": info.test, "at": at}
}

// traceWriteRec writes a matcher call record under its matcher's test.
func traceWriteRec(r map[string]any) {
	traceWrite(r["test"].(string), r)
}

func ExtractCanonicalTurns(format sdktranslator.Format, payload []byte) []CanonicalTurn {
	rec := traceEnter()
	defer traceLeave()
	turns := extractCanonicalTurnsUntraced(format, payload)
	if rec {
		test := traceTest()
		traceWrite(test, map[string]any{"op": "extract", "test": test, "format": string(format), "payload": payload, "turns": traceTurns(turns), "nil": turns == nil})
	}
	return turns
}

func FastTurnFingerprint(turn CanonicalTurn) string {
	rec := traceEnter()
	defer traceLeave()
	fp := fastTurnFingerprintUntraced(turn)
	if rec {
		test := traceTest()
		traceWrite(test, map[string]any{"op": "fingerprint", "test": test, "turn": traceTurns([]CanonicalTurn{turn})[0], "out": fp})
	}
	return fp
}

func NewMerklePrefixMatcherWithConfig(cfg MerklePrefixMatcherConfig) *MerklePrefixMatcher {
	m := newMerklePrefixMatcherWithConfigUntraced(cfg)
	// Goroutines race in TestMerklePrefixMatcherConcurrentAccess, so what it records
	// differs between runs; its matchers are left untraced.
	test := traceTest()
	if os.Getenv("LCP_TRACE") == "" || strings.HasSuffix(test, "TestMerklePrefixMatcherConcurrentAccess") {
		return m
	}
	traceMu.Lock()
	traceCount[test]++
	id := test + "#" + strconv.Itoa(traceCount[test])
	traceIDs[m] = traceInfo{id: id, test: test, base: m.now(), realClock: cfg.NowFunc == nil}
	traceMu.Unlock()
	traceWrite(test, map[string]any{"op": "new", "m": id, "test": test, "ttl": m.ttl.Nanoseconds(), "max_turns": m.maxTurns, "max_groups": m.maxGroups, "max_prefixes": m.maxPrefixes})
	return m
}

func (m *MerklePrefixMatcher) Prepare(turns []CanonicalTurn) ([]string, int) {
	rec := traceEnter()
	defer traceLeave()
	in := traceTurns(turns)
	fps, min := m.prepareUntraced(turns)
	if rec && m.traced() {
		r := m.traceRec("prepare")
		r["turns"], r["fps"], r["min"] = in, fps, min
		traceWriteRec(r)
	}
	return fps, min
}

func (m *MerklePrefixMatcher) PrepareExt(turns []CanonicalTurn) (fingerprints []string, minPrefixLength int, tailFingerprints []string, envDigest string) {
	rec := traceEnter()
	defer traceLeave()
	in := traceTurns(turns)
	fingerprints, minPrefixLength, tailFingerprints, envDigest = m.prepareExtUntraced(turns)
	if rec && m.traced() {
		r := m.traceRec("prepare_ext")
		r["turns"], r["fps"], r["min"], r["tails"], r["env"] = in, fingerprints, minPrefixLength, tailFingerprints, envDigest
		traceWriteRec(r)
	}
	return
}

func (m *MerklePrefixMatcher) MatchFingerprintsWithContext(namespace string, fingerprints, tailFingerprints []string, envDigest string, minPrefixLength int) (MerklePrefixMatch, bool) {
	rec := traceEnter()
	defer traceLeave()
	var r map[string]any
	if rec && m.traced() {
		r = m.traceRec("match")
		r["ns"], r["fps"], r["tails"], r["env"], r["min"] = namespace, append([]string(nil), fingerprints...), append([]string(nil), tailFingerprints...), envDigest, minPrefixLength
	}
	match, ok := m.matchFingerprintsWithContextUntraced(namespace, fingerprints, tailFingerprints, envDigest, minPrefixLength)
	if r != nil {
		r["ok"], r["out"] = ok, match
		traceWriteRec(r)
	}
	return match, ok
}

func (m *MerklePrefixMatcher) BindFingerprintsWithContext(namespace string, fingerprints, tailFingerprints []string, envDigest string, minPrefixLength int, authID string) MerklePrefixBindResult {
	rec := traceEnter()
	defer traceLeave()
	var r map[string]any
	if rec && m.traced() {
		r = m.traceRec("bind")
		r["ns"], r["fps"], r["tails"], r["env"], r["min"], r["auth"] = namespace, append([]string(nil), fingerprints...), append([]string(nil), tailFingerprints...), envDigest, minPrefixLength, authID
	}
	out := m.bindFingerprintsWithContextUntraced(namespace, fingerprints, tailFingerprints, envDigest, minPrefixLength, authID)
	if r != nil {
		r["out"] = out
		traceWriteRec(r)
	}
	return out
}

func (m *MerklePrefixMatcher) TouchFingerprintsWithContext(namespace string, fingerprints, tailFingerprints []string, envDigest string, minPrefixLength int, authID string) bool {
	rec := traceEnter()
	defer traceLeave()
	var r map[string]any
	if rec && m.traced() {
		r = m.traceRec("touch")
		r["ns"], r["fps"], r["tails"], r["env"], r["min"], r["auth"] = namespace, append([]string(nil), fingerprints...), append([]string(nil), tailFingerprints...), envDigest, minPrefixLength, authID
	}
	ok := m.touchFingerprintsWithContextUntraced(namespace, fingerprints, tailFingerprints, envDigest, minPrefixLength, authID)
	if r != nil {
		r["ok"] = ok
		traceWriteRec(r)
	}
	return ok
}

func (m *MerklePrefixMatcher) RemoveFingerprintsBefore(namespace string, fingerprints []string, authID string, maxGeneration uint64) bool {
	rec := traceEnter()
	defer traceLeave()
	var r map[string]any
	if rec && m.traced() {
		r = m.traceRec("remove")
		r["ns"], r["fps"], r["auth"], r["gen"] = namespace, append([]string(nil), fingerprints...), authID, maxGeneration
	}
	ok := m.removeFingerprintsBeforeUntraced(namespace, fingerprints, authID, maxGeneration)
	if r != nil {
		r["ok"] = ok
		traceWriteRec(r)
	}
	return ok
}

func (m *MerklePrefixMatcher) InvalidateAuth(authID string) {
	rec := traceEnter()
	defer traceLeave()
	m.invalidateAuthUntraced(authID)
	if rec && m.traced() {
		r := m.traceRec("invalidate")
		r["auth"] = authID
		traceWriteRec(r)
	}
}

func (m *MerklePrefixMatcher) Clear() {
	rec := traceEnter()
	defer traceLeave()
	m.clearUntraced()
	if rec && m.traced() {
		traceWriteRec(m.traceRec("clear"))
	}
}

func (m *MerklePrefixMatcher) LookupSession(sessionID string) (authIDs []string, namespace string, ok bool) {
	rec := traceEnter()
	defer traceLeave()
	var r map[string]any
	if rec && m.traced() {
		r = m.traceRec("lookup")
		r["session"] = sessionID
	}
	authIDs, namespace, ok = m.lookupSessionUntraced(sessionID)
	if r != nil {
		r["auths"], r["ns"], r["ok"] = authIDs, namespace, ok
		traceWriteRec(r)
	}
	return
}
