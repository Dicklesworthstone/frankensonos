#!/usr/bin/env python3
"""Accept-and-hold TCP listener that counts connections (frankensonos-9sd).

Point a transport record at this port to reproduce "the fetch cannot complete"
while still counting how often the device re-attempts: each connect line is one
walk re-entry. Nothing is sent back, so the client must time out.

    python3 h003_hold.py [port]
"""
import socket
import sys
import threading
import time

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 8901
count = 0
lock = threading.Lock()
held = []


def handle(c, addr, n):
    global count
    with lock:
        count = n
    print("%s conn#%d from %s:%d" % (time.strftime("%H:%M:%S"), n, *addr[:2]),
          flush=True)
    held.append(c)


srv = socket.socket()
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("0.0.0.0", PORT))
srv.listen(64)
print("listening on %d" % PORT, flush=True)
n = 0
while True:
    c, addr = srv.accept()
    n += 1
    threading.Thread(target=handle, args=(c, addr, n), daemon=True).start()
