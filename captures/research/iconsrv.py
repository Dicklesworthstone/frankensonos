import http.server, socketserver, time, os
ROOT='/tmp/icon'
class H(http.server.SimpleHTTPRequestHandler):
    def __init__(self,*a,**k): super().__init__(*a, directory=ROOT, **k)
    def log_message(self, fmt, *a):
        open('/tmp/icon_access.log','a').write("%s %s\n" % (time.strftime('%H:%M:%S'), fmt % a))
    def copyfile(self, src, dst):
        n=0
        try:
            while True:
                b=src.read(65536)
                if not b: break
                dst.write(b); n+=len(b)
        except Exception as e:
            open('/tmp/icon_access.log','a').write("%s ABORT %s %d\n"%(time.strftime('%H:%M:%S'),type(e).__name__,n)); raise
        open('/tmp/icon_access.log','a').write("%s DONE %d\n"%(time.strftime('%H:%M:%S'),n))
socketserver.TCPServer.allow_reuse_address=True
with socketserver.ThreadingTCPServer(('192.168.4.160',8270), H) as s: s.serve_forever()
