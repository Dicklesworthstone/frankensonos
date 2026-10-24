#!/usr/bin/env python3
"""Bound the /getrs?id=N setting-table index. Raw sockets so RST is distinguishable from close."""
import json, random, socket, sys, time, urllib.request

H = sys.argv[1] if len(sys.argv) > 1 else '192.168.4.202'
REQ = b'GET /getrs?id=%s HTTP/1.1\r\nHost: %s\r\nConnection: close\r\n\r\n'

def ask(q, timeout=5.0):
    t = time.time()
    try:
        s = socket.create_connection((H, 1400), timeout=timeout)
    except Exception as e:
        return 'CONNECT-' + type(e).__name__, 0, b''
    try:
        s.sendall(REQ % (str(q).encode(), H.encode()))
        s.settimeout(timeout)
        buf = b''
        while b'\r\n\r\n' not in buf:
            c = s.recv(4096)
            if not c: break
            buf += c
        head = buf.split(b'\r\n', 1)[0]
        parts = head.split()
        return (parts[1].decode() if len(parts) > 1 else '?'), round((time.time() - t) * 1000), buf
    except ConnectionResetError:
        return 'RST', round((time.time() - t) * 1000), b''
    except socket.timeout:
        return 'TIMEOUT', round((time.time() - t) * 1000), b''
    except Exception as e:
        return type(e).__name__, round((time.time() - t) * 1000), b''
    finally:
        try: s.close()
        except OSError: pass

def alive():
    try:
        with urllib.request.urlopen(f'http://{H}:1400/status', timeout=3) as r:
            return str(r.status)
    except Exception as e:
        return type(e).__name__

def body_len(buf):
    return len(buf.split(b'\r\n\r\n', 1)[1]) if b'\r\n\r\n' in buf else 0

def main():
    n = 0
    def one(q, label=''):
        nonlocal n
        st, ms, buf = ask(q)
        n += 1
        rec = {'id': q, 'status': st, 'ms': ms, 'bytes': body_len(buf)}
        if st in ('RST', 'TIMEOUT') or st.startswith('CONNECT-'):
            rec['alive'] = alive()
            rec['verdict'] = 'ANOMALY'
        print(json.dumps(rec), flush=True)
        if n % 200 == 0:
            print(json.dumps({'gap': n, 'alive': alive()}), flush=True)
            time.sleep(20)
        return rec

    for q in (-1, 0, 1, 100, 65535, ''):
        one(q)
    for q in (0x7fffffff, 0xffffffff, 0x80000000, 32768, 65536, 131072):
        one(q)
    for q in range(16384, 16601):
        one(q)
    random.seed(7)
    for _ in range(2000):
        one(random.randrange(0, 1000000))
    print(json.dumps({'done': n, 'alive': alive()}), flush=True)

main()
