# Installing cliproxy-rs

cliproxy-rs is one executable, `cliproxy` (`cliproxy.exe` on Windows), with the dashboard built in. It needs a `config.yaml` and a directory for credential files. Nothing else is installed.

## With the install script

On macOS or Linux:

```sh
curl -fsSL https://raw.githubusercontent.com/vayungodara/cliproxy-rs/master/install.sh | sh
```

On Windows, in PowerShell:

```powershell
irm https://raw.githubusercontent.com/vayungodara/cliproxy-rs/master/install.ps1 | iex
```

Neither needs `sudo`, administrator rights, Rust or any other tool. The first run:

1. Picks the release archive for your system, downloads it with the release's `SHA256SUMS`, and stops without installing anything if the checksum does not match.
2. Installs the binary: `~/.local/bin/cliproxy` on macOS and Linux, `%LOCALAPPDATA%\Programs\cliproxy-rs\cliproxy.exe` on Windows.
3. If `~/.cliproxy-rs/config.yaml` does not exist (`%USERPROFILE%\.cliproxy-rs` on Windows), writes it: the server listens on `127.0.0.1`, on port 8317 or the next free port if something such as CLIProxyAPI already uses 8317, with a credential folder `auth` and session affinity on. It creates a client key and a management key and saves both in `keys.env` next to the config. Only you can read the folder, and the script never prints the keys.
4. Starts the server in the background, logging to `cliproxy.log`, and checks that `/healthz` answers.
5. Prints the dashboard address and opens it in your browser when there is one.

Sign in to the dashboard with the `CLIPROXY_MANAGEMENT_KEY` line from `keys.env` (`cat ~/.cliproxy-rs/keys.env`, or `Get-Content ~\.cliproxy-rs\keys.env` on Windows). Your tools use `CLIPROXY_CLIENT_KEY`.

Running the script again upgrades the binary to the latest release and restarts the server. It never changes an existing `config.yaml` or `keys.env`, so it is safe to rerun and it also works on top of a config you wrote yourself.

On a machine without a browser, such as a server you reach over SSH, keep the server on `127.0.0.1` and open an SSH tunnel from your own computer, for example `ssh -L 8317:127.0.0.1:8317 you@server`, then open `http://127.0.0.1:8317/management.html` there. [Tailscale](MULTI-ACCOUNT.md#reach-the-proxy-from-other-machines) works too. Never listen on `0.0.0.0` without client keys.

Read [install.sh](../install.sh) or [install.ps1](../install.ps1) before you run them if you prefer.

### Options

| install.sh | install.ps1 | Effect |
| --- | --- | --- |
| `--service` | `-Service` | Also start cliproxy-rs at login; see below. |
| `--binary-only` | `-BinaryOnly` | Install or upgrade the binary and stop there. |

Pass options to a piped script like this:

```sh
curl -fsSL https://raw.githubusercontent.com/vayungodara/cliproxy-rs/master/install.sh | sh -s -- --service
```

```powershell
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/vayungodara/cliproxy-rs/master/install.ps1))) -Service
```

Both scripts read these environment variables:

| Variable | Default | Meaning |
| --- | --- | --- |
| `CLIPROXY_VERSION` | the latest release | Release tag to install, such as `v0.1.0`. |
| `CLIPROXY_INSTALL_DIR` | `~/.local/bin`, or `%LOCALAPPDATA%\Programs\cliproxy-rs` | Where the binary goes. |
| `CLIPROXY_HOME` | `~/.cliproxy-rs`, or `%USERPROFILE%\.cliproxy-rs` | Config, keys, credentials and log. |
| `CLIPROXY_NO_OPEN` | unset | Set to `1` to never open a browser. |
| `CLIPROXY_RELEASES` | `https://github.com/vayungodara/cliproxy-rs/releases` | Release page base URL for a mirror or installer tests. It must serve `/latest`, `/download/<tag>/SHA256SUMS` and the platform archive under `/download/<tag>/`. |

Use only a mirror you trust. The installer checks the archive against the `SHA256SUMS` file from that same mirror.

### Start at login

Without `--service`, the server runs until the computer restarts (on Windows, until you sign out). With it:

- Linux: a systemd user unit, `~/.config/systemd/user/cliproxy.service`, enabled and started. It runs while you are logged in; for a server that should run without a login session, run `loginctl enable-linger` once. Stop it with `systemctl --user disable --now cliproxy`.
- macOS: a launchd agent, `~/Library/LaunchAgents/io.github.vayungodara.cliproxy-rs.plist`, which also restarts the server if it crashes. Stop it with `launchctl bootout gui/$(id -u)/io.github.vayungodara.cliproxy-rs` and delete the file.
- Windows: an entry named `cliproxy-rs` under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` that runs `.cliproxy-rs\start.ps1` when you sign in. A window may flash briefly at sign-in. Remove it with `Remove-ItemProperty HKCU:\Software\Microsoft\Windows\CurrentVersion\Run -Name cliproxy-rs`.

Once set up, later runs of the script restart the server through the same mechanism.

### Removing it

Stop the server (`kill $(cat ~/.cliproxy-rs/cliproxy.pid)`, `Stop-Process -Id (Get-Content ~\.cliproxy-rs\cliproxy.pid)` on Windows, or the service command above), then delete the binary. `~/.cliproxy-rs` holds your config, keys and signed-in accounts; delete it only if you no longer need them.

## From a release

Each [release](https://github.com/vayungodara/cliproxy-rs/releases) has an archive per platform and a `SHA256SUMS` file:

| Platform | Archive |
| --- | --- |
| Linux x86_64 | `cliproxy-<version>-x86_64-unknown-linux-gnu.tar.gz` |
| Linux arm64 | `cliproxy-<version>-aarch64-unknown-linux-gnu.tar.gz` |
| macOS Apple silicon | `cliproxy-<version>-aarch64-apple-darwin.tar.gz` |
| macOS Intel | `cliproxy-<version>-x86_64-apple-darwin.tar.gz` |
| Windows x86_64 | `cliproxy-<version>-x86_64-pc-windows-msvc.zip` |

The release also carries `management.html`, the dashboard as a single file for Go CLIProxyAPI servers (see [ui/PANEL.md](../ui/PANEL.md)). The Rust binary does not need it.

Check the download before you run it:

```sh
sha256sum --check --ignore-missing SHA256SUMS      # Linux
shasum -a 256 --check --ignore-missing SHA256SUMS  # macOS
```

On Windows, compare the output of `Get-FileHash cliproxy-<version>-x86_64-pc-windows-msvc.zip` with the line in `SHA256SUMS`.

Unpack and put the binary on your `PATH`:

```sh
tar -xzf cliproxy-<version>-x86_64-unknown-linux-gnu.tar.gz
sudo install -m 0755 cliproxy-<version>-x86_64-unknown-linux-gnu/cliproxy /usr/local/bin/cliproxy
cliproxy --version
```

The macOS binaries are not signed or notarized. If Gatekeeper blocks the first run, remove the quarantine flag with `xattr -d com.apple.quarantine /usr/local/bin/cliproxy`.

The Linux binaries link against glibc and libstdc++ and run on current distributions (Debian 12, Ubuntu 22.04 and later, Fedora, Arch). For Alpine or other musl systems, build from source or use the Docker image.

## From source

For contributors, and for systems without a release binary such as Alpine or other musl systems. You need a Rust toolchain (stable, 1.88 or newer) and the tools to build BoringSSL: `cmake`, `clang` and `perl`. On Debian or Ubuntu:

```sh
sudo apt-get install -y build-essential cmake clang perl git
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

On macOS, `xcode-select --install` and `brew install cmake` are enough. On Windows, install Visual Studio Build Tools with the C++ workload, CMake, LLVM (bindgen needs `libclang`) and NASM (BoringSSL's x86_64 assembly), for example `choco install cmake llvm nasm`, and put NASM on `PATH`. The Windows build is produced by the release workflow and has not been tested by hand.

Then build:

```sh
git clone https://github.com/vayungodara/cliproxy-rs.git
cd cliproxy-rs
cargo build --release -p cliproxy
./target/release/cliproxy --version
```

The WebRTC media relay is an optional feature and is not part of the release binaries or the Docker image. To include it, build with `cargo build --release -p cliproxy --features cpa-server/media-relay`.

The dashboard in `ui/dist` is committed, so Node is not needed for a normal build. To change the dashboard, see [ui/README.md](../ui/README.md); rebuild the UI before building the binary.

## Docker

The repository's `Dockerfile` builds the binary and copies it into a small Debian image that runs as an unprivileged user. The build compiles BoringSSL and needs about 2 GB of memory; on a small machine add `--build-arg BUILD_JOBS=1`.

```sh
docker build -t cliproxy-rs .
mkdir -p data/auth
$EDITOR data/config.yaml                  # see the notes below
sudo chown -R 10001:10001 data            # the container runs as uid and gid 10001
docker run -d --name cliproxy -p 127.0.0.1:8317:8317 -v "$PWD/data:/data" cliproxy-rs
```

Inside the container the config is `/data/config.yaml`. In that config:

- Set `server.host` to `""` or `0.0.0.0` so the server listens on the container's interface.
- Set `oauth.auth-dir` to `/data/auth` so credentials persist in the mounted volume.
- Set `management.allow-remote: true` if you want the dashboard. Docker forwards the published port from its bridge network, so even requests from your own machine reach the server from a non-local address, and with `allow-remote: false` the management API refuses them ("remote management disabled"). Publishing the port on `127.0.0.1` as above still keeps other machines out. The Go server behaves the same way in a container.

The server must be able to write to `data/`: it saves the hashed management key into `config.yaml` on first start, and the dashboard writes settings and credential files. If it cannot, the key stays in plain text and saves fail, which is what the `chown` above prevents. Publish the port on `127.0.0.1` unless other machines need access; see [running it safely](GETTING-STARTED.md#running-it-safely).

Browser sign-in inside a container needs the OAuth callback ports (54545 for Claude, 1455 for Codex) published to your machine, so it is usually easier to sign in from the dashboard and paste the final callback URL, or to use `--codex-device-login`, Kimi or Meta, which use device codes:

```sh
docker exec -it cliproxy cliproxy --config /data/config.yaml --codex-device-login
```

## First run without the install script

If you installed the binary another way, set it up by hand:

1. Make two random keys, for example with `openssl rand -hex 24`, and write `config.yaml`:

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

   Keep a copy of the management key: on first start the server replaces it in `config.yaml` with a bcrypt hash. Every setting in CLIProxyAPI's [`config.example.yaml`](https://github.com/router-for-me/CLIProxyAPI/blob/main/config.example.yaml) is accepted, although settings for features listed as not yet supported have no effect; [CONFIGURATION.md](CONFIGURATION.md) explains the common ones.
2. Start the server: `cliproxy --config config.yaml`. With no `--config`, it reads `config.yaml` in the current directory. Check it with `curl http://127.0.0.1:8317/healthz`, which answers `{"status":"ok"}`.
3. Connect accounts with `--claude-login`, `--codex-login`, `--codex-device-login`, `--kimi-login`, `--kimi-ai-login`, `--meta-login`, `--xai-login` or `--devin-login`, import a Vertex AI service account with `--vertex-import key.json`, or connect accounts from the dashboard at `/management.html`. Add `--no-browser` on a machine without a browser.

Go-style single-dash flags such as `-config config.yaml` work too.

The server reloads `config.yaml` and the credential directory when they change. Set `RUST_LOG=debug` for more detailed logs; the default level is `info`.

## Running as a system service

For a per-user service, use the install script's `--service`. On a Linux server, a system unit like this runs it under a dedicated user:

```ini
# /etc/systemd/system/cliproxy.service
[Unit]
Description=cliproxy-rs
After=network-online.target
Wants=network-online.target

[Service]
User=cliproxy
WorkingDirectory=/var/lib/cliproxy
ExecStart=/usr/local/bin/cliproxy --config /var/lib/cliproxy/config.yaml
Restart=on-failure
NoNewPrivileges=true
ProtectSystem=strict
ReadWritePaths=/var/lib/cliproxy

[Install]
WantedBy=multi-user.target
```

```sh
sudo useradd --system --home /var/lib/cliproxy --create-home cliproxy
sudo systemctl enable --now cliproxy
journalctl -u cliproxy -f
```

The server writes to `config.yaml` (when the dashboard saves settings or hashes a plaintext management key) and to the credential directory, so both must be writable by the service user.

On Windows, to run it as a system service without a signed-in user, wrap it with a service manager such as NSSM.
