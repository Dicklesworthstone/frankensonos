#!/usr/bin/env python3
"""Malformed HTTP *response* server for the Sonos daemon's own HTTP client.

The daemon is made to call us two ways:
  - GENA: SUBSCRIBE with CALLBACK -> it sends NOTIFY to /<shape>/... (pre-auth)
  - updater: BeginSoftwareUpdate with a .upd URL under /<shape>/ (runs as root)

Path carries the shape: /<shape>/<anything>. Shapes are listed in SHAPES below.
Every request is logged with the shape, bytes written and how the peer ended.
"""
import socketserver, time, json, sys

LOG = '/tmp/hc_fuzz.log'
BODY = b'<?xml version="1.0"?><e:propertyset xmlns:e="urn:schemas-upnp-org:event-1-0"><e:property><LastChange><Event xmlns="urn:schemas-upnp-org:metadata-1-0/AVT/"><InstanceID val="0"><TransportState val="PLAYING"/></InstanceID></Event></LastChange></e:property></e:propertyset>'
UPD = open('/Users/jemanuel/projects/frankensonos/captures/research/cve-lane/poison/A-cert0-pss528.upd', 'rb').read()

def log(**kw):
    kw['t'] = time.strftime('%H:%M:%S')
    open(LOG, 'a').write(json.dumps(kw) + '\n')

class H(socketserver.BaseRequestHandler):
    def handle(self):
        s = self.request
        s.settimeout(15)
        data = b''
        try:
            while b'\r\n\r\n' not in data and len(data) < 65536:
                c = s.recv(4096)
                if not c: break
                data += c
        except OSError:
            pass
        line = data.split(b'\r\n', 1)[0].decode('latin-1', 'replace')
        try: path = line.split()[1]
        except IndexError: path = '/'
        shape = path.strip('/').split('/')[0] or 'control'
        msg, n = self.reply(shape, path)
        log(shape=shape, path=path[:80], write=msg, bytes=n)
        try: s.close()
        except OSError: pass

    def send(self, s, blob, count_bytes=True):
        try:
            s.sendall(blob)
        except OSError:
            return 0
        return len(blob)

    def reply(self, shape, path):
        s = self.request
        sent = 0
        def w(b):
            nonlocal sent
            sent += self.send(s, b)

        if shape == 'control':
            w(b'HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nContent-Length: %d\r\nConnection: close\r\n\r\n' % len(BODY)); w(BODY)
            return 'ok', sent
        if shape == 'status8k':
            w(b'HTTP/1.1 200 ' + b'A' * 8180 + b'\r\nContent-Length: 0\r\nConnection: close\r\n\r\n')
            return 'ok', sent
        if shape == 'hdr1000':
            h = b''.join(b'X-Filler-%04d: v\r\n' % i for i in range(1000))
            w(b'HTTP/1.1 200 OK\r\nContent-Length: 0\r\n' + h + b'Connection: close\r\n\r\n')
            return 'ok', sent
        if shape == 'hline4k':
            w(b'HTTP/1.1 200 OK\r\nX-Long: ' + b'B' * 4096 + b'\r\nContent-Length: 0\r\nConnection: close\r\n\r\n')
            return 'ok', sent
        if shape == 'nul':
            w(b'HTTP/1.1 200 OK\r\nX-Nul: ab\x00cd\x00\x00ef\r\nContent-Length: 0\r\nConnection: close\r\n\r\n')
            return 'ok', sent
        if shape == 'fold':
            w(b'HTTP/1.1 200 OK\r\nX-Folded: part1\r\n part2\r\n\tpart3\r\nContent-Length: 0\r\nConnection: close\r\n\r\n')
            return 'ok', sent
        if shape == 'negcl':
            w(b'HTTP/1.1 200 OK\r\nContent-Length: -5\r\nConnection: close\r\n\r\n')
            return 'ok', sent
        if shape == 'hugecl':
            w(b'HTTP/1.1 200 OK\r\nContent-Length: 4294967295\r\nConnection: close\r\n\r\n'); w(BODY)
            return 'ok', sent
        if shape == 'badchunk':
            w(b'HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\nZZZ\r\n')
            return 'ok', sent
        if shape == 'redirectloop':
            host = self.server.server_address[0]; port = self.server.server_address[1]
            w(b'HTTP/1.1 302 Found\r\nLocation: http://%s:%d/redirectloop/%s\r\nContent-Length: 0\r\nConnection: close\r\n\r\n'
              % (host.encode(), port, path.strip('/').split('/', 1)[-1].encode()))
            return 'ok', sent
        if shape == 'hdr1m':
            w(b'HTTP/1.1 200 OK\r\n'); 
            for i in range(1024):
                w(b'X-Fill-%d: ' % i + b'C' * 1000 + b'\r\n')
            w(b'Content-Length: 0\r\nConnection: close\r\n\r\n')
            return 'ok', sent

        if shape == 'noheaders':
            w(b'HTTP/1.1 200 OK\r\n\r\n'); return 'ok', sent
        if shape == 'truncstatus':
            w(b'HTTP/1.1 200'); return 'ok', sent
        if shape == 'garbagestatus':
            w(b'NOTHTTP 200 OK\r\nContent-Length: 0\r\n\r\n'); return 'ok', sent
        if shape == 'dupecl':
            w(b'HTTP/1.1 200 OK\r\nContent-Length: 0\r\nContent-Length: 99\r\n\r\n' + BODY); return 'ok', sent
        if shape == 'clpluschunk':
            w(b'HTTP/1.1 200 OK\r\nContent-Length: 12\r\nTransfer-Encoding: chunked\r\n\r\n%X\r\n' % len(BODY) + BODY + b'\r\n0\r\n\r\n'); return 'ok', sent
        if shape == 'chunkhuge':
            w(b'HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nFFFFFFFF\r\n'); w(BODY[:64]); return 'ok', sent
        if shape == 'chunkext':
            w(b'HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n%X;foo=bar\r\n' % len(BODY) + BODY + b'\r\n0\r\n\r\n'); return 'ok', sent
        if shape == 'chunk0thenbody':
            w(b'HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n' + BODY); return 'ok', sent
        if shape == 'trailers':
            w(b'HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nTrailer: X-T\r\n\r\n%X\r\n' % len(BODY) + BODY + b'\r\n0\r\nX-T: yes\r\n\r\n'); return 'ok', sent
        if shape == 'lfonly':
            w(b'HTTP/1.1 200 OK\nContent-Length: 0\nConnection: close\n\n'); return 'ok', sent
        if shape == 'nocolon':
            w(b'HTTP/1.1 200 OK\r\nBogusHeaderNoColon\r\nContent-Length: 0\r\n\r\n'); return 'ok', sent
        if shape == 'collapse':
            w(b'HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n'); return 'ok', sent   # closed immediately
        if shape == 'midclose':
            w(b'HTTP/1.1 200 OK\r\nContent-Length: 5000\r\n\r\n'); w(BODY); return 'ok', sent
        if shape == 'hang':
            w(b'HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n'); 
            import time as _t; _t.sleep(9); return 'ok', sent
        if shape == 'slow':
            w(b'HTTP/1.1 200 OK\r\nContent-Length: %d\r\n\r\n' % len(BODY))
            import time as _t
            for i in range(0, len(BODY), 16):
                if self.send(s, BODY[i:i+16]) == 0: return 'abort', sent
                sent += min(16, len(BODY)-i); _t.sleep(0.15)
            return 'ok', sent
        if shape == 'e412':
            w(b'HTTP/1.1 412 Precondition Failed\r\nContent-Length: 0\r\n\r\n'); return 'ok', sent
        if shape == 'e500':
            w(b'HTTP/1.1 500 Oops\r\nContent-Length: 0\r\n\r\n'); return 'ok', sent
        if shape == 'relredirect':
            w(b'HTTP/1.1 302 Found\r\nLocation: /control/avt\r\nContent-Length: 0\r\n\r\n'); return 'ok', sent
        if shape == 'refused':
            w(b'HTTP/1.1 302 Found\r\nLocation: http://192.168.4.160:9/control/avt\r\nContent-Length: 0\r\n\r\n'); return 'ok', sent
        if shape == 'upd-ok':
            w(b'HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: %d\r\nConnection: close\r\n\r\n' % len(UPD))
            for i in range(0, len(UPD), 65536):
                if self.send(s, UPD[i:i+65536]) == 0:
                    return 'abort', sent
                sent += len(UPD[i:i+65536])
            return 'ok', sent
        if shape == 'upd-badchunk':
            w(b'HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n')
            n = 0
            for i in range(0, min(len(UPD), 600000), 65536):
                chunk = UPD[i:i+65536]; n += 1
                w(('%X\r\n' % len(chunk)).encode() + chunk + b'\r\n')
            w(b'ZZZZ\r\n')          # bad chunk size mid-stream
            return 'ok', sent
        if shape == 'upd-shortcl':
            w(b'HTTP/1.1 200 OK\r\nContent-Length: %d\r\nConnection: close\r\n\r\n' % (len(UPD) - 100000))
            for i in range(0, len(UPD), 65536):
                if self.send(s, UPD[i:i+65536]) == 0: return 'abort', sent
                sent += len(UPD[i:i+65536])
            return 'ok', sent
        if shape == 'upd-hdr1m':
            w(b'HTTP/1.1 200 OK\r\n')
            for i in range(1024): w(b'X-Fill-%d: ' % i + b'C' * 1000 + b'\r\n')
            w(b'Content-Length: %d\r\nConnection: close\r\n\r\n' % len(UPD))
            for i in range(0, min(len(UPD), 300000), 65536):
                if self.send(s, UPD[i:i+65536]) == 0: return 'abort', sent
                sent += len(UPD[i:i+65536])
            return 'ok', sent
        if shape == 'upd-redirectloop':
            host, port = self.server.server_address
            w(b'HTTP/1.1 302 Found\r\nLocation: http://%s:%d/upd-redirectloop/x.upd\r\nContent-Length: 0\r\nConnection: close\r\n\r\n'
              % (host.encode(), port))
            return 'ok', sent
        w(b'HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n')
        return 'unknown-shape', sent

class S(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True

if __name__ == '__main__':
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8899
    S(('192.168.4.160', port), H).serve_forever()
