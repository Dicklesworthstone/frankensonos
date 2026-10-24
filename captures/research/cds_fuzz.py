#!/usr/bin/env python3
"""ContentDirectory Browse/Search argument-shape matrix (S1 .202)."""
import json, re, socket, sys, time, urllib.request

H = sys.argv[1] if len(sys.argv) > 1 else '192.168.4.202'
NS = 'urn:schemas-upnp-org:service:ContentDirectory:1'

def env(action, inner):
    return ('<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" '
            's:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body>'
            f'<u:{action} xmlns:u="{NS}">' + inner + f'</u:{action}></s:Body></s:Envelope>').encode()

def browse(oid='SQ:0', flag='BrowseDirectChildren', filt='*', start='0', count='10', sort=''):
    return env('Browse', f'<ObjectID>{oid}</ObjectID><BrowseFlag>{flag}</BrowseFlag><Filter>{filt}</Filter>'
                        f'<StartingIndex>{start}</StartingIndex><RequestedCount>{count}</RequestedCount>'
                        f'<SortCriteria>{sort}</SortCriteria>')

def search(crit='dc:title contains x', start='0', count='10', sort='', filt='*'):
    return env('Search', f'<ContainerID>SQ:0</ContainerID><SearchCriteria>{crit}</SearchCriteria>'
                        f'<StartingIndex>{start}</StartingIndex><RequestedCount>{count}</RequestedCount>'
                        f'<SortCriteria>{sort}</SortCriteria><Filter>{filt}</Filter>')

def send(payload):
    s = socket.create_connection((H, 1400), timeout=20)
    hdr = ('POST /MediaServer/ContentDirectory/Control HTTP/1.1\r\nHost: %s\r\n'
           'Content-Type: text/xml; charset="utf-8"\r\nSOAPACTION: "%s#%s"\r\n'
           'Content-Length: %d\r\nConnection: close\r\n\r\n' % (H, NS, 'Browse' if b'<u:Browse' in payload else 'Search', len(payload))).encode()
    t = time.time(); err = 'ok'
    try:
        s.sendall(hdr); s.sendall(payload)
    except OSError as e:
        err = type(e).__name__
    st = '?'; size = 0; code = ''
    try:
        buf = b''
        while b'\r\n\r\n' not in buf:
            c = s.recv(4096)
            if not c: break
            buf += c
        head = buf.split(b'\r\n', 1)[0].split()
        st = head[1].decode() if len(head) > 1 else '?'
        rest = buf.split(b'\r\n\r\n', 1)[1] if b'\r\n\r\n' in buf else b''
        while True:
            c = s.recv(65536)
            if not c: break
            rest += c
        size = len(rest)
        m = re.search(rb'<errorCode>(\d+)</errorCode>', rest)
        code = m.group(1).decode() if m else ''
    except ConnectionResetError:
        st = 'RST'
    except socket.timeout:
        st = 'TIMEOUT'
    finally:
        try: s.close()
        except OSError: pass
    return st, code, size, err, round((time.time() - t) * 1000)

def info():
    try:
        with urllib.request.urlopen(f'http://{H}:1400/info', timeout=3) as r: return str(r.status)
    except Exception as e: return type(e).__name__

def case(name, payload):
    pre = info()
    st, code, size, err, ms = send(payload)
    time.sleep(0.3); post = info()
    rec = {'case': name, 'len': len(payload), 'status': st, 'soap': code, 'body': size, 'ms': ms, 'pre': pre, 'post': post}
    rec['verdict'] = 'ANOMALY' if (post != '200' or st not in ('200', '500')) else 'ok'
    if rec['verdict'] == 'ANOMALY' or st == '200': open(f'/tmp/cds_{name}.bin', 'wb').write(payload)
    print(json.dumps(rec), flush=True)

big = lambda n, c='A': c * n
cases = [
    ('browse-control', browse()),
    ('oid-SQ-colon', browse(oid='SQ:')),
    ('oid-A-colon', browse(oid='A:')),
    ('oid-empty', browse(oid='')),
    ('oid-4k', browse(oid=big(4096))),
    ('sort-plus', browse(sort='+dc:title')),
    ('sort-minus', browse(sort='-dc:title')),
    ('sort-100', browse(sort=','.join(['+dc:title'] * 100))),
    ('sort-4k', browse(sort=big(4096, 's'))),
    ('filter-1000', browse(filt=','.join('dc:f%d' % i for i in range(1000)))),
    ('filter-4k', browse(filt=big(4096, 'f'))),
    ('filter-garbage', browse(filt='<<>>,,;;')),
    ('start-max', browse(start='2147483647')),
    ('start-neg', browse(start='-1')),
    ('count-max', browse(count='2147483647')),
    ('count-neg', browse(count='-1')),
    ('search-control', search()),
    ('search-empty-crit', search(crit='')),
    ('search-crit-4k', search(crit=big(4096))),
    ('search-sort-100', search(sort=','.join(['+dc:title'] * 100))),
]
for n, p in cases: case(n, p)
