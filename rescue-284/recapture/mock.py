#!/usr/bin/env python3
"""Stand-in Chat Completions providers for the dashboard screenshots. One server answers
for every provider; the config gives each provider its own path prefix. The key ending
in FAIL_KEY answers 429 between FAIL_FROM and FAIL_TO seconds after start. One log line
per POST. Loopback only, standard library only. Usage: mock.py <port>"""
import json, os, sys, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

START = time.time()
FAIL_KEY = os.environ.get("FAIL_KEY", "")
FAIL_FROM, FAIL_TO = float(os.environ.get("FAIL_FROM", "0")), float(os.environ.get("FAIL_TO", "0"))


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def send_body(self, status, ctype, body, extra=()):
        self.send_response(status)
        for k, v in extra:
            self.send_header(k, v)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        self.send_body(200 if self.path == "/ready" else 404, "text/plain", b"ok\n")

    def do_POST(self):
        req = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))) or b"{}")
        key = self.headers.get("Authorization", "").removeprefix("Bearer ")
        t = time.time() - START
        limited = bool(FAIL_KEY) and key.endswith(FAIL_KEY) and FAIL_FROM <= t < FAIL_TO
        status = 429 if limited else 200
        print(f"{t:8.1f} {self.path} ...{key[-4:]} {req.get('model')} -> {status}", flush=True)
        if limited:
            return self.send_body(429, "application/json", b'{"error":{"message":"rate limit reached","type":"rate_limit_error"}}', [("Retry-After", "60")])
        if req.get("stream"):
            chunks = [
                {"id": "chatcmpl-shot", "object": "chat.completion.chunk", "model": req.get("model"), "choices": [{"index": 0, "delta": {"role": "assistant", "content": "Hello."}, "finish_reason": None}]},
                {"id": "chatcmpl-shot", "object": "chat.completion.chunk", "model": req.get("model"), "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 8, "completion_tokens": 2, "total_tokens": 10}},
            ]
            body = "".join(f"data: {json.dumps(c)}\n\n" for c in chunks).encode() + b"data: [DONE]\n\n"
            return self.send_body(200, "text/event-stream", body)
        body = {"id": "chatcmpl-shot", "object": "chat.completion", "model": req.get("model"),
                "choices": [{"index": 0, "message": {"role": "assistant", "content": "Hello."}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 8, "completion_tokens": 2, "total_tokens": 10}}
        self.send_body(200, "application/json", json.dumps(body).encode())


ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), Handler).serve_forever()
