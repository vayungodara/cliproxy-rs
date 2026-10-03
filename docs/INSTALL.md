# Installing cliproxy-rs

cliproxy-rs is one executable, `cliproxy` (`cliproxy.exe` on Windows), with the dashboard built in. It needs a `config.yaml` and a directory for credential files. Nothing else is installed.

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

You need a Rust toolchain (stable, 1.88 or newer) and the tools to build BoringSSL: `cmake`, `clang` and `perl`. On Debian or Ubuntu:

```sh
sudo apt-get install -y build-essential cmake clang perl git
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

On macOS, `xcode-select --install` and `brew install cmake` are enough. On Windows, install Visual Studio Build Tools with the C++ workload, CMake and LLVM (bindgen needs `libclang`). BoringSSL builds there without assembly, so NASM is not needed. The Windows build is produced by the release workflow and has not been tested by hand.

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

The server must be able to write to `data/`: it saves the hashed management key into `config.yaml` on first start, and the dashboard writes settings and credential files. If it cannot, the key stays in plain text and saves fail, which is what the `chown` above prevents. Publish the port on `127.0.0.1` unless other machines need access; see the security notes in the [README](../README.md#security).

Browser sign-in inside a container needs the OAuth callback ports (54545 for Claude, 1455 for Codex) published to your machine, so it is usually easier to sign in from the dashboard and paste the final callback URL, or to use `--codex-device-login`, Kimi or Meta, which use device codes:

```sh
docker exec -it cliproxy cliproxy --config /data/config.yaml --codex-device-login
```

## First run

1. Write `config.yaml`. The [README](../README.md#quick-start) has a minimal one, and every setting in CLIProxyAPI's [`config.example.yaml`](https://github.com/router-for-me/CLIProxyAPI/blob/main/config.example.yaml) is accepted, although settings for features listed as not yet supported have no effect.
2. Start the server: `cliproxy --config config.yaml`. With no `--config`, it reads `config.yaml` in the current directory.
3. Connect accounts with `--claude-login`, `--codex-login`, `--codex-device-login`, `--kimi-login`, `--kimi-ai-login`, `--meta-login`, `--xai-login` or `--devin-login`, or from the dashboard at `/management.html`. Add `--no-browser` on a machine without a browser.

Go-style single-dash flags such as `-config config.yaml` work too.

The server reloads `config.yaml` and the credential directory when they change. Set `RUST_LOG=debug` for more detailed logs; the default level is `info`.

## Running as a service

On Linux with systemd, a unit like this runs it under a dedicated user:

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

On macOS, run it from a `launchd` agent or simply in a terminal. On Windows, run it from a terminal or wrap it with a service manager such as NSSM.
