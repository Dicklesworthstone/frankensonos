#!/usr/bin/env python3
"""Trustworthy crash signal: poll a player's /status/zp at a fixed cadence and timestamp every transition.

The point is to make "the daemon died" a fact with a time on it, not an inference from a slow or missing
answer. Every probe is classified, every transition is printed the moment it is seen, and the summary
carries the exact down/up instants plus the downtime, so a caller can attribute a crash to a specific
request instead of guessing.

usage: crash_oracle.py [--host H] [--port P] [--path PATH] [--interval S] [--duration S]
                       [--timeout S] [--wait-up S] [--json] [--quiet]

Exit status:
  0  at least one DOWN->UP recovery was observed (the daemon went away and came back)
  1  the daemon went DOWN and had not recovered when the run ended
  2  no DOWN was ever seen (nothing crashed)
  3  the daemon was not reachable at start (after --wait-up)

As a library:

    from crash_oracle import Oracle
    o = Oracle(host, port).start()
    ...drive the target...
    o.stop(); print(o.summary())

`o.events` is a list of (monotonic, wall_iso, state, detail); `o.first_down` / `o.first_up` are the
instants of the first transition in each direction, or None.
"""
import argparse
import json
import shutil
import socket
import subprocess
import sys
import threading
import time
from datetime import datetime, timezone

UP = 'UP'
DOWN = 'DOWN'


def probe_curl(host, port, path, timeout):
    """Same classification through curl. Needed where the interpreter has no local-network permission."""
    if shutil.which('curl') is None:
        return DOWN, 'no-curl'
    try:
        r = subprocess.run(
            ['curl', '-s', '-o', '/dev/null', '-m', str(timeout), '-w', '%{http_code}',
             'http://%s:%d%s' % (host, port, path)],
            capture_output=True, timeout=timeout + 2)
    except subprocess.TimeoutExpired:
        return DOWN, 'timeout'
    code = r.stdout.decode().strip()
    if r.returncode == 0 and code == '200':
        return UP, '200'
    if r.returncode == 7:
        return DOWN, 'refused'
    if r.returncode == 28:
        return DOWN, 'timeout'
    if r.returncode == 6:
        return DOWN, 'dns'
    return DOWN, 'curl:%d:%s' % (r.returncode, code or '-')


def probe_socket(host, port, path, timeout):
    """One classification. UP means a 200 status line was read; everything else is DOWN with a reason."""
    try:
        with socket.create_connection((host, port), timeout=timeout) as s:
            s.settimeout(timeout)
            s.sendall(
                ('GET %s HTTP/1.0\r\nHost: %s\r\nConnection: close\r\n\r\n' % (path, host)).encode()
            )
            buf = b''
            while b'\r\n' not in buf and len(buf) < 256:
                chunk = s.recv(256)
                if not chunk:
                    break
                buf += chunk
    except socket.timeout:
        return DOWN, 'timeout'
    except ConnectionRefusedError:
        return DOWN, 'refused'
    except ConnectionResetError:
        return DOWN, 'reset'
    except OSError as e:
        return DOWN, 'oserr:%s' % (e.errno,)
    line = buf.split(b'\r\n', 1)[0]
    if line.startswith(b'HTTP/') and b' 200' in line:
        return UP, '200'
    return DOWN, 'status:%s' % (line[:40].decode('latin1') or 'empty')


def probe(host, port, path, timeout, backend='auto'):
    """Dispatch. `auto` starts on sockets and falls back to curl if the interpreter cannot reach the LAN."""
    if backend == 'curl':
        return probe_curl(host, port, path, timeout)
    state, detail = probe_socket(host, port, path, timeout)
    if backend == 'socket' or detail != 'oserr:65':
        return state, detail
    return probe_curl(host, port, path, timeout)


class Oracle(threading.Thread):
    def __init__(self, host, port=1400, path='/status/zp', interval=0.1, timeout=1.0,
                 duration=None, on_event=None, backend='auto'):
        super().__init__(daemon=True)
        self.host, self.port, self.path = host, port, path
        self.interval, self.timeout, self.duration = interval, timeout, duration
        self.on_event = on_event
        self.backend = backend
        self.events = []
        self.first_down = self.first_up = None
        self.probes = self.ups = self.downs = 0
        self._stop = threading.Event()
        self._lock = threading.Lock()

    def run(self):
        t0 = time.monotonic()
        next_at = t0
        while not self._stop.is_set():
            state, detail = probe(self.host, self.port, self.path, self.timeout, self.backend)
            mono = time.monotonic()
            wall = datetime.now(timezone.utc).isoformat(timespec='milliseconds')
            with self._lock:
                self.probes += 1
                prev = self.events[-1][2] if self.events else None
                if state == UP:
                    self.ups += 1
                else:
                    self.downs += 1
                if state != prev:
                    self.events.append((mono, wall, state, detail))
                    if state == DOWN and self.first_down is None:
                        self.first_down = mono
                    if state == UP and self.first_down is not None and self.first_up is None:
                        self.first_up = mono
                    if self.on_event:
                        self.on_event(mono - t0, wall, state, detail)
            if self.duration is not None and mono - t0 >= self.duration:
                break
            next_at += self.interval
            delay = next_at - time.monotonic()
            if delay > 0:
                self._stop.wait(delay)
            else:
                next_at = time.monotonic()

    def stop(self):
        self._stop.set()

    def summary(self):
        with self._lock:
            down_ms = None
            if self.first_down is not None:
                end = self.first_up if self.first_up is not None else time.monotonic()
                down_ms = round((end - self.first_down) * 1000.0, 1)
            return {
                'host': self.host,
                'port': self.port,
                'probes': self.probes,
                'up': self.ups,
                'down': self.downs,
                'first_down': self.first_down,
                'first_up': self.first_up,
                'downtime_ms': down_ms,
                'recovered': self.first_up is not None,
                'events': [
                    {'t': round(m, 3), 'wall': w, 'state': s, 'detail': d}
                    for m, w, s, d in self.events
                ],
            }


def main():
    ap = argparse.ArgumentParser(description='Timestamped crash oracle for a Sonos player daemon.')
    ap.add_argument('--host', default='192.168.4.202')
    ap.add_argument('--port', type=int, default=1400)
    ap.add_argument('--path', default='/status/zp')
    ap.add_argument('--interval', type=float, default=0.1)
    ap.add_argument('--timeout', type=float, default=1.0)
    ap.add_argument('--duration', type=float, default=30.0)
    ap.add_argument('--wait-up', type=float, default=5.0,
                    help='seconds to wait for the daemon to answer before starting (0 disables)')
    ap.add_argument('--backend', choices=('auto', 'socket', 'curl'), default='auto',
                    help='auto falls back to curl when the interpreter cannot reach the LAN (EHOSTUNREACH)')
    ap.add_argument('--json', action='store_true', help='emit the summary as one JSON object')
    ap.add_argument('--quiet', action='store_true', help='only print transitions and the summary')
    a = ap.parse_args()

    if a.wait_up:
        deadline = time.monotonic() + a.wait_up
        while time.monotonic() < deadline:
            if probe(a.host, a.port, a.path, a.timeout, a.backend)[0] == UP:
                break
            time.sleep(a.interval)
        else:
            print('daemon not reachable at %s:%d%s' % (a.host, a.port, a.path), file=sys.stderr)
            return 3

    def on_event(t, wall, state, detail):
        print('%8.3f  %s  %-4s  %s' % (t, wall, state, detail), flush=True)

    o = Oracle(a.host, a.port, a.path, a.interval, a.timeout, a.duration, on_event, a.backend)
    t0 = time.monotonic()
    o.start()
    try:
        while o.is_alive():
            o.join(0.5)
    except KeyboardInterrupt:
        o.stop()
        o.join(2.0)

    s = o.summary()
    s['elapsed_s'] = round(time.monotonic() - t0, 3)
    if a.json:
        print(json.dumps(s))
    elif not a.quiet:
        print(json.dumps(s))
    if s['first_down'] is None:
        return 2
    return 0 if s['recovered'] else 1


if __name__ == '__main__':
    sys.exit(main())
