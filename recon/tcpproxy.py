#!/usr/bin/env python3
"""Dumb TCP proxy that hexdumps both directions. usage: tcpproxy.py LISTEN TARGET_PORT [LOG]"""
import socket, sys, threading, time

L = int(sys.argv[1]); T = int(sys.argv[2]); LOG = sys.argv[3] if len(sys.argv) > 3 else '/tmp/wire.log'
f = open(LOG, 'ab', buffering=0)
lock = threading.Lock()


def dump(tag, data):
    with lock:
        f.write(b'%s %d %s\n' % (tag, len(data), data.hex().encode()))


def pipe(a, b, tag):
    try:
        while True:
            d = a.recv(65536)
            if not d:
                break
            dump(tag, d)
            b.sendall(d)
    except Exception:
        pass
    finally:
        try: b.shutdown(socket.SHUT_WR)
        except Exception: pass


srv = socket.socket(); srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(('0.0.0.0', L)); srv.listen(8)
print('proxy', L, '->', T, flush=True)
while True:
    c, _ = srv.accept()
    u = socket.create_connection(('127.0.0.1', T))
    threading.Thread(target=pipe, args=(c, u, b'C>'), daemon=True).start()
    threading.Thread(target=pipe, args=(u, c, b'S>'), daemon=True).start()
