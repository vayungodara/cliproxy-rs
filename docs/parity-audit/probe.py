#!/usr/bin/env python3
"""Route probe for the parity audit: starts the cliproxy binary on a throwaway config with
no upstream credentials and requests every method/path PARITY.md lists, without a client
key and with a wrong management key. Routed paths answer from the client-auth or
management guard (401/403) or their handler; unrouted method/path pairs get the empty
404/405 axum and gin give. Nothing leaves localhost: no credential exists, so no
handler can reach an upstream, and the management guard stops before any handler.

Usage: cargo build -p cliproxy && python3 docs/parity-audit/probe.py [target/debug/cliproxy]
Writes docs/parity-audit/routes.json.
"""

import http.client
import json
import os
import re
import socket
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))
LINK = re.compile(r"\[`([^`]*)`\]\([^)]*\)")


def routes():
    out = []
    for line in open(os.path.join(ROOT, "docs", "PARITY.md"), encoding="utf-8"):
        m = re.match(r"^- \[[ x]\] \*\*\[M\d\] (M\d-\d{4})\*\* `([A-Z]+) (/\S*)`", LINK.sub(r"\1", line))
        if m:
            out.append((m.group(1), m.group(2), m.group(3)))
    return out


def concrete(path):
    path = re.sub(r":([A-Za-z_]+)", "probe", path)
    return re.sub(r"\*([A-Za-z_]+)", "probe", path)


def main():
    binary = sys.argv[1] if len(sys.argv) > 1 else os.path.join(ROOT, "target", "debug", "cliproxy")
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
    tmp = tempfile.mkdtemp(prefix="parity-probe-")
    os.makedirs(os.path.join(tmp, "auth"))
    config = os.path.join(tmp, "config.yaml")
    with open(config, "w") as f:
        f.write(
            f"host: 127.0.0.1\nport: {port}\nauth-dir: {tmp}/auth\napi-keys:\n  - probe-client-key\n"
            "remote-management:\n  secret-key: probe-management-key\n  disable-auto-update-panel: true\n"
        )
    proc = subprocess.Popen([binary, "--config", config], cwd=tmp, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(100):
            try:
                c = http.client.HTTPConnection("127.0.0.1", port, timeout=2)
                c.request("GET", "/healthz")
                c.getresponse().read()
                break
            except OSError:
                time.sleep(0.1)
        result = {}
        for item, method, path in routes():
            c = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
            headers = {"Authorization": "Bearer wrong-key", "Content-Type": "application/json"}
            body = b"{}" if method in ("POST", "PUT", "PATCH") else None
            try:
                c.request(method, concrete(path), body=body, headers=headers)
                r = c.getresponse()
                data = r.read()
                status = r.status
            except OSError as e:
                status, data = 0, str(e).encode()
            routed = not ((status in (404, 405) and not data.strip()) or status == 0)
            result[f"{method} {path}"] = {"id": item, "status": status, "routed": routed, "body": data[:80].decode("utf-8", "replace")}
        json.dump(result, open(os.path.join(HERE, "routes.json"), "w"), indent=1, sort_keys=True)
        routed = sum(v["routed"] for v in result.values())
        print(f"{routed}/{len(result)} routed")
    finally:
        proc.terminate()
        proc.wait()


if __name__ == "__main__":
    main()
