#!/bin/sh
# Starts the mock provider and cliproxy-rs for the README demo, in a temporary
# directory, and waits until the proxy answers. Source it: `. docs/demo/start.sh`.
# It needs `cliproxy` on PATH (or CLIPROXY_BIN) and ports 8317 and 8399 free.
# `demo_stop` stops both and removes the directory.
demo_src=$(cd "$(dirname "${DEMO_SRC:-docs/demo/start.sh}")" && pwd)
demo_dir=$(mktemp -d)
mkdir "$demo_dir/auth"
sed "s|__AUTH_DIR__|$demo_dir/auth|" "$demo_src/config.yaml" > "$demo_dir/config.yaml"
python3 "$demo_src/mock_upstream.py" 8399 > "$demo_dir/upstream.log" 2>&1 &
demo_mock=$!
"${CLIPROXY_BIN:-cliproxy}" --config "$demo_dir/config.yaml" --local-model > "$demo_dir/proxy.log" 2>&1 &
demo_proxy=$!
demo_stop() { kill "$demo_mock" "$demo_proxy" 2>/dev/null; rm -rf "$demo_dir"; }
for _ in $(seq 50); do
  curl -fs -o /dev/null -H "Authorization: Bearer demo-client-key" http://127.0.0.1:8317/v1/models &&
    curl -s -o /dev/null http://127.0.0.1:8399/ && break
  sleep 0.1
done
export CLIPROXY_CLIENT_KEY=demo-client-key UPSTREAM_LOG="$demo_dir/upstream.log"
