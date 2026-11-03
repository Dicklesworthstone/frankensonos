#!/usr/bin/env python3
"""h003_arms.py - H-003 follow-up arms (frankensonos-9sd).

The 2026-10-29 8K/30K/60K ladder was a clean negative, and the 2026-11-03
read-back showed WHY it could not have been a title-length test: on 57.23 the
queue record's displayed dc:title is DERIVED FROM THE URI PATH, never from the
enqueued DIDL dc:title (tested http / x-file-cifs / x-rincon-mp3radio, reachable
and unreachable, and with a desc/cdudn).  A multi-KB dc:title therefore only
changes the stored metadata blob, not the title field.

This harness runs the model's ordering arms plus the two carrier variants:

  blob  arms - the ladder's carrier (multi-KB dc:title inside the DIDL blob,
               display title stays the filename).  Ordering is the variable.
  uri   arms - the only way to make the record title itself long: a long URI
               filename.  AddURI accepts up to somewhere in 3000..4000 bytes
               and the read-back title truncates at ~1007.

Every arm: clear queue, verify empty, add item(s), then poll /status/zp SWGen
at 0.1 s for --watch seconds and time one Browse at +90 s (F7 oracle).  A SWGen
change (or a failed poll that survives 3 retries) is a daemon death.

usage: ./h003_arms.py <ip> [--watch 120] [--arms a,b,c] [--logdir DIR]
"""
from __future__ import annotations

import argparse
import html
import json
import re
import subprocess
import sys
import time
from datetime import datetime, timezone

Q = "urn:schemas-sonos-com:service:Queue:1"
QCTL = "/MediaRenderer/Queue/Control"
BIG = 61440          # dc:title bytes for the blob arms (matches ladder shot 3/4)
LONG_URI = 3000      # URI bytes for the uri arms (below the ~3-4 KB 804 limit)


def now() -> str:
    return datetime.now(timezone.utc).strftime("%H:%M:%S.%f")[:-3] + "Z"


def soap(ip: str, action: str, args: str, timeout: float = 40.0):
    body = ('<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">'
            '<s:Body><u:%s xmlns:u="%s">%s</u:%s></s:Body></s:Envelope>' % (action, Q, args, action))
    p = subprocess.run(
        ["curl", "-s", "-m", str(timeout), "-o", "/tmp/h003_arm.out", "-w", "%{http_code}",
         "-H", 'Content-Type: text/xml; charset="utf-8"',
         "-H", 'SOAPACTION: "%s#%s"' % (Q, action),
         "--data-binary", body, "http://%s:1400%s" % (ip, QCTL)],
        capture_output=True, text=True)
    out = open("/tmp/h003_arm.out", encoding="utf-8", errors="replace").read()
    err = re.search(r"<errorCode>(\d+)", out)
    return p.stdout.strip(), (err.group(1) if err else None), out


def browse(ip: str, start: int = 0, count: int = 200, timeout: float = 70.0):
    t = time.time()
    code, err, body = soap(ip, "Browse",
                           "<QueueID>0</QueueID><StartingIndex>%d</StartingIndex>"
                           "<RequestedCount>%d</RequestedCount>" % (start, count), timeout)
    return code, err, body, time.time() - t


def state(ip: str):
    """(count, update_id) or (None, None) when the daemon is gone."""
    code, err, body, _ = browse(ip, 0, 1, timeout=10)
    if code != "200":
        return None, None
    m = re.search(r"<TotalMatches>(\d+)", body)
    u = re.search(r"<UpdateID>(\d+)", body)
    return (int(m.group(1)) if m else None), (u.group(1) if u else None)


def clear(ip: str) -> bool:
    n, uid = state(ip)
    if uid is None:
        return False
    if n == 0:
        return True
    code, err, _ = soap(ip, "RemoveAllTracks", "<QueueID>0</QueueID><UpdateID>%s</UpdateID>" % uid)
    n2, _ = state(ip)
    return code == "200" and n2 == 0


def item_xml(uri: str, title: str) -> str:
    return ('<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/"'
            ' xmlns:dc="http://purl.org/dc/elements/1.1/"'
            ' xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">'
            '<item id="1" parentID="0" restricted="1"><dc:title>%s</dc:title>'
            '<upnp:class>object.item.audioItem.musicTrack</upnp:class>'
            '<res protocolInfo="http-get:*:audio/mpeg:*">%s</res></item></DIDL-Lite>'
            % (html.escape(title), html.escape(uri)))


def add(ip: str, uri: str, title: str):
    _, uid = state(ip)
    if uid is None:
        return None, "no-updateid"
    args = ("<QueueID>0</QueueID><UpdateID>%s</UpdateID><EnqueuedURI>%s</EnqueuedURI>"
            "<EnqueuedURIMetaData>%s</EnqueuedURIMetaData>"
            "<DesiredFirstTrackNumberEnqueued>0</DesiredFirstTrackNumberEnqueued>"
            "<EnqueueAsNext>0</EnqueueAsNext>" % (uid, html.escape(uri), html.escape(item_xml(uri, title))))
    code, err, _ = soap(ip, "AddURI", args)
    return code, err


def setavt(ip: str, title_len: int, uri: str = "http://192.0.2.1/t.mp3", meta: bool = True):
    """SetAVTransportURI with a long dc:title - the record that KEEPS the title."""
    if meta:
        pass
    didl = item_xml(uri, "V" * title_len) if meta else ""
    args = ("<InstanceID>0</InstanceID><CurrentURI>%s</CurrentURI>"
            "<CurrentURIMetaData>%s</CurrentURIMetaData>" % (html.escape(uri), html.escape(didl)))
    body = ('<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">'
            '<s:Body><u:SetAVTransportURI xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">%s'
            '</u:SetAVTransportURI></s:Body></s:Envelope>' % args)
    for attempt in range(6):
        p = subprocess.run(
            ["curl", "-s", "-m", "30", "-o", "/tmp/h003_avt.out", "-w", "%{http_code}",
             "-H", 'Content-Type: text/xml; charset="utf-8"',
             "-H", 'SOAPACTION: "urn:schemas-upnp-org:service:AVTransport:1#SetAVTransportURI"',
             "--data-binary", body, "http://%s:1400/MediaRenderer/AVTransport/Control" % ip],
            capture_output=True, text=True)
        out = open("/tmp/h003_avt.out", encoding="utf-8", errors="replace").read()
        err = re.search(r"<errorCode>(\d+)", out)
        if p.stdout.strip() != "000":
            return p.stdout.strip(), (err.group(1) if err else None)
        print("  setavt retry %d (daemon down)" % attempt, flush=True)
        time.sleep(3)
    return "000", None


def swgen(ip: str):
    p = subprocess.run(["curl", "-s", "-m", "1", "http://%s:1400/status/zp" % ip],
                       capture_output=True, text=True)
    m = re.search(r"<SWGen>(\d+)</SWGen>", p.stdout)
    return int(m.group(1)) if m else None


def watch(ip: str, seconds: float):
    """Poll SWGen at 0.1 s. Returns (deaths, samples, browse_latency_at_90s)."""
    base = swgen(ip)
    deaths, samples, lat = 0, 0, None
    t0 = time.time()
    last_ok = base
    while time.time() - t0 < seconds:
        time.sleep(0.1)
        samples += 1
        v = swgen(ip)
        if v is None:
            # confirm with three retries before calling it a death
            for _ in range(3):
                time.sleep(0.4)
                v = swgen(ip)
                if v is not None:
                    break
        if v is None:
            deaths += 1
            print("  %s daemon GONE at +%.1fs" % (now(), time.time() - t0), flush=True)
            for _ in range(60):
                time.sleep(2)
                if swgen(ip) is not None:
                    break
            last_ok = swgen(ip)
            continue
        if last_ok is not None and v != last_ok:
            deaths += 1
            print("  %s SWGen %s->%s at +%.1fs" % (now(), last_ok, v, time.time() - t0), flush=True)
        last_ok = v
        if lat is None and time.time() - t0 >= 90:
            c, e, b, dt = browse(ip, 0, 20)
            lat = dt
            print("  %s browse@+90s code=%s %.2fs" % (now(), c, dt), flush=True)
    return deaths, samples, lat


def short_uri() -> str:
    return "http://192.0.2.1/s.mp3"


def long_uri(host: str = "192.0.2.1") -> str:
    n = LONG_URI
    return "http://%s/" % host + "F" * (n - 4 - len(host)) + ".mp3"


ARMS = {
    # name: (title for item 1, title for item 2 or None, uri kind)
    "blob-single": (BIG, None, "short"),      # ladder control
    "blob-short-big": (8, BIG, "short"),      # [short, multi-KB]
    "blob-big-short": (BIG, 8, "short"),      # [multi-KB, short]
    "uri-single": (8, None, "long"),          # long derived title alone
    "uri-short-long": (8, 8, "short+long"),   # [short, long title]
    "uri-long-short": (8, 8, "long+short"),   # [long title, short]
    "uri-single-live": (8, None, "longlive"),  # long title, live host
    "uri-short-longlive": (8, 8, "short+longlive"),
    "avt-8k": (8192, None, "avt"),
    "avt-30k": (30720, None, "avt"),
    "avt-short": (100, None, "avt"),
    "avt-live-short": (100, None, "avt-live-short"),
    "avt-live-8k": (8192, None, "avt-live-8k"),
    "avt-dead-empty": (0, None, "avt-dead-empty"),
    "avt-refused": (100, None, "avt-refused"),
}


def run_arm(ip: str, name: str, watch_s: float) -> dict:
    t1, t2, kind = ARMS[name]
    print("=== arm %s %s ===" % (name, now()), flush=True)
    row = {"arm": name, "ts": now(), "adds": [], "deaths": None, "browse90": None}
    for attempt in range(10):
        if clear(ip):
            break
        print("  clear retry %d" % attempt, flush=True)
        time.sleep(3)
    else:
        print("  clear FAILED", flush=True)
        row["error"] = "clear-failed"
        return row
    plan = []
    if kind.startswith("avt"):
        # clear any previous long transport record first, or the unit re-kills
        setavt(ip, 8)
        time.sleep(2)
        if kind == "avt-live-short":
            code, err = setavt(ip, t1, "http://192.168.4.160:8903/t.mp3")
        elif kind == "avt-live-8k":
            code, err = setavt(ip, t1, "http://192.168.4.160:8903/t.mp3")
        elif kind == "avt-refused":
            code, err = setavt(ip, t1, "http://192.168.4.160:9999/t.mp3")
        elif kind == "avt-dead-empty":
            code, err = setavt(ip, 0, meta=False)
        else:
            code, err = setavt(ip, t1)
        row["adds"].append({"avt_title_len": t1, "http": code, "err": err})
        print("  SetAVTransportURI title_len=%d http=%s err=%s" % (t1, code, err), flush=True)
        if code != "200":
            row["error"] = "avt-%s" % code
            return row
        deaths, samples, lat = watch(ip, watch_s)
        row.update({"deaths": deaths, "samples": samples, "browse90": lat})
        print("  arm done deaths=%d samples=%d" % (deaths, samples), flush=True)
        return row
    if kind == "short":
        plan = [(short_uri(), "A" * t1)] if t2 is None else \
               [(short_uri(), "A" * t1), (short_uri(), "A" * t2)]
    elif kind == "long":
        plan = [(long_uri(), "A")]
    elif kind == "short+long":
        plan = [(short_uri(), "A"), (long_uri(), "A")]
    elif kind == "longlive":
        plan = [(long_uri("192.168.4.160:8903"), "A")]
    elif kind == "short+longlive":
        plan = [(short_uri(), "A"), (long_uri("192.168.4.160:8903"), "A")]
    else:
        plan = [(long_uri(), "A"), (short_uri(), "A")]
    for uri, title in plan:
        code, err = add(ip, uri, title)
        row["adds"].append({"uri_len": len(uri), "title_len": len(title), "http": code, "err": err})
        print("  add uri_len=%d title_len=%d http=%s err=%s" % (len(uri), len(title), code, err), flush=True)
        if code != "200":
            row["error"] = "add-%s" % code
            return row
    n, _ = state(ip)
    row["queue_after_add"] = n
    print("  queue=%s watching %ss" % (n, watch_s), flush=True)
    deaths, samples, lat = watch(ip, watch_s)
    row.update({"deaths": deaths, "samples": samples, "browse90": lat})
    print("  arm done deaths=%d samples=%d" % (deaths, samples), flush=True)
    return row


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("ip")
    ap.add_argument("--watch", type=float, default=120.0)
    ap.add_argument("--arms", default=",".join(ARMS))
    ap.add_argument("--logdir", default="recon/queue9sd/logs")
    ap.add_argument("--watch-only", action="store_true",
                    help="do not clear or add; just poll the current queue")
    a = ap.parse_args()
    names = [x for x in a.arms.split(",") if x]
    path = "%s/h003_arms.json" % a.logdir
    try:
        rows = [r for r in json.load(open(path)) if r.get("arm") not in names]
    except (OSError, ValueError):
        rows = []
    for name in names:
        if a.watch_only:
            n, uid = state(a.ip)
            print("=== watch-only %s queue=%s updateid=%s swgen=%s ===" % (name, n, uid, swgen(a.ip)), flush=True)
            d, s_, lat = watch(a.ip, a.watch)
            rows.append({"arm": name, "ts": now(), "watch_only": True, "queue": n,
                         "deaths": d, "samples": s_, "browse90": lat})
        else:
            rows.append(run_arm(a.ip, name, a.watch))
        json.dump(rows, open(path, "w"), indent=1)
        time.sleep(60)
    n, uid = state(a.ip)
    print("final queue=%s updateid=%s swgen=%s" % (n, uid, swgen(a.ip)))
    return 0 if all(r.get("deaths") == 0 for r in rows) else 1


if __name__ == "__main__":
    sys.exit(main())
