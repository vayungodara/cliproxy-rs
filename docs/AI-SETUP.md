# Setting up cliproxy-rs with a coding agent

These are exact steps for a coding agent (Claude Code, Codex, Amp, Cursor's agent or similar) that is installing cliproxy-rs on a user's computer. A person can follow them too. Run every command on the user's machine, in order, and stop to ask the user wherever a step says so.

## Rules for the agent

- Never sign in to a provider account for the user, and never type, paste or read the user's passwords. Signing in is the user's step (step 9).
- Never print, log or repeat the management key or the client key. They live in `~/.cliproxy-rs/keys.env`; refer to that file instead.
- Do not stop, change or reconfigure an existing CLIProxyAPI (Go) or cliproxy-rs install unless the user asks you to migrate it (see the end of this page).
- Do not open the proxy to the internet. Keep `server.host` at `127.0.0.1`.
- Ask before installing a background service.

## 1. Detect the system

```sh
uname -s; uname -m
```

| Output | Use |
|---|---|
| `Linux` with `x86_64` or `aarch64` | the release binary (step 3) |
| `Darwin` with `arm64` or `x86_64` | the release binary (step 3) |
| Windows | WSL: run these steps inside WSL. Otherwise download `cliproxy-<version>-x86_64-pc-windows-msvc.zip` from the [releases page](https://github.com/vayungodara/cliproxy-rs/releases) and adapt the paths. |
| anything else | build from source (step 3) |

## 2. Look for an existing install

```sh
command -v cliproxy cli-proxy-api CLIProxyAPI 2>/dev/null
ls -d ~/.cliproxy-rs ~/.cli-proxy-api 2>/dev/null
curl -s -m 2 http://127.0.0.1:8317/healthz; echo " exit=$?"
```

- If `~/.cliproxy-rs/config.yaml` exists, cliproxy-rs is already set up. Ask the user before changing anything, and skip to step 7 to check it.
- If `cli-proxy-api` or `CLIProxyAPI` exists, or `~/.cli-proxy-api` exists, or something answers on port 8317, the user probably runs CLIProxyAPI. Tell the user and ask: migrate it (see [Migrating from CLIProxyAPI](#migrating-from-cliproxyapi)) or install cliproxy-rs next to it. Next to it means a different port and a separate auth folder, which the steps below use; never point both servers at the same auth folder.

## 3. Install the binary

Try the release first:

```sh
curl -fsSL https://raw.githubusercontent.com/vayungodara/cliproxy-rs/master/install.sh | sh
```

It installs `~/.local/bin/cliproxy` after checking the download against the release's checksums. If it says no release is published yet, or the system has no release binary, build from source. That needs `git`, a Rust toolchain (`cargo`), `cmake`, `clang` and `perl`; ask the user before installing any of them.

```sh
git clone https://github.com/vayungodara/cliproxy-rs.git ~/cliproxy-rs-src
cd ~/cliproxy-rs-src && cargo build --release -p cliproxy
mkdir -p ~/.local/bin && install -m 0755 target/release/cliproxy ~/.local/bin/cliproxy
```

Then confirm:

```sh
~/.local/bin/cliproxy --version | tail -n 1
```

It prints `cliproxy` and a version number.

## 4. Pick a free port

```sh
for p in 8317 8318 8319 8320 8321 8322 8323 8324 8325 8326; do
  curl -s -m 1 -o /dev/null "http://127.0.0.1:$p/"; [ $? -eq 7 ] && { echo "$p"; break; }
done
```

curl's exit code 7 means nothing is listening, so the first port printed is free. Use it as `PORT` below. If none is printed, ask the user which port to use.

## 5. Create the keys and the config in one step

Run this as one command, with `PORT` set to the port from step 4. It runs in a subshell, writes the keys to files only the user can read, and prints nothing secret.

```sh
PORT=8317
(
umask 077
mkdir -p ~/.cliproxy-rs/auth || exit 1
CLIENT="sk-$(od -An -N24 -tx1 /dev/urandom | tr -d ' \n')"
MANAGEMENT="$(od -An -N24 -tx1 /dev/urandom | tr -d ' \n')"
printf 'CLIPROXY_PORT=%s\nCLIPROXY_CLIENT_KEY=%s\nCLIPROXY_MANAGEMENT_KEY=%s\n' "$PORT" "$CLIENT" "$MANAGEMENT" > ~/.cliproxy-rs/keys.env
cat > ~/.cliproxy-rs/config.yaml <<EOF
config-version: 8
server:
  host: "127.0.0.1"
  port: $PORT
access:
  api-keys:
    - "$CLIENT"
management:
  secret-key: "$MANAGEMENT"
oauth:
  auth-dir: "$HOME/.cliproxy-rs/auth"
routing:
  session-affinity: true
EOF
chmod 600 ~/.cliproxy-rs/keys.env ~/.cliproxy-rs/config.yaml
echo "wrote ~/.cliproxy-rs/config.yaml and ~/.cliproxy-rs/keys.env"
)
```

`routing.session-affinity: true` keeps each conversation on one account, which keeps the provider's prompt cache warm. On first start the server replaces the management key in `config.yaml` with a bcrypt hash; `keys.env` keeps the plain key for the dashboard.

## 6. Start it

```sh
nohup ~/.local/bin/cliproxy --config ~/.cliproxy-rs/config.yaml > ~/.cliproxy-rs/cliproxy.log 2>&1 &
```

This runs it until the computer restarts. To keep it running after that, ask the user first, then follow [running as a service](INSTALL.md#running-as-a-service) (on Linux) or use a `launchd` agent (on macOS).

## 7. Verify

```sh
. ~/.cliproxy-rs/keys.env
for i in $(seq 1 30); do curl -fsS "http://127.0.0.1:$CLIPROXY_PORT/healthz" && break; sleep 1; done; echo
curl -s -o /dev/null -w '%{http_code}\n' -H "Authorization: Bearer $CLIPROXY_CLIENT_KEY" "http://127.0.0.1:$CLIPROXY_PORT/v1/models"
```

Expect `{"status":"ok"}` and then `200`. If the health check never answers, read the last lines of `~/.cliproxy-rs/cliproxy.log` (they contain no keys) and fix what it reports. A `401` means the key in `keys.env` and the one in `config.yaml` differ.

## 8. Open the dashboard

```sh
. ~/.cliproxy-rs/keys.env
URL="http://127.0.0.1:$CLIPROXY_PORT/management.html"
(open "$URL" || xdg-open "$URL") >/dev/null 2>&1 || echo "Open $URL in a browser"
```

Tell the user: the dashboard is at that address, and the management key to sign in with is the `CLIPROXY_MANAGEMENT_KEY` line in `~/.cliproxy-rs/keys.env`, which they can show with `cat ~/.cliproxy-rs/keys.env` in their own terminal.

## 9. Hand over the sign-in to the user

Tell the user to connect their accounts themselves, in the dashboard (Connect account) or in their own terminal:

```sh
cliproxy --config ~/.cliproxy-rs/config.yaml --claude-login
```

(`--codex-login`, `--codex-device-login`, `--kimi-login`, `--meta-login`, `--xai-login` and `--devin-login` work the same way.) Wait until the user says they are done. Also tell them once: using a subscription outside its official app can break the provider's terms, and providers have suspended accounts for it; that is their decision.

## 10. Point the user's tools at it

The dashboard's Use with tools page has copy-paste settings with the address and key filled in, and [CLIENTS.md](CLIENTS.md) covers each tool. Ask the user which tools they use before changing their settings.

For Claude Code, this adds the proxy to the `env` block of `~/.claude/settings.json`, keeps a backup, and reads the key from `keys.env` so it never appears in the conversation:

```sh
set -a; . ~/.cliproxy-rs/keys.env; set +a
python3 - <<'PY'
import json, os, shutil
path = os.path.expanduser("~/.claude/settings.json")
os.makedirs(os.path.dirname(path), exist_ok=True)
data = {}
if os.path.exists(path):
    shutil.copy(path, path + ".bak")
    data = json.load(open(path))
data.setdefault("env", {}).update({
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:" + os.environ["CLIPROXY_PORT"],
    "ANTHROPIC_AUTH_TOKEN": os.environ["CLIPROXY_CLIENT_KEY"],
})
json.dump(data, open(path, "w"), indent=2)
print("updated", path)
PY
```

After the user has signed in, confirm with the `/v1/models` request from step 7: the list now includes their models.

## Tips for heavier use

- Several Claude or Codex accounts: see [MULTI-ACCOUNT.md](MULTI-ACCOUNT.md) for routing strategies and limits.
- Codex accounts: set `"websockets": true` in each Codex account's file in `~/.cliproxy-rs/auth` to use the faster WebSocket transport.
- Session affinity: keep `routing.session-affinity: true` from step 5.
- Other machines: use [Tailscale](https://tailscale.com/) instead of opening a port to the internet; see [Reach the proxy from other machines](MULTI-ACCOUNT.md#reach-the-proxy-from-other-machines). Never expose the proxy to the internet without client keys.

## Migrating from CLIProxyAPI

Only when the user asks for it:

1. Find the Go server's config file (often `config.yaml` next to its binary, or the path in its service definition) and its `auth-dir` (`~/.cli-proxy-api` by default).
2. Back them up: `cp -a <config.yaml> <config.yaml>.bak` and `cp -a <auth-dir> <auth-dir>.bak`.
3. Stop the Go server (its service, or the process). Both servers must never run on the same auth folder at the same time, because each refreshes the sign-in tokens and can invalidate the other's.
4. Start cliproxy-rs on the same file: `cliproxy --config <config.yaml>`. It reads the same settings and account files, and the user's existing management key keeps working.
5. Verify as in step 7 with the port and a client key from that config.

[MIGRATING-FROM-GO.md](MIGRATING-FROM-GO.md) lists what carries over and how to switch back.
