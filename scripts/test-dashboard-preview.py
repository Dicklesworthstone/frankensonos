#!/usr/bin/env python3
"""Check the real, loopback-only design-preview HTTP bundle without a browser."""
from __future__ import annotations
import http.client
import importlib.util
from pathlib import Path
import threading

path = Path(__file__).with_name('dashboard-preview.py')
spec = importlib.util.spec_from_file_location('dashboard_preview', path)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
server = module.ThreadingHTTPServer(('127.0.0.1', 0), module.Preview)
thread = threading.Thread(target=server.serve_forever, daemon=True)
thread.start()
try:
    def request(method, path):
        client = http.client.HTTPConnection(*server.server_address, timeout=5)
        try:
            client.request(method, path)
            response = client.getresponse()
            return response.status, dict(response.getheaders()), response.read()
        finally:
            client.close()

    status, headers, body = request('GET', '/dashboard?demo=1')
    assert status == 200 and b'data-preview="true"' in body
    assert headers['Content-Security-Policy'] == module.CSP
    assert headers['X-Frame-Options'] == 'DENY'
    assert headers['Cache-Control'] == 'no-store'
    status, headers, body = request('GET', '/dashboard.js')
    assert status == 200
    assert body == b'\n'.join((module.ASSETS / name).read_bytes() for name in module.SCRIPTS)
    assert body.index(b'const DashboardCore') < body.index(b'const DashboardSession')
    assert request('GET', '/dashboard.css')[0] == 200
    assert request('GET', '/zones')[0] == 404
    assert request('POST', '/play')[0] == 405
    status, headers, _ = request('GET', '/dashboard')
    assert status == 302 and headers['Location'] == '/dashboard?demo=1'
    print('PASS: real loopback preview HTML/CSS/bundle, explicit preview, CSP, redirect, missing speaker API, and refused POST.')
finally:
    server.shutdown()
    server.server_close()
    thread.join(timeout=5)
