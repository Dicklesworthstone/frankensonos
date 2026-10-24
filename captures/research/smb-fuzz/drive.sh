#!/bin/sh
cd "$(dirname "$0")"
port=1500; i=0
SMBFILE=folder.jpg /Users/jemanuel/sonos-root-local/venv/bin/python smb_state_fuzz.py 192.168.4.86 $((port)) warmup 2>/dev/null | head -1 > /dev/null

for c in "$@"; do
  port=$((port+1)); i=$((i+1))
  f=$(printf "f%02d.jpg" $(( (i % 40) + 1 )))
  SMBFILE=$f /Users/jemanuel/sonos-root-local/venv/bin/python smb_state_fuzz.py 192.168.4.86 $port "$c" 2>/dev/null | head -1
  sleep 2
done
