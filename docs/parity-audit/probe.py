#!/usr/bin/env python3
"""Route probe for the parity audit: starts the cliproxy binary on a throwaway config with
no upstream credentials and requests every method/path checklist.md lists. Unrouted
method/path pairs get the empty 404/405 axum and gin give; anything else is routed.

API paths are requested with a wrong client key, so the client-auth guard answers. The
management guard cannot be used the same way: it answers before the router (an unknown
management path also gets its 401) and bans an address after repeated bad keys. So
management paths carry the real management key and reach their handlers: reads share one
server, and every write gets a fresh server and config, because a write can disable
management for the requests after it. A 5 s timeout counts as routed (a handler is
running). Nothing leaves localhost: no upstream credential exists. Run it with external
network denied all the same.

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
MANAGEMENT_KEY = "probe-management-key"


def routes():
    out = []
    for line in open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "checklist.md"), encoding="utf-8"):
        m = re.match(r"^- \[[ x]\] \*\*\[M\d\] (M\d-\d{4})\*\* `([A-Z]+) (/\S*)`", LINK.sub(r"\1", line))
        if m:
            out.append((m.group(1), m.group(2), m.group(3)))
    return out


def concrete(path):
    path = re.sub(r":([A-Za-z_]+)", "probe", path)
    return re.sub(r"\*([A-Za-z_]+)", "probe", path)


class Server:
    """The binary on a fresh throwaway config."""

    def __init__(self, binary):
        with socket.socket() as s:
            s.bind(("127.0.0.1", 0))
            self.port = s.getsockname()[1]
        tmp = tempfile.mkdtemp(prefix="parity-probe-")
        os.makedirs(os.path.join(tmp, "auth"))
        config = os.path.join(tmp, "config.yaml")
        with open(config, "w") as f:
            f.write(
                f"host: 127.0.0.1\nport: {self.port}\nauth-dir: {tmp}/auth\napi-keys:\n  - probe-client-key\n"
                f"remote-management:\n  secret-key: {MANAGEMENT_KEY}\n  disable-auto-update-panel: true\n"
            )
        self.proc = subprocess.Popen([binary, "--config", config], cwd=tmp, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for _ in range(100):
            try:
                c = http.client.HTTPConnection("127.0.0.1", self.port, timeout=2)
                c.request("GET", "/healthz")
                c.getresponse().read()
                return
            except OSError:
                time.sleep(0.1)
        raise SystemExit("the binary did not start")

    def send(self, method, path, key):
        c = http.client.HTTPConnection("127.0.0.1", self.port, timeout=5)
        headers = {"Authorization": f"Bearer {key}", "Content-Type": "application/json"}
        body = b"{}" if method in ("POST", "PUT", "PATCH") else None
        try:
            c.request(method, concrete(path), body=body, headers=headers)
            r = c.getresponse()
            return r.status, r.read()
        except socket.timeout:
            return -1, b"timeout"
        except OSError as e:
            return 0, str(e).encode()

    def close(self):
        self.proc.terminate()
        self.proc.wait()


def main():
    binary = sys.argv[1] if len(sys.argv) > 1 else os.path.join(ROOT, "target", "debug", "cliproxy")
    result = {}

    def record(server, item, method, path):
        management = "/management/" in path
        status, data = server.send(method, path, MANAGEMENT_KEY if management else "wrong-key")
        unrouted = (status in (404, 405) and not data.strip()) or status == 0
        result[f"{method} {path}"] = {"id": item, "status": status, "routed": status == -1 or not unrouted,
                                      "body": data[:80].decode("utf-8", "replace")}

    shared = Server(binary)
    writes = []
    try:
        for item, method, path in routes():
            if "/management/" in path and method not in ("GET", "HEAD"):
                writes.append((item, method, path))
            else:
                record(shared, item, method, path)
        status, _ = shared.send("GET", "/v0/management/config", MANAGEMENT_KEY)
        if status != 200:
            raise SystemExit(f"the management key stopped working on the shared server (GET /v0/management/config -> {status})")
    finally:
        shared.close()
    for item, method, path in writes:
        server = Server(binary)
        try:
            record(server, item, method, path)
        finally:
            server.close()
    json.dump(result, open(os.path.join(HERE, "routes.json"), "w"), indent=1, sort_keys=True)
    routed = sum(v["routed"] for v in result.values())
    print(f"{routed}/{len(result)} routed")


if __name__ == "__main__":
    main()
