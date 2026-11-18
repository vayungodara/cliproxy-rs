#!/bin/sh
# Starts the mock provider and cliproxy-rs for the README demo, in a temporary
# directory, and waits until both answer. Source it: `. docs/demo/start.sh`.
# It needs `cliproxy` on PATH (or CLIPROXY_BIN) and ports 8317 and 8399 free.
# `demo_stop` stops both and removes the directory.
#
# The proxy starts inside the temporary directory, so it loads no .env file from
# yours, and without the variables that make it take its config from Home or a
# remote store instead of docs/demo/config.yaml.
#
# It returns non-zero, with nothing left running, when a file or the binary is
# missing, a port is taken, or the proxy and this demo's mock do not both answer.
# The first three are checked before anything starts.
demo_fail() {
  echo "docs/demo/start.sh: $1" >&2
  return 1
}
demo_stop() {
  kill "$demo_mock" "$demo_proxy" 2>/dev/null
  if [ -n "$demo_dir" ]; then rm -rf "$demo_dir"; fi
}
# curl exits with 7 when nothing accepts the connection.
demo_port_free() {
  curl -s -m 2 -o /dev/null "http://127.0.0.1:$1/"
  [ $? -eq 7 ]
}
demo_alive() { kill -0 "$demo_mock" 2>/dev/null && kill -0 "$demo_proxy" 2>/dev/null; }
demo_answers() {
  curl -fs -m 2 -o /dev/null -H "Authorization: Bearer demo-client-key" http://127.0.0.1:8317/v1/models &&
    [ "$(curl -fs -m 2 http://127.0.0.1:8399/demo-mock)" = "cliproxy-rs demo mock" ]
}
demo_start() {
  demo_src=$(cd "$(dirname "${DEMO_SRC:-docs/demo/start.sh}")" 2>/dev/null && pwd)
  if [ ! -f "$demo_src/config.yaml" ] || [ ! -f "$demo_src/mock_upstream.py" ]; then
    demo_fail "run it from the repository root, or set DEMO_SRC to this file's path"
    return
  fi
  demo_bin=$(command -v "${CLIPROXY_BIN:-cliproxy}")
  if [ -z "$demo_bin" ]; then
    demo_fail "no cliproxy on PATH; set CLIPROXY_BIN"
    return
  fi
  case "$demo_bin" in /*) ;; *) demo_bin="$PWD/$demo_bin" ;; esac
  for demo_port in 8317 8399; do
    if ! demo_port_free "$demo_port"; then
      demo_fail "port $demo_port is in use; stop what listens there, or an earlier demo with demo_stop"
      return
    fi
  done
  demo_dir=$(mktemp -d)
  if [ ! -d "$demo_dir" ]; then
    demo_dir=''
    demo_fail "could not make a temporary directory"
    return
  fi
  if ! mkdir "$demo_dir/auth" ||
    ! sed "s|__AUTH_DIR__|$demo_dir/auth|" "$demo_src/config.yaml" > "$demo_dir/config.yaml" ||
    [ ! -s "$demo_dir/config.yaml" ]; then
    rm -rf "$demo_dir"
    demo_dir=''
    demo_fail "could not write the demo config"
    return
  fi
  python3 "$demo_src/mock_upstream.py" 8399 > "$demo_dir/upstream.log" 2>&1 &
  demo_mock=$!
  (
    cd "$demo_dir" || exit 1
    unset HOME_JWT home_jwt PGSTORE_DSN pgstore_dsn OBJECTSTORE_ENDPOINT objectstore_endpoint \
      GITSTORE_GIT_URL gitstore_git_url
    exec "$demo_bin" --config "$demo_dir/config.yaml" --local-model
  ) > "$demo_dir/proxy.log" 2>&1 &
  demo_proxy=$!
  demo_ready=''
  for _ in $(seq 50); do
    # A process that lost its port has exited by now or will not answer below.
    demo_alive || break
    if demo_answers; then
      demo_ready=1
      break
    fi
    sleep 0.1
  done
  if [ -z "$demo_ready" ]; then
    kill "$demo_mock" "$demo_proxy" 2>/dev/null
    demo_fail "the proxy or the mock provider did not start; see proxy.log and upstream.log in $demo_dir"
    return
  fi
  export CLIPROXY_CLIENT_KEY=demo-client-key UPSTREAM_LOG="$demo_dir/upstream.log"
}
demo_start || return 1 2>/dev/null || exit 1
