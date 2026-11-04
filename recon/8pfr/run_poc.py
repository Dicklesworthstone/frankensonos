#!/usr/bin/env python3
"""8pfr — deliver every generated PoC to the coordinator and record the device verdict.

Per case: Stop, SetAVTransportURI (res protocolInfo http-get:*:audio/mp4:*), Play, sample the
transport at 2 s and 5 s, Stop. `fetched` counts the server's access-log lines added during the
case, so "the device never even opened the file" is distinguishable from "it opened it and
rejected it". A 10 Hz :1400/status/VERSION poller records daemon survival.

Verdict classes: playing (state PLAYING), parsed (TrackDuration > 0), rejected (no duration,
STOPPED), hung (still TRANSITIONING at 5 s).
"""
import json, os, subprocess, sys, threading, time

IP = "192.168.6.112"
U = "RINCON_5CAAFD21D47001400"
API = f"https://{IP}:1443/api/v1/players/{U}/upnpAVTransport"
HOST = "192.168.4.28"
OUT = os.environ.get("OUT", "recon/8pfr/verdicts.tsv")
LOG = "/tmp/m4a_poc/access.log"
POCDIR = "/tmp/m4a_poc/poc"

def post(method, args):
    open("/tmp/p8.json", "w").write(json.dumps({"method": method, "input": [{"name": n, "value": v} for n, v in args]}))
    r = subprocess.run(["curl", "-sk", "-m", "25", "--http1.1", "-X", "POST",
                        "-H", "X-Sonos-Api-Key: x", "-H", "Content-Type: application/json",
                        "--data-binary", "@/tmp/p8.json", "-w", "\n%{http_code}", API],
                       capture_output=True, text=True)
    o = r.stdout.rsplit("\n", 1)
    try: j = json.loads(o[0])
    except Exception: j = {}
    return j, o[-1]

def param(j, name):
    return next((p["value"] for p in j.get("parameters", []) if p["name"] == name), "")

def meta(url):
    return ('<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" '
            'xmlns:dc="http://purl.org/dc/elements/1.1/" '
            'xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">'
            '<item id="-1" parentID="-1" restricted="true"><dc:title>poc</dc:title>'
            '<upnp:class>object.item.audioItem.musicTrack</upnp:class>'
            f'<res protocolInfo="http-get:*:audio/mp4:*">{url}</res></item></DIDL-Lite>')

def run_one(name, path, stamp):
    before = sum(1 for _ in open(LOG))
    post("Stop", [("InstanceID", "0")])
    url = f"http://{HOST}:8791/{path}?c={stamp}"
    j1, c1 = post("SetAVTransportURI", [("InstanceID", "0"), ("CurrentURI", url), ("CurrentURIMetaData", meta(url))])
    # wait until the demuxer has produced a duration (or the pipeline gives up), then Play;
    # a second Play covers the case where the transport stopped between Set and readiness
    j2, c2 = post("Play", [("InstanceID", "0"), ("Speed", "1")])
    states, durs, rels = [], [], []
    for _ in range(16):
        time.sleep(0.5)
        jp, _ = post("GetPositionInfo", [("InstanceID", "0")])
        if param(jp, "TrackDuration") not in ("", "0:00:00"):
            break
    jp, _ = post("GetPositionInfo", [("InstanceID", "0")])
    if param(jp, "TrackDuration") not in ("", "0:00:00"):
        jt, _ = post("GetTransportInfo", [("InstanceID", "0")])
        if param(jt, "CurrentTransportState") == "STOPPED":
            post("Play", [("InstanceID", "0"), ("Speed", "1")])
    for wait in (1.0, 2.5):
        time.sleep(wait)
        jt, _ = post("GetTransportInfo", [("InstanceID", "0")])
        jp, _ = post("GetPositionInfo", [("InstanceID", "0")])
        states.append(param(jt, "CurrentTransportState"))
        durs.append(param(jp, "TrackDuration"))
        rels.append(param(jp, "RelTime"))
    fetched = sum(1 for _ in open(LOG)) - before
    post("Stop", [("InstanceID", "0")])
    dur_ok = any(d and d != "0:00:00" for d in durs)
    if "PLAYING" in states and any(r not in ("", "0:00:00") for r in rels): verdict = "playing"
    elif "PLAYING" in states: verdict = "playing-no-clock"
    elif dur_ok: verdict = "parsed"
    elif states and states[-1] == "TRANSITIONING": verdict = "hung"
    else: verdict = "rejected"
    return dict(case=name, set_rc=j1.get("result"), set_http=c1, play_rc=j2.get("result"),
                play_http=c2, states="/".join(states), durs="/".join(durs),
                rel="/".join(rels), fetched=fetched, verdict=verdict)

def main():
    stamp = int(time.time())
    log = open(OUT, "w")
    log.write("ts\tcase\tset_rc\tset_http\tplay_rc\tplay_http\tstates\tdurs\trel\tfetched\tverdict\n")
    cases = [("BASE-base", "base.m4a")] + [(os.path.basename(p)[:-4], "poc/" + os.path.basename(p))
             for p in sorted(f for f in os.listdir(POCDIR) if f.endswith(".m4a"))]
    for name, path in cases:
        r = run_one(name, path, stamp)
        log.write("\t".join([time.strftime("%H:%M:%S")] + [str(r[k]) for k in
                  ("case", "set_rc", "set_http", "play_rc", "play_http", "states", "durs", "rel", "fetched", "verdict")]) + "\n")
        log.flush()
        print(f"{name:28s} fetch={r['fetched']} {r['states']:28s} {r['verdict']}", flush=True)
        time.sleep(1.5)
    alive = subprocess.run(["curl", "-s", "-m", "4", "-o", "/dev/null", "-w", "%{http_code}",
                            f"http://{IP}:1400/status/VERSION"], capture_output=True, text=True).stdout
    log.write(f"{time.strftime('%H:%M:%S')}\tBATCH_END\t-\t-\t-\t-\t{alive}\t-\t-\t-\t-\n"); log.close()
    print("BATCH_END", alive)

main()
