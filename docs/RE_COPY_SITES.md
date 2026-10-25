# Copy-site audit: packet-length `memcpy`/`memmove` in `anacapad`

Static pass over the S2 86.10 `anacapad` (sha256 `1b598f11…`), 2026-10-25,
lane DustyBison. 472 `memcpy`/`memmove` call sites; 46 take their length from
memory rather than a constant. The rest are C++ string/vector internals with
their own capacity logic.

Ranked by packet exposure. "Bound" means an instruction-level check on the
length before the copy, not a comment.

| # | site | copy | bound |
|---|---|---|---|
| 1 | `0x4c04a0` | `memcpy(obj+0x154, packet, u32be(packet+0x20))` — registration signing key | **present**: `sltiu v0,33` at `0x4c0442` rejects `len >= 33`; the destination is the 32-byte field zeroed by `ZZ0038(obj+0x154,32)` at `0x4c0474` |
| 2 | `0x4f38ce`, `0x4f3924` | devmode payload frame, length from `obj+0x408` (u8) | **present**: `memset(local_118,0,0xff)` at `0x4f38bc` plus a `max()` clamp; a u8 length cannot exceed 255 in a 256-byte frame |
| 3 | `0x4f1fce` | ring writer, `memcpy(dst, src, min(requested, free))` | **present**: `subu`/`sltu` at `0x4f1fbe`–`0x4f1fc4` |
| 4 | `0x4c036e` | `memcpy(caller_buf, obj+0x154, *(obj+0x174))` — signing-key getter | **absent in the callee**; safe only because the writer at #1 caps the stored length at 32 |
| 5 | `0x4f43be` | `memcpy(s1, sp+32, *(sp+100))` — hash slice | **indirect**: the length is `*(104(sp))-4`, already checked against the available bytes by `sltiu 4` at `0x4f437a` |

## What is not here

- The registration path (#1) is S2-only; 57.23 has no `Registration signing
  key cleared` string and no equivalent copy.
- `FUN_004f9818` (telemetry ring) and `FUN_004c0420` were already settled in
  `RE_DEVMODE.md`; #1 is the same function as the latter, re-checked here with
  the actual guard instruction.
- No unchecked packet-length copy into a fixed-size buffer was found in this
  pass. The remaining memory-sourced sites are `std::string`/`std::vector`
  growth (`0x4cd334`, `0x5b54a6`, `0x5bc904`, `0x5eef0a`, `0x5f6f9e`) or
  hash/`snprintf` outputs whose length is a digest or a bounded format width.

## Method

```
mips-linux-gnu-objdump -d -m mips:32 captures/firmware/s2_overlay/bin/anacapad > /tmp/anacapad.dis
# every call site, then keep those whose a2 is set by lw/lbu/lhu in the 14 instructions before it
grep -nE 'jal.*<(memcpy|memmove)@' /tmp/anacapad.dis
```

The binary is MIPS16 mixed with MIPS32; `r2` cannot decode it, `objdump -m
mips:32` can. Literal pools sit inline in `.text`, so a disassembly window that
starts inside a pool decodes garbage — start at a `save` instruction.
