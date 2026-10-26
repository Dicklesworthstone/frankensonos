# `:1443` JSON reader: no count/length sink

Cross-check of the local-API JSON path on S2 86.10 `anacapad` (MIPS16, base
`0x400000`). Question: is there a length or count field consumed before
validation, and where does it land.

## The reader is schema-validated, and strings carry an explicit capacity

The API does not hand-parse JSON. It uses rapidjson with
`GenericSchemaValidator` (the validator symbols are at `0x7054a0`–`0x705520`)
and the binding helpers in `sonos::json`:

```
0x5f1428  sonos::json::bindString(RJsonDataBindingInterface&, u8 type, u8 flags, char *buf, u32 cap)
0x5f15e8  sonos::json::bindStdStringArrayAndSize(...)
0x5f1508  sonos::json::bindObject(...)
```

`bindString` takes the destination capacity as its fifth argument and forwards
it to `0x5f13c0`; the store copies behind it use constant `strlcpy` caps
(`li a2,25` at `0x475e60`, `li a2,8192` at `0x477c3a`, `li a2,4096` at
`0x478e10`, `li a2,37` at `0x47afba`). A long string is therefore truncated at
the field's cap, not copied unbounded.

## The `input` array count is an iterator, not a size

`input` is bound as an array of `{name,value}` pairs. The loop is
`0x4f12c2`–`0x4f12ce`:

```
4f12c2  lw v0,84(sp)      ; begin
4f12c4  beqz v0,4f12e8
4f12c8  lw v0,88(sp)      ; end
4f12cc  cmp v1,v0
4f12ce  bteqz 4f1324
```

Both bounds come from the parsed document, so the count is never materialised
into a size and never used to allocate. An `input` with 20 000 pairs costs
memory in the rapidjson pool, not a fixed-buffer overflow.

## Where the value length does land

`0x4f12d0`–`0x4f12de` loads the pair's pointer and length and calls the setter
`0x4ef878(obj, ptr, len)`. That function forwards to vtable slot 2 of the store
object (`lw v0,0(a0)` / `lw v0,8(v0)` / `jalr v0` at `0x4ef8dc`–`0x4ef8e0`)
with the raw rapidjson length in `a2`. That slot is the only place a
request-controlled length reaches a copy, and it is the one to audit if the
store ever grows a variable-cap field.

## Verdict

No count- or length-driven sink was found on this path. The residual risk is
per-field capacity mistakes in the binding table, not the JSON reader itself.
