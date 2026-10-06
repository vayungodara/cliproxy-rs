#!/usr/bin/env bash
# Memory soak: does resident memory stay flat over hours of large requests, and are its
# peaks bounded?
#
#   bench/soak.sh <server binary> <minutes> [output dir]
#
# MIX=claude (default) is the load of bench/messages.sh: 8 sessions of 100 to 500 KB
# streamed /v1/messages requests against the fake Claude upstream, in batches of BATCH
# requests (default 600).
#
# MIX=field models a day of coding-agent traffic: 4 sessions, 2 of them Claude
# (streamed /v1/messages) and 2 Codex (streamed /v1/responses through the Codex executor
# and an API key whose base-url is the fake Codex upstream), each a conversation that
# grows from 200 KB to 2 MB in 100 KB steps and then starts over (requests average about
# 1 MB). Every 4th Claude turn first sends the same body to /v1/messages/count_tokens,
# alternating between the Claude model and the Codex model, so both local token counts
# run. Batches of BATCH turns (default 160).
#
# After each batch the server rests REST seconds (default 20) and its VmRSS and RssAnon
# are recorded in <output dir>/soak.tsv. Every 10 s, <output dir>/samples.tsv records
# VmRSS, VmHWM, threads, CPU ticks (1/100 s), context switches and RssAnon (VmRSS without
# the binary's file pages, which come and go with the page cache). After the last batch
# the server stays idle for IDLE seconds (default 0 for claude, 300 for field), then its
# wakeups are counted over 30 more seconds.
#
# The run fails on any failed request; when the resting RSS climbs: the highest resting
# RSS of the last third of the batches is more than 10% (plus 2 MB) above the highest of
# the first third, or the lowest of the last third is that far above the lowest of the
# first third (a floor that rises under peaks that do not); or when VmHWM ends above
# PEAK_KB (for MIX=field, soak.field.peak_hwm_kb in bench/budgets.txt with its
# tolerance, unless PEAK_KB is set; 0 turns the check off). The summary goes to
# <output dir>/summary.json. GitHub-hosted jobs stop at 6 hours, so CI runs at most 330
# minutes. External network is denied as in bench/run.sh.
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
[[ $MINUTES =~ ^[1-9][0-9]{0,3}$ ]] || { echo "minutes must be a positive whole number" >&2; exit 2; }
OUT=$(realpath -m "${3:-/tmp/cliproxy-soak}")
HERE=$(cd "$(dirname "$0")" && pwd)
MIX=${MIX:-claude}
PORT=8345
UP_PORT=9203
KEY=sk-bench-client-key
case $MIX in
  claude)
    LOAD=(-n "${BATCH:-600}" -c "${C:-8}" -min 100000 -max 500000 -step 25000)
    IDLE=${IDLE:-0} ;;
  field)
    LOAD=(-n "${BATCH:-160}" -c 4 -codex 2 -codex-model gpt-5.5 -count 4 -min 200000 -max 2000000 -step 100000)
    IDLE=${IDLE:-300}
    PEAK_KB=${PEAK_KB-$(awk '$1 == "soak.field.peak_hwm_kb" {gsub("_", "", $2); print int($2 * (1 + $3 / 100))}' "$HERE/budgets.txt")} ;;
  *) echo "MIX must be claude or field" >&2; exit 2 ;;
esac
mkdir -p "$OUT/bin" "$OUT/auth"
[[ -x $OUT/bin/messages ]] || go build -o "$OUT/bin/messages" "$HERE/messages/main.go"

"$OUT/bin/messages" upstream -addr "127.0.0.1:$UP_PORT" > "$OUT/upstream.log" 2>&1 &
UP_PID=$!
trap 'kill $UP_PID ${SRV_PID:-} ${SAMPLER:-} 2>/dev/null || true' EXIT
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
if [[ $MIX == field ]]; then
  cat >> "$OUT/config.yaml" <<EOF
  codex:
    - name: bench-codex
      base-url: "http://127.0.0.1:$UP_PORT"
      models:
        - name: "gpt-5.5"
          alias: "gpt-5.5"
      keys:
        - api-key: "sk-FAKE-bench-codex-key"
EOF
fi
"$BIN" -config "$OUT/config.yaml" -local-model > "$OUT/server.log" 2>&1 &
SRV_PID=$!
ready=false
for _ in $(seq 1 400); do
  curl -sf -m 2 -o /dev/null -H "Authorization: Bearer $KEY" "http://127.0.0.1:$PORT/v1/models" && { ready=true; break; }
  kill -0 "$SRV_PID" 2>/dev/null || break
  sleep 0.025
done
$ready || { echo "server did not answer" >&2; cat "$OUT/server.log" >&2; exit 1; }

status() { awk -v f="$1" '$1 == f ":" {print $2}' "/proc/$SRV_PID/status"; }
# Context switches of every thread, voluntary and not (as bench/idle.sh counts wakeups).
switches() { cat /proc/"$SRV_PID"/task/*/status 2>/dev/null | awk '$1 ~ /_ctxt_switches:$/ {n += $2} END {print n + 0}'; }
start=$(date +%s)
{ printf 't_s\trss_kb\thwm_kb\tthreads\tcpu_ticks\tctxsw\tanon_kb\n'
  while kill -0 "$SRV_PID" 2>/dev/null; do
    printf '%d\t%s\t%s\t%s\t%s\t%s\t%s\n' $(( $(date +%s) - start )) "$(status VmRSS)" "$(status VmHWM)" \
      "$(status Threads)" "$(awk '{print $14 + $15}' "/proc/$SRV_PID/stat")" "$(switches)" "$(status RssAnon)"
    sleep 10
  done; } > "$OUT/samples.tsv" 2>/dev/null &
SAMPLER=$!

printf 'batch\tminute\tok\tbad\trest_rss_kb\thwm_kb\trest_anon_kb\n' > "$OUT/soak.tsv"
: > "$OUT/batches.jsonl"
end=$(( start + MINUTES * 60 ))
batch=0
while (( $(date +%s) < end )); do
  batch=$((batch + 1))
  result=$("$OUT/bin/messages" load -url "http://127.0.0.1:$PORT/v1/messages" -key "$KEY" -seed "$batch" "${LOAD[@]}")
  echo "$result" >> "$OUT/batches.jsonl"
  sleep "${REST:-20}"
  printf '%d\t%d\t%s\t%s\t%s\t%s\t%s\n' "$batch" $(( ($(date +%s) - start) / 60 )) \
    "$(jq -r .ok <<<"$result")" "$(jq -r .bad <<<"$result")" "$(status VmRSS)" "$(status VmHWM)" "$(status RssAnon)" >> "$OUT/soak.tsv"
  tail -n 1 "$OUT/soak.tsv"
done
cpu_load=$(awk '{print $14 + $15}' "/proc/$SRV_PID/stat")
load_s=$(( $(date +%s) - start ))

idle_json='{}'
if (( IDLE > 0 )); then
  sleep "$IDLE"
  # Wakeups over the last 30 s of the idle phase, measured as bench/idle.sh does.
  c0=$(switches) t0=$(awk '{print $14 + $15}' "/proc/$SRV_PID/stat")
  sleep 30
  idle_json=$(jq -cn --argjson s "$IDLE" --argjson r "$(status VmRSS)" --argjson a "$(status RssAnon)" --argjson w "$(( $(switches) - c0 ))" \
    --argjson c "$(( $(awk '{print $14 + $15}' "/proc/$SRV_PID/stat") - t0 ))" --argjson t "$(status Threads)" \
    '{idle_after_s: $s, idle_rss_kb: $r, idle_anon_kb: $a, idle_wakeups_30s: $w, idle_cpu_ticks_30s: $c, idle_threads: $t}')
fi

verdict=0
awk -F'\t' -v peak="${PEAK_KB:-0}" -v hwm="$(status VmHWM)" 'NR > 1 {rss[++n] = $5; bad += $4}
  END {
    third = int(n / 3); if (third < 1) { print "too few batches for a verdict"; exit 1 }
    first_max = last_max = 0; first_min = rss[1]; last_min = rss[n]
    for (i = 1; i <= third; i++) { if (rss[i] > first_max) first_max = rss[i]; if (rss[i] < first_min) first_min = rss[i] }
    for (i = n - third + 1; i <= n; i++) { if (rss[i] > last_max) last_max = rss[i]; if (rss[i] < last_min) last_min = rss[i] }
    max_limit = first_max * 1.10 + 2048; min_limit = first_min * 1.10 + 2048
    printf "%d batches; %d failed requests\n", n, bad
    printf "resting RSS, highest: first third %d kB, last third %d kB (limit %d kB)\n", first_max, last_max, max_limit
    printf "resting RSS, lowest: first third %d kB, last third %d kB (limit %d kB)\n", first_min, last_min, min_limit
    fail = last_max > max_limit || last_min > min_limit || bad > 0
    if (peak > 0) {
      printf "VmHWM %d kB (limit %d kB)\n", hwm, peak
      fail = fail || hwm > peak
    }
    exit fail
  }' "$OUT/soak.tsv" || verdict=1

# CPU time per request depends on the machine, so the summary names it.
cpu_model=$(awk -F': ' '$1 ~ /^model name/ {print $2; exit}' /proc/cpuinfo)
jq -cn --arg mix "$MIX" --argjson minutes "$MINUTES" --argjson load_s "$load_s" \
  --arg cpu_model "$cpu_model" --argjson cpus "$(nproc)" \
  --argjson cpu "$cpu_load" --argjson hwm "$(status VmHWM)" --argjson idle "$idle_json" \
  --slurpfile batches "$OUT/batches.jsonl" --rawfile soak "$OUT/soak.tsv" --rawfile samples "$OUT/samples.tsv" '
  # Whole rows only: the sampler may be writing the last one.
  def col($text; $i): [$text | split("\n")[1:][] | split("\t") | select(length >= 7) | .[$i] | tonumber];
  (col($soak; 4)) as $rest | (col($soak; 6)) as $rest_anon | (col($samples; 1)) as $rss
  | (col($samples; 6)) as $anon | (col($samples; 3)) as $threads
  | ($rest | length / 3 | floor) as $third
  # The resting readings over the whole run and over the first and last thirds.
  | def windows($v): {min: ($v | min), max: ($v | max),
      first_third_min: ($v[:$third] | min), first_third_max: ($v[:$third] | max),
      last_third_min: ($v[-$third:] | min), last_third_max: ($v[-$third:] | max)};
  {mix: $mix, minutes: $minutes, load_seconds: $load_s, cpu_model: $cpu_model, cpus: $cpus, batches: ($rest | length),
     requests_ok: ([$batches[].ok] | add), requests_bad: ([$batches[].bad] | add),
     # messages_ok, responses_ok, count_tokens_ok and their _bad counts, when the load
     # reports them (MIX=field).
     requests_by_kind: ([$batches[] | to_entries[] | select(.key | test("^[a-z_]+_(ok|bad)$")) | select(.key != "ok" and .key != "bad")]
       | group_by(.key) | map({key: .[0].key, value: (map(.value) | add)}) | from_entries),
     avg_request_kb: (([$batches[] | .avg_request_kb * .ok] | add) / ([$batches[].ok] | add) | floor),
     rest_rss_kb: windows($rest), rest_anon_kb: windows($rest_anon),
     sampled_rss_kb: {min: ($rss | min), median: ($rss | sort | .[length / 2 | floor]), max: ($rss | max)},
     sampled_anon_kb: {max: ($anon | max)},
     hwm_kb: $hwm, max_threads: ($threads | max), cpu_ticks_under_load: $cpu, cpu_ms_per_request:
       ($cpu * 10 / (([$batches[].ok] | add)) | floor)} + $idle' | tee "$OUT/summary.json"
exit "$verdict"
