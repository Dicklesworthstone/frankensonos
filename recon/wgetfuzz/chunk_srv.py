#!/usr/bin/env python3
"""srv.py - serve one canned HTTP response, byte-exact.

The response is read from a file so a case is a file on disk (reproducible,
diffable). Listens on 127.0.0.1:<port>, prints READY once listening, then
serves the same bytes to each accepted connection.

  ./srv.py --port 8899 --file case.http                 # one connection
  ./srv.py --port 8899 --file case.http --serve 40      # redirect chains
  ./srv.py --port 8899 --file case.http --repeat 0      # stream forever
"""
import argparse
import socket
import sys


def serve_one(s, body, a):
    c, _ = s.accept()
    req = b""
    while b"\r\n\r\n" not in req:
        d = c.recv(4096)
        if not d:
            break
        req += d
    print("REQ " + req.split(b"\r\n", 1)[0].decode("latin-1"), flush=True)
    if a.repeat == 1:
        c.sendall(body)
    else:
        n = 0
        try:
            while a.repeat == 0 or n < a.repeat:
                c.sendall(body)
                n += 1
        except OSError:
            pass
    if a.keep:
        try:
            c.recv(1)
        except OSError:
            pass
    c.close()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=8899)
    ap.add_argument("--file", required=True, nargs="+",
                    help="response file(s); the last one repeats")
    ap.add_argument("--keep", action="store_true", help="do not close after the write")
    ap.add_argument("--serve", type=int, default=1,
                    help="accept this many connections, serving the same body each time")
    ap.add_argument("--repeat", type=int, default=1,
                    help="send the file this many times (0 = until the client goes away)")
    a = ap.parse_args()
    bodies = [open(f, "rb").read() for f in a.file]
    s = socket.socket()
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("127.0.0.1", a.port))
    s.listen(8)
    print("READY", flush=True)
    for i in range(a.serve):
        serve_one(s, bodies[min(i, len(bodies) - 1)], a)
    s.close()


if __name__ == "__main__":
    sys.exit(main())
