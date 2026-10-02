"""Run both proxies in a loopback-only Linux namespace and retain every observation."""
import argparse
import copy
import errno
import gzip
import http.client
import json
import os
from pathlib import Path
import re
import socket
import socketserver
import ssl
import subprocess
import sys
import threading
import time
import zlib

from fixtures import CREDENTIAL, PROFILE_REPLY, TOKEN_REPLY, cases
from tls_capture import peek_hello


def assert_isolated():
    links = json.loads(subprocess.check_output(["ip", "-j", "link"]))
    if [link["ifname"] for link in links] != ["lo"]:
        raise RuntimeError("refusing to run: namespace must contain only loopback")
    routes = json.loads(subprocess.check_output(["ip", "-j", "route", "show", "table", "all"]))
    if any(route.get("dev") != "lo" for route in routes):
        raise RuntimeError("refusing to run: non-loopback route")
    # In this namespace there must be no route to a documentation-only IP.
    with socket.socket() as conn:
        conn.settimeout(0.2)
        try:
            conn.connect(("192.0.2.1", 443))
        except OSError as exc:
            if exc.errno != errno.ENETUNREACH:
                raise RuntimeError("egress denial was not established") from exc
        else:
            raise RuntimeError("refusing to run: egress is reachable")


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2, ensure_ascii=False) + "\n")


def certificates(directory):
    """Ephemeral test keys only; no global trust installation or disabled verification."""
    def openssl(*args):
        subprocess.run(["openssl", *args], cwd=directory, check=True, stdout=subprocess.DEVNULL,
                       stderr=subprocess.DEVNULL)
    openssl("req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1", "-subj", "/CN=CPA LOCAL TEST CA",
            "-keyout", "ca.key", "-out", "ca.pem", "-addext", "basicConstraints=critical,CA:TRUE")
    openssl("req", "-newkey", "rsa:2048", "-nodes", "-subj", "/CN=api.anthropic.com",
            "-keyout", "server.key", "-out", "server.csr")
    (directory / "extensions").write_text(
        "subjectAltName=DNS:api.anthropic.com,DNS:platform.claude.com\n"
        "basicConstraints=CA:FALSE\nkeyUsage=digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n")
    openssl("x509", "-req", "-in", "server.csr", "-CA", "ca.pem", "-CAkey", "ca.key", "-CAcreateserial",
            "-days", "1", "-out", "server.pem", "-extfile", "extensions")
    for name in ("ca.key", "server.key"):
        (directory / name).chmod(0o600)


class FakeUpstream(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True

    def __init__(self, directory):
        super().__init__(("0.0.0.0", 443), Capture)
        self.context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        self.context.load_cert_chain(directory / "server.pem", directory / "server.key")
        # The oracle uses HTTP/1.1; advertise only that. Capture the client's full
        # offered ALPN, including h2, before negotiation instead of forcing it away.
        self.context.set_alpn_protocols(["http/1.1"])
        self.script = {}
        self.observations = {"go": {"tls": [], "http": [], "errors": []},
                             "rust": {"tls": [], "http": [], "errors": []}}


class Capture(socketserver.BaseRequestHandler):
    def handle(self):
        side = {"127.0.0.2": "go", "127.0.0.3": "rust"}.get(self.request.getsockname()[0])
        if side is None:
            return
        log = self.server.observations[side]
        self.request.settimeout(10)
        try:
            raw, hello = peek_hello(self.request)
            if hello["sni"] not in ("api.anthropic.com", "platform.claude.com"):
                raise ValueError("non-allowlisted SNI")
            records = []
            pos = 0
            while pos < len(raw):
                size = int.from_bytes(raw[pos + 3:pos + 5], "big")
                records.append([raw[pos], int.from_bytes(raw[pos + 1:pos + 3], "big"), size])
                pos += size + 5
            entry = {"client_hello_hex": raw.hex(), "record_layout": records, "profile": hello}
            log["tls"].append(entry)
            with self.server.context.wrap_socket(self.request, server_side=True) as conn:
                entry["negotiated_alpn"] = conn.selected_alpn_protocol()
                entry["negotiated_version"] = conn.version()
                entry["session_reused"] = conn.session_reused
                file = conn.makefile("rb")
                while True:
                    line = file.readline(65537)
                    if not line:
                        return
                    if len(line) > 65536:
                        raise ValueError("request line too long")
                    raw_headers = b""
                    headers = []
                    while True:
                        header = file.readline(65537)
                        if not header or len(header) > 65536:
                            raise ValueError("incomplete/oversized header")
                        raw_headers += header
                        if header == b"\r\n":
                            break
                        name, value = header[:-2].split(b":", 1)
                        headers.append([name.decode("ascii"), value.decode("latin1").strip()])
                    lookup = {k.lower(): v for k, v in headers}
                    if lookup.get("host") not in ("api.anthropic.com", "platform.claude.com"):
                        raise ValueError("non-allowlisted Host")
                    if "transfer-encoding" in lookup:
                        raise ValueError("chunked request capture is not implemented")
                    size = int(lookup.get("content-length", 0))
                    if not 0 <= size <= 64 * 1024 * 1024:
                        raise ValueError("request body too large")
                    body = file.read(size)
                    if len(body) != size:
                        raise ValueError("truncated request body")
                    capture = {"request_line": line.decode("latin1").rstrip("\r\n"), "headers": headers,
                               "raw_head_hex": (line + raw_headers).hex(), "body_hex": body.hex(),
                               "body_text": body.decode("utf-8", errors="replace"), "write_result": "pending"}
                    log["http"].append(capture)
                    path = line.split()[1].decode("ascii").split("?", 1)[0]
                    if path == "/v1/oauth/token":
                        script = {"status": 200, "body": TOKEN_REPLY}
                    elif path == "/api/oauth/profile":
                        script = {"status": 200, "body": PROFILE_REPLY}
                    elif path == "/api/oauth/claude_cli/roles":
                        script = {"status": 200, "body": "{}"}
                    elif path in ("/v1/messages", "/v1/messages/count_tokens"):
                        script = self.server.script
                    else:
                        raise ValueError(f"unscripted upstream path: {path}")
                    try:
                        self.respond(conn, script)
                        capture["write_result"] = "complete"
                    except (BrokenPipeError, ConnectionResetError, ssl.SSLError) as exc:
                        capture["write_result"] = type(exc).__name__
                        return
                    if script.get("truncate") or script.get("close_connection"):
                        return
        except (socket.timeout, ConnectionResetError):
            # Idle keep-alive close and process shutdown are not capture failures.
            return
        except Exception as exc:
            log["errors"].append(f"{type(exc).__name__}: {exc}")

    def respond(self, conn, script):
        payload = script["body"].encode()
        encoding = script.get("encoding")
        if encoding == "gzip":
            payload = gzip.compress(payload, mtime=0)
        elif encoding == "deflate":
            payload = zlib.compress(payload)
        headers = [["Content-Type", "text/event-stream" if script.get("sse") else "application/json"],
                   ["Request-Id", "req_local_fixture"], ["X-Fixture-Private", "must-not-leak"]]
        headers += script.get("headers", [])
        if script.get("close_connection"):
            headers.append(["Connection", "close"])
        if encoding and not script.get("unlabelled"):
            headers.append(["Content-Encoding", encoding])
        chunked = script.get("sse", False)
        headers.append(["Transfer-Encoding", "chunked"] if chunked else ["Content-Length", str(len(payload))])
        head = f"HTTP/1.1 {script['status']} {http.client.responses[script['status']]}\r\n"
        conn.sendall((head + "".join(f"{k}: {v}\r\n" for k, v in headers) + "\r\n").encode())
        if not chunked:
            conn.sendall(payload)
            return
        fragment = script.get("fragment", len(payload) or 1)
        for start in range(0, len(payload), fragment):
            chunk = payload[start:start + fragment]
            conn.sendall(f"{len(chunk):x}\r\n".encode() + chunk + b"\r\n")
        if script.get("slow_tail"):
            # Force enough writes after downstream closure to observe upstream
            # cancellation rather than trusting a successful kernel-buffer write.
            for _ in range(40):
                time.sleep(0.05)
                chunk = b"event: ping\ndata: {\"type\":\"ping\"}\n\n" + b":" + b"x" * 8192 + b"\n\n"
                conn.sendall(f"{len(chunk):x}\r\n".encode() + chunk + b"\r\n")
        if not script.get("truncate"):
            conn.sendall(b"0\r\n\r\n")


class RecordingReader:
    """Retain status/header lines and transfer framing read by HTTPResponse."""
    def __init__(self, reader, wire):
        self.reader = reader
        self.wire = wire

    def read(self, size=-1):
        data = self.reader.read(size)
        self.wire.extend(data)
        return data

    def readline(self, size=-1):
        data = self.reader.readline(size)
        self.wire.extend(data)
        return data

    def flush(self):
        self.reader.flush()

    def close(self):
        self.reader.close()


def downstream(port, case, turn):
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=12)
    wire = bytearray()

    def response_factory(*args, **kwargs):
        response = http.client.HTTPResponse(*args, **kwargs)
        response.fp = RecordingReader(response.fp, wire)
        return response

    conn.response_class = response_factory
    body = case["body"].encode()
    if turn:
        message = json.loads(body)
        message["messages"] += [{"role": "assistant", "content": "local answer"},
                                {"role": "user", "content": "Second local question"}]
        body = json.dumps(message, separators=(",", ":")).encode()
    result = {"request_body_hex": body.hex()}
    try:
        conn.putrequest(case["method"], case["path"], skip_accept_encoding=True)
        for name, value in case["headers"]:
            conn.putheader(name, value)
        conn.putheader("Content-Length", str(len(body)))
        conn.endheaders(body)
        response = conn.getresponse()
        result.update(status=response.status, reason=response.reason, version=response.version,
                      headers=response.getheaders())
        data = bytearray()
        if case.get("disconnect"):
            while not data.endswith(b"\n\n"):
                byte = response.read(1)
                if not byte:
                    break
                data += byte
            result["client_closed_after_event"] = True
            response.close()
        else:
            try:
                data += response.read()
            except http.client.IncompleteRead as exc:
                data += exc.partial
                result["read_error"] = "IncompleteRead"
        result.update(body_hex=data.hex(), body_text=data.decode("utf-8", errors="replace"))
    except Exception as exc:
        result["transport_error"] = f"{type(exc).__name__}: {exc}"
    finally:
        conn.close()
        result["raw_response_hex"] = wire.hex()
    return result


def differences(go, rust, path=""):
    """No lossy JSON/body normalization. Return every differing leaf/count."""
    if type(go) is not type(rust):
        return [{"path": path, "go": go, "rust": rust}]
    if isinstance(go, dict):
        out = []
        for key in sorted(go.keys() | rust.keys()):
            child = f"{path}/{key}"
            if key not in go or key not in rust:
                out.append({"path": child, "go": go.get(key), "rust": rust.get(key), "missing": True})
            else:
                out += differences(go[key], rust[key], child)
        return out
    if isinstance(go, list):
        out = []
        if len(go) != len(rust):
            out.append({"path": path + "/length", "go": len(go), "rust": len(rust)})
        for index, (left, right) in enumerate(zip(go, rust)):
            out += differences(left, right, f"{path}/{index}")
        for index in range(min(len(go), len(rust)), max(len(go), len(rust))):
            out.append({"path": f"{path}/{index}", "go": go[index] if index < len(go) else None,
                        "rust": rust[index] if index < len(rust) else None, "missing": True})
        return out
    return [] if go == rust else [{"path": path, "go": go, "rust": rust}]


def comparable(observation):
    out = copy.deepcopy(observation)
    for hello in out["upstream"]["tls"]:
        # Client random, session ID, ephemeral key shares and PSK binders are
        # nondeterministic. Compare parsed profile, not raw cryptographic bytes.
        del hello["client_hello_hex"]
    for response in out["downstream"]:
        # Preserve Date header casing/order/presence, ignore only its time value.
        response["headers"] = [[k, "<wall-clock>" if k.lower() == "date" else v]
                               for k, v in response.get("headers", [])]
        if "raw_response_hex" in response:
            raw = bytes.fromhex(response["raw_response_hex"])
            head, separator, body = raw.partition(b"\r\n\r\n")
            head = re.sub(rb"(?im)^(Date:[ \t]*)[^\r\n]*", rb"\1<wall-clock>", head)
            response["raw_response_hex"] = (head + separator + body).hex()
    return out


def wait_ready(process, port, registration_log=None):
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"proxy exited during startup: {process.returncode}")
        try:
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=0.3)
            conn.request("GET", "/healthz")
            response = conn.getresponse()
            response.read()
            conn.close()
            registered = registration_log is None or "full client load complete" in registration_log.read_text()
            if response.status == 200 and registered:
                return
        except OSError:
            pass
        time.sleep(0.05)
    raise TimeoutError("proxy did not become ready")


def run_case(root, output, upstream, case):
    directory = output / case["name"]
    directory.mkdir()
    write_json(directory / "fixture.json", case)
    upstream.observations = {side: {"tls": [], "http": [], "errors": []} for side in ("go", "rust")}
    upstream.script = case["script"]
    processes = []
    handles = []
    ports = {"go": 18317, "rust": 18318}
    observations = {}
    try:
        for side, port in ports.items():
            working = directory / side
            auth_dir = working / "auth"
            auth_dir.mkdir(parents=True)
            credential = copy.deepcopy(CREDENTIAL)
            if case.get("expired"):
                credential["expired"] = "2000-01-01T00:00:00Z"
            if case.get("disabled"):
                credential["disabled"] = True
            write_json(auth_dir / "fixture.json", credential)
            config = working / "config.yaml"
            config.write_text(f'config-version: 8\nserver:\n  host: "127.0.0.1"\n  port: {port}\n'
                              'access:\n  api-keys: [fixture-client-key]\n'
                              f'oauth:\n  auth-dir: "{auth_dir}"\n'
                              'management:\n  disable-control-panel: true\n  disable-auto-update-panel: true\n'
                              'routing:\n  retry:\n    request-retry: 0\n    max-retry-interval: 0\n')
            # Whitelist environment: no real credentials, storage backends, proxy
            # URLs, Home settings or unrelated runner secrets reach either proxy.
            env = {"PATH": os.defpath, "HOME": str(working), "TZ": "UTC", "LANG": "C.UTF-8",
                   "SSL_CERT_FILE": str(output / "certs" / "ca.pem"), "SSL_CERT_DIR": str(output / "empty-trust"),
                   "NO_PROXY": "*", "RUST_LOG": "off"}
            if side == "go":
                command = [str(root / "harness/.cache/go-reference"), "-config", str(config), "-local-model"]
            else:
                command = [str(root / "harness/rust/target/debug/cpa-differential-driver"),
                           str(config), str(output / "certs/ca.pem")]
            log = (working / "process.log").open("wb")
            handles.append(log)
            process = subprocess.Popen(command, cwd=working, env=env, stdout=log, stderr=log)
            processes.append(process)
            wait_ready(process, port, working / "process.log" if side == "go" else None)
        if case.get("wait_refresh"):
            # Both proxies refresh in the background; send only after each persisted
            # the rotation so the case measures refresh, not a race with it.
            for side in ports:
                deadline = time.monotonic() + 20
                while time.monotonic() < deadline:
                    persisted = json.loads((directory / side / "auth/fixture.json").read_text())
                    if persisted["access_token"] != CREDENTIAL["access_token"]:
                        break
                    time.sleep(0.05)
                else:
                    raise TimeoutError(f"{side} did not persist the scripted token rotation")
        for side, port in ports.items():
            replies = [downstream(port, case, turn) for turn in range(case.get("turns", 1))]
            observations[side] = {"downstream": replies}
        if case.get("disconnect"):
            time.sleep(2.5)
        # Snapshot before shutdown; namespace cleanup/process exit is not cancellation.
        for side in ports:
            observations[side]["upstream"] = copy.deepcopy(upstream.observations[side])
            observations[side]["credential_after"] = json.loads((directory / side / "auth/fixture.json").read_text())
            write_json(directory / f"{side}.json", observations[side])
        diff = differences(comparable(observations["go"]), comparable(observations["rust"]))
        write_json(directory / "diff.json", diff)
        errors = [error for side in ports for error in observations[side]["upstream"]["errors"]]
        expected_traffic = not case.get("no_upstream") and case["name"] not in {
            "auth-missing", "auth-wrong", "auth-query-first", "models-openai", "models-anthropic", "disabled-credential"}
        if expected_traffic:
            for side in ports:
                if not any(request["request_line"].startswith("POST /v1/messages")
                           for request in observations[side]["upstream"]["http"]):
                    errors.append(f"{side}: fixture did not reach the inference upstream")
        if case.get("wait_refresh") and not any("/v1/oauth/token " in request["request_line"]
                                               for request in observations["go"]["upstream"]["http"]):
            errors.append("Go: expired fixture did not exercise OAuth refresh")
        transport_errors = [reply["transport_error"] for side in ports
                            for reply in observations[side]["downstream"] if "transport_error" in reply]
        return {"name": case["name"], "differences": len(diff), "capture_errors": errors,
                "transport_errors": transport_errors,
                "go_status": [r.get("status") for r in observations["go"]["downstream"]],
                "rust_status": [r.get("status") for r in observations["rust"]["downstream"]]}
    finally:
        for process in processes:
            process.terminate()
        for process in processes:
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
        for handle in handles:
            handle.close()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("root", type=Path)
    parser.add_argument("--case", action="append", help="run only named fixture(s)")
    parser.add_argument("--strict", action="store_true", help="exit 1 if any difference remains")
    args = parser.parse_args()
    assert_isolated()
    root = args.root.resolve()
    output = root / "harness/.cache" / ("run-" + time.strftime("%Y%m%d-%H%M%S"))
    output.mkdir()
    os.umask(0o077)
    certs = output / "certs"
    certs.mkdir()
    (output / "empty-trust").mkdir()
    certificates(certs)
    hosts = output / "hosts"
    hosts.write_text("127.0.0.1 localhost\n127.0.0.2 api.anthropic.com platform.claude.com\n")
    subprocess.run(["mount", "--bind", str(hosts), "/etc/hosts"], check=True)
    selected = [case for case in cases() if not args.case or case["name"] in args.case]
    if not selected or (args.case and set(args.case) - {case["name"] for case in selected}):
        raise ValueError("unknown or empty case selection")
    results = []
    with FakeUpstream(certs) as upstream:
        threading.Thread(target=upstream.serve_forever, daemon=True).start()
        for case in selected:
            result = run_case(root, output, upstream, case)
            results.append(result)
            print(f"{result['name']}: Go {result['go_status']} Rust {result['rust_status']}; "
                  f"{result['differences']} differing fields", flush=True)
        upstream.shutdown()
    failed = any(result["capture_errors"] or result["transport_errors"] for result in results)
    summary = {"reference": "6fecc6e5567912661654a4eaf9b8f5436facd1c2", "cases": results,
               "rust_revision": subprocess.check_output(["git", "-C", str(root), "rev-parse", "HEAD"]).decode().strip(),
               "isolation": "new user/net/mount/pid namespace; loopback only; egress ENETUNREACH",
               "normalizations": ["downstream Date value only", "raw TLS random/session/key/PSK bytes not compared"],
               "infrastructure_failed": failed}
    write_json(output / "summary.json", summary)
    latest = root / "harness/.cache/latest"
    if latest.is_symlink():
        latest.unlink()
    latest.symlink_to(output.name)
    # Test keys are disposable. Retain public test CA/server certificates only.
    for key in certs.glob("*.key"):
        key.unlink()
    print(f"Results: {output}")
    if failed:
        print("Capture/transport failures: inspect summary.json; this is not parity evidence", file=sys.stderr)
        return 2
    return int(args.strict and any(result["differences"] for result in results))


if __name__ == "__main__":
    sys.exit(main())
