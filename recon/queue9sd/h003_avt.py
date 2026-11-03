#!/usr/bin/env python3
"""H-003 transport-record probe (frankensonos-9sd).

Sets an AVTransport record with a chosen URI and dc:title length, reads it back,
and reports the read-back metadata length. Used to measure the carrier caps of
the record path without the queue.

    python3 h003_avt.py set   <host> <uri> <title_len>
    python3 h003_avt.py read  <host>
    python3 h003_avt.py sweep <host> <uri> 1000 2000 4000 4096 5000

Measured on 57.23-74170-1-16 (.84): a 10 382 B metadata blob is accepted (200);
the GetMediaInfo read-back truncates hard at 4173 B (274 B fixed prefix, title
cut mid-string) for every title >= 4000. A 404 body is not a failed fetch.
"""
import html
import re
import socket
import sys
import time

PORT = 1400
NS_AVT = "urn:schemas-upnp-org:service:AVTransport:1"
DIDL = ('<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" '
        'xmlns:dc="http://purl.org/dc/elements/1.1/" '
        'xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">'
        '<item id="1" parentID="0" restricted="1"><dc:title>%s</dc:title>'
        '<upnp:class>object.item.audioItem.musicTrack</upnp:class>'
        '<res protocolInfo="http-get:*:audio/mpeg:*">%s</res></item>'
        '</DIDL-Lite>')


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


def do_set(host, uri, n):
    title = ("T%d" % n) + "x" * max(0, n - len("T%d" % n))
    didl = DIDL % (title, uri)
    r = soap(host, "SetAVTransportURI",
             "<InstanceID>0</InstanceID><CurrentURI>%s</CurrentURI>"
             "<CurrentURIMetaData>%s</CurrentURIMetaData>"
             % (html.escape(uri), html.escape(didl)))
    print("set n=%d didl=%d status=%s" % (n, len(didl), r.split("\r\n")[0]))
    time.sleep(0.4)
    do_read(host)


def do_read(host):
    r = soap(host, "GetMediaInfo", "<InstanceID>0</InstanceID>")
    m = re.search(r"<CurrentURIMetaData>(.*?)</CurrentURIMetaData>", r, re.S)
    t = m.group(1) if m else ""
    tm = re.search(r"&lt;dc:title&gt;(T\d+)", t)
    print("readback len=%d title_tag=%s closed=%s"
          % (len(t), tm.group(1) if tm else None, t.endswith("&gt;")))


def main(argv):
    if len(argv) < 3:
        print(__doc__)
        return 2
    op, host = argv[1], argv[2]
    if op == "set":
        do_set(host, argv[3], int(argv[4]))
    elif op == "read":
        do_read(host)
    elif op == "sweep":
        for n in argv[4:]:
            do_set(host, argv[3], int(n))
            time.sleep(1)
    else:
        print(__doc__)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
