#!/usr/bin/env python3
"""Serves a dashboard build (ui/dist) on one loopback port and passes every other request
to the proxy on another, so a release binary shows a newer dashboard without a rebuild.
Usage: front.py <listen-port> <backend-port> <dist-dir>"""
import mimetypes, os, sys, urllib.error, urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

LISTEN, BACKEND, DIST = int(sys.argv[1]), int(sys.argv[2]), os.path.realpath(sys.argv[3])


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *a):
        return None


opener = urllib.request.build_opener(NoRedirect, urllib.request.ProxyHandler({}))


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def reply(self, status, headers, body):
        self.send_response(status)
        for k, v in headers:
            if k.lower() not in ("transfer-encoding", "content-length", "connection"):
                self.send_header(k, v)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(body)

    def handle_any(self):
        path = self.path.split("?")[0]
        full = os.path.realpath(os.path.join(DIST, "management.html" if path == "/" else path.lstrip("/")))
        if self.command == "GET" and full.startswith(DIST + os.sep) and os.path.isfile(full):
            with open(full, "rb") as f:
                return self.reply(200, [("Content-Type", mimetypes.guess_type(full)[0] or "application/octet-stream"), ("Cache-Control", "no-store")], f.read())
        body = self.rfile.read(int(self.headers.get("Content-Length", 0) or 0)) or None
        headers = {k: v for k, v in self.headers.items() if k.lower() not in ("host", "connection", "accept-encoding")}
        req = urllib.request.Request(f"http://127.0.0.1:{BACKEND}{self.path}", data=body, headers=headers, method=self.command)
        try:
            with opener.open(req, timeout=30) as r:
                self.reply(r.status, r.getheaders(), r.read())
        except urllib.error.HTTPError as e:
            self.reply(e.code, e.headers.items(), e.read())

    do_GET = do_POST = do_PUT = do_PATCH = do_DELETE = do_OPTIONS = do_HEAD = handle_any


ThreadingHTTPServer(("127.0.0.1", LISTEN), Handler).serve_forever()
