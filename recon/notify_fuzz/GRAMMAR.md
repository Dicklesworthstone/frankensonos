# Request grammar extracted from anacapad 57.23-74170 (static, r2)

Route table: descriptor array at `.data 0x1053d040..` stride 0x1c `{u32 pad; const char* route;
handler*; u32 flags/pad}` (verified for /notify, /setstring, /advconfig, /support/directsubmit).

## Handlers of this lane (bead frankensonos-v8z)

| route | handler | notes |
|---|---|---|
| /notify | 0x1028a5b8 | GENA NOTIFY simulation |
| /setstring | 0x1000b170 | SystemProperties setter (the daemon-killer per ROOT_RUNBOOK E) |
| /advconfig | 0x1028a724 | GET page + POST `FirstZP`/`PriorityBridge` -> `%s/netsettings_mod.txt` |
| /support/directsubmit | 0x1028b41c | diagnostics submit, POST form |
| /support/aggregate | 0x1028b680 | POST form w/ several toggles |
| /support/review | 0x1028b908 | (adjacent, same family) |

## Shared plumbing

- `get_param(req, name)` = 0x1040bfec -> 0x1040aac8: looks up parsed request token table at
  `req+0` (list of name/value pairs, stride 0x10 via 0x1040a7d8); returns pointer or NULL.
  Returns the **name** table if the header name itself is empty — both name and value come
  from the same string pool.
- POST body reader 0x1038a950(req, dst, dstlen, flags, min_len=r7):
  - requires method==4 (POST) else 0x195
  - `Content-Length` (0x1043e520, case-insensitive lookup) parsed with `strtoul` base 10;
    errno checked; absent => r9=0 and *flags=1.
  - read loop 0x1038aa08..: accumulates into `dst` with `subf r5, r29, r30` (remaining =
    dstlen - written); recv via 0x10403340; loop guard `r29 < r31` where r31 = the
    **strtoul result** (Content-Length), NOT dstlen.
  - truncation paths: `r29 >= dstlen-1` -> `dst[r29]=0` (fits); full length reached ->
    `dst[dstlen-1]=0`.
  - **Bound observation (candidate B1):** loop condition compares `r29` (bytes written)
    against `r31` (Content-Length) but the recv length `r5 = dstlen - r29`. If
    Content-Length < dstlen and the peer sends more bytes than Content-Length, extra bytes
    are still read into `dst` at offset `r29` while `r29 < r31`... (recv bounded by r5 so
    max write = dstlen-1; no overflow found). Verdict: bounded.
- form parser 0x1038a5d4 -> 0x1038a508 (`&`=0x26 pair sep, `=`=0x3d key/value sep):
  - 0x1038a420 decodes: `%`+2 hex (isxdigit checked, else parse FAILS whole pair -> r27=0),
    `+`->space, stops at sep byte.
  - copy bound: `cmplwi r28,1; ble fail` with r28 = remaining capacity -> **NUL always
    lands inside** (writes at most cap-1 bytes then terminator at r29 when cap>=1; when cap
    hits 1 the pair is rejected with r27=0 but `stb r0,0(r29)` still writes the terminator
    at the last slot — in-bounds).
  - **Candidate B2:** terminator write at 0x1038a4e8 executes even on the capacity-fail
    path with `r29` already advanced to `dst+cap` — i.e. writes `dst[cap]`?? Re-check: on
    `ble` branch r28 was NOT decremented and r29 NOT advanced, so `stb` writes `dst[cap-1]`.
    Bounded. Verdict: bounded.
- advconfig value buffers: `r1+0x10` (0x1000 bytes) for the raw body via 0x1038a950
  (dstlen 0x1000, min 0x7d0); kv pair dst `r1+0x1040` name cap 0x20, `r1+0x1060` value cap
  0x80. After parse: values compared with `strcmp` against "1"/"0"-style literals
  (0x1046781c page text), written with `fprintf(f, "FirstZP: [%d]\n", FirstZP)` and
  `"PriorityBridge: [%d]\n"` into `snprintf("%s/netsettings_mod.txt", base)` — integers
  only, no string echo into the file.
- setstring: body reader dstlen 0x800 (min 0x3e8) at `r1+0x440`; kv: name `r1+0xc40` cap
  0x80, value `r1+0xcc0` cap 0x401. Key dispatch by strcmp: `"key"` (0x104ef91c) /
  `"value"` (0x104fb4a4) are the form field names; known property names dispatch via
  0x1031a650 (`valid property name?`) into 0x10318eb0 (`store`, caps 0x80 / 0x401).
  Unknown key -> 405 path (0x195).
- directsubmit: body reader dstlen 0x800 min 0x3e8 at `r1+0x540`; kv name `r1+0xd40` cap
  0x80, value `r1+0xdc0` cap 0x401; requires form key `"csrfToken"`?? (0x1043e8a8 =
  'csrfToken' compared at 0x1028b578-0x1028b584 loop; plus gate flag r25 set by the
  hidden field). Response built with snprintf caps 0x100/0x400.
- /notify: params `nt`(0x1047291c), `nts`(0x1048cd14), `sid`(0x10498dfc), `seq`(0x10496830);
  requires `nt == "upnp:event"` (0x104927f0) and `nts == "upnp:propchange"` (0x104927fc);
  all four params must be present (null-checked with the not-equal trick), sid non-null
  (cr6 branch) else 400/405. Then virtual call `obj->vtable[0x2c](sid, seq, nts_val, nt_val)`
  on singleton `0x10545dc8`. SEQ reaches the callback as an opaque string.

## Fuzz dimensions

1. Header/param presence x order x duplicates (lookup is first-match over the token table).
2. Value shapes: empty, 1-byte, cap-boundary (0x1f/0x20 name, 0x7f/0x80 value, 0x400/0x401),
   `%`-encoding edge (trailing `%`, `%z`, `%00`, `+`), NUL via `%00`.
3. Content-Length: 0, absent, < min_len gate (0x7d0 advconfig, 0x3e8 setstring), > actual
   body, >> dstlen (0x1000/0x800), unsigned wrap (`4294967295`), leading `-`/`+`/spaces
   (strtoul), hex-ish digits.
4. Form grammar: `=`, `&&`, `&`, duplicate keys, percent-encoded separators `%26`/`%3D`
   (NOT separators after decode — decoder runs inside pair scan).
5. /notify seq: `SEQ` with embedded `:`/spaces/`%0A`, huge length (cap 0x401 at
   0x1038a698? seq comes from get_param not form — no copy bound found; callback is
   out-of-binary → out of static scope, marked DEVICE-TEST).
6. advconfig GET path: `?` query param name at 0x10359660(dst,0x21) — 33-byte cap lookup
   (name cap 0x21); candidates: name length 0x20/0x21/0x22.

## Static verdicts so far

- B1 Content-Length vs buffer: bounded (recv clamp by remaining).
- B2 kv copier terminator: bounded (r29 not advanced on fail).
- No reachable `sprintf` into fixed buffer with request-sourced `%s` in these five handlers
  (response builders all use bounded snprintf with literal formats).
- The only request bytes that flow into format-like sinks are the /notify `seq` string into
  a vtable callback (device-side only).
