#!/usr/bin/env bash
# Claude Messages soak test behind the "Claude soak" section of docs/BENCHMARKS.md.
#
#   bench/messages.sh <output dir> <label> <server binary> [env NAME=value ...]
#   bench/messages.sh <output dir> direct
#
# Starts the fake Claude upstream (bench/messages), then the server with a config that
# routes claude-sonnet-4-5-20250929 through one Claude API key whose base-url is the
# fake upstream, then sends N (default 3000) streamed /v1/messages requests of 100 to
# 500 KB from C (default 8) concurrent sessions. The server's VmRSS, VmHWM and RssAnon
# are sampled every second into <output dir>/<label>.rss.tsv; the summary line goes to
# <output dir>/results.jsonl, and the RSS again SETTLE (default 30) seconds after the load. The label "direct" sends the same load straight to the
# upstream, which gives the baseline for the latency a proxy adds.
#
# The credential is a Claude API key: an OAuth token would make both servers fetch the
# account profile from api.anthropic.com, which this loopback-only run cannot reach. Trailing
# NAME=value arguments become the server's environment, for example MALLOC_ARENA_MAX=2.
# External network is denied the same way as bench/run.sh.
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

OUT=$(realpath -m "$1")
LABEL=$2
BIN=${3:+$(realpath "$3")}
shift $(( $# < 3 ? $# : 3 ))
HERE=$(cd "$(dirname "$0")" && pwd)
PORT=8341
UP_PORT=9202
KEY=sk-bench-client-key
N=${N:-3000}
C=${C:-8}
# The server gets SERVER_CPUS, the upstream and load generator LOAD_CPUS. DELAY (default
# 2ms) is the pause between the upstream's 150 delta events. MIN, MAX and STEP (default
# 100000, 500000 and 25000 bytes) shape the conversations.
SERVER_CPUS=${SERVER_CPUS:-0-3}
LOAD_CPUS=${LOAD_CPUS:-4-7}

mkdir -p "$OUT/bin" "$OUT/logs"
[[ -x $OUT/bin/messages ]] || go build -o "$OUT/bin/messages" "$HERE/messages/main.go"

taskset -c "$LOAD_CPUS" "$OUT/bin/messages" upstream -addr "127.0.0.1:$UP_PORT" -delay "${DELAY:-2ms}" > "$OUT/logs/upstream-$LABEL.log" 2>&1 &
UP_PID=$!
trap 'kill $UP_PID ${SRV_PID:-} ${SAMPLER:-} 2>/dev/null || true' EXIT
sleep 0.5

url="http://127.0.0.1:$UP_PORT/v1/messages"
if [[ $LABEL != direct ]]; then
  dir="$OUT/run/$LABEL"
  rm -rf "$dir" && mkdir -p "$dir/auth"
  cat > "$dir/config.yaml" <<EOF
config-version: 8
server:
  host: "127.0.0.1"
  port: $PORT
access:
  api-keys:
    - "$KEY"
oauth:
  auth-dir: "$dir/auth"
api-keys:
  claude:
    - name: bench
      base-url: "http://127.0.0.1:$UP_PORT"
      models:
        - name: "claude-sonnet-4-5-20250929"
          alias: "claude-sonnet-4-5-20250929"
      keys:
        - api-key: "sk-ant-api03-FAKE-bench-key"
EOF
  env "$@" taskset -c "$SERVER_CPUS" "$BIN" -config "$dir/config.yaml" -local-model > "$OUT/logs/$LABEL.log" 2>&1 &
  SRV_PID=$!
  for _ in $(seq 1 400); do
    curl -sf -H "Authorization: Bearer $KEY" "http://127.0.0.1:$PORT/v1/models" > /dev/null && break
    sleep 0.025
  done
  url="http://127.0.0.1:$PORT/v1/messages"
  t0=$(date +%s)
  { printf 't_s\tvmrss_kb\tvmhwm_kb\trssanon_kb\n'
    while kill -0 "$SRV_PID" 2>/dev/null; do
      awk -v t=$(( $(date +%s) - t0 )) '$1=="VmRSS:"{r=$2} $1=="VmHWM:"{h=$2} $1=="RssAnon:"{a=$2} END{print t "\t" r "\t" h "\t" a}' "/proc/$SRV_PID/status"
      sleep 1
    done; } > "$OUT/$LABEL.rss.tsv" 2>/dev/null &
  SAMPLER=$!
  idle_kb=$(awk '$1=="VmRSS:"{print $2}' "/proc/$SRV_PID/status")
  cpu0=$(awk '{print $14 + $15}' "/proc/$SRV_PID/stat")
fi

result=$(taskset -c "$LOAD_CPUS" "$OUT/bin/messages" load -url "$url" -key "$KEY" -n "$N" -c "$C" \
  -min "${MIN:-100000}" -max "${MAX:-500000}" -step "${STEP:-25000}")

extra='{}'
if [[ $LABEL != direct ]]; then
  cpu=$(( $(awk '{print $14 + $15}' "/proc/$SRV_PID/stat") - cpu0 ))
  end_kb=$(awk '$1=="VmRSS:"{print $2}' "/proc/$SRV_PID/status")
  sleep "${SETTLE:-30}"
  settled_kb=$(awk '$1=="VmRSS:"{print $2}' "/proc/$SRV_PID/status")
  hwm_kb=$(awk '$1=="VmHWM:"{print $2}' "/proc/$SRV_PID/status")
  extra=$(jq -cn --argjson i "$idle_kb" --argjson e "$end_kb" --argjson s "$settled_kb" \
    --argjson h "$hwm_kb" --argjson c "$cpu" \
    '{idle_rss_kb:$i, end_rss_kb:$e, settled_rss_kb:$s, hwm_kb:$h, cpu_s:($c/100)}')
fi
jq -cn --arg l "$LABEL" --arg v "$*" --argjson r "$result" --argjson m "$extra" \
  '{label:$l, env:$v} + $m + {load:$r}' | tee -a "$OUT/results.jsonl"
