# jc7 — the two latent weaknesses, on the wire

Runner `recon/jc7/wirefuzz_weak.sh`, log `wirefuzz4.tsv`, 3 s spacing, liveness after every
batch (ICMP/HTTP 200 each time; the `:1443` poller returns 400 because it omits the api key —
expected, not a fault).

**Verdict: both weaknesses are latent on the wire — 22 cases, every one byte-identical
(499 / rc 402 / 127 B / ERROR_COMMAND_FAILED), no fault, no daemon death.**

## The oracle had to be fixed first

§8's 200s were meaningless: `GetZoneAttributes` takes no arguments and 200s on anything, so
it cannot show that a body arrived. `GetString` *requires* `StringVariable`, so its rc is the
only usable signal. The rc is also meaningful, which the earlier pass did not establish:

| request | rc |
|---|---|
| `{"method":"NoSuchAction"}` | 401 (invalid action) |
| `{"method":"GetString",…}` on `upnpDeviceProperties` | 401 (wrong service) |
| `{"method":"GetString"}` (no args) | 402 (invalid args) |
| `{"method":"GetZoneAttributes","args":{"a":1}}` | 200 (args ignored) |

So 401 = action/service resolution, 402 = invalid args, and action/service selection is
correct. The failure is specifically that the argument element is not accepted.

## T1 — XML escaping

Values sent as `StringVariable`: `ZoneName` (plain baseline), `<x>`, `a&b`, `a&b;c`, `"q"`,
`'q'`, `<a/>`, `]]>`, `&#0;`, `<` (unterminated), `a<b>c&d"e`, `a<TAB>b`.

All twelve: 499 / rc 402 / 127 B, identical to the plain baseline. A bare `&` is fatal to a
strict XML parser, so if the value reached the body unescaped the device should have answered
differently — it did not. Either the value never reaches the element, or the device's parser
is lenient. **No second-order parser hit.**

## T2 — size under-request

`2*len(value)+16` requested vs `2*len(name)+len(value)+6` needed, so a 70 000-byte name with a
1-byte value under-requests by ~140 kB per write and the cursor is advanced by the full
`snprintf` return.

| shape | cases | result |
|---|---|---|
| name 70000 + value 1, arg counts 1/2/3/5/10 | 5 | all 402 |
| control, short names, counts 1/5 | 2 | all 402 |
| name 200000 + value 1, 5 args | 1 | 402 |
| same long-name shape on the setter (`SetZoneAttributes`) | 1 | 402 |
| setter with metacharacters in the value | 1 | 402 |

Ten writes of ~140 kB each walk the cursor ~1.4 MB past a 34 148-byte inline buffer. No crash,
no rc change, no "Data overflowed; ignore submit to %s" (the builder's own overflow message at
`0x68cc28`) — so the grow path absorbs it, or the args never reach the builder.

## What this leaves

The 402 is **value-independent and shape-independent** (object, array of `{name,value}`,
array of single-key objects, array of pairs, flattened, reordered, lowercased, plus `tunnel`
in the body and `x-sonos-upnp-tunnel` as a header). That is not what a wrong *value* looks
like; it is what a wrong *element* looks like. Until one arg-carrying action succeeds, both
§8 weaknesses stay unproven on the wire, and the argument element is the single thing to
chase — the envelope template is `<u:%s%s xmlns:u="%s">` (`0x6bc50c`) with two `%s` after the
action name, so the second one (a suffix) is the most likely missing piece.
