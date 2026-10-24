#!/usr/bin/env python3
"""Probe for the c25 accept-loop wedge on a player.

The anomaly: after mixed traffic the kernel listener still accepts TCP but the
daemon answers nothing on :1400/:1443 for minutes, then recovers. This drives
the shapes that were involved (held partial requests, NOTIFY bodies, /getrs)
and probes /status with a short deadline, so a stall shows up as
"connected, no answer" instead of a refused connection.

usage: wedge-probe.py <player-ip> [seconds] [held-connections]
"""
import socket
import sys
import time
import urllib.request

ip = sys.argv[1]
seconds = float(sys.argv[2]) if len(sys.argv) > 2 else 60.0
held_n = int(sys.argv[3]) if len(sys.argv) > 3 else 24

held = []
stalls = 0
probes = 0


def hold():
    try:
        s = socket.create_connection((ip, 1400), timeout=2)
        s.sendall(b"GET /status/VERSION HTTP/1.1\r\nHost: x\r\n")  # no blank line
        held.append(s)
    except OSError:
        pass


def notify(size=4096):
    body = "<e:propertyset xmlns:e='urn:schemas-upnp-org:event-1-0'><e:property><LastChange>" + "x" * size + "</LastChange></e:property></e:propertyset>"
    req = urllib.request.Request(
        "http://%s:1400/notify" % ip,
        data=body.encode(),
        headers={"Content-Type": "text/xml", "NOTIFY": "/MediaRenderer/AVTransport/Event"},
    )
    try:
        urllib.request.urlopen(req, timeout=2).read()
    except Exception:
        pass


def getrs():
    try:
        urllib.request.urlopen("http://%s:1400/status/getrs" % ip, timeout=2).read()
    except Exception:
        pass


def probe():
    global stalls, probes
    probes += 1
    t0 = time.time()
    try:
        s = socket.create_connection((ip, 1400), timeout=2)
    except OSError:
        return "refused"
    try:
        s.sendall(b"GET /status/VERSION HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        s.settimeout(2.0)
        data = s.recv(64)
        if not data:
            stalls += 1
            return "accepted-no-answer"
        return "ok %.0fms" % ((time.time() - t0) * 1000)
    except socket.timeout:
        stalls += 1
        return "accepted-timeout"
    finally:
        s.close()


start = time.time()
last = 0.0
while time.time() - start < seconds:
    if len(held) < held_n:
        hold()
    notify()
    getrs()
    if time.time() - last > 1.0:
        last = time.time()
        print("%5.1fs %-18s held=%d" % (time.time() - start, probe(), len(held)), flush=True)
    time.sleep(0.05)

for s in held:
    s.close()
print("probes=%d stalls=%d" % (probes, stalls))
sys.exit(1 if stalls else 0)
