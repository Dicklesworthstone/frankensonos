#!/usr/bin/env bash
set -u
IP=${IP:-192.168.4.86}; U=${U:-RINCON_7828CA9221DC01400}; OUT=${OUT:-recon/jc7/inputshape.tsv}
SPACING=${SPACING:-3}
printf 'ts\tshape\tstatus\tbytes\tbody\n' > "$OUT"
send() { local l=$1 j=$2
  printf '%s' "$j" > /tmp/jc7_in.json
  local code; code=$(curl -sk -m 15 -o /tmp/jc7_in.out -w '%{http_code}' --http1.1 -X POST \
    -H 'X-Sonos-Api-Key: x' -H 'Content-Type: application/json' --data-binary @/tmp/jc7_in.json \
    "https://$IP:1443/api/v1/players/$U/upnpSystemProperties")
  printf '%s\t%s\t%s\t%s\t%s\n' "$(date -u +%H:%M:%S)" "$l" "$code" "$(wc -c < /tmp/jc7_in.out | tr -d ' ')" \
    "$(tr -d '\n' < /tmp/jc7_in.out | cut -c1-200)" | tee -a "$OUT"
  sleep "$SPACING"
}
send input-pair '{"method":"GetString","input":[{"name":"StringVariable","value":"ZoneName"}]}'
send input-pair-typed '{"method":"GetString","input":[{"name":{"value":"StringVariable"},"value":{"value":"ZoneName"}}]}'
send input-obj '{"method":"GetString","input":{"StringVariable":"ZoneName"}}'
send input-single '{"method":"GetString","input":{"name":"StringVariable","value":"ZoneName"}}'
send params-map '{"method":"GetString","params":{"StringVariable":"ZoneName"}}'
