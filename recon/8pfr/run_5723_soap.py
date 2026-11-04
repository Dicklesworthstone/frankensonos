#!/usr/bin/env python3
"""8pfr follow-up — deliver the esds / ALAC-cookie / stsd-entry classes to a 57.23 unit
(the PPC build that actually contains the hand-rolled demuxer), over UPnP SOAP.

The 57.23 REST wrapper returns empty `parameters`, so SOAP on :1400 is the only way to read
TrackDuration / transport state.  Per case: Stop, SetAVTransportURI (audio/mp4 res), Play,
sample twice, Stop.  `fetched` counts server access-log lines added during the case; the daemon
is polled on :1400/status/VERSION per case so a restart is visible.
"""
import os, re, subprocess, sys, time

IP = "192.168.4.84"
UDN = "RINCON_000E5880108A01400"
HOST = "192.168.4.160"
OUT = os.environ.get("OUT", "/tmp/m4a_poc/verdicts_5723.tsv")
LOG = "/tmp/m4a_poc/access.log"
POCDIR = "/tmp/m4a_poc/poc"
CTL = f"http://{IP}:1400/MediaRenderer/AVTransport/Control"
SVC = "urn:schemas-upnp-org:service:AVTransport:1"


def soap(action, extra=""):
    body = ('<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" '
            's:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body>'
            f'<u:{action} xmlns:u="{SVC}"><InstanceID>0</InstanceID>{extra}</u:{action}>'
            '</s:Body></s:Envelope>')
    r = subprocess.run(["curl", "-s", "-m", "25", "-X", "POST", "-H",
                        'Content-Type: text/xml; charset="utf-8"',
                        "-H", f'SOAPACTION: "{SVC}#{action}"', "--data-binary", body,
                        "-w", "\n%{http_code}", CTL], capture_output=True, text=True)
    return r.stdout.rsplit("\n", 1)


def param(x, name):
    m = re.search(rf"<{name}>([^<]*)</{name}>", x)
    return m.group(1) if m else ""


def meta(url):
    return ('<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" '
            'xmlns:dc="http://purl.org/dc/elements/1.1/" '
            'xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">'
            '<item id="-1" parentID="-1" restricted="true"><dc:title>poc</dc:title>'
            '<upnp:class>object.item.audioItem.musicTrack</upnp:class>'
            f'<res protocolInfo="http-get:*:audio/mp4:*">{url}</res></item></DIDL-Lite>')


def alive():
    return subprocess.run(["curl", "-s", "-m", "4", "-o", "/dev/null", "-w", "%{http_code}",
                           f"http://{IP}:1400/status/VERSION"], capture_output=True, text=True).stdout


def run_one(name, path, stamp):
    before = sum(1 for _ in open(LOG))
    soap("Stop")
    url = f"http://{HOST}:8791/{path}?c={stamp}"
    esc = meta(url).replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")
    j1, c1 = soap("SetAVTransportURI", f"<CurrentURI>{url}</CurrentURI><CurrentURIMetaData>{esc}</CurrentURIMetaData>")
    j2, c2 = soap("Play", "<Speed>1</Speed>")
    states, durs, rels = [], [], []
    for _ in range(16):
        time.sleep(0.5)
        jp, _ = soap("GetPositionInfo")
        if param(jp, "TrackDuration") not in ("", "0:00:00"):
            break
    jp, _ = soap("GetPositionInfo")
    if param(jp, "TrackDuration") not in ("", "0:00:00"):
        jt, _ = soap("GetTransportInfo")
        if param(jt, "CurrentTransportState") == "STOPPED":
            soap("Play", "<Speed>1</Speed>")
    for wait in (1.0, 2.5):
        time.sleep(wait)
        jt, _ = soap("GetTransportInfo")
        jp, _ = soap("GetPositionInfo")
        states.append(param(jt, "CurrentTransportState"))
        durs.append(param(jp, "TrackDuration"))
        rels.append(param(jp, "RelTime"))
    fetched = sum(1 for _ in open(LOG)) - before
    live = alive()
    soap("Stop")
    dur_ok = any(d and d != "0:00:00" for d in durs)
    if "PLAYING" in states and any(r not in ("", "0:00:00") for r in rels):
        verdict = "playing"
    elif "PLAYING" in states:
        verdict = "playing-no-clock"
    elif dur_ok:
        verdict = "parsed"
    elif states and states[-1] == "TRANSITIONING":
        verdict = "hung"
    else:
        verdict = "rejected"
    return dict(case=name, set_http=c1, play_http=c2, states="/".join(states),
                durs="/".join(durs), rel="/".join(rels), fetched=fetched, alive=live,
                verdict=verdict)


def main():
    stamp = int(time.time())
    log = open(OUT, "w")
    cols = ("case", "set_http", "play_http", "states", "durs", "rel", "fetched", "alive", "verdict")
    log.write("ts\t" + "\t".join(cols) + "\n")
    keep = ("cookie_", "esds_", "stsd_")
    cases = [("BASE-base", "base.m4a")] + [
        (f[:-4], "poc/" + f) for f in sorted(os.listdir(POCDIR))
        if f.endswith(".m4a") and f.startswith(keep)]
    for name, path in cases:
        r = run_one(name, path, stamp)
        log.write("\t".join([time.strftime("%H:%M:%S")] + [str(r[k]) for k in cols]) + "\n")
        log.flush()
        print(f"{name:26s} fetch={r['fetched']} alive={r['alive']} {r['states']:26s} {r['verdict']}",
              flush=True)
        time.sleep(1.5)
    log.write(f"{time.strftime('%H:%M:%S')}\tBATCH_END\t-\t-\t-\t-\t-\t{alive()}\t-\n")
    log.close()
    print("BATCH_END", alive())


main()
