# jc7 second pair of eyes — findings (PinkMill pane, 2026-10-27)

Scope: verify SapphireIvy's open conclusions on the :1443 JSON path against the
shipped binary (`captures/firmware/s2_overlay/bin/anacapad`, 86.10 MIPS32 BE,
disassembled with `mips-linux-gnu-objdump -d -m mips:isa32`). Three checks.

## 1. The bindStdString u16 truncation is SELF-CONSISTENT — the hypothesized
##    overflow shape does not exist

The lane established `FUN_005f1890` stores the JSON string length as a
halfword (`0x5f189a sh a3,0x18(a0)`) and hypothesized a copy that uses the
original length into a buffer sized from the truncated field. Instruction-level
check says otherwise:

- The descriptor has **no 32-bit length** (full store list at `0x5f1890`:
  `+0x0/+0x4/+0x8/+0xc/+0x10` are the type word, `+0x18` is the only length,
  `+0x1a/+0x1b/+0x20/+0x21` single bytes). After the `sh`, the original length
  is gone — no copy can use it.
- Every consumer found loads the **same halfword**: `0x48068a lhu a1,24(a2)`
  feeds `a1` directly into the binder stub `0x51b9b4` call at `0x4806a2`;
  the other `lhu ...,24(...)` sites (`0x475b52`, `0x477a86`, `0x4c6166`,
  `0x4c850a`, ...) follow the same pattern.
- Consequence: copy length == stored length == wrapped value. A 65 536 + n
  byte JSON string copies n bytes — truncation, never overrun. The u16 wrap
  only ever *loses* bytes.

**Verdict: the u16 truncation is not an overflow primitive on this build.**
It is a correctness bug (long strings silently truncated), not memory safety.

## 2. Correction: the "34148-byte object" is a misread of pointer arithmetic

`docs/RE_1443_JSON_LENGTH.md` and `RE_1443_PARSER.md` both reason from "the
34148-byte object allocated on the caller's stack". The construction site
(`0x4832e0..0x483342`) shows:

```
48330a  li   a1,34148
483328  addu a1,s1,a1        ; a1 = s1 + 34148  <- buffer END pointer argument
48332a  addiu a0,sp,40       ; the controller object is at caller sp+40
483334  jal  4e8458          ; RMuseController ctor(..., end, ...)
48333c  jal  4eb1a0          ; execute(this = sp+40)
```

`34148` is a **buffer-size argument** (end pointer for the stream the
controller wraps), not an allocation. The controller object is ~40 bytes of
caller frame, and `execute(this = sp+40)` runs immediately after. So the
doc's justification "both destinations fit inside the object" is void:

- `strlcpy(this+0x6b, src, 0x2500)` writes up to `0x250b` bytes from
  `sp+40+0x6b` — into the **caller's** frame. That frame is large (locals to
  `sp+17220`), so it likely still fits, but for a different reason than
  stated, and only if the caller frame really is the one observed.
- `memcpy(local_144, producer, 0x2800)` remains bounded only by the frame.

## 3. The one live candidate left: the memcpy producer buffer

`execute`'s `memcpy(&local_144, <result of (*(this+0x424c))->vtable[+0x20]>,
0x2800)` is fixed-size on both the copy and the destination. Its safety
depends entirely on the **producer's buffer being >= 0x2800**. The producer is
`*(this+0x424c)` — set in the ctor from stack arguments (`sw v0,16972(s0)` at
`0x4e85ee`, values from `sp+88`/`sp+92`), i.e. the identity of the producer
object is caller-supplied and unresolved. If any concrete producer's backing
buffer is smaller than 0x2800, this memcpy is an OOB **read** feeding a
fixed-size copy — the shape worth naming.

Also unverified at instruction level: the constants `0x2500`/`0x2800` vs the
Ghidra-rendered `0x250000`/`0x280000` (RE_1443_JSON_LENGTH caveat). Neither
form appears as a plain immediate in the `0x4eb1a0..0x4ec200` disassembly —
they are MIPS16 composite `li` sequences; confirming them needs a correct
MIPS16-aware decode, not a raw immediate grep (checked: no `,0x2500`,
`,0x2800`, `,0x250000`, `,0x280000` operands anywhere in the full
disassembly).

## Residue for the lane

1. Identify `*(this+0x424c)`'s concrete type from the `0x48330a` caller's
   arguments (sp+88/sp+92 at the ctor call) and its buffer size; compare
   against the `0x2800` memcpy.
2. MIPS16-aware decode of the two copy sites to pin the exact size constants.
3. The websocket-gate question (`updateHeaderTargets` refusing before the
   protocol header) is untouched by this pass and still gates all JSON
   reachability on the wire — it remains the practical blocker for *any*
   :1443 JSON claim.
