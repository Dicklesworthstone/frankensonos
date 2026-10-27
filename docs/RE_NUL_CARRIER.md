# RE_NUL_CARRIER — the 2.1 stack-NUL carrier hunt (bead frankensonos-z4j, .84 coordinator)

Question of record: does any SOAP action accept a field value **> 2100 bytes**
so the object-generic copy `0x101cc878 → 0x101ccc9c` (`memcpy(sp+16,
obj+4212, obj+4208)`, frame 2160, buffer 2100) runs with a stored length big
enough to reach the saved registers / LR (2.1 map: r21@2101 … LR@2149-2152)?
All measurements on **192.168.4.84** (S1 Play:5, 57.23-74170, the stereo-pair
coordinator), 2026-10-27.

## Static corrections first

- The setter entry is `0x101cbb3c`, not `0x101cbb38` (the doc's address is 4
  bytes into the prologue; every known call site's `bl` resolves there).
- It has **14 direct `bl` callers**, not ten: `0x101cbd78`, `0x101cbec0`,
  `0x101cd168`, `0x101cd2b4`, `0x101cdb5c`, `0x101cdca0`, `0x101cde48`,
  `0x101ce0b4`, `0x101da6c8`, `0x101db304`, `0x101df714`, `0x101e06b4`,
  `0x101e0cdc`, `0x101ebe1c` — enclosing functions `0x101cbb38` (thunk),
  `0x101cbe80`, `0x101ccdb0` (x2), `0x101cd320` (x4), `0x101da480`,
  `0x101db298`, `0x101deec0` (vtable slot `0x10477d78`), `0x101e0398` (x2),
  `0x101ebb1c` (vtable slot `0x104798bc`).
- All the enclosing functions are themselves unreferenced by `bl` except
  `0x101da480` (called from `0x101daaa4`/`0x101daacc`) — the family is
  vtable-dispatched, and the two confirmed vtable members reinforce that.
- Every one of these functions is inside the CIFS client block
  (`0x101cb*..0x101eb*`); the doc's conclusion stands: the +4208 field is a
  **CIFS failure-message detail**, so only CIFS-path actions can feed it.

## Device cap table (byte-precise bisects)

| action | field | accepted up to | first error | code | notes |
|---|---|---|---|---|---|
| `SetAVTransportURI` | `CurrentURI` (x-file-cifs) | **1019** | 1020 | 714 | byte-precise: 1019 OK, 1020 714 |
| `AddURIToQueue` | `EnqueuedURI` (x-file-cifs) | **1019** | 1020 | 804 | 1000-1019 OK; also contradicts the doc's "804 past ~1000" — exactly 1019 is fine |
| `SetNextAVTransportURI` | `NextURI` | — | every length | **701** | 701 even while PLAYING mp3radio and in TRANSITIONING; see below |
| `CreateSavedQueue` | `Title` (plain arg) | **3000+** | none seen | 200 | >2100 accepted and stored, **but** browse returns it truncated to 1023 and this object is a playlist container, not a CIFS message object |
| `CreateObject` into `FV:2` | `dc:title` | (402 in this run's shape) | — | 402 | the doc's earlier 200/2046-byte run used a different shape; stored cap was measured at 1023 there too |

### SetNextAVTransportURI is 701-gated beyond reach

The doc's "answers 701 unless a track is playing" is not the real gate. 701 is
returned in **every** state measured: STOPPED, PLAYING (mp3radio via
`SetAVTransportURI`+`Play`, confirmed `CurrentTransportState=PLAYING`),
TRANSITIONING, and with a `x-rincon-queue:<udn>#N` source. The queue-based
playback path itself is broken for these entry types on this unit today
(queue entries of `x-rincon-mp3radio://` accepted into the queue but
`Play` from the queue context goes straight to STOPPED). No state produced a
non-701 answer, so SetNext carries **no** length into the +4208 field on this
build as reachable today.

### CreateSavedQueue accepts >2100 but is the wrong object

`CreateSavedQueue` with a plain `Title` argument accepts 3000 bytes (`SQ:16`
created, daemon healthy). Browse of `SQ:16` returns the title **truncated to
1023 bytes** — the stored value is longer than the browse path hands back,
matching the doc's 2.1e observation. But the 2.1 reader copies
`obj+4208/obj+4212` on objects owned by the CIFS client family; a saved-queue
playlist container is a different object type and none of the 14 setter
callers sits in the saved-queue path. The >2100 title therefore **does not
reach the 2.1 copy**.

## Verdict

**The falsifier's condition is met**: every SOAP action probed with maximal
accepted field values either caps below 2100 (SetAVTransportURI 1019,
AddURIToQueue 1019), is unreachable (SetNextAVTransportURI 701 in all
states), or stores its >2100 value in an object the 2.1 reader never touches
(CreateSavedQueue/FV:2 titles). **No >2100 wire carrier into the
`obj+4208/+4212` field exists on 57.23 as reachable today.** The 2.1
stack-NUL/overwrite primitive stays un-triggerable from the wire; its stored
length can only be 0 or 1024 per the setter guard (`0x101cbbbc`), and neither
reaches the saved registers (2.1 map: first saved register r21 at 2101).

## Reopen conditions

1. A CIFS-client action (the 14-caller family) reachable with a failure
   detail string >2100 — the failure messages are built from the URL, share,
   or path of `x-file-cifs:` URIs; if any validator on that path accepts more
   than 1019 bytes for the *detail* (as opposed to the URI itself), the copy
   gets a live length. The `/getaa?u=` route reaches the CIFS code through the
   AVT/URI layer; its URI gate (4016/4017, see RE_OVERFLOW_EXPLOITABILITY.md)
   applies to the URI, not to the failure-detail substring, so a long *share
   name* inside a valid URI remains untested as a detail source.
2. A unit/transport state where `SetNextAVTransportURI` answers non-701
   (queue context with a playable next track), then a >2100 `NextURI`.
3. The S2 86.10 build's equivalent sites (RE_COPY_SITES.md finds none
   unchecked, so no reopening there).

## Repro

```
python3 recon/nulcarrier/nulcarrier.py 192.168.4.84 table --pace 60 \
    --jsonl recon/nulcarrier/cap84.jsonl        # 35-probe cap table
# byte-precise bisects for SetAVT/Add: 1019 OK / 1020 error (3s spacing)
# CreateSavedQueue Title probe: plain Title arg (NOT dc:title); SQ:16 = 3000 B
```

Artifacts: `recon/nulcarrier/cap84.jsonl` (35 rows), `recon/nulcarrier/
cap84_summary.json`, harness `recon/nulcarrier/nulcarrier.py`. Unit settled
after the run: `Stop` + non-HLS mp3radio URI per RE_LESSONS.
