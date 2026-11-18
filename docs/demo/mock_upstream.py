#!/usr/bin/env python3
"""A stand-in Chat Completions provider for the README demo (docs/demo/demo.tape).

It answers 429 with `Retry-After: 60` to the first key that sends a request and 200 to any
other key, so the recording does not depend on which key cliproxy-rs tries first. It prints
one line per request, which shows what cliproxy-rs sent upstream. It listens on 127.0.0.1
only and needs nothing outside the standard library.
"""
import json
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

REPLY = "Hello from the mock provider."
limited_key = None  # the first key seen; it is rate limited for the whole run


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def send_body(self, status, content_type, body):
        self.send_response(status)
        if status == 429:
            self.send_header("Retry-After", "60")
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))) or b"{}")
        key = self.headers.get("Authorization", "").removeprefix("Bearer ")
        global limited_key
        if limited_key is None:
            limited_key = key
        status = 429 if key == limited_key else 200
        print(f"{self.command} {self.path}  {key}  ->  {status}", flush=True)
        if status == 429:
            error = {"error": {"message": "rate limit reached", "type": "rate_limit_error"}}
            return self.send_body(429, "application/json", json.dumps(error).encode())
        model = request.get("model", "mock-model")
        if request.get("stream"):
            chunks = [
                {"delta": {"role": "assistant", "content": REPLY}, "finish_reason": None},
                {"delta": {}, "finish_reason": "stop"},
            ]
            events = [
                {"id": "chatcmpl-demo", "object": "chat.completion.chunk", "created": 0, "model": model,
                 "choices": [dict(index=0, **chunk)]}
                for chunk in chunks
            ]
            body = "".join(f"data: {json.dumps(event)}\n\n" for event in events) + "data: [DONE]\n\n"
            return self.send_body(200, "text/event-stream", body.encode())
        answer = {
            "id": "chatcmpl-demo", "object": "chat.completion", "created": 0, "model": model,
            "choices": [{"index": 0, "message": {"role": "assistant", "content": REPLY}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 8, "completion_tokens": 6, "total_tokens": 14},
        }
        self.send_body(200, "application/json", json.dumps(answer).encode())


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8399
    ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
