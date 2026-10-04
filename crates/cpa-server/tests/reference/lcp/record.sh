#!/bin/sh
# Regenerates tests/fixtures/lcp_go.jsonl.zst from Go's own LCP and selector tests.
# lcp.go's public entry points are renamed to *Untraced and zz_trace.go wraps them,
# both through a go build overlay; the Go checkout itself is never modified.
# Usage (network denied): record.sh <CLIProxyAPI checkout> <output .jsonl.zst>
set -eu
go_root=$1 out=$2 here=$(cd "$(dirname "$0")" && pwd) tmp=$(mktemp -d)
session=$go_root/sdk/cliproxy/session
sed -e 's/^func FastTurnFingerprint(/func fastTurnFingerprintUntraced(/' \
    -e 's/^func ExtractCanonicalTurns(/func extractCanonicalTurnsUntraced(/' \
    -e 's/^func NewMerklePrefixMatcherWithConfig(/func newMerklePrefixMatcherWithConfigUntraced(/' \
    -e 's/^func (m \*MerklePrefixMatcher) Prepare(/func (m *MerklePrefixMatcher) prepareUntraced(/' \
    -e 's/^func (m \*MerklePrefixMatcher) PrepareExt(/func (m *MerklePrefixMatcher) prepareExtUntraced(/' \
    -e 's/^func (m \*MerklePrefixMatcher) MatchFingerprintsWithContext(/func (m *MerklePrefixMatcher) matchFingerprintsWithContextUntraced(/' \
    -e 's/^func (m \*MerklePrefixMatcher) BindFingerprintsWithContext(/func (m *MerklePrefixMatcher) bindFingerprintsWithContextUntraced(/' \
    -e 's/^func (m \*MerklePrefixMatcher) TouchFingerprintsWithContext(/func (m *MerklePrefixMatcher) touchFingerprintsWithContextUntraced(/' \
    -e 's/^func (m \*MerklePrefixMatcher) RemoveFingerprintsBefore(/func (m *MerklePrefixMatcher) removeFingerprintsBeforeUntraced(/' \
    -e 's/^func (m \*MerklePrefixMatcher) InvalidateAuth(/func (m *MerklePrefixMatcher) invalidateAuthUntraced(/' \
    -e 's/^func (m \*MerklePrefixMatcher) Clear(/func (m *MerklePrefixMatcher) clearUntraced(/' \
    -e 's/^func (m \*MerklePrefixMatcher) LookupSession(/func (m *MerklePrefixMatcher) lookupSessionUntraced(/' \
    "$session/lcp.go" > "$tmp/lcp.go"
sed "1,2d" "$here/zz_trace.go" > "$tmp/zz_trace.go"  # drops the go:build ignore line
printf '{"Replace":{"%s/lcp.go":"%s/lcp.go","%s/zz_trace.go":"%s/zz_trace.go"}}' "$session" "$tmp" "$session" "$tmp" > "$tmp/overlay.json"
cd "$go_root"
LCP_TRACE=$tmp/trace.jsonl GOFLAGS=-mod=mod go test -count=1 -p 1 -parallel 1 -overlay "$tmp/overlay.json" \
  -run 'Merkle|Canonical|FastTurn|LookupSession|LCP|SessionAffinity|NormalizeCanonical' \
  ./sdk/cliproxy/session/ ./sdk/cliproxy/auth/
# Records carry a "test<TAB>" prefix: a stable sort by test keeps each test's own call
# order and hides the order the tests ran in.
tab=$(printf '\t')
LC_ALL=C sort -s -t "$tab" -k1,1 "$tmp/trace.jsonl" | cut -f2- > "$tmp/sorted.jsonl"
zstd -19 -q -f "$tmp/sorted.jsonl" -o "$out"
