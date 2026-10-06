#!/usr/bin/env bash
# Idle cost of one server process on macOS or Windows (Git Bash), the counterpart of
# bench/idle.sh for the release and weekly idle gates.
#
#   bench/idle-other.sh <macos|windows> <server binary> [auth files] [seconds]
#
# Same config and credential files as bench/idle.sh. After WARMUP seconds (default 30, as
# in bench/idle.sh: there is no heap trim here, but the config watcher's first look runs
# on the blocking pool at startup, and that thread exits after a 10 s keep-alive) without
# any request it samples for <seconds> (default 60) and prints one JSON line:
#
#   cpu_ms        user + system CPU time in the window, in milliseconds
#   idle_wakeups  (macOS) package idle exits caused by the process in the window,
#                 from `top -c d -stats idlew`
#   threads       threads at the end of the window
#   rss_kb        resident memory (macOS RSS, Windows working set) at the end
#
# Outbound requests go to a closed local proxy port, so nothing leaves the runner.
set -euo pipefail

OS=$1
BIN=$2
FILES=${3:-1}
WINDOW=${4:-60}
WARMUP=${WARMUP:-30}
PORT=${PORT:-8341}
KEY=sk-bench-client-key
export HTTPS_PROXY=http://127.0.0.1:9 HTTP_PROXY=http://127.0.0.1:9 ALL_PROXY=http://127.0.0.1:9 \
  NO_PROXY=127.0.0.1,localhost,::1
export https_proxy=$HTTPS_PROXY http_proxy=$HTTP_PROXY all_proxy=$ALL_PROXY no_proxy=$NO_PROXY
DIR=$(mktemp -d)
cleanup() {
  if [[ $OS == windows ]]; then
    taskkill //F //PID "$WINPID" > /dev/null 2>&1 || true
  else
    kill "$PID" 2> /dev/null || true
  fi
  rm -rf "$DIR" 2> /dev/null || true
}
trap cleanup EXIT

auth_dir=$DIR/auth
mkdir -p "$auth_dir"
[[ $OS == windows ]] && auth_dir=$(cygpath -m "$auth_dir")
cat > "$DIR/config.yaml" <<EOF
config-version: 8
server:
  host: "127.0.0.1"
  port: $PORT
access:
  api-keys:
    - "$KEY"
oauth:
  auth-dir: "$auth_dir"
api-keys:
  openai-compatibility:
    - name: bench
      base-url: "http://127.0.0.1:9/v1"
      keys:
        - api-key: "sk-bench-upstream"
      models:
        - name: "bench-model"
          alias: "bench-model"
EOF
expiry=$(date -u -v+30d +%Y-%m-%dT%H:%M:%SZ 2> /dev/null || date -u -d '+30 days' +%Y-%m-%dT%H:%M:%SZ)
for i in $(seq 1 "$FILES"); do
  printf '{"type":"claude","email":"idle%d@example.com","access_token":"bench-access-%d","refresh_token":"bench-refresh-%d","expired":"%s"}\n' \
    "$i" "$i" "$i" "$expiry" > "$DIR/auth/claude-idle$i.json"
done

config=$DIR/config.yaml
[[ $OS == windows ]] && config=$(cygpath -w "$config")
"$BIN" -config "$config" > "$DIR/server.log" 2>&1 &
PID=$!
WINPID=$PID
[[ $OS == windows ]] && WINPID=$(cat "/proc/$PID/winpid")
ready=false
for _ in $(seq 1 400); do
  curl -sf -m 2 -o /dev/null -H "Authorization: Bearer $KEY" "http://127.0.0.1:$PORT/v1/models" && { ready=true; break; }
  sleep 0.05
done
$ready || { echo "server did not answer" >&2; cat "$DIR/server.log" >&2; exit 1; }
sleep "$WARMUP"

case $OS in
  macos)
    cpu_ms() { ps -o time= -p "$PID" | awk -F'[:.]' '{print ($1 * 60 + $2) * 1000 + $3 * 10}'; }
    c0=$(cpu_ms)
    # Delta mode: the second sample counts the events of the window only. top exits 0
    # without a row when it cannot see the process, so a missing row is a failure.
    samples=$(top -l 2 -s "$WINDOW" -c d -pid "$PID" -stats pid,idlew)
    wakeups=$(awk -v p="$PID" '$1 == p {n++; w = $2} END {if (n >= 2) print w}' <<<"$samples")
    [[ -n $wakeups ]] || { echo "top reported no second sample for pid $PID:" >&2; echo "$samples" >&2; exit 1; }
    c1=$(cpu_ms)
    threads=$(ps -M -p "$PID" | tail -n +2 | wc -l | tr -d ' ')
    rss=$(ps -o rss= -p "$PID" | tr -d ' ')
    kill -0 "$PID" 2> /dev/null && [[ -n $c0 && -n $c1 && ${threads:-0} -gt 0 && ${rss:-0} -gt 0 ]] ||
      { echo "server gone or not measured (threads=$threads rss=$rss)" >&2; exit 1; }
    printf '{"os":"macos","auth_files":%d,"seconds":%d,"cpu_ms":%d,"idle_wakeups":%d,"threads":%d,"rss_kb":%d}\n' \
      "$FILES" "$WINDOW" $((c1 - c0)) "$wakeups" "$threads" "$rss"
    ;;
  windows)
    stats() {
      powershell -NoProfile -Command \
        "\$p = Get-Process -Id $WINPID -ErrorAction Stop; '{0} {1} {2}' -f [int64]\$p.TotalProcessorTime.TotalMilliseconds, \$p.Threads.Count, [int64](\$p.WorkingSet64 / 1024)" |
        tr -d '\r'
    }
    # Assigned first, so a failing Get-Process stops the script (set -e) instead of
    # reading as zeros.
    first=$(stats)
    read -r c0 _ _ <<<"$first"
    sleep "$WINDOW"
    second=$(stats)
    read -r c1 threads rss <<<"$second"
    [[ ${threads:-0} -gt 0 && ${rss:-0} -gt 0 && -n $c0 && -n $c1 ]] ||
      { echo "server gone or not measured: '$first' / '$second'" >&2; exit 1; }
    printf '{"os":"windows","auth_files":%d,"seconds":%d,"cpu_ms":%d,"threads":%d,"rss_kb":%d}\n' \
      "$FILES" "$WINDOW" $((c1 - c0)) "$threads" "$rss"
    ;;
  *) echo "usage: $0 <macos|windows> <binary> [auth files] [seconds]" >&2; exit 2 ;;
esac
