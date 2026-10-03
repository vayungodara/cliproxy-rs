#!/bin/sh
# Regenerates ../../fixtures/pluginhost_go.json from the Go plugin host.
# Usage: gen.sh /absolute/path/to/CLIProxyAPI   (checkout at 6fecc6e; not modified)
set -eu
# Offline: no toolchain download, modules from the local cache only. Run `go mod download`
# in the reference checkout and in tests/goplugins once beforehand.
export GOTOOLCHAIN=local GOPROXY=off GONOPROXY=none GOPRIVATE= GOSUMDB=off
reference=${1:?usage: gen.sh /path/to/CLIProxyAPI}
here=$(cd "$(dirname "$0")" && pwd)
crate=$(cd "$here/../../.." && pwd)
built=$(mktemp -d)
module=$(mktemp -d)
trap 'rm -r "$built" "$module"' EXIT
(
  cd "$crate/tests/goplugins"
  for example in examples/*/; do
    name=$(basename "$example")
    go build -buildmode=c-shared -o "$built/$name.so" "./$example"
  done
)
cp "$here"/*.go "$module/"
(
  cd "$module"
  go mod init github.com/router-for-me/CLIProxyAPI/v8/fixture >/dev/null 2>&1
  go mod edit -require=github.com/router-for-me/CLIProxyAPI/v8@v8.0.0
  go mod edit -replace=github.com/router-for-me/CLIProxyAPI/v8="$reference"
  go mod tidy >/dev/null 2>&1
  go run . "$built" "$crate/tests/fixtures/pluginhost_go.json"
)
