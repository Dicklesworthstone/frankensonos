#!/usr/bin/env bash
# jc7 — find the argument element for the UPnP passthrough.
# Oracle: GetString + StringVariable=ZoneName. rc 402 = body arrived, args missing;
# rc 200 + "Kitchen Counter"-ish value = args accepted.
set -u
IP=${IP:-192.168.4.86}
U=${U:-RINCON_7828CA9221DC01400}
OUT=${OUT:-recon/jc7/argshape.tsv}
BODY=/tmp/jc7_arg.json
SPACING=${SPACING:-3}
printf 'ts\tshape\tstatus\tbytes\tbody\n' > "$OUT"

poll() { while :; do printf '%s poll %s\n' "$(date -u +%H:%M:%S.%N)" \
    "$(curl -s -m 2 -o /dev/null -w '%{http_code}' "http://$IP:1400/status/VERSION")" >> "$OUT.poll"; sleep 0.1; done; }
: > "$OUT.poll"; poll & POLL=$!

send() { # send <label> <json>
  local l=$1 j=$2
  printf '%s' "$j" > "$BODY"
  local code; code=$(curl -sk -m 15 -o /tmp/jc7_arg.out -w '%{http_code}' --http1.1 -X POST \
      -H 'X-Sonos-Api-Key: x' -H 'Content-Type: application/json' --data-binary "@$BODY" \
      "https://$IP:1443/api/v1/players/$U/upnpSystemProperties")
  local bytes; bytes=$(wc -c < /tmp/jc7_arg.out | tr -d ' ')
  local body; body=$(tr -d '\n' < /tmp/jc7_arg.out | cut -c1-180)
  printf '%s\t%s\t%s\t%s\t%s\n' "$(date -u +%H:%M:%S)" "$l" "$code" "$bytes" "$body" >> "$OUT"
  printf '%-28s %s %sB %s\n' "$l" "$code" "$bytes" "$body"
  sleep "$SPACING"
}

send baseline            '{"method":"GetString","args":{"StringVariable":"ZoneName"}}'
send arguments           '{"method":"GetString","arguments":{"StringVariable":"ZoneName"}}'
send arg                 '{"method":"GetString","arg":{"StringVariable":"ZoneName"}}'
send parameters          '{"method":"GetString","parameters":{"StringVariable":"ZoneName"}}'
send params              '{"method":"GetString","params":{"StringVariable":"ZoneName"}}'
send flattened           '{"method":"GetString","StringVariable":"ZoneName"}'
send args-singleobj-arr  '{"method":"GetString","args":[{"StringVariable":"ZoneName"}]}'
send args-pairarray      '{"method":"GetString","args":[["StringVariable","ZoneName"]]}'
send args-namevalue      '{"method":"GetString","args":[{"name":"StringVariable","value":"ZoneName"}]}'
send args-typed          '{"method":"GetString","args":{"StringVariable":{"value":"ZoneName"}}}'
send args-string         '{"method":"GetString","args":"StringVariable=ZoneName"}'
send unknownkey          '{"method":"GetString","zzz":1}'
kill $POLL 2>/dev/null
echo done
