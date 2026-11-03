#!/usr/bin/env bash
set -u
IP=${IP:-192.168.4.86}; U=${U:-RINCON_7828CA9221DC01400}; OUT=${OUT:-recon/jc7/inputshape2.tsv}
SPACING=${SPACING:-3}
printf 'ts\tshape\tstatus\tbytes\tbody\n' > "$OUT"
send() { local l=$1 j=$2
  printf '%s' "$j" > /tmp/jc7_i2.json
  local code; code=$(curl -sk -m 15 -o /tmp/jc7_i2.out -w '%{http_code}' --http1.1 -X POST \
    -H 'X-Sonos-Api-Key: x' -H 'Content-Type: application/json' --data-binary @/tmp/jc7_i2.json \
    "https://$IP:1443/api/v1/players/$U/upnpSystemProperties")
  printf '%s\t%s\t%s\t%s\t%s\n' "$(date -u +%H:%M:%S)" "$l" "$code" "$(wc -c < /tmp/jc7_i2.out|tr -d ' ')" \
    "$(tr -d '\n' < /tmp/jc7_i2.out | cut -c1-160)" | tee -a "$OUT"
  sleep "$SPACING"
}
send setstring-pair   '{"method":"SetString","input":[{"name":"VariableName","value":"jc7probe"},{"name":"StringValue","value":"ok"}]}'
send getstring-pair2  '{"method":"GetString","input":[{"name":"StringVariable","value":"ZoneName"}]}'
send reversed         '{"method":"GetString","input":[{"value":"ZoneName","name":"StringVariable"}]}'
send withtype         '{"method":"GetString","input":[{"name":"StringVariable","value":"ZoneName","type":"string"}]}'
send extra-field      '{"method":"GetString","input":[{"name":"StringVariable","value":"ZoneName","x":1}]}'
send inputempty       '{"method":"GetString","input":[]}'
send cmd-call         '{"method":"call","command":"call","input":[{"name":"StringVariable","value":"ZoneName"}]}'
send action-command   '{"method":"GetString","command":"call","input":[{"name":"StringVariable","value":"ZoneName"}]}'
send tunnel          '{"method":"GetString","tunnel":true,"input":[{"name":"StringVariable","value":"ZoneName"}]}'
