# Getting started

This guide takes you from nothing to a coding tool that runs on your own accounts through cliproxy-rs. It takes about ten minutes. If you would rather have a coding agent do it for you, give it [AI-SETUP.md](AI-SETUP.md).

## 1. Install

On macOS or Linux:

```sh
curl -fsSL https://raw.githubusercontent.com/vayungodara/cliproxy-rs/master/install.sh | sh
```

The script downloads the release for your machine, checks it against the release's `SHA256SUMS` and puts `cliproxy` in `~/.local/bin`. Release archives for Windows, the Docker image and building from source are covered in [INSTALL.md](INSTALL.md).

## 2. Write a config

cliproxy-rs reads one file, `config.yaml`. Make a folder for it and two random keys:

```sh
mkdir -p ~/.cliproxy-rs && cd ~/.cliproxy-rs
openssl rand -hex 24   # copy this: your client key
openssl rand -hex 24   # copy this: your management key
```

Then create `~/.cliproxy-rs/config.yaml`, putting the two keys in place:

```yaml
server:
  host: "127.0.0.1"          # only this computer can connect
  port: 8317
access:
  api-keys:
    - "PASTE-CLIENT-KEY"     # your tools send this key to the proxy
management:
  secret-key: "PASTE-MANAGEMENT-KEY"  # the dashboard password
oauth:
  auth-dir: "~/.cliproxy-rs/auth"     # where account sign-ins are saved
```

Keep a copy of the management key somewhere safe. On first start the server replaces it in `config.yaml` with a bcrypt hash, so the file no longer shows it. [CONFIGURATION.md](CONFIGURATION.md) explains the other settings.

## 3. Start it

```sh
cliproxy --config ~/.cliproxy-rs/config.yaml
```

Leave it running and check it from another terminal:

```sh
curl http://127.0.0.1:8317/healthz
```

It answers `{"status":"ok"}`. To keep it running after you log out, see [running as a service](INSTALL.md#running-as-a-service).

## 4. Open the dashboard

Go to <http://127.0.0.1:8317/management.html> and sign in with the management key. A short "Get started" list stays on the Overview until you have connected an account and created a client key.

## 5. Connect an account

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="img/connect-dark.png">
  <img src="img/connect-light.png" alt="The Connect account page: sign in to Claude, ChatGPT, Kimi, Meta, xAI or Devin, or add a provider API key" width="880">
</picture>

In the dashboard, choose Connect account and pick a provider. Claude and ChatGPT (Codex) open the provider's own sign-in page; Kimi, Meta, xAI and Devin show a device code to enter on their site. The proxy stores the sign-in token in `auth-dir`. It never sees your password.

You can also sign in from a terminal, which helps on a server without a browser:

```sh
cliproxy --config ~/.cliproxy-rs/config.yaml --claude-login --no-browser
```

The other sign-in flags are `--codex-login`, `--codex-device-login`, `--kimi-login`, `--meta-login`, `--xai-login` and `--devin-login`. API keys for Claude, Codex, Gemini, Vertex AI, xAI and OpenAI-compatible services go on the dashboard's Provider keys page.

Using a subscription outside its official app can break the provider's terms, and providers have suspended accounts for it. Whether to do that is your call and your risk.

## 6. Point a tool at it

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="img/use-dark.png">
  <img src="img/use-light.png" alt="The Use with tools page: the proxy address, the client key test and a ready-to-copy Claude Code setup" width="880">
</picture>

The dashboard's Use with tools page shows this server's address and your client key with copy buttons, and tests the key. For Claude Code, for example:

```sh
export ANTHROPIC_BASE_URL=http://127.0.0.1:8317
export ANTHROPIC_AUTH_TOKEN=PASTE-CLIENT-KEY
claude
```

[CLIENTS.md](CLIENTS.md) has the setup for Codex CLI, Gemini CLI, Amp, OpenCode, Cursor, Cline, Zed, Aider, the SDKs and more.

## Running it safely

- Always set `access.api-keys`. With an empty list, anyone who can reach the proxy can use your accounts. The server logs a warning at start when the list is empty.
- Keep `server.host` on `127.0.0.1` unless other machines need it. An empty host listens on every network interface. To use the proxy from your other machines, a private network such as Tailscale is the simplest safe option; see [MULTI-ACCOUNT.md](MULTI-ACCOUNT.md#reach-the-proxy-from-other-machines).
- The management API is off until `management.secret-key` (or the `MANAGEMENT_PASSWORD` environment variable) is set. With `management.allow-remote: false`, the default, only requests from this computer are accepted. Five wrong keys from one address block that address for 30 minutes.
- Behind a tunnel or reverse proxy on the same machine (cloudflared, `tailscale serve`, Caddy, nginx), every request arrives from `127.0.0.1`, so the server treats every internet client as local. Then `allow-remote: false` no longer keeps them out, a local-only `--password` is accepted from the internet, and five wrong keys from anyone block the tunnel for everyone. Set `server.trusted-proxies` to the proxy's address, for example `[127.0.0.1, "::1"]` for a local cloudflared, and restart. The server then takes the client address from `X-Forwarded-For` from that proxy only. CLIProxyAPI behaves the same way.
- Do not send keys over plain HTTP across a network. Use a tunnel or reverse proxy with HTTPS, or serve HTTPS directly with `server.tls`.
- The dashboard is built into the binary, and cliproxy-rs never downloads code to run. (CLIProxyAPI downloads its dashboard from GitHub every three hours by default.) cliproxy-rs does fetch model lists from the CLIProxyAPI project's mirrors at start and every three hours, as CLIProxyAPI does; they are JSON data, checked before use, and `--local-model` turns the fetch off.
- The files in `auth-dir` hold sign-in tokens. Anyone who can read them can use your accounts. The server writes them with mode 0600; treat backups the same way.

## Next

- [MULTI-ACCOUNT.md](MULTI-ACCOUNT.md): several Claude or Codex accounts, routing, quotas and remote access.
- [CONFIGURATION.md](CONFIGURATION.md): the settings you are most likely to change.
- [MIGRATING-FROM-GO.md](MIGRATING-FROM-GO.md): moving over from CLIProxyAPI.
