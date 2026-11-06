#!/bin/bash
# q57 arming services: DNS override + HTTPS responder + pf rdr. Idempotent.
PF=/usr/local/lib/q57-serve/pf-rdr-sonos.conf
Q=/usr/local/lib/q57-serve
# kill stale instances of ours only (match script paths)
pkill -f "q57_dns.py" 2>/dev/null
pkill -f "q57_https_server.py" 2>/dev/null
sleep 1
pfctl -f "$PF" >/dev/null 2>&1
pfctl -E >/dev/null 2>&1
python3 "$Q/q57_dns.py" 192.168.4.160 >>/tmp/q57-serve/dns-run.log 2>&1 &
python3 "$Q/q57_https_server.py" 8643 >>/tmp/q57-serve/server-8643.log 2>&1 &
wait
