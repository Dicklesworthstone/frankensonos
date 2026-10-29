# jc7 wire fuzz — verdict

Runner: `recon/jc7/wirefuzz.sh` (3 s spacing, liveness check after every batch).
Target: `.86`, 86.10-81100, `https://192.168.4.86:1443`, `X-Sonos-Api-Key: x` (presence
gate, findings2 §7). Logs: `wirefuzz.tsv` (53 cases), `wirefuzz2.tsv` (22 cases).

**Verdict: 75 malformed-JSON cases, 0 faults, 0 daemon deaths, liveness 200 after every
batch.** The three `000` results in the first run were transport blips — each returned 200
on retry (`diag-65536` ×2, `info?a=` ×2).

## The binder target had to be found first

`upnpDeviceProperties` rejects unknown fields with `ERROR_INVALID_PARAMETER` /
`"Parsing terminated:method"`, which names the required field: the route is a UPnP
passthrough taking `{"method":"<name>","args":{...}}`. `{"method":"GetZoneAttributes"}`
returns 200 with a real `upnpResponse`; `SetZoneAttributes` with `DesiredZoneName` reaches
the UPnP layer (402). So `args.DesiredZoneName` is a genuinely **bound string field** — the
shape the u16 truncation needs.

## Results

| family | cases | verdict |
|---|---|---|
| huge value in a bound string, then a second field (65535/65536/65537/70000/200000) | 5 | all 499 `ERROR_COMMAND_FAILED` rc 402 — bound, forwarded, rejected by UPnP; no fault |
| huge method (70000) | 1 | 499 rc 401 |
| huge arg key (70000) | 1 | 499 rc 402 |
| two 70000 strings then a field | 1 | 499 rc 402 |
| deep nesting 32/256/1024/4096 | 4 | 200 |
| deep nesting 32/256/1024/4096/16384 on `upnpDeviceProperties` (no method) | 5 | 400 `ERROR_INVALID_PARAMETER` |
| `args` as array `[]` / `[1,2,3]` | 2 | 200 (tolerated) |
| `method` as array / null | 2 | 400 `Parsing terminated:method` |
| `method` as number | 1 | 499 rc 401 (coerced to a string) |
| `args` null / absent | 2 | 200 / 400 |
| `DesiredZoneName` as array / object | 2 | 200 (tolerated) |
| missing `method` | 1 | 400 `Parsing terminated:method` |
| `1e400` exponent overflow | 1 | 400 `ERROR_INVALID_SYNTAX` (rapidjson rejects it) |
| huge path parameter (70000) on `/info` | 1 | 200 |
| query junk on `/info` (`%00`, dup key, `%ZZ`, empty, `a[]`, `%ff%fe`) | 6 | all 200 |
| trailing slash / double slash | 2 | 200 / 400 |

## Reading

- The u16 truncation is **survivable at every length tested**, including 200 000 — a
  >65535 bound string followed by another field does not fault. The truncation stays a
  correctness bug (silent loss), not memory safety, on this route.
- The parser is rapidjson and strict: `1e400` is the only case that reached a *syntax*
  error rather than a parameter error.
- `args` and the argument values are type-tolerant (arrays and objects pass through), so
  the binder is not where a type confusion would show.
- Nothing here is a finding yet. What is now in place is a working pre-auth binder target
  (`upnpDeviceProperties` + `method`/`args`) and a runner, so the next pass can go after
  the UPnP argument marshalling (rc 402/401) rather than the JSON layer.
