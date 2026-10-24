#!/usr/bin/env python3
"""x-file-cifs:// URL shapes for /getaa (parser runs before any SMB traffic)."""
import json, socket, sys, time, urllib.parse, urllib.request

H = sys.argv[1] if len(sys.argv) > 1 else '192.168.4.86'
BIG = 'A' * 4096
CASES = [
    ('control',           'x-file-cifs://u:p@192.168.4.170:1445/SHARE/folder.jpg'),
    ('user-4k',           'x-file-cifs://' + BIG + ':p@192.168.4.170:1445/SHARE/folder.jpg'),
    ('pass-4k',           'x-file-cifs://u:' + BIG + '@192.168.4.170:1445/SHARE/folder.jpg'),
    ('host-4k',           'x-file-cifs://u:p@' + BIG + ':1446/S/x.jpg'),
    ('share-4k',          'x-file-cifs://u:p@192.168.4.170:1445/' + BIG + '/x.jpg'),
    ('path-4k',           'x-file-cifs://u:p@192.168.4.170:1445/S/' + BIG + '.jpg'),
    ('user-empty',        'x-file-cifs://:p@192.168.4.170:1445/SHARE/folder.jpg'),
    ('pass-empty',        'x-file-cifs://u:@192.168.4.170:1445/SHARE/folder.jpg'),
    ('both-empty',        'x-file-cifs://@192.168.4.170:1445/SHARE/folder.jpg'),
    ('at-100',            'x-file-cifs://' + '@' * 100 + 'h/S/x.jpg'),
    ('ipv6-literal',      'x-file-cifs://u:p@[::1]:1445/S/x.jpg'),
    ('pct-escapes',       'x-file-cifs://u:p@192.168.4.170:1445/S/%2e%2e%2f%00%zz%2F.jpg'),
    ('embedded-question', 'x-file-cifs://u:p@192.168.4.170:1445/SHARE/folder.jpg?a=b&c=d'),
    ('no-share',          'x-file-cifs://u:p@192.168.4.170:1445'),
    ('port-huge',         'x-file-cifs://u:p@192.168.4.160:99999999/S/x.jpg'),
]

def info():
    try:
        with urllib.request.urlopen(f'http://{H}:1400/info', timeout=3) as r: return str(r.status)
    except Exception as e: return type(e).__name__

def get(u, timeout=6):
    q = f'http://{H}:1400/getaa?u={urllib.parse.quote(u, safe="")}&w=1&h=1'
    t = time.time()
    try:
        with urllib.request.urlopen(q, timeout=timeout) as r:
            data = r.read(); return str(r.status), len(data), round((time.time() - t) * 1000)
    except urllib.error.HTTPError as e:
        return f'http{e.code}', 0, round((time.time() - t) * 1000)
    except Exception as e:
        return type(e).__name__, 0, round((time.time() - t) * 1000)

for name, u in CASES:
    pre = info()
    st, size, ms = get(u)
    time.sleep(0.3)
    post = info()
    rec = {'case': name, 'url_len': len(u), 'status': st, 'bytes': size, 'ms': ms, 'pre': pre, 'post': post}
    rec['verdict'] = 'ANOMALY' if post != '200' else 'ok'
    print(json.dumps(rec), flush=True)
