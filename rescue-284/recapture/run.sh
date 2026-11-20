#!/usr/bin/env bash
# Recaptures docs/img/overview-light.png and overview-dark.png into OUT, not the repo.
# Everything runs in a loopback-only network namespace: a mock provider, the proxy with
# a temporary config and auth-dir, demo traffic, then a headless browser.
#
#   run.sh <cliproxy binary> <out-dir> [seconds of traffic, default 12600] [requests, default 275]
#
# DIST=<ui/dist> serves that dashboard build in front of the binary, for a release
# binary that embeds an older dashboard. Without DIST the binary serves its own.
# The traffic is real requests through the proxy to the mock. The dashboard's traffic
# view has twenty 10-minute buckets on the wall clock, so a full picture takes 210
# minutes. The key ending 451d answers 429 for the stretch from 40% to 62% of the run.
# The proxy counts traffic per credential, and a credential's identity includes its base
# URL, so the config keeps the mock's URLs to the end. Two sets of pictures come out:
# plain/ shows the page as served, with the mock's URLs, and relabelled/ shows the same
# page with those URLs replaced by the providers' real ones on the way to the browser.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
if [[ ${1:-} != --inside ]]; then
  [[ $# -ge 2 ]] || { sed -n 2,6p "$0" >&2; exit 2; }
  exec unshare -rn bash -c 'ip link set lo up && exec unshare --user --map-user="$1" --map-group="$2" -- "$3" --inside "${@:4}"' \
    _ "$(id -u)" "$(id -g)" "$here/run.sh" "$@"
fi
shift
bin=$(realpath "$1") out=$(realpath -m "$2") seconds=${3:-12600} requests=${4:-275}
pw=${PW:-/home/vayun/projects/.amp/in/wt-dash-design/ui/node_modules/playwright-core}
d=$(mktemp -d)
mkdir -p "$d/auth" "$out"
client=sk-shot-client-key management=shot-management-key
port=8317; [[ -z ${DIST:-} ]] || port=8318
groups() { # $1: which base URL of each pair to use (mock or real)
  python3 - "$1" <<'PY'
import json, sys
mock = sys.argv[1] == "mock"
rows = [
    ("DeepSeek", "http://127.0.0.1:9101/deepseek", "https://api.deepseek.com", ["sk-shot-7f3a9c0e5b1d42a6451d", "sk-shot-1c8e4d7a2f6b9035b518"], [("deepseek-v4-flash", "deepseek")]),
    ("GLM", "http://127.0.0.1:9101/glm/api/paas/v4", "https://api.z.ai/api/paas/v4", ["2bf0shot6a1e9d3c7b5f8024dc99"], [("glm-5.3", "glm")]),
    ("Kimi", "http://127.0.0.1:9101/kimi/v1", "https://api.moonshot.ai/v1", ["sk-shot-9d2b6f4a0c7e1835e5c2"], [("kimi-k3", "kimi")]),
    ("OpenRouter", "http://127.0.0.1:9101/openrouter/api/v1", "https://openrouter.ai/api/v1", ["sk-shot-4a7c1e9b3d5f6208dad6"], [("deepseek/deepseek-v4-flash", "deepseek"), ("qwen/qwen3.8-max", "qwen")]),
]
print(json.dumps([{"name": n, "base-url": m if mock else r, "keys": [{"api-key": k} for k in ks], "models": [{"name": a, "alias": b} for a, b in ms]} for n, m, r, ks, ms in rows]))
PY
}
cat > "$d/config.yaml" <<YAML
config-version: 8
server:
  host: "127.0.0.1"
  port: $port
access:
  api-keys:
    - "$client"
management:
  secret-key: "$management"
oauth:
  auth-dir: "$d/auth"
YAML
api="http://127.0.0.1:$port/v8/management"
mgmt() { curl -fsS -m 10 -H "Authorization: Bearer $management" "$@"; }
FAIL_KEY=451d FAIL_FROM=$((seconds * 40 / 100)) FAIL_TO=$((seconds * 62 / 100)) python3 "$here/mock.py" 9101 > "$out/mock.log" 2>&1 &
pids=($!)
(cd "$d" && unset HOME_JWT PGSTORE_DSN OBJECTSTORE_ENDPOINT GITSTORE_GIT_URL && exec "$bin" --config "$d/config.yaml" --local-model) > "$out/proxy.log" 2>&1 &
pids+=($!)
if [[ -n ${DIST:-} ]]; then python3 "$here/front.py" 8317 8318 "$DIST" > "$out/front.log" 2>&1 & pids+=($!); fi
trap 'kill "${pids[@]}" 2>/dev/null; rm -rf "$d"' EXIT
for _ in $(seq 100); do
  if curl -fs -m 2 -o /dev/null -H "Authorization: Bearer $client" "http://127.0.0.1:$port/v1/models" && curl -fs -m 2 -o /dev/null http://127.0.0.1:9101/ready && curl -fs -m 2 -o /dev/null http://127.0.0.1:8317/management.html; then ready=1; break; fi
  sleep 0.1
done
[[ -n ${ready:-} ]] || { echo "the proxy, the mock or the dashboard did not start; see $out" >&2; exit 1; }
mgmt -o /dev/null -X PUT -H 'Content-Type: application/json' -d "$(groups mock)" "$api/config/api-keys/openai-compatibility"
sleep 1
echo "models: $(curl -fsS -m 5 -H "Authorization: Bearer $client" "http://127.0.0.1:$port/v1/models" | python3 -c 'import json,sys; print([m["id"] for m in json.load(sys.stdin)["data"]])')"
python3 "$here/traffic.py" "$port" "$client" "$seconds" "$requests" > "$out/traffic.log" 2>&1
tail -1 "$out/traffic.log"
mgmt "$api/observability/usage/api-keys" > "$out/usage.json"
python3 - "$out" <<'PY'
import json, sys
data = json.load(open(f"{sys.argv[1]}/usage.json"))
for p, keys in data.items():
    for k, v in keys.items():
        print(f"{p} ...{k[-4:]}: {v['success']} ok, {v['failed']} failed; failed buckets {[i for i, b in enumerate(v['recent_requests']) if b['failed']]}; busy buckets {sum(1 for b in v['recent_requests'] if b['success'] + b['failed'])}/20")
PY
echo "upstream: $(grep -c -- '-> 200' "$out/mock.log") x 200, $(grep -c -- '-> 429' "$out/mock.log") x 429"
relabel=$(python3 - "$(groups mock)" "$(groups real)" <<'PY'
import json, sys
mock, real = (json.loads(a) for a in sys.argv[1:3])
print(json.dumps({m["base-url"]: r["base-url"] for m, r in zip(mock, real)}))
PY
)
PW=$pw node "$here/capture.mjs" http://127.0.0.1:8317/management.html "$management" "$out/plain" "${HEIGHTS:-1000,1100}"
PW=$pw RELABEL=$relabel node "$here/capture.mjs" http://127.0.0.1:8317/management.html "$management" "$out/relabelled" "${HEIGHTS:-1000,1100}"
echo "done $(date -Is)"
