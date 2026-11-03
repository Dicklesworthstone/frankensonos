#!/usr/bin/env python3
"""jc7 T1 regression — the :1443 proxy escapes XML, the device's own parser does not tolerate raw
metacharacters. Same value down both paths, one verdict per path:
  proxy  -> value survives the SOAP body, device decodes it, read-back identical  (200)
  direct -> identical raw value in a hand-built body is rejected by the device parser (402)
The 402 is the contrast, not a fault. Exit 0 only when both verdicts hold."""
import json, subprocess, sys, time

IP = "192.168.4.86"
U = "RINCON_7828CA9221DC01400"
API = f"https://{IP}:1443/api/v1/players/{U}/upnpSystemProperties"
V = sys.argv[1] if len(sys.argv) > 1 else 'a<b>c&d"e'
K = f"jc7esc_{int(time.time())}"

def proxy(method, args):
    body = json.dumps({"method": method, "input": [{"name": n, "value": v} for n, v in args]})
    open("/tmp/jc7_e.json", "w").write(body)
    r = subprocess.run(["curl", "-sk", "-m", "20", "--http1.1", "-X", "POST",
                        "-H", "X-Sonos-Api-Key: x", "-H", "Content-Type: application/json",
                        "--data-binary", "@/tmp/jc7_e.json", "-w", "%{http_code}", API],
                       capture_output=True, text=True)
    return json.loads(r.stdout[:-3] or "{}"), r.stdout[-3:]

print(f"-- proxy SetString value=[{V}]")
j, code = proxy("SetString", [("VariableName", K), ("StringValue", V)])
print(f"   rc={j.get('result')} http={code}")
print("-- proxy GetString read-back")
j, code = proxy("GetString", [("VariableName", K)])
rb = next((p["value"] for p in j.get("parameters", []) if p["name"] == "StringValue"), "")
print(f"   http={code} readback=[{rb}]")
ok_proxy = (rb == V)
print("   VERDICT identical: proxy escaped, device decoded, no injection" if ok_proxy
      else "   VERDICT DIFFERS: escaping regressed")

print("-- direct SOAP, same raw value, unescaped")
body = ('<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"'
        ' s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:SetString'
        ' xmlns:u="urn:schemas-upnp-org:service:SystemProperties:1"><VariableName>'
        f'{K}</VariableName><StringValue>{V}</StringValue></u:SetString></s:Body></s:Envelope>')
code = subprocess.run(["curl", "-s", "-m", "20", "-o", "/tmp/jc7_e_d.out", "-w", "%{http_code}",
                       "-X", "POST",
                       '-H', 'SOAPACTION: "urn:schemas-upnp-org:service:SystemProperties:1#SetString"',
                       "-H", 'Content-Type: text/xml; charset="utf-8"',
                       "--data-binary", body, f"http://{IP}:1400/SystemProperties/Control"],
                      capture_output=True, text=True).stdout
err = ""
out = open("/tmp/jc7_e_d.out").read()
if "<errorCode>" in out:
    err = out.split("<errorCode>")[1].split("<")[0]
print(f"{code} errorCode={err}")
ok_direct = (code == "500" and err == "402")
sys.exit(0 if (ok_proxy and ok_direct) else 1)
