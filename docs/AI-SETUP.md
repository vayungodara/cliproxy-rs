# Setting up cliproxy-rs with a coding agent

These are exact steps for a coding agent (Claude Code, Codex, Amp, Cursor's agent or similar) that is installing cliproxy-rs on a user's computer. A person can follow them too. Run every command on the user's machine, in order, and stop to ask the user wherever a step says so.

## Rules for the agent

- Never sign in to a provider account for the user, and never type, paste or read the user's passwords. Signing in is the user's step (step 7).
- Never print, log or repeat the management key or the client key. They live in `~/.cliproxy-rs/keys.env`; refer to that file instead.
- Do not stop, change or reconfigure an existing CLIProxyAPI (Go) or cliproxy-rs install unless the user asks you to migrate it (see the end of this page).
- Do not open the proxy to the internet. Keep `server.host` at `127.0.0.1`.
- Ask before setting up start at login (step 5).

## 1. Detect the system

```sh
uname -s; uname -m
```

| Output | Use |
|---|---|
| `Linux` with `x86_64` or `aarch64` | `install.sh` (step 3) |
| `Darwin` with `arm64` or `x86_64` | `install.sh` (step 3) |
| Windows (no `uname`, or `MINGW`/`MSYS` in its output) | `install.ps1` in PowerShell (step 3), or WSL with `install.sh` |
| anything else | build from source (step 3) |

## 2. Look for an existing install

```sh
command -v cliproxy cli-proxy-api CLIProxyAPI 2>/dev/null
ls -d ~/.cliproxy-rs ~/.cli-proxy-api 2>/dev/null
curl -s -m 2 http://127.0.0.1:8317/healthz; echo " exit=$?"
```

- If `~/.cliproxy-rs/config.yaml` exists, cliproxy-rs is already set up. Ask the user before going on. Running the installer again only upgrades the binary and restarts the server; it never changes the config or the keys.
- If `cli-proxy-api` or `CLIProxyAPI` exists, or `~/.cli-proxy-api` exists, or something answers on port 8317, the user probably runs CLIProxyAPI. Tell the user and ask: migrate it (see [Migrating from CLIProxyAPI](#migrating-from-cliproxyapi)) or install cliproxy-rs next to it. The installer installs next to it without touching it: it takes the next free port and its own auth folder.

## 3. Install, configure and start

On macOS and Linux:

```sh
curl -fsSL https://raw.githubusercontent.com/vayungodara/cliproxy-rs/master/install.sh | sh
```

On Windows, in PowerShell:

```powershell
irm https://raw.githubusercontent.com/vayungodara/cliproxy-rs/master/install.ps1 | iex
```

The installer:

1. Downloads the release binary, checks it against the release's checksums and installs it as `~/.local/bin/cliproxy` (`%LOCALAPPDATA%\Programs\cliproxy-rs\cliproxy.exe` on Windows).
2. Writes `~/.cliproxy-rs/config.yaml` (`%USERPROFILE%\.cliproxy-rs` on Windows) with a free port, starting at 8317, and session affinity on. It creates a client key and a management key and saves them in `~/.cliproxy-rs/keys.env`, readable only by the user.
3. Starts the server in the background, logging to `~/.cliproxy-rs/cliproxy.log`, and waits for `/healthz`.

It prints the dashboard address and file locations, never the keys. If it fails, it says why in one line; read the last lines of `cliproxy.log` (they contain no keys) and fix what they report.

`keys.env` looks like this, with real values:

```sh
CLIPROXY_PORT=8317
CLIPROXY_CLIENT_KEY=sk-...
CLIPROXY_MANAGEMENT_KEY=...
```

If there is no release binary for the system, build from source. That needs `git`, a Rust toolchain (`cargo`), `cmake`, `clang` and `perl`; ask the user before installing any of them. Then follow [the first run without the install script](INSTALL.md#first-run-without-the-install-script), writing the two keys into `~/.cliproxy-rs/keys.env` in the format above with `chmod 600`.

```sh
git clone https://github.com/vayungodara/cliproxy-rs.git ~/cliproxy-rs-src
cd ~/cliproxy-rs-src && cargo build --release -p cliproxy
mkdir -p ~/.local/bin && install -m 0755 target/release/cliproxy ~/.local/bin/cliproxy
```

The installer starts the server with `nohup`, which keeps it running until the computer restarts. In a container or sandbox that has its own service or process manager, use that manager instead: stop the background server with `kill $(cat ~/.cliproxy-rs/cliproxy.pid)` and run `~/.local/bin/cliproxy --config ~/.cliproxy-rs/config.yaml` under the manager.

## 4. Verify

```sh
. ~/.cliproxy-rs/keys.env
curl -fsS "http://127.0.0.1:$CLIPROXY_PORT/healthz"; echo
curl -s -o /dev/null -w '%{http_code}\n' -H "Authorization: Bearer $CLIPROXY_CLIENT_KEY" "http://127.0.0.1:$CLIPROXY_PORT/v1/models"
```

On Windows, in PowerShell:

```powershell
$k = @{}; Get-Content ~\.cliproxy-rs\keys.env | Where-Object { $_ -match '^(\w+)=(.*)$' } | ForEach-Object { $k[$Matches[1]] = $Matches[2] }
Invoke-RestMethod "http://127.0.0.1:$($k.CLIPROXY_PORT)/healthz"
(Invoke-WebRequest "http://127.0.0.1:$($k.CLIPROXY_PORT)/v1/models" -Headers @{ Authorization = "Bearer $($k.CLIPROXY_CLIENT_KEY)" } -UseBasicParsing).StatusCode
```

Expect `{"status":"ok"}` (PowerShell shows `status: ok`) and then `200`. A `401` means the key in `keys.env` and the one in `config.yaml` differ.

## 5. Start at login (only if the user wants it)

Ask the user first. If they agree, run the installer again with `--service`:

```sh
curl -fsSL https://raw.githubusercontent.com/vayungodara/cliproxy-rs/master/install.sh | sh -s -- --service
```

On Windows: `& ([scriptblock]::Create((irm https://raw.githubusercontent.com/vayungodara/cliproxy-rs/master/install.ps1))) -Service`. This adds a systemd user unit on Linux, a launchd agent on macOS or a sign-in entry on Windows; [INSTALL.md](INSTALL.md#start-at-login) has the details and how to undo it.

## 6. Open the dashboard

If the user sits at this computer and it has a desktop:

```sh
. ~/.cliproxy-rs/keys.env
URL="http://127.0.0.1:$CLIPROXY_PORT/management.html"
(open "$URL" || xdg-open "$URL") >/dev/null 2>&1 || echo "Open $URL in a browser"
```

If no browser can open here (a server over SSH, a container, a remote sandbox), print the address and tell the user how to reach it from their own computer without exposing it to the internet:

- an SSH tunnel, run on their computer: `ssh -L 8317:127.0.0.1:8317 user@host` (use the port from `keys.env` on both sides), then open `http://127.0.0.1:8317/management.html` there;
- or [Tailscale](MULTI-ACCOUNT.md#reach-the-proxy-from-other-machines) on both machines.

Keep `server.host` on `127.0.0.1` for the tunnel. Never set it to `0.0.0.0` or `""` while `access.api-keys` is empty.

Tell the user: the management key to sign in with is the `CLIPROXY_MANAGEMENT_KEY` line in `~/.cliproxy-rs/keys.env`, which they can show with `cat ~/.cliproxy-rs/keys.env` in their own terminal.

## 7. Hand over the sign-in to the user

Tell the user to connect their accounts themselves, in the dashboard (Connect account) or in their own terminal:

```sh
cliproxy --config ~/.cliproxy-rs/config.yaml --claude-login
```

(`--codex-login`, `--codex-device-login`, `--kimi-login`, `--meta-login`, `--xai-login` and `--devin-login` work the same way; add `--no-browser` on a machine without a browser.) Wait until the user says they are done. Point them to [Accounts and provider terms](../README.md#accounts-and-provider-terms). This setup is for one person using their own accounts on their own machines. Use provider-sanctioned integrations or API keys where available.

## 8. Point the user's tools at it

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

After the user has signed in, confirm with the `/v1/models` request from step 4: the list now includes their models.

## Tips for heavier use

- Several Claude or Codex accounts: see [MULTI-ACCOUNT.md](MULTI-ACCOUNT.md) for routing strategies and limits.
- Codex accounts: set `"websockets": true` in each Codex account's file in `~/.cliproxy-rs/auth` to use the faster WebSocket transport.
- Session affinity: keep `routing.session-affinity: true`, which the installer sets.
- Other machines: use [Tailscale](https://tailscale.com/) instead of opening a port to the internet; see [Reach the proxy from other machines](MULTI-ACCOUNT.md#reach-the-proxy-from-other-machines). Never expose the proxy to the internet without client keys.

## Migrating from CLIProxyAPI

Only when the user asks for it:

1. Find the Go server's config file (often `config.yaml` next to its binary, or the path in its service definition) and its `auth-dir` (`~/.cli-proxy-api` by default).
2. Back them up: `cp -a <config.yaml> <config.yaml>.bak` and `cp -a <auth-dir> <auth-dir>.bak`.
3. Stop the Go server (its service, or the process). Both servers must never run on the same auth folder at the same time, because each refreshes the sign-in tokens and can invalidate the other's.
4. Start cliproxy-rs on the same file: `cliproxy --config <config.yaml>`. It reads the same settings and account files, and the user's existing management key keeps working.
5. Verify as in step 4 with the port and a client key from that config.

[MIGRATING-FROM-GO.md](MIGRATING-FROM-GO.md) lists what carries over and how to switch back.
