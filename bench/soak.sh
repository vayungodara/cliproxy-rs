#!/usr/bin/env bash
# Memory soak: does resident memory stay flat over hours of large Claude requests?
#
#   bench/soak.sh <server binary> <minutes> [output dir]
#
# The load of bench/messages.sh (8 sessions of 100 to 500 KB streamed /v1/messages
# requests against the fake Claude upstream), sent in batches of BATCH requests
# (default 600) until <minutes> have passed. After each batch the server rests REST
# seconds (default 20) and its VmRSS is recorded in <output dir>/soak.tsv. The run fails
# when the highest resting RSS of the last third of the batches is more than 10% (plus
# 2 MB) above the highest of the first third. GitHub-hosted jobs stop at 6 hours, so
# CI runs at most 330 minutes. External network is denied as in bench/run.sh.
set -euo pipefail

if [[ -z ${BENCH_ISOLATION:-} ]]; then
  if unshare -rn true 2>/dev/null; then
    BENCH_ISOLATION=netns exec unshare -rn bash -c \
      'ip link set lo up && exec unshare --user --map-user="$1" --map-group="$2" -- "${@:3}"' \
      _ "$(id -u)" "$(id -g)" "$0" "$@"
  fi
  export BENCH_ISOLATION=proxy-env HTTPS_PROXY=http://127.0.0.1:9 HTTP_PROXY=http://127.0.0.1:9 \
    ALL_PROXY=http://127.0.0.1:9 NO_PROXY=127.0.0.1,localhost,::1
  # curl and Go read the lowercase names first.
  export https_proxy=$HTTPS_PROXY http_proxy=$HTTP_PROXY all_proxy=$ALL_PROXY no_proxy=$NO_PROXY
fi
export GOPROXY=off GOTOOLCHAIN=local

BIN=$(realpath "$1")
MINUTES=$2
OUT=$(realpath -m "${3:-/tmp/cliproxy-soak}")
HERE=$(cd "$(dirname "$0")" && pwd)
PORT=8345
UP_PORT=9203
KEY=sk-bench-client-key
mkdir -p "$OUT/bin" "$OUT/auth"
[[ -x $OUT/bin/messages ]] || go build -o "$OUT/bin/messages" "$HERE/messages/main.go"

"$OUT/bin/messages" upstream -addr "127.0.0.1:$UP_PORT" > "$OUT/upstream.log" 2>&1 &
UP_PID=$!
trap 'kill $UP_PID ${SRV_PID:-} 2>/dev/null || true' EXIT
cat > "$OUT/config.yaml" <<EOF
config-version: 8
server:
  host: "127.0.0.1"
  port: $PORT
access:
  api-keys:
    - "$KEY"
oauth:
  auth-dir: "$OUT/auth"
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
"$BIN" -config "$OUT/config.yaml" -local-model > "$OUT/server.log" 2>&1 &
SRV_PID=$!
ready=false
for _ in $(seq 1 400); do
  curl -sf -m 2 -o /dev/null -H "Authorization: Bearer $KEY" "http://127.0.0.1:$PORT/v1/models" && { ready=true; break; }
  kill -0 "$SRV_PID" 2>/dev/null || break
  sleep 0.025
done
$ready || { echo "server did not answer" >&2; cat "$OUT/server.log" >&2; exit 1; }

rss() { awk '$1 == "VmRSS:" {print $2}' "/proc/$SRV_PID/status"; }
printf 'batch\tminute\tok\tbad\trest_rss_kb\thwm_kb\n' > "$OUT/soak.tsv"
end=$(( $(date +%s) + MINUTES * 60 ))
start=$(date +%s)
batch=0
while (( $(date +%s) < end )); do
  batch=$((batch + 1))
  result=$("$OUT/bin/messages" load -url "http://127.0.0.1:$PORT/v1/messages" -key "$KEY" \
    -n "${BATCH:-600}" -c "${C:-8}" -min 100000 -max 500000 -step 25000)
  sleep "${REST:-20}"
  printf '%d\t%d\t%s\t%s\t%s\t%s\n' "$batch" $(( ($(date +%s) - start) / 60 )) \
    "$(jq -r .ok <<<"$result")" "$(jq -r .bad <<<"$result")" "$(rss)" \
    "$(awk '$1 == "VmHWM:" {print $2}' "/proc/$SRV_PID/status")" >> "$OUT/soak.tsv"
  tail -n 1 "$OUT/soak.tsv"
done

awk -F'\t' 'NR > 1 {rss[++n] = $5; bad += $4}
  END {
    third = int(n / 3); if (third < 1) { print "too few batches for a verdict"; exit 1 }
    for (i = 1; i <= third; i++) if (rss[i] > first) first = rss[i]
    for (i = n - third + 1; i <= n; i++) if (rss[i] > last) last = rss[i]
    limit = first * 1.10 + 2048
    printf "%d batches; resting RSS: first third up to %d kB, last third up to %d kB (limit %d kB); %d failed requests\n", n, first, last, limit, bad
    exit (last > limit || bad > 0)
  }' "$OUT/soak.tsv"
