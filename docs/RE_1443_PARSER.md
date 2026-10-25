# RE_1443_PARSER — lane log (:1443 request/JSON path, pre-auth)

Lane SapphireIvy (bead `frankensonos-jc7`). Target: a copy/bounds bug reachable **before** the
Bearer check in the :1443 API. Binary: `captures/firmware/s2_overlay/bin/anacapad` (86.10,
MIPS16; Ghidra project `captures/research/ghidra`). Read-only, no hardware.

## The gate this lane sits in front of

`0x4aa99c` is the API-key check: `memset(sp+24,0,32)`, `strlen(input)`, `ZZ0058(1, input, len,
sp+24)` (SHA-256), then `memcmp(sp+24, 0x676ea8, 32)` and `memcmp(sp+24, 0x676e88, 32)`, return
1 on either match, 0 otherwise. The two 32-byte constants are the ones DustyBison is inverting
(`frankensonos-c08`). Everything below runs before that check.

## Request-body path (read so far)

```
0x4ece50  body-read driver
  4ece64  lbu a0,32(this)            ; mode flag
  4ece72  vtable -28096(gp)          ; shouldRewriteBody()
  4ece86  vtable -28084(gp)          ; readFromNetwork(this, buf, len, timeout)
  4ece94  used = *(this+0x18) - *(this+0x14)     ; vector end - begin
  4ece9c  wanted = *(this+8)
  4ecea0  sltu used, wanted ; btnez -> error path when used < wanted
```

`MuseRESTStream::readFromNetwork` (`0x4ecd28`, decompiles cleanly):

```c
if (shouldRewriteBody() == 0)
    vtable[+8](this->sock, this->buf, param_1, param_2, param_3 * 1000);   // no resize
else {
    if (*(this+8) == 0) { resize(this+0x14, param_2 << 1); vtable[+8](..., param_1, param_2, 0); }
    else                { resize(this+0x14, *(this+8) << 1); readContentLengthBytes(this, param_3); }
    resize(this+0x14, ret);
}
```

`readContentLengthBytes` (`0x4ec1f0`) polls with `n * 1000` ms and loops `read()` into
`*(this+4) + *(this+20) + done` while decrementing the remaining count — the loop itself is
bounded by the caller's length.

## Candidate bug shape (not yet proven)

The `shouldRewriteBody() == 0` branch reads `param_2` bytes into `param_1` **without** resizing
anything, so the bound is whatever the caller guarantees. The open question is exactly where
`param_1`/`param_2` come from and whether `param_2` is the raw `Content-Length` while `param_1`
is a fixed buffer. The body-read driver `0x4ece50` is reached through a vtable, so the caller
has not been located yet.

## Next reads

1. Find the vtable slot that reaches `0x4ece50` and its caller; trace `param_1`/`param_2`.
2. Check whether `Content-Length` is capped before the read (search the header parse for a
   limit constant) and whether a negative/oversized value reaches `resize`.
3. Then the JSON body parser itself (the object/sajson-style consumer) for its own bounds.
