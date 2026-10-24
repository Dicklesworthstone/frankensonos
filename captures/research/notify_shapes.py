#!/usr/bin/env python3
"""LastChange body-shape matrix for /notify on an S2 player (GENA headers, single request each)."""
import json, socket, sys, time, urllib.request, subprocess

H = sys.argv[1] if len(sys.argv) > 1 else '192.168.4.86'
HEAD = ('NOTIFY /notify HTTP/1.1\r\nHost: %s\r\nContent-Type: text/xml\r\nNT: upnp:event\r\n'
        'NTS: upnp:propchange\r\nSID: uuid:test\r\nSEQ: %d\r\nContent-Length: %d\r\nConnection: close\r\n\r\n')

def wrap(inner):
    return (b'<?xml version="1.0"?><e:propertyset xmlns:e="urn:schemas-upnp-org:event-1-0"><e:property>'
            b'<LastChange><Event xmlns="urn:schemas-upnp-org:metadata-1-0/AVT/"><InstanceID val="0">'
            + inner + b'</InstanceID></Event></LastChange></e:property></e:propertyset>')

def prop(nbytes):
    return b'<X val="' + b'A' * nbytes + b'"/>'

def nest(depth):
    return b'<A>' * depth + b'<TransportState val="PLAYING"/>' + b'</A>' * depth

def siblings(n, vlen):
    return b''.join(b'<K%d val="%s"/>' % (i, b'x' * vlen) for i in range(n))

CASES = [
    ('control',      lambda: wrap(b'<TransportState val="PLAYING"/>')),
    ('prop-2k',      lambda: wrap(prop(2000))),
    ('prop-8k',      lambda: wrap(prop(8000))),
    ('prop-64k',     lambda: wrap(prop(64000))),
    ('siblings-500', lambda: wrap(siblings(500, 2))),
    ('siblings-500-128B', lambda: wrap(siblings(500, 128))),
    ('nest-2000',    lambda: wrap(nest(2000))),
    ('attr-300k',    lambda: wrap(prop(300000))),
    ('attr-300k-2',  lambda: wrap(prop(300000))),
    ('nest-2000-2',  lambda: wrap(nest(2000))),
    ('unclosed-64k', lambda: wrap(prop(64000))[:-len(b'</InstanceID></Event></LastChange></e:property></e:propertyset>')]),
    ('nul-64k',      lambda: wrap(b'<X val="' + b'A' * 32768 + b'\x00' * 3 + b'A' * 32768 + b'"/>')),
]

def info():
    try:
        with urllib.request.urlopen(f'http://{H}:1400/info', timeout=3) as r:
            return str(r.status)
    except Exception as e:
        return type(e).__name__

def send(body, seq):
    s = socket.create_connection((H, 1400), timeout=10)
    s.sendall((HEAD % (H, seq, len(body))).encode())
    t = time.time()
    err = 'ok'; sent = 0
    try:
        for i2 in range(0, len(body), 32768):
            s.sendall(body[i2:i2+32768]); sent += len(body[i2:i2+32768])
    except OSError as e:
        err = type(e).__name__
    try:
        s.settimeout(10); r = s.recv(64); st = r.split(b' ')[1].decode() if r else 'empty'
    except Exception as e:
        st = type(e).__name__
    finally:
        try: s.close()
        except OSError: pass
    return st, err, round((time.time() - t) * 1000), body, sent

for i, (name, mk) in enumerate(CASES):
    body = mk()
    pre = info()
    st, err, ms, b, sent = send(body, i)
    time.sleep(0.4)
    post = info()
    rec = {'case': name, 'len': len(b), 'status': st, 'write': err, 'ms': ms, 'sent': sent, 'pre': pre, 'post': post}
    if st not in ('200',) or post != '200':
        rec['verdict'] = 'ANOMALY'
        open(f'/tmp/notify_{name}.bin', 'wb').write(b)
    else:
        rec['verdict'] = 'ok'
    print(json.dumps(rec), flush=True)
