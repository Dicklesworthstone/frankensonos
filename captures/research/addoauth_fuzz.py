#!/usr/bin/env python3
"""AddOAuthAccountX argument-shape matrix (SystemProperties, unauthenticated)."""
import json, socket, sys, time, urllib.request

H = sys.argv[1] if len(sys.argv) > 1 else '192.168.4.202'
ARGS = ['AccountType', 'AccountToken', 'AccountKey', 'OAuthDeviceID', 'AuthorizationCode',
        'RedirectURI', 'UserIdHashCode', 'AccountTier', 'AccountUDN', 'AccountNickname']
BASE = {'AccountType': '2', 'AccountToken': 'tok', 'AccountKey': 'key', 'OAuthDeviceID': 'dev',
        'AuthorizationCode': 'code', 'RedirectURI': 'http://h/r', 'UserIdHashCode': 'uhc',
        'AccountTier': '1', 'AccountUDN': 'udn', 'AccountNickname': 'nick'}

def body(vals):
    inner = ''.join(f'<{a}>{v}</{a}>' for a, v in zip(ARGS, vals))
    return ('<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" '
            's:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body>'
            '<u:AddOAuthAccountX xmlns:u="urn:schemas-upnp-org:service:SystemProperties:1">'
            + inner + '</u:AddOAuthAccountX></s:Body></s:Envelope>').encode()

def send(payload):
    s = socket.create_connection((H, 1400), timeout=15)
    hdr = ('POST /SystemProperties/Control HTTP/1.1\r\nHost: %s\r\nContent-Type: text/xml; charset="utf-8"\r\n'
           'SOAPACTION: "urn:schemas-upnp-org:service:SystemProperties:1#AddOAuthAccountX"\r\n'
           'Content-Length: %d\r\nConnection: close\r\n\r\n' % (H, len(payload))).encode()
    t = time.time(); err = 'ok'
    try:
        s.sendall(hdr); s.sendall(payload)
    except OSError as e:
        err = type(e).__name__
    try:
        s.settimeout(15); buf = b''
        while b'\r\n\r\n' not in buf:
            c = s.recv(4096)
            if not c: break
            buf += c
        head = buf.split(b'\r\n', 1)[0].split()
        st = head[1].decode() if len(head) > 1 else '?'
        rest = b''
        while True:
            c = s.recv(4096)
            if not c: break
            rest += c
        fb = __import__('re').search(rb'<errorCode>(\d+)</errorCode>', buf + rest)
        code = fb.group(1).decode() if fb else ''
    except ConnectionResetError:
        st, code = 'RST', ''
    except socket.timeout:
        st, code = 'TIMEOUT', ''
    finally:
        try: s.close()
        except OSError: pass
    return st, code, round((time.time() - t) * 1000)

def alive():
    try:
        with urllib.request.urlopen(f'http://{H}:1400/status', timeout=3) as r:
            return str(r.status)
    except Exception as e:
        return type(e).__name__

def case(name, vals, **extra):
    p = body(vals)
    pre = alive()
    st, code, ms = send(p)
    time.sleep(0.3)
    post = alive()
    rec = {'case': name, 'len': len(p), 'status': st, 'soap': code, 'ms': ms, 'pre': pre, 'post': post}
    rec['verdict'] = 'ANOMALY' if (post != '200' or st not in ('200', '500')) else 'ok'
    if rec['verdict'] == 'ANOMALY':
        open(f'/tmp/addoauth_{name}.bin', 'wb').write(p)
    print(json.dumps(rec), flush=True)

def vals(**over):
    v = dict(BASE); v.update(over); return [v[a] for a in ARGS]

case('control', vals())
for a in ARGS:
    case(f'{a}=2048', vals(**{a: 'A' * 2048}))
case('AccountType=empty', vals(AccountType=''))
case('AccountToken=1B', vals(AccountToken='A'))
case('AccountToken=2050', vals(AccountToken='A' * 2050))
case('AccountToken=100k', vals(AccountToken='A' * 100000))
case('AccountKey=100k', vals(AccountKey='A' * 100000))
case('all-ten-long', vals(**{a: 'A' * 800 for a in ARGS}))
case('AccountType=text', vals(AccountType='abc'))
case('AccountType=-1', vals(AccountType='-1'))
case('AccountType=0x100', vals(AccountType='0x100'))
