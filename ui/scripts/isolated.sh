#!/usr/bin/env bash
# Runs a shell command with external network denied: inside a loopback-only network
# namespace, keeping the caller's uid and gid so file permissions behave as usual.
# Nothing outside the namespace can reach servers started inside it, so start the fake
# upstreams, the server under test and the browser in the same command.
#
#   scripts/isolated.sh 'server --config c.yaml & node scripts/panel-check-go.mjs ...'
set -euo pipefail
[[ $# -eq 1 ]] || { echo "usage: $0 '<shell command>'" >&2; exit 2; }
exec unshare -rn bash -c \
  'ip link set lo up && exec unshare --user --map-user="$1" --map-group="$2" -- bash -c "$3"' \
  _ "$(id -u)" "$(id -g)" "$1"
