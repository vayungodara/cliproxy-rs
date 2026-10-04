#!/bin/sh
# Regenerates ../../../src/management/iso_currency.rs from Go's currency.ParseISO.
# Usage: gen.sh /absolute/path/to/CLIProxyAPI   (checkout at 6fecc6e; not modified)
set -eu
export GOTOOLCHAIN=local GOPROXY=off GONOPROXY=none GOPRIVATE= GOSUMDB=off
reference=${1:?usage: gen.sh /path/to/CLIProxyAPI}
here=$(cd "$(dirname "$0")" && pwd)
crate=$(cd "$here/../../.." && pwd)
module=$(mktemp -d)
trap 'rm -r "$module"' EXIT
cp "$here"/*.go "$module/"
(
  cd "$module"
  go mod init github.com/router-for-me/CLIProxyAPI/v8/currencyfixture >/dev/null 2>&1
  go mod edit -require=github.com/router-for-me/CLIProxyAPI/v8@v8.0.0
  go mod edit -replace=github.com/router-for-me/CLIProxyAPI/v8="$reference"
  cp "$reference/go.sum" .
  go mod tidy -e >/dev/null 2>&1
  go run . "$crate/src/management/iso_currency.rs"
)
