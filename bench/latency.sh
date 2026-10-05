#!/usr/bin/env bash
# Latency the proxy adds before the first byte on the Claude route, behind the "Claude
# latency" section of docs/BENCHMARKS.md.
#
#   bench/latency.sh <output.jsonl> [harness binary]
#
# Builds crates/cpa-server/examples/claude_latency.rs (release) unless a binary is given,
# then runs it for the OAuth/cloak path (native TLS client against a local TLS mock) and
# the API-key path (plain HTTP mock), appending one JSON line per body size and
# concurrency to <output.jsonl>. N, SIZES, CONC and WARMUP pass through to the harness;
# CPUS (default 0-7) pins the whole process.
#
# Everything runs in a loopback-only network namespace, so nothing can reach a provider;
# without unshare the script refuses to run.
set -euo pipefail

OUT=$(realpath -m "$1")
HERE=$(cd "$(dirname "$0")" && pwd)
BIN=${2:-}
if [[ -z $BIN ]]; then
  cargo build --release -p cpa-server --example claude_latency --manifest-path "$HERE/../Cargo.toml"
  BIN=$HERE/../target/release/examples/claude_latency
fi
BIN=$(realpath "$BIN")
unshare -rn true 2>/dev/null || { echo "needs unshare -rn (a loopback-only network namespace)" >&2; exit 1; }
mkdir -p "$(dirname "$OUT")"
for mode in oauth apikey; do
  unshare -rn bash -c 'ip link set lo up && exec taskset -c "$1" "$2" "$3"' _ "${CPUS:-0-7}" "$BIN" "$mode" | tee -a "$OUT"
done
