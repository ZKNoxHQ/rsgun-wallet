#!/usr/bin/env python3
"""Serves dist/ on 127.0.0.1:8100 with Cache-Control: no-store, so the browser never runs a
stale wasm build (a plain `python3 -m http.server` sends no cache headers and the worker's wasm
fetch is aggressively cached), and proxies POST /poi to the POI node — same relay the Netlify
`_redirects` provides, so local serving behaves like the deployed site.
Usage: python3 crates/railgun-wallet-web/serve.py [port]"""
import http.server
import os
import sys
import urllib.error
import urllib.request

POI_UPSTREAM = "https://ppoi.fdi.network/"

port = int(sys.argv[1]) if len(sys.argv) > 1 else 8100
os.chdir(os.path.join(os.path.dirname(os.path.abspath(__file__)), "dist"))


class H(http.server.SimpleHTTPRequestHandler):
    def end_headers(self):
        self.send_header("Cache-Control", "no-store, max-age=0")
        super().end_headers()

    def do_POST(self):
        if self.path.rstrip("/") != "/poi":
            self.send_response(404)
            self.end_headers()
            return
        body = self.rfile.read(int(self.headers.get("content-length", "0")))
        req = urllib.request.Request(
            POI_UPSTREAM, data=body, method="POST",
            headers={"content-type": "application/json"},
        )
        try:
            with urllib.request.urlopen(req, timeout=30) as up:
                data, status = up.read(), up.status
        except urllib.error.HTTPError as e:
            data, status = e.read(), e.code
        except Exception as e:  # noqa: BLE001
            data, status = str(e).encode(), 502
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.end_headers()
        self.wfile.write(data)


http.server.ThreadingHTTPServer.allow_reuse_address = True
with http.server.ThreadingHTTPServer(("127.0.0.1", port), H) as httpd:
    print(f"railgun-wallet-web: http://127.0.0.1:{port} (no-store)")
    httpd.serve_forever()
