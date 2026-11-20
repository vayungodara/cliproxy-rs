#!/bin/sh
# Replays what the dashboard's OpenAI-compatible Add group form writes (Providers.svelte
# add() -> store.replace("api-keys/openai-compatibility")) against a fresh-install config,
# then what Use with tools does (clientModels) and one request. Run inside a private
# network namespace: 8317 on this machine is the live proxy.
#   unshare -r -n sh form-replay.sh <cliproxy binary> <repo worktree>
set -u
bin=$1 repo=$2
ip link set lo up
d=$(mktemp -d)
mkdir "$d/auth"
# The config install.sh writes on a fresh install (install.sh, "Written by install.sh").
cat > "$d/config.yaml" <<EOF
config-version: 8
server:
  host: "127.0.0.1"
  port: 8317
access:
  api-keys:
    - "sk-fresh-client"
management:
  secret-key: "fresh-management"
oauth:
  auth-dir: "$d/auth"
routing:
  session-affinity: true
EOF
python3 "$repo/docs/demo/mock_upstream.py" 8399 > "$d/upstream.log" 2>&1 &
mock=$!
(cd "$d" && exec "$bin" --config "$d/config.yaml" --local-model) > "$d/proxy.log" 2>&1 &
proxy=$!
trap 'kill $mock $proxy 2>/dev/null; rm -rf "$d"' EXIT
i=0
until curl -fs -m 2 -o /dev/null -H "Authorization: Bearer sk-fresh-client" http://127.0.0.1:8317/v1/models; do
  i=$((i + 1)); [ $i -lt 50 ] || { echo "proxy did not start"; cat "$d/proxy.log"; exit 1; }
  sleep 0.1
done
m="http://127.0.0.1:8317/v8/management/config/api-keys/openai-compatibility"
auth="Authorization: Bearer fresh-management"
models() { curl -s -m 5 -H "Authorization: Bearer sk-fresh-client" http://127.0.0.1:8317/v1/models | python3 -c 'import json,sys; print([m["id"] for m in json.load(sys.stdin).get("data", [])])'; }
echo "1. models before: $(models)"
echo "2. GET before: $(curl -s -m 5 -w ' [%{http_code}]' -H "$auth" "$m")"
# Model "mock-model", Alias left empty: the form saves alias = model.
echo "3. PUT (alias empty): $(curl -s -m 5 -w ' [%{http_code}]' -X PUT -H "$auth" -H 'Content-Type: application/json' \
  -d '[{"name":"mock","keys":[{"api-key":"keyA"}],"base-url":"http://127.0.0.1:8399/v1","models":[{"name":"mock-model","alias":"mock-model"}]}]' "$m")"
sleep 1
echo "4. models after: $(models)"
echo "5. Responses request, model mock-model:"
curl -s -m 10 -w ' [%{http_code}]\n' http://127.0.0.1:8317/v1/responses -H "Authorization: Bearer sk-fresh-client" -H 'Content-Type: application/json' \
  -d '{"model":"mock-model","input":"Say hello."}' | cut -c1-300
# A second group with an alias, as the form saves it when Alias is filled in.
echo "6. PUT (two groups, second with alias demo): $(curl -s -m 5 -w ' [%{http_code}]' -X PUT -H "$auth" -H 'Content-Type: application/json' \
  -d '[{"name":"mock","keys":[{"api-key":"keyA"}],"base-url":"http://127.0.0.1:8399/v1","models":[{"name":"mock-model","alias":"mock-model"}]},{"name":"mock2","keys":[{"api-key":"keyB"}],"base-url":"http://127.0.0.1:8399/v1","models":[{"name":"mock-model","alias":"demo"}]}]' "$m")"
sleep 1
echo "7. models after: $(models)"
echo "8. Messages request, model demo:"
curl -s -m 10 -w ' [%{http_code}]\n' http://127.0.0.1:8317/v1/messages -H "x-api-key: sk-fresh-client" -H 'anthropic-version: 2023-06-01' -H 'Content-Type: application/json' \
  -d '{"model":"demo","max_tokens":64,"messages":[{"role":"user","content":"Say hello."}]}' | cut -c1-300
echo "9. provider log:"; cat "$d/upstream.log"
echo "10. config on disk:"; sed -n '/^api-keys/,$p' "$d/config.yaml"
