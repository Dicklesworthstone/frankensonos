#!/usr/bin/env bash
set -u
IP=${IP:-192.168.4.86}; U=${U:-RINCON_7828CA9221DC01400}; OUT=${OUT:-recon/jc7/inputshape3.tsv}
SPACING=${SPACING:-3}
printf 'ts\tshape\troute\tstatus\tbytes\tbody\n' > "$OUT"
send() { local l=$1 r=$2 j=$3
  printf '%s' "$j" > /tmp/jc7_i3.json
  local code; code=$(curl -sk -m 15 -o /tmp/jc7_i3.out -w '%{http_code}' --http1.1 -X POST \
    -H 'X-Sonos-Api-Key: x' -H 'Content-Type: application/json' --data-binary @/tmp/jc7_i3.json \
    "https://$IP:1443/api/v1/players/$U/$r")
  printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$(date -u +%H:%M:%S)" "$l" "$r" "$code" \
    "$(wc -c < /tmp/jc7_i3.out|tr -d ' ')" "$(tr -d '\n' < /tmp/jc7_i3.out | cut -c1-160)" | tee -a "$OUT"
  sleep "$SPACING"
}
send setstring-noinput      upnpSystemProperties '{"method":"SetString"}'
send setstring-bogus        upnpSystemProperties '{"method":"SetString","input":[{"name":"Bogus","value":"x"}]}'
send setstring-onearg       upnpSystemProperties '{"method":"SetString","input":[{"name":"VariableName","value":"jc7probe"}]}'
send getstring-jc7probe     upnpSystemProperties '{"method":"GetString","input":[{"name":"StringVariable","value":"jc7probe"}]}'
send getstring-zoneName     upnpSystemProperties '{"method":"GetString","input":[{"name":"StringVariable","value":"ZoneName"}]}'
send setzoneattr-3args      upnpDeviceProperties '{"method":"SetZoneAttributes","input":[{"name":"DesiredZoneName","value":"Kitchen Counter"},{"name":"DesiredIcon","value":"x-rincon-roomicon:kitchen"},{"name":"DesiredConfiguration","value":"1"}]}'
send getzoneattr-0args      upnpDeviceProperties '{"method":"GetZoneAttributes"}'
