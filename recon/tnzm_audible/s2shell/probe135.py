#!/usr/bin/env python3
import subprocess, threading, http.server, socketserver, os, sys, time
IP="192.168.4.135"; HOST="192.168.4.160"; PORT=8972
SVC="urn:schemas-upnp-org:service:AVTransport:1"; CTL=f"http://{IP}:1400/MediaRenderer/AVTransport/Control"
served=[]
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        served.append(self.path)
        p=f"recon/tnzm_audible/sweep2/{self.path.split('?')[0].lstrip('/')}"
        data=open(p,"rb").read() if os.path.exists(p) else b"x"
        self.send_response(200); self.send_header("Content-Length",str(len(data))); self.end_headers(); self.wfile.write(data)
    def log_message(self,*a): pass
def soap(action,extra=""):
    body=('<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body>'
          f'<u:{action} xmlns:u="{SVC}"><InstanceID>0</InstanceID>{extra}</u:{action}></s:Body></s:Envelope>')
    r=subprocess.run(["curl","-s","-m","15","-X","POST","-H",'Content-Type: text/xml; charset="utf-8"',"-H",f'SOAPACTION: "{SVC}#{action}"',"--data-binary",body,CTL],capture_output=True,text=True)
    if "errorCode" in r.stdout:
        import re
        m=re.search(r"errorCode>(\d+)<",r.stdout); return f"ERR{m.group(1)}"
    return "200"
def didl(url,proto):
    return ('&lt;DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/"&gt;&lt;item id="-1" parentID="-1" restricted="true"&gt;&lt;dc:title&gt;t&lt;/dc:title&gt;&lt;upnp:class&gt;object.item.audioItem.musicTrack&lt;/upnp:class&gt;&lt;res protocolInfo="'+proto+'"&gt;'+url+'&lt;/res&gt;&lt;/item&gt;&lt;/DIDL-Lite&gt;')
socketserver.TCPServer.allow_reuse_address=True
t=threading.Thread(target=lambda: socketserver.TCPServer((HOST,PORT),H).serve_forever(),daemon=True); t.start()
soap("Stop")
url=f"http://{HOST}:{PORT}/H_valid_min.aa"
protos=["http-get:*:audio/audible:*","http-get:*:audio/x-audible:*","x-file-cifs:*:audio/audible:*",
        "http-get:*:audio/audible:*:*","http-get:*:application/octet-stream:*","http-get:*:audio/aac:*",
        "http-get:*:audio/mp4:*","x-sonos-http:*:audio/audible:*","http-get:*:audio/mpeg:*"]
for p in protos:
    c=soap("SetAVTransportURI",f"<CurrentURI>{url}</CurrentURI><CurrentURIMetaData>{didl(url,p)}</CurrentURIMetaData>")
    time.sleep(1.2)
    got=bool(served); served.clear()
    print(f"{p:42s} SetAV={c} fetched={got}")
