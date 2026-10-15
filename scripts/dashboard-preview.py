#!/usr/bin/env python3
"""Serve the dashboard's explicit, read-only design preview on loopback.

No Sonos API calls are accepted here. For real speakers, run `fsonos serve`
and open /dashboard on its HTTP listener instead.
"""
from __future__ import annotations
import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlsplit

ASSETS = Path(__file__).resolve().parents[1] / "crates/fsonos-api/src/remote"
SCRIPTS = ["dashboard-core.js", "dashboard-visualizer.js", "dashboard-audio.js", "dashboard.js"]
CSP = ("default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; "
       "connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'")

class Preview(BaseHTTPRequestHandler):
    def do_GET(self):
        target = urlsplit(self.path)
        if target.path in ("/", "/dashboard", "/dashboard/"):
            if target.path != "/dashboard" or parse_qs(target.query).get("demo") != ["1"]:
                self.send_response(302)
                self.send_header("Location", "/dashboard?demo=1")
                self.end_headers()
                return
            kind, body = "text/html", (ASSETS / "dashboard.html").read_bytes().replace(b"<body>", b'<body data-preview="true">')
        elif target.path == "/dashboard.css":
            kind, body = "text/css", (ASSETS / "dashboard.css").read_bytes()
        elif target.path == "/dashboard.js":
            kind, body = "text/javascript", b"\n".join((ASSETS / p).read_bytes() for p in SCRIPTS)
        else:
            self.send_error(404, "Read-only preview: no speaker API is available")
            return
        self.send_response(200)
        self.send_header("Content-Type", kind + "; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Content-Security-Policy", CSP)
        self.send_header("Cache-Control", "no-store")
        self.send_header("X-Content-Type-Options", "nosniff")
        self.send_header("Referrer-Policy", "no-referrer")
        self.send_header("X-Frame-Options", "DENY")
        self.send_header("Cross-Origin-Resource-Policy", "same-origin")
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        self.send_error(405, "The preview cannot control speakers")

if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=int, default=8765)
    args = parser.parse_args()
    if not 1 <= args.port <= 65535:
        parser.error("port must be between 1 and 65535")
    server = ThreadingHTTPServer(("127.0.0.1", args.port), Preview)
    print(f"Read-only design preview: http://127.0.0.1:{args.port}/dashboard?demo=1", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
