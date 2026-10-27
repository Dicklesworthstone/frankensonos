#!/usr/bin/env python3
"""Local HTTP server that answers one request with a scripted response.

Used by wgetfuzz.py to feed busybox wget (PPC, qemu-user on css) a crafted
body. Deterministic: the response bytes come from a file given on the command
line, and one connection is served per invocation unless --keep is set.

usage: resp_server.py PORT BODY_FILE [--chunked-script SCRIPT_JSON]
"""
import json
import socket
import sys
import threading


def serve_once(port, body, header=None, close_mid=False):
    s = socket.socket()
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(('127.0.0.1', port))
    s.listen(4)
    conn, _ = s.accept()
    try:
        conn.recv(65536)
        if header is not None:
            conn.sendall(header)
            if close_mid:
                conn.close()
            else:
                conn.sendall(body)
                conn.close()
        else:
            conn.sendall(body)
            conn.close()
    except OSError:
        pass
    s.close()


def main():
    port = int(sys.argv[1])
    path = sys.argv[2]
    with open(path, 'rb') as fh:
        body = fh.read()
    header = None
    if '--header-file' in sys.argv:
        with open(sys.argv[sys.argv.index('--header-file') + 1], 'rb') as fh:
            header = fh.read()
    serve_once(port, body, header, close_mid='--close-mid' in sys.argv)


if __name__ == '__main__':
    main()
