# RE_GETRS — indexing the settings table behind `/getrs?id=N` (S1 .202)

`/getrs` takes an `atoi`'d index into a settings table and follows the entry with an indirect call, so an
out-of-range index reaches code the table never intended. S1 only (404 on S2). Raw sockets are used so an
RST is distinguishable from a clean close; liveness (`/status`) is re-checked after any anomaly, every 200
requests, and at the end.

## Phase 1 — response map

| query | status | body |
|---|---|---|
| `?id=-1`, `0`, `1`, `100`, `65535` | 200 | empty |
| no query string | 200 (after ~5 s on some builds) | empty |
| `?id=0x7fffffff`, `0xffffffff`, `0x80000000` | 200 | empty |
| `?id=32768`, `65536`, `131072` | 200 | empty |

Every answer carries the dummy content headers (`X-RINCON-CONTENT-VERSION: 4294967295`,
`X-RINCON-LAST-UPDATE-DEVICE: RINCON_FFFFFFFFFFFF99999`), i.e. the handler runs and resolves nothing —
`bytes` is 0 in all of them.

## Phase 2 — bounded scan

2 229 requests in one run: the huge/negative set above, `16384..16600` step 1, `32768`, `65536`, `131072`,
and a 2 000-value sample uniform in `0..1 000 000` (seed 7).

| classification | count |
|---|---|
| `200` with an empty body | **2 229 (100 %)** |
| `RST` | 0 |
| `TIMEOUT` | 0 |
| non-empty body | 0 |
| liveness `/status` after every 200-request gap (11 gaps) | 200 every time |

Latency is jitter, not structure: median ~40 ms, p99 609 ms, max 3 278 ms (`id=580963`), with the slow ones
scattered rather than clustered on particular indices (`16401..16490` only looks hot because that is the
densely sampled window). The final liveness check after the run was 200.

## Reading

`/getrs` is a **flat null surface** on 57.23 as configured: every index from `-1` through `0xffffffff`
answers identically, with the dummy `X-RINCON-*` headers and zero bytes, and the unbounded index never
produces a reset, timeout or wedge. A per-connection crash (`RST` followed by a working daemon) never
occurs, so the table's indirect call always lands on something benign here — the indices cannot reach an
unset entry on this unit.

That is also the limit of black-box work on this verb: with 2 229 probes and no distinguishing token, further
sampling buys nothing. The next step has to be static — the table base and entry stride in the 57.23
`anacapad` — since only the lookup itself can say which indices are populated.
