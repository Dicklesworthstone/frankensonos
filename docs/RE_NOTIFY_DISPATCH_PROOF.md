# /notify dispatch: Unicorn emulation PROOF of attacker-controlled fn (2026-11-04)

## Setup
Unicorn PPC32 BE emulation of the /notify dispatch (singleton at 0x10545dc8,
vt = *(singleton), fn = *(vt+0x2c)), with the module arena simulated at a
mapped address and the request-record bytes written to singleton+1 (the
bump position after the 1-byte singleton).

## Results

| scenario | vt | fn = *(vt+0x2c) | meaning |
|---|---|---|---|
| fresh zeros | 0 | NULL deref at ~0x2c | the baseline crash |
| attacker URL bytes ("http...") | 0x68747470 ("http") | unmapped read at 0x6874749c | **URL bytes ARE interpreted as the vtable pointer** — the first 4 bytes of any parsed string that lands at record offset 0 become vt |
| descriptor pointer (heap) | arena addr | 0 (fresh) | parser-written pointer = the clean case |
| **vt → controlled arena** | **arena+0x100** | **our written value 0x2028a6b4** | **PROOF OF CONTROL: if vt points at memory we wrote, fn is whatever we placed at vt+0x2c** |

## The exploitation chain
1. Pre-traffic: send :1400 requests whose parsed fields leave attacker bytes
   in the session arena (the strlcps at +1748/+1811/+2322 write URL/path/UUID
   data into the session ctx).
2. The next allocation from the shared bump pointer places those bytes at
   singleton+1 (the vt source).
3. The /notify handler reads vt = those bytes and fn = *(vt+0x2c).
4. If the bytes form a pointer into controlled memory (e.g. a prior
   allocation's address, or the session's own data), fn = attacker-chosen.
5. fn is called with (this, sid, seq, nts, nt) — all four attacker strings
   in registers r4–r7. That is a full RCE gadget setup in the root daemon.

## What's left
- Identify which parser writes a pointer (vs inline data) at record offset 0
  (the descriptor entry #1 flags 0x0205 = indirect + array).
- Shape the pre-traffic so that pointer points at a region with 0x2c bytes
  of attacker data.
- Device: fire the shaped sequence + capture the fault address from the
  watchdog log (or infer it from the recovery timing pattern).
