# :1443 JSON — where a parsed length goes, unchecked

Lane LilacRaven. Binary: S2 86.10 `anacapad`. `.text` is at `0x466d80` (file offset
`0x66d80`), so vaddr = file offset + `0x400000`; `.data.rel.ro` is at `0x6f17e0`
(offset `0x2e17e0`), so vaddr = offset + `0x410000`.

## The reader

`RMuseController::parseCommand(std::string&, bool)` at **`0x4ead2c`** is the :1443
frame reader. It loops `read(buf, 0x1000, 10)` into `auStack_11a0[4096]`, appends
each chunk to a `std::string`, then hands the whole string to a rapidjson
`Reader` with a `sonos_json::ReaderHandler` sink. The frame itself is therefore
bounded; the interesting lengths are the ones the *scanner* produces.

## The unchecked length

`ParseStringToStream` at **`0x4ea888`** is rapidjson's string scanner. It counts
decoded characters in `local_24` and, at the closing quote, hands the handler

```
0x4eaaa0: iVar2 = *(int *)(local_28 + 0xc) - local_24;   // rewind
0x4eaaa4: *(int *)(local_28 + 0xc) = iVar2;
0x4eaaa8: pcVar3 = String or Key
0x4eaab4: iVar2 = (*pcVar3)(param_3, iVar2, local_24 - 1, 1);   // (str, len, copy)
```

`local_24 - 1` is the decoded length and nothing compares it to anything before
the call. `sonos::json::ReaderHandler::Key(char const*, unsigned int, bool)` at
**`0x5f41a8`** forwards it unchanged to the bound handler
(`uVar1 = Key(this_00, param_1, param_2, param_3)`), so the length reaches the
sink with no validation anywhere on the way.

## The sink

`RMuseController::execute(Header const*)` at **`0x4eb1a0`** (the `0x4eb199` in
earlier notes is the `extend` prefix of the MIPS16 prologue).

**Corrected 2026-10-28 (pane omp_2): the `0x2500`/`0x2800` copies do not exist.**
Every `li` of a size register in the whole function (`0x4eb1a0..0x4ec400`) is one
of `0, 1, 4, 37, 40, 16, 5, 4, 6, 255`, and `this+0x6b` is never addressed in it.
The two fixed copies that are actually there:

```
4eb530  lw   a0,16972(v0)       ; *(this+0x424c)
4eb534  lw   v0,0(a0)           ; vtable
4eb536  lw   v0,32(v0)          ; slot 8
4eb538  jalr v0
4eb53c  addiu a0,sp,9660        ; dest
4eb542  move  a1,v0             ; src
4eb54a  li    a2,40
4eb54c  jalr  memcpy@plt
```

and, immediately before it, `RCorrelationInfo::operator=` at `0x59f394`
(`li a2,37`). Both are fixed on both sides; the destination of the 40-byte copy is
the 40-byte field at `+40` of the `RCorrelationInfo` at `sp+9620`.

`*(this+0x424c)` is the controller ctor's `a3`, i.e. **`RMuseAuth const&`** — the
MIPS16e save layout puts `a0-a3` in the argument home area at `80..92` of the
ctor frame and the stack arguments at `96..108`, and the ctor's stores match that
one for one (`sp+88 -> +0x4248`, `sp+92 -> +0x424c`; `sp+96/100/104/108` are the
context, the `optional<reference_wrapper>`, the namespace factory and the
`unsigned`). So the 40-byte copy is a virtual on the **auth** object, not on
parsed JSON, and a JSON string length cannot drive it.

Consequence: the failure mode "a parsed string longer than a fixed destination"
has no fixed destination to land in on this path. The u16 length is still
unvalidated, but every consumer of it is a `std::string` (heap-backed), and the
`Authorization` value is split into one as well (`RAuthSplitToken`, `0x5f8e88`,
`std::string::assign`).

## Caveat (superseded)

Ghidra renders the two constants as `0x250000` and `0x280000`; the instruction
stream has neither. See `recon/jc7/findings2.md` for the full instruction-level
audit and for the handshake gate that still stands between the wire and this
function.
