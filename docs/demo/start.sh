#!/bin/sh
# Starts the mock provider and cliproxy-rs for the README demo, in a temporary
# directory, and waits until both answer. Source it: `. docs/demo/start.sh`.
# It needs `cliproxy` on PATH (or CLIPROXY_BIN) and ports 8317 and 8399 free.
# `demo_stop` stops both and removes the directory.
#
# The proxy starts inside the temporary directory, so it loads no .env file from
# yours, and without the variables that make it take its config from Home or a
# remote store instead of docs/demo/config.yaml.
demo_src=$(cd "$(dirname "${DEMO_SRC:-docs/demo/start.sh}")" && pwd)
demo_bin=$(command -v "${CLIPROXY_BIN:-cliproxy}")
case "$demo_bin" in /*) ;; *) demo_bin="$PWD/$demo_bin" ;; esac
demo_dir=$(mktemp -d)
mkdir "$demo_dir/auth"
sed "s|__AUTH_DIR__|$demo_dir/auth|" "$demo_src/config.yaml" > "$demo_dir/config.yaml"
python3 "$demo_src/mock_upstream.py" 8399 > "$demo_dir/upstream.log" 2>&1 &
demo_mock=$!
(
  cd "$demo_dir" || exit 1
  unset HOME_JWT home_jwt PGSTORE_DSN pgstore_dsn OBJECTSTORE_ENDPOINT objectstore_endpoint \
    GITSTORE_GIT_URL gitstore_git_url
  exec "$demo_bin" --config "$demo_dir/config.yaml" --local-model
) > "$demo_dir/proxy.log" 2>&1 &
demo_proxy=$!
demo_stop() { kill "$demo_mock" "$demo_proxy" 2>/dev/null; rm -rf "$demo_dir"; }
demo_ready=
for _ in $(seq 50); do
  if curl -fs -o /dev/null -H "Authorization: Bearer demo-client-key" http://127.0.0.1:8317/v1/models &&
    curl -s -o /dev/null http://127.0.0.1:8399/; then
    demo_ready=1
    break
  fi
  sleep 0.1
done
if [ -z "$demo_ready" ]; then
  echo "docs/demo/start.sh: the proxy or the mock provider did not answer; see $demo_dir/proxy.log" >&2
  kill "$demo_mock" "$demo_proxy" 2>/dev/null
  return 1 2>/dev/null || exit 1
fi
export CLIPROXY_CLIENT_KEY=demo-client-key UPSTREAM_LOG="$demo_dir/upstream.log"
