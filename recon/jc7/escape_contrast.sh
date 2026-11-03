#!/usr/bin/env bash
# jc7 T1 regression — the proxy escapes XML while the device's own parser is strict.
# Same value, both paths, one verdict per path:
#   proxy  -> the value survives the SOAP body, the device decodes it, read-back is identical (200)
#   direct -> the identical raw value in a hand-built body is rejected by the device parser (402)
# The 402 is the contrast, not a fault.
set -u
IP=${IP:-192.168.4.86}
U=${U:-RINCON_7828CA9221DC01400}
V=${V:-a<b>c&d"e}
API="https://$IP:1443/api/v1/players/$U/upnpSystemProperties"
K="jc7esc_$$"
json() { python3 -c 'import json,sys;print(json.dumps({"method":sys.argv[1],"input":[{"name":a,"value":sys.argv[2]} for a in sys.argv[3:]]}))' "$@"; }

echo "-- proxy SetString value=[$V]"
json SetString "$V" VariableName "$K" | python3 -c 'import json,sys;d=json.load(sys.stdin);print(json.dumps({"method":"SetString","input":[{"name":"VariableName","value":sys.argv[1]},{"name":"StringValue","value":sys.argv[2]}]}))' "$K" "$V" > /tmp/jc7_e.json
r=$(curl -sk -m 20 --http1.1 -X POST -H 'X-Sonos-Api-Key: x' -H 'Content-Type: application/json' --data-binary @/tmp/jc7_e.json -w '\n%{http_code}' "$API")
echo "   rc=$(printf '%s' "$r" | grep -o '"result":[0-9-]*') http=$(printf '%s' "$r" | tail -1)"
echo "-- proxy GetString read-back"
json GetString "$K" VariableName > /tmp/jc7_e.json
r=$(curl -sk -m 20 --http1.1 -X POST -H 'X-Sonos-Api-Key: x' -H 'Content-Type: application/json' --data-binary @/tmp/jc7_e.json -w '\n%{http_code}' "$API")
rb=$(printf '%s' "$r" | head -n -1 | python3 -c 'import json,sys;d=json.load(sys.stdin);print(d.get("parameters",[{}])[0].get("value",""))' 2>/dev/null)
echo "   http=$(printf '%s' "$r" | tail -1) readback=[$rb]"
[ "$rb" = "$V" ] && echo "   VERDICT identical: the proxy escaped, the device decoded, no injection" \
                  || echo "   VERDICT DIFFERS: escaping regressed"

echo "-- direct SOAP, same raw value, unescaped"
BODY='<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:SetString xmlns:u="urn:schemas-upnp-org:service:SystemProperties:1"><VariableName>'"$K"'</VariableName><StringValue>'"$V"'</StringValue></u:SetString></s:Body></s:Envelope>'
curl -s -m 20 -o /tmp/jc7_e_direct.out -w '   http=%{http_code}\n' -X POST \
  -H 'SOAPACTION: "urn:schemas-upnp-org:service:SystemProperties:1#SetString"' \
  -H 'Content-Type: text/xml; charset="utf-8"' --data-binary "$BODY" "http://$IP:1400/SystemProperties/Control"
grep -o '<errorCode>[0-9]*</errorCode>' /tmp/jc7_e_direct.out | sed 's/^/   /'
