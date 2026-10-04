#!/bin/sh
# Regenerates ../../fixtures/plugin_wiring_go.json from Go's server binary.
# Usage: gen.sh /absolute/path/to/CLIProxyAPI   (checkout at 6fecc6e; not modified)
# Run it with the network denied: the servers and the upstream are on loopback.
set -eu
# Offline: no toolchain download, modules from the local cache only.
export GOTOOLCHAIN=local GOPROXY=off GONOPROXY=none GOPRIVATE= GOSUMDB=off
reference=${1:?usage: gen.sh /path/to/CLIProxyAPI}
here=$(cd "$(dirname "$0")" && pwd)
crate=$(cd "$here/../../.." && pwd)
plugins=$(cd "$crate/../cpa-plugin/tests/goplugins" && pwd)
built=$(mktemp -d)
trap 'rm -r "$built"' EXIT
(cd "$reference" && go build -o "$built/server" ./cmd/server)
(cd "$plugins" && go build -buildmode=c-shared -o "$built/recorder.so" ./examples/recorder)
(cd "$here" && go run main.go scenarios.go "$built/server" "$built/recorder.so" "$crate/tests/fixtures/plugin_wiring_go.json")
