#!/usr/bin/env python3
"""Exercise installers against private, local release archives, never user accounts."""
import hashlib
import http.server
import os
from pathlib import Path
import platform
import shlex
import shutil
import ssl
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
import urllib.request
import zipfile


def main():
    binary = Path(sys.argv[1]).resolve()
    repo = Path(__file__).resolve().parents[2]
    windows = os.name == "nt"
    version = subprocess.check_output([binary, "--version"], text=True).splitlines()[-1].split()[-1]
    target = {
        ("Linux", "x86_64"): "x86_64-unknown-linux-gnu",
        ("Linux", "aarch64"): "aarch64-unknown-linux-gnu",
        ("Darwin", "arm64"): "aarch64-apple-darwin",
        ("Darwin", "x86_64"): "x86_64-apple-darwin",
        ("Windows", "AMD64"): "x86_64-pc-windows-msvc",
    }[(platform.system(), platform.machine())]
    exe = "cliproxy.exe" if windows else "cliproxy"
    requests = []

    with tempfile.TemporaryDirectory(prefix="installer-test-") as tmp:
        root = Path(tmp)
        releases = root / "releases"
        releases.mkdir()

        def package(tag, source):
            name = f"cliproxy-{tag.removeprefix('v')}-{target}"
            directory = root / name
            directory.mkdir()
            shutil.copy2(source, directory / exe)
            download = releases / "download" / tag
            download.mkdir(parents=True)
            archive = download / (name + (".zip" if windows else ".tar.gz"))
            if windows:
                with zipfile.ZipFile(archive, "w") as z:
                    z.write(directory / exe, f"{name}/{exe}")
            else:
                with tarfile.open(archive, "w:gz") as t:
                    t.add(directory, arcname=name)
            digest = hashlib.sha256(archive.read_bytes()).hexdigest()
            (download / "SHA256SUMS").write_text(f"{digest}  {archive.name}\n")

        package(f"v{version}", binary)
        if windows:
            package("v999.1.0", binary)
        else:
            wrapper = root / "version-wrapper"
            wrapper.write_text(f'#!/bin/sh\nif [ "$1" = --version ]; then echo "cliproxy 999.1.0"; else exec {shlex.quote(str(binary))} "$@"; fi\n')
            wrapper.chmod(0o755)
            package("v999.1.0", wrapper)
        hanging = root / ("hanging" + (".exe" if windows else ""))
        subprocess.run(["rustc", str(repo / ".github/scripts/installer-fixture.rs"), "-o", hanging], check=True)
        package("v999.3.0", hanging)
        if windows:
            legacy = root / "legacy.exe"
            subprocess.run(["rustc", "--cfg", "legacy", str(repo / ".github/scripts/installer-fixture.rs"), "-o", legacy], check=True)
            package("v999.0.0", legacy)
        if not windows:
            bad = root / "broken"
            bad.write_text("#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'cliproxy 999.2.0'; else exit 1; fi\n")
            bad.chmod(0o755)
            package("v999.2.0", bad)

        class Releases(http.server.SimpleHTTPRequestHandler):
            def __init__(self, *args, **kwargs):
                super().__init__(*args, directory=root, **kwargs)

            def do_HEAD(self):
                requests.append(self.path)
                if self.path == "/releases/latest":
                    self.send_response(302)
                    self.send_header("Location", f"/releases/tag/v{version}")
                    self.end_headers()
                elif self.path == f"/releases/tag/v{version}":
                    self.send_response(200)
                    self.end_headers()
                else:
                    super().do_HEAD()

            def do_GET(self):
                requests.append(self.path)
                super().do_GET()

            def log_message(self, *_):
                pass

        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Releases)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        home = root / "private home"
        bindir = root / "private bin"
        env = {k: v for k, v in os.environ.items() if k.upper() in {
            "PATH", "SYSTEMROOT", "WINDIR", "COMSPEC", "PATHEXT", "TEMP", "TMP",
            "PROCESSOR_ARCHITECTURE", "PROCESSOR_ARCHITEW6432",
        }}
        env.update({"HOME": str(root), "USERPROFILE": str(root), "LOCALAPPDATA": str(root),
               "CLIPROXY_RELEASES": f"http://127.0.0.1:{server.server_port}/releases",
               "CLIPROXY_HOME": str(home), "CLIPROXY_INSTALL_DIR": str(bindir),
               "CLIPROXY_NO_OPEN": "1", "CLIPROXY_NO_UPDATE_CHECK": "1"})
        # Catalog fetches cannot leave loopback even on native runners without namespaces.
        env.update(HTTP_PROXY="http://127.0.0.1:9", HTTPS_PROXY="http://127.0.0.1:9",
                   ALL_PROXY="http://127.0.0.1:9", NO_PROXY="127.0.0.1,localhost,::1")
        original_run = None

        def ps(code, check=True, cwd=None):
            result = subprocess.run(["powershell.exe", "-NoProfile", "-Command", code], env=env, cwd=cwd,
                                    text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
            if check:
                assert result.returncode == 0, result.stdout
            return result.stdout.strip()

        def install(*args, success=True, pipe=False):
            if windows:
                if pipe:
                    script = str(repo / "install.ps1").replace("'", "''")
                    command = ["powershell.exe", "-NoProfile", "-Command", f"$ErrorActionPreference='Stop'; Get-Content -Raw '{script}' | Invoke-Expression"]
                else:
                    shell = "pwsh.exe" if "-Service" in args else "powershell.exe"
                    command = [shell, "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", str(repo / "install.ps1"), *args]
            else:
                command = ["sh", str(repo / "install.sh"), *args]
            result = subprocess.run(command, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, timeout=120)
            assert (result.returncode == 0) == success, result.stdout
            return result.stdout

        def health():
            for _ in range(40):
                try:
                    with urllib.request.urlopen("http://127.0.0.1:8317/healthz", timeout=2) as r:
                        assert r.status == 200
                    return
                except OSError:
                    time.sleep(0.25)
            raise AssertionError("server not healthy")

        def pid():
            return int((home / "cliproxy.pid").read_text().strip())

        try:
            process_log = root / "process.log"
            process_log.write_text("keep existing log\n")
            failed = subprocess.run([binary, "--config", str(root / "missing-config.yaml"),
                                     "--log-file", process_log, "--local-model"], cwd=root, env=env,
                                    stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=20)
            assert failed.returncode != 0
            logged = process_log.read_text()
            assert logged.startswith("keep existing log\n") and "config" in logged[len("keep existing log\n"):], logged
            if windows:
                original_run = ps("$e=(Get-ItemProperty 'HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Run' -ErrorAction SilentlyContinue).'cliproxy-rs'; if($e){$e}")
            else:
                assert "not installed" in install("--check")
                assert not home.exists() and not bindir.exists()
                assert not any("/download/" in r for r in requests)
            install(pipe=windows)
            health()
            keys = dict(line.split("=", 1) for line in (home / "keys.env").read_text().splitlines() if line.startswith("CLIPROXY_"))
            for path, key, expected in [
                ("/v1/models", None, 401),
                ("/v1/models", keys["CLIPROXY_CLIENT_KEY"], 200),
                ("/v8/management/config", keys["CLIPROXY_MANAGEMENT_KEY"], 200),
            ]:
                request = urllib.request.Request("http://127.0.0.1:8317" + path)
                if key:
                    request.add_header("Authorization", "Bearer " + key)
                try:
                    with urllib.request.urlopen(request, timeout=5) as response:
                        code = response.status
                except urllib.error.HTTPError as error:
                    code = error.code
                assert code == expected, (path, code)
            if windows:
                ps("$ErrorActionPreference='Stop'; $me=[Security.Principal.WindowsIdentity]::GetCurrent().Name; "
                   "$who=(Get-Acl \"$env:CLIPROXY_HOME\\keys.env\").Access.IdentityReference.Value; "
                   "if($who | Where-Object { $_ -notin $me, 'NT AUTHORITY\\SYSTEM', 'BUILTIN\\Administrators' }){throw 'keys readable by others'}")
            else:
                assert (home / "keys.env").stat().st_mode & 0o777 == 0o600
            before = [(home / f).read_bytes() for f in ("config.yaml", "keys.env")]
            old_pid = pid()
            old_inode = (bindir / exe).stat().st_ino
            if not windows:
                requests.clear()
                assert "already current" in install("--check")
                assert "already current" in install()
                assert pid() == old_pid
                assert (bindir / exe).stat().st_ino == old_inode
                assert not any("/download/" in r for r in requests)
            env["CLIPROXY_VERSION"] = "v999.1.0"
            install("-BinaryOnly" if windows else "--binary-only")
            health()
            assert pid() == old_pid, "binary-only must leave the old server running"
            if not windows:
                assert (bindir / "cliproxy.prev").stat().st_ino == old_inode
                requests.clear()
                print(subprocess.run(["ps", "-p", str(old_pid), "-o", "pid=,comm="], text=True, capture_output=True).stdout)
                if platform.system() == "Darwin":
                    print(subprocess.run(["ps", "-p", str(old_pid), "-o", "ucomm="], text=True, capture_output=True).stdout)
            install()
            health()
            assert pid() != old_pid
            assert [(home / f).read_bytes() for f in ("config.yaml", "keys.env")] == before
            if not windows:
                assert not any("/download/" in r for r in requests), "current on-disk upgrade must restart without downloading"
                restarted = pid()
                assert "already current" in install()
                assert pid() == restarted
                subprocess.run(["kill", str(restarted)], check=True)
                time.sleep(2)
                install()
                health()
                assert pid() != restarted, "a stopped current binary must start again"
            second = root / "second"
            env.update(CLIPROXY_HOME=str(second))
            try:
                install()
                assert "CLIPROXY_PORT=8318" in (second / "keys.env").read_text()
                with urllib.request.urlopen("http://127.0.0.1:8318/healthz", timeout=5) as response:
                    assert response.status == 200
                second_pid = int((second / "cliproxy.pid").read_text().strip())
                env.update(CLIPROXY_HOME=str(home))
                install()
                health()
                with urllib.request.urlopen("http://127.0.0.1:8318/healthz", timeout=5) as response:
                    assert response.status == 200, "upgrading one config must not stop another config sharing the binary"
                health()
            finally:
                if (second / "cliproxy.pid").exists():
                    second_pid = int((second / "cliproxy.pid").read_text().strip())
                    if windows:
                        ps(f"Stop-Process -Id {second_pid} -ErrorAction SilentlyContinue", check=False)
                    else:
                        subprocess.run(["kill", str(second_pid)], check=False)
                env.update(CLIPROXY_HOME=str(home))
            if not windows:
                # A service command fails once after the swap; rollback must restart and
                # verify the previous image rather than losing it through `set -e`.
                mockbin = root / "service-mocks"
                mockbin.mkdir()
                shim = mockbin / ("launchctl" if platform.system() == "Darwin" else "systemctl")
                shim.write_text('''#!/bin/sh
case "$*" in
  *show-environment*|*daemon-reload*|*enable*) exit 0 ;;
  print*|*is-active*)
    if [ -f "$CLIPROXY_HOME/service.pid" ] && kill -0 "$(cat "$CLIPROXY_HOME/service.pid")" 2>/dev/null; then
      echo 'state = running'; exit 0
    fi
    exit 1 ;;
  bootstrap*|kickstart*|*restart*)
    if [ -f "$CLIPROXY_HOME/fail-service-once" ]; then
      rm "$CLIPROXY_HOME/fail-service-once"; echo 'injected service failure' >&2; exit 1
    fi
    (cd "$CLIPROXY_HOME" && exec nohup "$CLIPROXY_INSTALL_DIR/cliproxy" --config "$CLIPROXY_HOME/config.yaml" </dev/null >>"$CLIPROXY_HOME/cliproxy.log" 2>&1) &
    echo $! > "$CLIPROXY_HOME/service.pid" ;;
  *) exit 1 ;;
esac
''')
                shim.chmod(0o755)
                (home / "fail-service-once").touch()
                original_path = env["PATH"]
                env["PATH"] = str(mockbin) + os.pathsep + original_path
                env["CLIPROXY_VERSION"] = f"v{version}"
                image = (bindir / exe).read_bytes()
                try:
                    output = install("--service", success=False)
                    assert "injected service failure" in output and "verified it is healthy" in output, output
                    assert (bindir / exe).read_bytes() == image
                    health()
                finally:
                    marker = home / "service.pid"
                    if marker.exists():
                        (home / "cliproxy.pid").write_text(marker.read_text())
                    unit = (root / "Library/LaunchAgents/io.github.vayungodara.cliproxy-rs.plist" if platform.system() == "Darwin"
                            else root / ".config/systemd/user/cliproxy.service")
                    unit.unlink(missing_ok=True)
                    env["PATH"] = original_path
            env["CLIPROXY_VERSION"] = "v999.3.0"
            image = (bindir / exe).read_bytes()
            output = install(success=False)
            assert ("Rolled back" if windows else "verified it is healthy") in output, output
            if not windows:
                assert "sending SIGKILL" in output, "a hung process ignoring SIGTERM must be killed before restarting"
            assert (bindir / exe).read_bytes() == image, "unhealthy live upgrade must restore the old image"
            health()
            if not windows:
                env["CLIPROXY_VERSION"] = "v999.2.0"
                inode = (bindir / exe).stat().st_ino
                assert "restored the previous binary" in install(success=False)
                assert (bindir / exe).stat().st_ino == inode
                health()
                # A checksum failure must not replace a healthy image or restart it.
                env["CLIPROXY_VERSION"] = f"v{version}"
                sums = releases / f"download/v{version}/SHA256SUMS"
                sums.write_text(sums.read_text().replace(sums.read_text()[:64], "0" * 64))
                old_pid = pid()
                assert "checksum mismatch" in install(success=False)
                assert pid() == old_pid and (bindir / exe).stat().st_ino == inode
                health()
            else:
                env["CLIPROXY_VERSION"] = "v999.1.0"
                install("-Service")
                entry = ps("(Get-ItemProperty 'HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Run').'cliproxy-rs'")
                assert entry.startswith(f'"{bindir / exe}" --config '), entry
                assert "--log-file" in entry and "--working-dir" in entry and not any(x in entry.lower() for x in ("powershell", "cmd.exe", "start.ps1"))
                existing_image = (bindir / exe).read_bytes()
                existing_pid = pid()
                env["CLIPROXY_VERSION"] = "v999.0.0"
                assert "too old for this installer" in install(success=False)
                assert (bindir / exe).read_bytes() == existing_image and pid() == existing_pid
                assert ps("(Get-ItemProperty 'HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Run').'cliproxy-rs'") == entry
                health()
                env["CLIPROXY_VERSION"] = "v999.1.0"
                ps(f"Stop-Process -Id {pid()}")
                # Execute the exact Run entry, not an installer-generated launcher.
                (home / ".env").write_text("WRITABLE_PATH=./relative-state\n")
                config = home / "config.yaml"
                config.write_text(config.read_text() + "\nobservability:\n  logs:\n    logging-to-file: true\n")
                launched = int(ps(f"$e=(Get-ItemProperty 'HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Run').'cliproxy-rs'; $e -match '^\"([^\"]+)\" (.*)$' | Out-Null; (Start-Process -FilePath $Matches[1] -ArgumentList $Matches[2] -PassThru).Id", cwd=root))
                (home / "cliproxy.pid").write_text(str(launched))
                health()
                assert (home / "cliproxy.log").stat().st_size > 0
                assert (home / "relative-state/logs/main.log").is_file(), "Run entry must read its home .env and resolve relative paths there"
                # A later installer must find the direct Run process with a stale pid file.
                (home / "cliproxy.pid").write_text("0")
                install()
                health()
                assert pid() != launched
            # Non-default bind host: derive both the health probe and dashboard URL.
            config = home / "config.yaml"
            config.write_text(config.read_text().replace('host: "127.0.0.1"', 'host: "::1"'))
            env["CLIPROXY_VERSION"] = f"v{version}" if not windows else "v999.1.0"
            if not windows:
                # Make the on-disk current file newer than its last-start marker.
                env["CLIPROXY_VERSION"] = "v999.1.0"
                os.utime(bindir / exe, None)
            assert "http://[::1]:8317/management.html" in install()
            connection = urllib.request.build_opener(urllib.request.ProxyHandler({}))
            with connection.open("http://[::1]:8317/healthz", timeout=5) as response:
                assert response.status == 200
            if not windows:
                cert = root / "fixture-cert.pem"
                key = root / "fixture-key.pem"
                subprocess.run(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
                                "-subj", "/CN=localhost", "-keyout", key, "-out", cert],
                               check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                config.write_text(config.read_text().replace("  port: 8317", f"  port: 8317\n  tls:\n    enable: true\n    cert: '{cert}'\n    key: '{key}'"))
                os.utime(bindir / exe, None)
                assert "https://[::1]:8317/management.html" in install()
                connection = urllib.request.build_opener(urllib.request.ProxyHandler({}),
                              urllib.request.HTTPSHandler(context=ssl._create_unverified_context()))
                with connection.open("https://[::1]:8317/healthz", timeout=5) as response:
                    assert response.status == 200
            print(f"PASS {platform.system()}: install, upgrade, binary-only, unchanged keys" +
                  (", direct Run entry and process log" if windows else ", check, current no-op, hard link, rollback, checksum rejection"))
        except BaseException:
            for folder in (home, root / "second"):
                print(f"Diagnostics: {folder}", file=sys.stderr)
                logfile = folder / "cliproxy.log"
                if logfile.exists():
                    print("\n".join(logfile.read_text(errors="replace").splitlines()[-40:]), file=sys.stderr)
                marker = folder / "cliproxy.pid"
                if marker.exists() and not windows:
                    subprocess.run(["ps", "-p", marker.read_text().strip(), "-o", "pid=,ppid=,stat=,comm=,args="], check=False)
            if windows:
                ps("Get-NetTCPConnection -State Listen -ErrorAction SilentlyContinue | Where-Object { $_.LocalPort -in 8317,8318 } | Format-Table LocalAddress,LocalPort,OwningProcess", check=False)
            elif shutil.which("lsof"):
                subprocess.run(["lsof", "-nP", "-iTCP:8317", "-sTCP:LISTEN"], check=False)
            elif shutil.which("ss"):
                subprocess.run(["ss", "-ltnp"], check=False)
            raise
        finally:
            try:
                if (home / "cliproxy.pid").exists():
                    if windows:
                        ps(f"Stop-Process -Id {pid()} -ErrorAction SilentlyContinue", check=False)
                    else:
                        subprocess.run(["kill", str(pid())], check=False)
                if windows:
                    if original_run:
                        ps("Set-ItemProperty 'HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Run' -Name cliproxy-rs -Value '" + original_run.replace("'", "''") + "'", check=False)
                    else:
                        ps("Remove-ItemProperty 'HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Run' -Name cliproxy-rs -ErrorAction SilentlyContinue", check=False)
            except (OSError, ValueError, subprocess.SubprocessError) as error:
                print(f"Best-effort cleanup: {error}", file=sys.stderr)
            server.shutdown()
            server.server_close()


if __name__ == "__main__":
    main()
