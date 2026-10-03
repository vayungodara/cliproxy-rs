#!/usr/bin/env bash
# Memory and throughput comparison behind docs/BENCHMARKS.md.
#
#   bench/run.sh <cliproxy binary> <Go cli-proxy-api binary> <output dir> [rounds]
#
# Each server gets the same config.yaml: one OpenAI-compatible provider pointing at the
# fake upstream in bench/upstream, one client key, an empty credential directory. The
# server is pinned to CPU 0; the fake upstream and the load generator share CPU 1.
# Every scenario starts a fresh server process, so peak memory is per scenario.
# Needs Go (to build the two helpers), curl, jq and taskset. DURATION (default 20s),
# SCENARIOS and SERVERS narrow a run.
#
# External network is denied for the whole run: the script re-runs itself in a
# loopback-only network namespace (unshare), or, where that is not allowed, points every
# proxy variable at a closed local port.
set -euo pipefail

if [[ -z ${BENCH_ISOLATION:-} ]]; then
  if unshare -rn true 2>/dev/null; then
    BENCH_ISOLATION=netns exec unshare -rn bash -c \
      'ip link set lo up && exec unshare --user --map-user="$1" --map-group="$2" -- "${@:3}"' \
      _ "$(id -u)" "$(id -g)" "$0" "$@"
  fi
  export BENCH_ISOLATION=proxy-env HTTPS_PROXY=http://127.0.0.1:9 HTTP_PROXY=http://127.0.0.1:9 \
    ALL_PROXY=http://127.0.0.1:9 NO_PROXY=127.0.0.1,localhost,::1
fi
export GOPROXY=off GOTOOLCHAIN=local
echo "network isolation: $BENCH_ISOLATION" >&2

RUST_BIN=$(realpath "$1")
GO_BIN=$(realpath "$2")
OUT=$(realpath -m "$3")
ROUNDS=${4:-3}
HERE=$(cd "$(dirname "$0")" && pwd)
PORT=8340
UP_PORT=9201
KEY=sk-bench-client-key
DURATION=${DURATION:-20s}

mkdir -p "$OUT/bin" "$OUT/logs"
go build -o "$OUT/bin/upstream" "$HERE/upstream/main.go"
go build -o "$OUT/bin/load" "$HERE/load/main.go"

filler=$(printf 'Explain the change in this diff and list any risks. %.0s' $(seq 1 30))
cat > "$OUT/chat.json" <<EOF
{"model":"bench-model","messages":[{"role":"system","content":"You are a careful reviewer."},{"role":"user","content":"$filler"}]}
EOF
cat > "$OUT/chat-stream.json" <<EOF
{"model":"bench-model","stream":true,"messages":[{"role":"system","content":"You are a careful reviewer."},{"role":"user","content":"$filler"}]}
EOF
cat > "$OUT/messages-stream.json" <<EOF
{"model":"bench-model","max_tokens":256,"stream":true,"system":"You are a careful reviewer.","messages":[{"role":"user","content":"$filler"}]}
EOF

write_config() { # dir
  mkdir -p "$1/auth"
  cat > "$1/config.yaml" <<EOF
config-version: 8
server:
  host: "127.0.0.1"
  port: $PORT
access:
  api-keys:
    - "$KEY"
oauth:
  auth-dir: "$1/auth"
api-keys:
  openai-compatibility:
    - name: bench
      base-url: "http://127.0.0.1:$UP_PORT/v1"
      keys:
        - api-key: "sk-bench-upstream-not-real"
      models:
        - name: "bench-model"
          alias: "bench-model"
EOF
}

kb() { awk -v k="$2:" '$1 == k {print $2}' "/proc/$1/status"; }
ticks() { awk '{print $14 + $15}' "/proc/$1/stat"; }
ms() { date +%s%3N; }

start_upstream() { # delay
  taskset -c 1 "$OUT/bin/upstream" -addr "127.0.0.1:$UP_PORT" -delay "$1" > "$OUT/logs/upstream.log" 2>&1 &
  UP_PID=$!
  for _ in $(seq 1 100); do curl -sf "http://127.0.0.1:$UP_PORT/v1/models" > /dev/null && return; sleep 0.05; done
  echo "upstream did not start" >&2; exit 1
}

start_server() { # name scenario round
  local dir="$OUT/run/$1-$2-$3"
  mkdir -p "$OUT/run"
  [[ -e "$dir" ]] && mv "$dir" "$(mktemp -d /tmp/bench-old.XXXXXX)"
  write_config "$dir"
  local t0; t0=$(ms)
  # -local-model on both: no remote model catalog download during the run.
  local bin=$GO_BIN
  [[ $1 == rust ]] && bin=$RUST_BIN
  taskset -c 0 "$bin" -config "$dir/config.yaml" -local-model > "$OUT/logs/$1-$2-$3.log" 2>&1 &
  SRV_PID=$!
  for _ in $(seq 1 400); do
    if curl -sf -H "Authorization: Bearer $KEY" "http://127.0.0.1:$PORT/v1/models" | grep -q bench-model; then
      STARTUP_MS=$(( $(ms) - t0 )); return
    fi
    sleep 0.025
  done
  echo "$1 did not start; see $OUT/logs/$1-$2-$3.log" >&2; exit 1
}

stop() { kill "$1" 2>/dev/null || true; wait "$1" 2>/dev/null || true; }

load() { # path body expect workers duration [headers...]
  local path=$1 body=$2 expect=$3 c=$4 d=$5; shift 5
  local args=()
  for h in "$@"; do args+=(-H "$h"); done
  taskset -c 1 "$OUT/bin/load" -url "http://127.0.0.1:$PORT$path" -body "$OUT/$body" -expect "$expect" \
    -c "$c" -d "$d" -H "Authorization: Bearer $KEY" "${args[@]}"
}

scenario() { # name round server
  local name=$1 round=$2 server=$3 delay=0s path body expect c headers=()
  case $name in
    idle) ;;
    chat) path=/v1/chat/completions body=chat.json expect='"chat.completion"' c=32 ;;
    chat-stream) path=/v1/chat/completions body=chat-stream.json expect='[DONE]' c=32 ;;
    chat-stream-slow) path=/v1/chat/completions body=chat-stream.json expect='[DONE]' c=256 delay=50ms ;;
    messages-stream) path=/v1/messages body=messages-stream.json expect='message_stop' c=32
      headers=("anthropic-version: 2023-06-01") ;;
  esac
  start_upstream "$delay"
  start_server "$server" "$name" "$round"
  local idle_rss="" result='{}' cpu=0 hwm rss settled
  if [[ $name == idle ]]; then
    sleep 15
    idle_rss=$(kb "$SRV_PID" VmRSS)
  else
    load "$path" "$body" "$expect" 8 3s "${headers[@]}" > /dev/null # warm-up
    local t1; t1=$(ticks "$SRV_PID")
    result=$(load "$path" "$body" "$expect" "$c" "$DURATION" "${headers[@]}")
    cpu=$(( $(ticks "$SRV_PID") - t1 ))
  fi
  hwm=$(kb "$SRV_PID" VmHWM); rss=$(kb "$SRV_PID" VmRSS)
  settled=$rss
  [[ $name != idle ]] && { sleep 10; settled=$(kb "$SRV_PID" VmRSS); }
  stop "$SRV_PID"; stop "$UP_PID"
  jq -cn --arg server "$server" --arg scenario "$name" --argjson round "$round" \
    --argjson startup_ms "$STARTUP_MS" --arg idle_rss "$idle_rss" --argjson hwm "$hwm" \
    --argjson rss "$rss" --argjson settled "$settled" --argjson cpu_ticks "$cpu" --argjson load "$result" \
    '{server:$server, scenario:$scenario, round:$round, startup_ms:$startup_ms,
      idle_rss_kb:($idle_rss|tonumber? // null), hwm_kb:$hwm, rss_end_kb:$rss, rss_settled_kb:$settled,
      cpu_s:($cpu_ticks/100), load:$load}' | tee -a "$OUT/results.jsonl"
}

: > "$OUT/results.jsonl"
for round in $(seq 1 "$ROUNDS"); do
  for name in ${SCENARIOS:-idle chat chat-stream chat-stream-slow messages-stream}; do
    for server in ${SERVERS:-rust go}; do scenario "$name" "$round" "$server"; done
  done
done
