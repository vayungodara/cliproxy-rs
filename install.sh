#!/bin/sh
# Installs cliproxy-rs on macOS or Linux, sets it up and starts it.
#
#   curl -fsSL https://raw.githubusercontent.com/vayungodara/cliproxy-rs/master/install.sh | sh
#
# The first run downloads the release for this machine, checks it against the release's
# SHA256SUMS and installs ~/.local/bin/cliproxy. It writes ~/.cliproxy-rs/config.yaml with
# a free port (8317, or the next free one) and new keys, which it saves in
# ~/.cliproxy-rs/keys.env (mode 600) and never prints. Then it starts the server in the
# background and checks that it answers. Running it again upgrades the binary and
# restarts the server; an existing config.yaml or keys.env is never changed.
#
# Options (when piping, pass them as: sh -s -- --service):
#   --service      also start cliproxy-rs at login, with a systemd user unit or a launchd agent
#   --binary-only  only install or upgrade the binary
#   --check        report installed and available versions without changing anything
#
# Environment:
#   CLIPROXY_VERSION      release tag to install, such as v0.1.0 (default: the latest)
#   CLIPROXY_INSTALL_DIR  where the binary goes (default: $HOME/.local/bin)
#   CLIPROXY_HOME         config, keys, credentials and log (default: $HOME/.cliproxy-rs)
#   CLIPROXY_NO_OPEN=1    never open a browser
#   CLIPROXY_RELEASES     release page base URL, for mirrors and tests
#                         (default: https://github.com/vayungodara/cliproxy-rs/releases)
set -eu

fail() {
  printf 'install.sh: %s\n' "$1" >&2
  exit 1
}

# Everything runs from main, so a download cut short cannot run half a script.
main() {
  releases="${CLIPROXY_RELEASES:-https://github.com/vayungodara/cliproxy-rs/releases}"
  dir="${CLIPROXY_INSTALL_DIR:-$HOME/.local/bin}"
  home="${CLIPROXY_HOME:-$HOME/.cliproxy-rs}"
  bin="$dir/cliproxy"
  config="$home/config.yaml" keys="$home/keys.env" log="$home/cliproxy.log" pidfile="$home/cliproxy.pid"
  label=io.github.vayungodara.cliproxy-rs
  os=$(uname -s)
  if [ "$os" = Darwin ]; then
    unit="$HOME/Library/LaunchAgents/$label.plist"
  else
    unit="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/cliproxy.service"
  fi

  setup=1 service='' check='' requested_service=''
  for arg in "$@"; do
    case "$arg" in
      --service) service=1 requested_service=1 ;;
      --binary-only) setup= ;;
      --check) check=1 ;;
      *) fail "unknown option $arg; the options are --service, --binary-only and --check" ;;
    esac
  done
  [ -n "$setup" ] || [ -z "$service" ] || fail "--service and --binary-only do not go together"
  # A service installed by an earlier run keeps being used.
  [ ! -f "$unit" ] || service=1

  case "$os-$(uname -m)" in
    Linux-x86_64 | Linux-amd64) target=x86_64-unknown-linux-gnu ;;
    Linux-aarch64 | Linux-arm64) target=aarch64-unknown-linux-gnu ;;
    Darwin-arm64) target=aarch64-apple-darwin ;;
    Darwin-x86_64) target=x86_64-apple-darwin ;;
    *) fail "there is no release binary for $os $(uname -m); build from source (docs/INSTALL.md)" ;;
  esac

  tag="${CLIPROXY_VERSION:-}"
  if [ -z "$tag" ]; then
    # /releases/latest redirects to /releases/tag/<tag> once a release exists.
    latest=$(curl -fsSLI --retry 3 -o /dev/null -w '%{url_effective}' "$releases/latest") || fail "cannot reach $releases"
    tag=${latest##*/tag/}
    [ "$tag" != "$latest" ] || fail "no release is published yet; build from source (docs/INSTALL.md)"
  fi
  name="cliproxy-${tag#v}-$target"

  installed=$([ ! -x "$bin" ] || "$bin" --version 2>/dev/null | tail -n 1)
  current=
  [ "$installed" != "cliproxy ${tag#v}" ] || current=1
  if [ -n "$current" ]; then
    printf 'cliproxy %s is already current.\n' "${tag#v}"
  else
    printf 'Installed: %s; available: cliproxy %s\n' "${installed:-not installed}" "${tag#v}"
  fi
  [ -z "$check" ] || return 0
  # A current file is not enough after --binary-only or an interrupted upgrade.
  if [ -n "$current" ]; then
    [ -n "$setup" ] || return 0
    if [ -z "$requested_service" ] && [ -f "$config" ]; then
      # -nt is supported by both Linux /bin/sh and macOS /bin/sh.
      # shellcheck disable=SC3013
      if [ -z "$service" ]; then
        if managed_pid && pid_running "$pid" && [ ! "$bin" -nt "$pidfile" ]; then return 0; fi
      elif [ ! "$bin" -nt "$unit" ]; then
        if [ "$os" = Darwin ]; then
          if launchctl print "gui/$(id -u)/$label" 2>/dev/null | grep -q 'state = running'; then return 0; fi
        elif systemctl --user is-active --quiet cliproxy.service; then
          return 0
        fi
      fi
    fi
  fi
  if [ -n "$setup" ] && [ -n "$service" ] && [ "$os" != Darwin ]; then
    systemctl --user show-environment >/dev/null 2>&1 ||
      fail "systemd user services are not available here; run this without --service"
  fi
  backup=
  [ ! -f "$dir/cliproxy.prev" ] || backup=1
  staged=
  if [ -z "$current" ]; then
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    curl -fsSL --retry 3 -o "$tmp/$name.tar.gz" "$releases/download/$tag/$name.tar.gz" || fail "could not download $name.tar.gz from release $tag"
    curl -fsSL --retry 3 -o "$tmp/SHA256SUMS" "$releases/download/$tag/SHA256SUMS" || fail "could not download SHA256SUMS from release $tag"
    expected=$(awk -v f="$name.tar.gz" '$2 == f || $2 == "*" f { print $1 }' "$tmp/SHA256SUMS")
    [ -n "$expected" ] || fail "$name.tar.gz is not listed in SHA256SUMS"
    if command -v sha256sum >/dev/null 2>&1; then
      actual=$(sha256sum "$tmp/$name.tar.gz" | awk '{ print $1 }')
    else
      actual=$(shasum -a 256 "$tmp/$name.tar.gz" | awk '{ print $1 }')
    fi
    [ "$actual" = "$expected" ] || fail "checksum mismatch for $name.tar.gz; nothing was installed"

    tar -xzf "$tmp/$name.tar.gz" -C "$tmp"
    mkdir -p "$dir"
    # Renamed into place, so a running server keeps its old binary until it restarts.
    install -m 0755 "$tmp/$name/cliproxy" "$dir/.cliproxy.new"
    staged=1
  fi
  case ":$PATH:" in
    *":$dir:"*) ;;
    *) printf 'Add %s to your PATH to run cliproxy by name.\n' "$dir" ;;
  esac
  if [ -z "$setup" ]; then
    swap_binary || fail "could not replace $bin"
    return 0
  fi

  umask 077
  mkdir -p "$home/auth"
  if [ ! -f "$config" ]; then
    [ ! -f "$keys" ] || fail "$keys exists but $config does not; restore config.yaml, or move keys.env away to make new keys"
    port=8317
    until port_free "$port"; do
      port=$((port + 1))
      [ "$port" -le 8336 ] || fail "ports 8317 to 8336 are all in use; free one and run this again"
    done
    client="sk-$(random_hex)" management=$(random_hex)
    cat >"$keys" <<EOF
# cliproxy-rs keys. Sign in to the dashboard with CLIPROXY_MANAGEMENT_KEY;
# your tools send CLIPROXY_CLIENT_KEY. Keep this file private.
CLIPROXY_PORT=$port
CLIPROXY_CLIENT_KEY=$client
CLIPROXY_MANAGEMENT_KEY=$management
EOF
    cat >"$config" <<EOF
# Written by install.sh. The plain keys are in keys.env next to this file.
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
  auth-dir: "$home/auth"
routing:
  session-affinity: true
EOF
    printf 'Wrote %s, with new keys in %s\n' "$config" "$keys"
  fi
  read_probe

  start_server() {
    stop_pidfile || return 1
    if [ -z "$service" ]; then
      (cd "$home" && exec nohup "$bin" --config "$config" </dev/null >>"$log" 2>&1) &
      echo $! >"$pidfile" || return 1
      how="kill \$(cat '$pidfile')"
    elif [ "$os" = Darwin ]; then
      mkdir -p "$(dirname "$unit")" || return 1
      cat >"$unit" <<EOF || return 1
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$label</string>
  <key>ProgramArguments</key><array><string>$bin</string><string>--config</string><string>$config</string></array>
  <key>WorkingDirectory</key><string>$home</string>
  <key>StandardOutPath</key><string>$log</string>
  <key>StandardErrorPath</key><string>$log</string>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
</dict>
</plist>
EOF
      domain="gui/$(id -u)"
      if launchctl print "$domain/$label" >/dev/null 2>&1; then
        launchctl kickstart -k "$domain/$label" || return 1
      else
        launchctl bootstrap "$domain" "$unit" || return 1
      fi
      how="launchctl bootout $domain/$label (and delete $unit to stop starting it at login)"
    else
      mkdir -p "$(dirname "$unit")" || return 1
      cat >"$unit" <<EOF || return 1
[Unit]
Description=cliproxy-rs

[Service]
ExecStart="$bin" --config "$config"
WorkingDirectory=$home
StandardOutput=append:$log
StandardError=inherit
Restart=on-failure

[Install]
WantedBy=default.target
EOF
      systemctl --user daemon-reload || return 1
      systemctl --user enable --quiet cliproxy.service || return 1
      systemctl --user restart cliproxy.service || return 1
      how="systemctl --user disable --now cliproxy"
    fi
    return 0
  }
  swap_binary || upgrade_failed "could not replace $bin"
  start_server || upgrade_failed "could not stop or start cliproxy-rs; see $log"
  wait_healthy || upgrade_failed "cliproxy-rs did not start and answer at $base; see $log"

  url="$base/management.html"
  printf '\ncliproxy-rs is running at %s\n' "$base"
  printf '  Dashboard  %s\n' "$url"
  [ ! -f "$keys" ] || printf '  Keys       %s (show them with: cat %s)\n' "$keys" "$keys"
  printf '  Config     %s\n  Log        %s\n  Stop       %s\n' "$config" "$log" "$how"
  [ -n "$service" ] || printf 'It runs until this computer restarts. Run this again with --service to start it at login.\n'

  opened=
  if [ -t 1 ] && [ -z "${CLIPROXY_NO_OPEN:-}" ]; then
    if [ "$os" = Darwin ]; then
      open "$url" && opened=1
    elif [ -n "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ] && command -v xdg-open >/dev/null 2>&1; then
      xdg-open "$url" >/dev/null 2>&1 && opened=1
    fi
  fi
  if [ -z "$opened" ] && { [ -n "${SSH_CONNECTION:-}" ] || [ "$os${DISPLAY:-}${WAYLAND_DISPLAY:-}" = Linux ]; }; then
    printf '\nNo browser on this machine? Keep the server on 127.0.0.1 and reach it from your own computer\n'
    printf 'through an SSH tunnel, then open the dashboard address there:\n'
    printf '  ssh -L %s:127.0.0.1:%s %s@%s\n' "$port" "$port" "$(id -un)" "$(hostname)"
    printf 'Tailscale works too; see docs/MULTI-ACCOUNT.md.\n'
  fi
  if [ -f "$keys" ]; then
    printf '\nNext: open the dashboard, sign in with CLIPROXY_MANAGEMENT_KEY from keys.env, and choose Connect account.\n'
  else
    printf '\nNext: open the dashboard, sign in with your management key, and choose Connect account.\n'
  fi
}

# Restore the same inode the old server used, then restart using its unchanged config.
upgrade_failed() {
  if [ -n "$backup" ]; then
    if mv -f "$dir/cliproxy.prev" "$bin" && start_server && wait_healthy; then
      printf 'Upgrade failed; restored the previous binary and verified it is healthy.\n' >&2
    else
      printf 'Upgrade failed; rollback failed too. The server may not be running; see %s.\n' "$log" >&2
    fi
  fi
  fail "$1"
}

swap_binary() {
  [ -n "$staged" ] || return 0
  if [ -f "$bin" ]; then
    rm -f "$dir/cliproxy.prev" || return 1
    ln "$bin" "$dir/cliproxy.prev" || return 1
    backup=1
  fi
  mv -f "$dir/.cliproxy.new" "$bin" || return 1
  [ "$os" != Darwin ] || xattr -d com.apple.quarantine "$bin" 2>/dev/null || true
  printf 'Installed %s at %s\n' "$("$bin" --version 2>/dev/null | tail -n 1)" "$bin"
}

# ponytail: block-style YAML scalar keys, like the existing port reader; use a
# binary config-inspection command if flow-style mappings need installer support.
read_probe() {
  settings=$(awk '
    /^[ ]*[A-Za-z0-9_-]+[ ]*:/ {
      indent = match($0, /[^ ]/) - 1
      while (depth && levels[depth] >= indent) depth--
      line = substr($0, indent + 1); key = line; sub(/[ ]*:.*$/, "", key)
      value = line; sub(/^[^:]*:[ ]*/, "", value); sub(/(^|[ ]+)#.*$/, "", value)
      gsub(/^[ ]+|[ ]+$/, "", value); gsub(/^[\047\042]|[\047\042]$/, "", value)
      path = ""; for (j = 1; j <= depth; j++) path = path names[j] "."
      values[path key] = value
      if (value == "") { depth++; levels[depth] = indent; names[depth] = key }
    }
    END {
      print ("server.port" in values ? values["server.port"] : values["port"])
      print ("server.host" in values ? values["server.host"] : values["host"])
      print ("server.tls.enable" in values ? values["server.tls.enable"] : values["tls.enable"])
    }' "$config")
  port=$(printf '%s\n' "$settings" | sed -n '1p')
  port=${port:-8317}
  host=$(printf '%s\n' "$settings" | sed -n '2p')
  case "$host" in '' | 0.0.0.0) host=127.0.0.1 ;; :: | '[::]') host=::1 ;; esac
  scheme=http
  case "$(printf '%s\n' "$settings" | sed -n '3p' | tr '[:upper:]' '[:lower:]')" in true | y | yes | on) scheme=https ;; esac
  case "$host" in *:*)
    host="[${host#[}]"
    host="${host%]}]"
    ;;
  esac
  base="$scheme://$host:$port"
}

wait_healthy() {
  sleep 1
  tries=0
  while [ "$tries" -lt 20 ]; do
    if [ -z "$service" ]; then managed_pid && pid_running "$pid" || return 1; fi
    if curl --noproxy '*' -kfs -m 2 -o /dev/null "$base/healthz"; then
      sleep 1
      if [ -z "$service" ]; then
        managed_pid && pid_running "$pid" || return 1
      elif [ "$os" = Darwin ]; then
        launchctl print "gui/$(id -u)/$label" 2>/dev/null | grep -q 'state = running' || return 1
      else systemctl --user is-active --quiet cliproxy.service || return 1; fi
      return 0
    fi
    tries=$((tries + 1))
    sleep 1
  done
  return 1
}

# True when nothing listens on 127.0.0.1:$1 (curl exits with 7 when the connection is refused).
port_free() {
  curl -s -m 2 -o /dev/null "http://127.0.0.1:$1/" && return 1
  [ $? -eq 7 ]
}

random_hex() {
  od -An -N24 -tx1 /dev/urandom | tr -d ' \n'
}

# Stops the server an earlier run started in the background, if it is still running.
stop_pidfile() {
  if managed_pid && pid_running "$pid"; then
    kill "$pid" || { ! pid_running "$pid" || return 1; }
    i=0
    while pid_running "$pid" && [ "$i" -lt 10 ]; do
      sleep 1
      i=$((i + 1))
    done
    if pid_running "$pid"; then
      printf 'Process %s did not stop after SIGTERM; sending SIGKILL.\n' "$pid" >&2
      kill -KILL "$pid" || { ! pid_running "$pid" || return 1; }
      i=0
      while pid_running "$pid" && [ "$i" -lt 10 ]; do
        sleep 1
        i=$((i + 1))
      done
      if pid_running "$pid"; then return 1; fi
    fi
  fi
  rm -f "$pidfile" || return 1
}

managed_pid() {
  pid=$(cat "$pidfile" 2>/dev/null) || return 1
  case "$pid" in '' | *[!0-9]*) return 1 ;; esac
  [ "$pid" -gt 0 ] || return 1
  if [ "$os" = Linux ]; then
    # procfs verifies ownership without procps; never signal an unknown stale PID.
    command=$(cat "/proc/$pid/comm" 2>/dev/null) || return 1
  elif [ "$os" = Darwin ]; then
    # Seatbelt refuses setuid ps; bash 3.2's substitution status is not reliable.
    if ! ps -p $$ -o pid= >/dev/null 2>&1; then
      # pgrep is not setuid. Never trust liveness alone for a stale pid file.
      pgrep -x 'cliproxy([.]prev)?' 2>/dev/null | grep -qx "$pid"
      return $?
    fi
    command=$(ps -p "$pid" -o ucomm= 2>/dev/null) || return 1
    command=$(printf '%s\n' "$command" | sed 's/^[ ]*//;s/[ ]*$//')
  else
    command=$(ps -p "$pid" -o comm= 2>/dev/null) || return 1
    command=$(printf '%s\n' "$command" | sed 's/^[ ]*//;s/[ ]*$//')
  fi
  case "$command" in cliproxy | cliproxy.prev) return 0 ;; *) return 1 ;; esac
}

pid_running() {
  kill -0 "$1" 2>/dev/null || return 1
  if [ "$os" = Linux ]; then
    state=$(cat "/proc/$1/stat" 2>/dev/null) || return 1
    state=${state##*) }
    case "$state" in '' | Z\ *) return 1 ;; esac
  else
    if [ "$os" = Darwin ] && ! ps -p $$ -o pid= >/dev/null 2>&1; then
      pgrep -x 'cliproxy([.]prev)?' 2>/dev/null | grep -qx "$1"
      return $?
    fi
    state=$(ps -p "$1" -o stat= 2>/dev/null) || return 1
    case "$state" in '' | *Z*) return 1 ;; esac
  fi
  return 0
}

main "$@"
