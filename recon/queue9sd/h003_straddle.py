#!/usr/bin/env python3
"""H-003 blob-straddle arm (frankensonos-9sd).

Sets one AVTransport record with a chosen blob (DIDL) size against an
UNFETCHABLE URI, then watches /status/zp SWGen at 0.2 s for a daemon restart.
Fixed URI, only the blob size changes between arms, so it tests the record
store's 1024-byte blob cap (> 1024 is SKIPPED, not truncated) rather than the
title length, which F11 already showed to be irrelevant.

    python3 h003_straddle.py <host> <uri> <blob_bytes> [watch_s]

Reset to a short, fetchable record between arms; a record left behind re-kills
on every watchdog restart.
"""
import re
import socket
import sys
import time

PORT = 1400
NS_AVT = "urn:schemas-upnp-org:service:AVTransport:1"
WRAP = 381  # DIDL envelope around the title at title=4


def http(host, path, timeout=3):
    s = socket.create_connection((host, PORT), timeout)
    s.sendall(("GET %s HTTP/1.0\r\nHost: %s\r\n\r\n" % (path, host)).encode())
    out = b""
    while True:
        c = s.recv(65536)
        if not c:
            break
        out += c
    s.close()
    return out.decode("utf-8", "replace")


def soap(host, action, body, timeout=5):
    xml = ('<?xml version="1.0"?><s:Envelope '
           'xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" '
           's:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">'
           '<s:Body><u:%s xmlns:u="%s">%s</u:%s></s:Body></s:Envelope>'
           % (action, NS_AVT, body, action))
    req = ('POST /MediaRenderer/AVTransport/Control HTTP/1.0\r\n'
           'Host: %s:%d\r\nSOAPACTION: "%s#%s"\r\n'
           'Content-Type: text/xml; charset="utf-8"\r\n'
           'Content-Length: %d\r\n\r\n%s'
           % (host, PORT, NS_AVT, action, len(xml), xml))
    s = socket.create_connection((host, PORT), timeout)
    s.sendall(req.encode())
    out = b""
    while True:
        c = s.recv(65536)
        if not c:
            break
        out += c
    s.close()
    return out.decode("utf-8", "replace")


import html


def set_record(host, uri, blob):
    title = "T%d_" % blob + "x" * max(0, blob - WRAP - len("T%d_" % blob))
    didl = ('<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" '
            'xmlns:dc="http://purl.org/dc/elements/1.1/" '
            'xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">'
            '<item id="1" parentID="0" restricted="1"><dc:title>%s</dc:title>'
            '<upnp:class>object.item.audioItem.musicTrack</upnp:class>'
            '<res protocolInfo="http-get:*:audio/mpeg:*">%s</res></item>'
            '</DIDL-Lite>' % (title, uri))
    r = soap(host, "SetAVTransportURI",
             "<InstanceID>0</InstanceID><CurrentURI>%s</CurrentURI>"
             "<CurrentURIMetaData>%s</CurrentURIMetaData>"
             % (html.escape(uri), html.escape(didl)))
    return len(didl), r.split("\r\n")[0]


def swgen(host):
    z = http(host, "/status/zp")
    m = re.search(r"<SWGen>(\d+)</SWGen>", z)
    return int(m.group(1)) if m else None


def main(argv):
    host, uri, blob = argv[1], argv[2], int(argv[3])
    watch = int(argv[4]) if len(argv) > 4 else 210
    base = swgen(host)
    size, status = set_record(host, uri, blob)
    print("blob=%d didl=%d set=%s swgen0=%s" % (blob, size, status, base))
    t0 = time.time()
    deaths = 0
    while time.time() - t0 < watch:
        time.sleep(0.2)
        g = swgen(host)
        if g is None:
            deaths += 1
            print("%+.1fs daemon gone (no /status/zp)" % (time.time() - t0))
        elif g != base:
            deaths += 1
            print("%+.1fs restart, SWGen %s->%s" % (time.time() - t0, base, g))
            base = g
    print("blob=%d deaths=%d watch=%ds" % (blob, deaths, watch))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
