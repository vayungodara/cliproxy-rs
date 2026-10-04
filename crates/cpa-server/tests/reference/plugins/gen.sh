#!/bin/sh
# Regenerates ../../fixtures/plugin_routes_go.json from Go's management server.
# Usage: gen.sh /absolute/path/to/CLIProxyAPI   (checkout at 6fecc6e; not modified)
set -eu
# Offline: no toolchain download, modules from the local cache only. Run `go mod download`
# in the reference checkout and in crates/cpa-plugin/tests/goplugins once beforehand.
export GOTOOLCHAIN=local GOPROXY=off GONOPROXY=none GOPRIVATE= GOSUMDB=off
reference=${1:?usage: gen.sh /path/to/CLIProxyAPI}
here=$(cd "$(dirname "$0")" && pwd)
crate=$(cd "$here/../../.." && pwd)
plugins=$(cd "$crate/../cpa-plugin/tests/goplugins" && pwd)
built=$(mktemp -d)
module=$(mktemp -d)
trap 'rm -r "$built" "$module"' EXIT
(
  cd "$plugins"
  go build -buildmode=c-shared -o "$built/recorder.so" ./examples/recorder
)
cp "$here"/*.go "$module/"
(
  cd "$module"
  go mod init github.com/router-for-me/CLIProxyAPI/v8/fixture >/dev/null 2>&1
  go mod edit -require=github.com/router-for-me/CLIProxyAPI/v8@v8.0.0
  go mod edit -replace=github.com/router-for-me/CLIProxyAPI/v8="$reference"
  go mod tidy >/dev/null 2>&1
  # Test seams for the plugin store, overlaid into the build (the checkout is untouched).
  printf '{"Replace": {"%s": "%s", "%s": "%s"}}\n' \
    "$reference/internal/api/zz_fixture_hooks.go" "$here/overlay/api_hooks.go" \
    "$reference/internal/api/handlers/management/zz_fixture_hooks.go" "$here/overlay/management_hooks.go" \
    > overlay.json
  go run -overlay overlay.json . "$built" "$crate/tests/fixtures/plugin_routes_go.json"
)
