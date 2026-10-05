#!/usr/bin/env python3
"""Exercise installers against private, local release archives, never user accounts."""
import hashlib
import http.server
import os
from pathlib import Path
import platform
import shutil
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
        package("v999.1.0", binary)
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
            "PROCESSOR_ARCHITECTURE", "PROCESSOR_ARCHITEW6432", "PSMODULEPATH",
        }}
        env.update({"HOME": str(root), "USERPROFILE": str(root), "LOCALAPPDATA": str(root),
               "CLIPROXY_RELEASES": f"http://127.0.0.1:{server.server_port}/releases",
               "CLIPROXY_HOME": str(home), "CLIPROXY_INSTALL_DIR": str(bindir),
               "CLIPROXY_NO_OPEN": "1", "CLIPROXY_NO_UPDATE_CHECK": "1"})
        # Catalog fetches cannot leave loopback even on native runners without namespaces.
        env.update(HTTP_PROXY="http://127.0.0.1:9", HTTPS_PROXY="http://127.0.0.1:9",
                   ALL_PROXY="http://127.0.0.1:9", NO_PROXY="127.0.0.1,localhost")
        original_run = None

        def ps(code):
            return subprocess.check_output(["powershell.exe", "-NoProfile", "-Command", code], env=env, text=True).strip()

        def install(*args, success=True):
            if windows:
                command = ["powershell.exe", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", str(repo / "install.ps1"), *args]
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
            install()
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
            install()
            health()
            assert pid() != old_pid
            assert [(home / f).read_bytes() for f in ("config.yaml", "keys.env")] == before
            second = root / "second"
            env.update(CLIPROXY_HOME=str(second), CLIPROXY_INSTALL_DIR=str(root / "second-bin"))
            try:
                install()
                assert "CLIPROXY_PORT=8318" in (second / "keys.env").read_text()
                with urllib.request.urlopen("http://127.0.0.1:8318/healthz", timeout=5) as response:
                    assert response.status == 200
                health()
            finally:
                if (second / "cliproxy.pid").exists():
                    second_pid = int((second / "cliproxy.pid").read_text().strip())
                    if windows:
                        ps(f"Stop-Process -Id {second_pid}")
                    else:
                        subprocess.run(["kill", str(second_pid)], check=True)
                env.update(CLIPROXY_HOME=str(home), CLIPROXY_INSTALL_DIR=str(bindir))
            if not windows:
                env["CLIPROXY_VERSION"] = "v999.2.0"
                inode = (bindir / exe).stat().st_ino
                assert "restored the previous binary" in install(success=False)
                assert (bindir / exe).stat().st_ino == inode
                health()
                # A checksum failure must not replace a healthy image or restart it.
                sums = releases / "download/v999.1.0/SHA256SUMS"
                sums.write_text(sums.read_text().replace(sums.read_text()[:64], "0" * 64))
                env["CLIPROXY_VERSION"] = "v999.1.0"
                old_pid = pid()
                assert "checksum mismatch" in install(success=False)
                assert pid() == old_pid and (bindir / exe).stat().st_ino == inode
                health()
            else:
                install("-Service")
                entry = ps("(Get-ItemProperty 'HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Run').'cliproxy-rs'")
                assert entry.startswith(f'"{bindir / exe}" --config '), entry
                assert "--log-file" in entry and not any(x in entry.lower() for x in ("powershell", "cmd.exe", "start.ps1"))
                ps(f"Stop-Process -Id {pid()}")
                # Execute the exact Run entry, not an installer-generated launcher.
                launched = int(ps(f"$e=(Get-ItemProperty 'HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Run').'cliproxy-rs'; $e -match '^\"([^\"]+)\" (.*)$' | Out-Null; (Start-Process -FilePath $Matches[1] -ArgumentList $Matches[2] -PassThru).Id"))
                (home / "cliproxy.pid").write_text(str(launched))
                health()
                assert (home / "cliproxy.log").stat().st_size > 0
                # A later installer must find the direct Run process with a stale pid file.
                (home / "cliproxy.pid").write_text("0")
                install()
                health()
                assert pid() != launched
            print(f"PASS {platform.system()}: install, upgrade, binary-only, unchanged keys" +
                  (", direct Run entry and process log" if windows else ", check, current no-op, hard link, rollback, checksum rejection"))
        finally:
            if (home / "cliproxy.pid").exists():
                if windows:
                    ps(f"Stop-Process -Id {pid()} -ErrorAction SilentlyContinue")
                else:
                    subprocess.run(["kill", str(pid())], check=False)
            if windows:
                if original_run:
                    ps("Set-ItemProperty 'HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Run' -Name cliproxy-rs -Value '" + original_run.replace("'", "''") + "'")
                else:
                    ps("Remove-ItemProperty 'HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Run' -Name cliproxy-rs -ErrorAction SilentlyContinue")
            server.shutdown()
            server.server_close()


if __name__ == "__main__":
    main()
