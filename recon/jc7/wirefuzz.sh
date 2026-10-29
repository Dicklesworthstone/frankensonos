#!/usr/bin/env bash
# jc7 wire fuzz — malformed JSON against the pre-auth :1443 muse API on .86.
# Gate facts (recon/jc7/findings2.md §7): X-Sonos-Api-Key is a presence check; the
# route shape is /api/v1/players/<playerId>/<...>; the method bitmask is enforced.
# Every case is one request, 3 s apart, with a liveness check after each batch.
set -u
IP=${IP:-192.168.4.86}
U=${U:-RINCON_7828CA9221DC01400}
OUT=${OUT:-recon/jc7/wirefuzz.tsv}
BODY=/tmp/jc7_body.json
SPACING=${SPACING:-3}

[ -f "$OUT" ] || printf 'ts\tbatch\tcase\tmethod\tpath\tstatus\tbytes\terrorCode\tnote\n' > "$OUT"

alive() { curl -s -m 4 -o /dev/null -w '%{http_code}' "http://$IP:1400/status/VERSION"; }

# fire <batch> <case> <method> <path> [bodyfile]
fire() {
  local batch=$1 case=$2 method=$3 path=$4 bf=${5:-}
  local args=(-sk -m 8 -o /tmp/jc7_resp.bin -w '%{http_code}' --http1.1 -X "$method"
              -H 'X-Sonos-Api-Key: x' -H 'Content-Type: application/json')
  [ -n "$bf" ] && args+=(--data-binary "@$bf")
  local code; code=$(curl "${args[@]}" "https://$IP:1443$path")
  local bytes; bytes=$(wc -c < /tmp/jc7_resp.bin 2>/dev/null | tr -d ' ' || echo 0)
  local ec; ec=$(grep -o '"errorCode":"[A-Z_]*"' /tmp/jc7_resp.bin 2>/dev/null | head -1 | cut -d'"' -f4)
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$(date -u +%H:%M:%S)" "$batch" "$case" "$method" "$path" "$code" "$bytes" "${ec:--}" "" >> "$OUT"
  printf '%-4s %-26s %-6s %-52s %s %s %s\n' "$batch" "$case" "$method" "$path" "$code" "${bytes}B" "${ec:--}"
  sleep "$SPACING"
}

batch_end() {
  local b=$1 st; st=$(alive)
  printf '%s\t%s\tBATCH_END\t-\t/status/VERSION\t%s\t-\t-\t%s\n' \
    "$(date -u +%H:%M:%S)" "$b" "$st" "$([ "$st" = 200 ] && echo alive || echo DEAD)" >> "$OUT"
  printf -- '--- batch %s end: liveness %s\n' "$b" "$st"
}

P=/api/v1/players/$U

# ---- B1: what do the POST/PUT routes require? ({}) -------------------------
for r in settings/player upnpDeviceProperties upnpSystemProperties upnpZoneGroupTopology \
         diagnostics update/check networkTest/start platformInternal/sync settings/protectedAdmin; do
  echo '{}' > "$BODY"; fire B1 empty-body POST "$P/$r" "$BODY"
done
echo '{}' > "$BODY"; fire B1 empty-body PUT "$P/settings/player" "$BODY"
echo '{}' > "$BODY"; fire B1 empty-body PUT "$P/settings/sonosNetChannel" "$BODY"
batch_end B1

# ---- B2: huge strings (>65535) — the u16 truncation shape ------------------
# Target diagnostics/update-check: they 200 on {}, so the parse always completes and the
# u16 length is set even when the field is unused. upnpDeviceProperties kept for contrast.
for n in 65535 65536 65537 70000 200000; do
  python3 -c "import json;open('$BODY','w').write(json.dumps({'a':'A'*$n,'b':1}))"
  fire B2 huge-value-$n POST "$P/diagnostics" "$BODY"
done
python3 -c "import json;open('$BODY','w').write(json.dumps({'A'*70000:1,'b':2}))"
fire B2 huge-key-70000 POST "$P/diagnostics" "$BODY"
python3 -c "import json;open('$BODY','w').write(json.dumps({'a':'A'*70000,'b':'B'*70000,'c':1}))"
fire B2 two-huge-then-field POST "$P/diagnostics" "$BODY"
python3 -c "import json;open('$BODY','w').write(json.dumps({'a':'A'*70000,'b':1}))"
fire B2 huge-on-updatecheck POST "$P/update/check" "$BODY"
fire B2 huge-on-deviceprops POST "$P/upnpDeviceProperties" "$BODY"
fire B2 huge-on-zonegroup POST "$P/upnpZoneGroupTopology" "$BODY"
# huge path parameter
fire B2 huge-path-param GET "/api/v1/players/$U/$(python3 -c 'print("A"*70000)')/info"
batch_end B2

# ---- B3: deep nesting ------------------------------------------------------
for d in 32 256 1024 4096 16384; do
  python3 -c "
d=$d
open('$BODY','w').write('{\"a\":' + '{\"a\":'*d + '1' + '}'*d + ',\"b\":1}')"
  fire B3 nest-$d POST "$P/upnpDeviceProperties" "$BODY"
done
batch_end B3

# ---- B4: array element-count mismatches ------------------------------------
for c in '{"a":[],"b":1}' '{"a":[1],"b":1}' '{"a":[1,2,3,4,5,6,7,8],"b":1}' \
         '{"a":[{"x":1},2,"s"],"b":1}' '{"a":[[1],[2]],"b":1}'; do
  printf '%s' "$c" > "$BODY"; fire B4 "$(echo "$c" | cut -c1-24)" POST "$P/upnpDeviceProperties" "$BODY"
done
batch_end B4

# ---- B5: wrong types -------------------------------------------------------
for c in '{"a":"1","b":1}' '{"a":1,"b":"1"}' '{"a":{"x":1},"b":[1]}' '{"a":[1],"b":{"x":1}}' \
         '{"a":null,"b":1}' '{"a":true,"b":1}' '{"a":-1,"b":1}' '{"a":1e400,"b":1}' \
         '{"a":{},"b":1}' '{"a":[],"b":1}'; do
  printf '%s' "$c" > "$BODY"; fire B5 "$(echo "$c" | cut -c1-24)" POST "$P/upnpDeviceProperties" "$BODY"
done
batch_end B5

# ---- B6: the three 200 GET routes, malformed query ------------------------
for q in '?a=%00' '?a=1&a=2' '?%ZZ=1' '?a=' '?a[]=1' '?a=%ff%fe' ; do
  fire B6 "query$(echo "$q" | cut -c1-12)" GET "$P/info$q"
done
fire B6 trailing-slash GET "$P/info/"
fire B6 double-slash GET "/api/v1/players//info"
batch_end B6

# ---- B7: POST routes with a real-ish body ---------------------------------
printf '%s' '{"volumeScaling":{"mode":"VARIABLE"}}' > "$BODY"
fire B7 settings-player-body POST "$P/settings/player" "$BODY"
printf '%s' '{"volumeScaling":{"mode":"FIXED","x":1}}' > "$BODY"
fire B7 settings-player-bad POST "$P/settings/player" "$BODY"
printf '%s' '{"serviceType":"upnp:service:DeviceProperties:1"}' > "$BODY"
fire B7 deviceprops-body POST "$P/upnpDeviceProperties" "$BODY"
batch_end B7

echo "done -> $OUT"
