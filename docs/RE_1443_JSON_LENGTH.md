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

`RMuseController::execute(Header const*)` at **`0x4eb199`**. Two copies of parsed
data with fixed sizes and no check:

```
strlcpy(this + 0x6b, local_60[0], 0x2500)                       // decompiled
memcpy(&local_144, <virtual call at *(this+0x424c)+0x20>, 0x2800)
```

Both destinations are small and both sizes are fixed, so the *content* length is
what decides whether they fit:

- `this` is a 34148-byte (`0x8564`) object, allocated on the caller's stack
  (`li a1, 34148` at `0x48330a`, ctor `0x4e8458`, `execute` called at `0x48333c`).
  `this+0x6b + 0x2500 = 0x256b`, inside the object.
- `execute`'s own frame is `0x7fe8` (`save` with frame `0x7fe8` at `0x4eb199`).
  `local_144 + 0x2800` is inside the frame.

So both fit at the fixed sizes, and the failure mode is a parsed string longer
than the destination — which `strlcpy` truncates silently and `memcpy` does not.

## Caveat

Ghidra renders the two constants as `0x250000` and `0x280000`. Only the shorter
form is consistent with the measured frame (`0x7fe8`) and object (`0x8564`)
sizes, so the constants are recorded here as `0x2500`/`0x2800`; the exact
immediate should be confirmed from the instruction stream before either is called
a bug. The `memcpy` is the one that matters: `strlcpy` is bounded by the source
NUL, `memcpy` is not.
