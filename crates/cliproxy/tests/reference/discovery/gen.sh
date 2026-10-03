#!/bin/sh
# Regenerates ../../fixtures/discovery_*.json from CLIProxyAPI at 6fecc6e by overlaying
# the *_fixture_test.go files into the reference packages (`go test -overlay`); the
# reference checkout itself is not modified. Usage: gen.sh /abs/path/to/CLIProxyAPI
set -eu
reference=$1
here=$(cd "$(dirname "$0")" && pwd)
fixtures=$(cd "$here/../../fixtures" && pwd)
tmp=$(mktemp -d)
trap 'mv "$tmp" /tmp/cpa-fixture-trash.$$' EXIT
cat > "$tmp/overlay.json" <<EOF
{"Replace": {
  "$reference/internal/discovery/zz_fixture_test.go": "$here/discovery_fixture_test.go",
  "$reference/internal/cmd/zz_fixture_test.go": "$here/cmd_fixture_test.go",
  "$reference/cmd/server/zz_fixture_test.go": "$here/main_fixture_test.go"
}}
EOF
cd "$reference"
WRITABLE_PATH="$tmp/state" CPA_FIXTURE_OUT="$fixtures/discovery_go.json" \
  go test -count=1 -overlay "$tmp/overlay.json" -run '^TestZZDiscoveryFixture$' ./internal/discovery
CPA_FIXTURE_OUT="$fixtures/discovery_cmd_go.json" \
  go test -count=1 -overlay "$tmp/overlay.json" -run '^TestZZCmdFixture$' ./internal/cmd
CPA_FIXTURE_OUT="$fixtures/discovery_main_go.json" \
  go test -count=1 -overlay "$tmp/overlay.json" -run '^TestZZMainFixture$' ./cmd/server
