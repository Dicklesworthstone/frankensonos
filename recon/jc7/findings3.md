# jc7 — the 34148-byte wall, fired (and the reachability bound that hides it)

Pane omp_2, 2026-11-03. Target `.86` (S12, 86.10-81100) for the writes and
`192.168.6.112` (`RINCON_5CAAFD21D47001400`, the same household's coordinator) for the
transport oracle — `.86` is a grouped satellite and answers `1023`/`800` to every
transport mutation, the coordinator answers `0`. Read-only on the LAN, transport restored
afterwards (queue URI back, appended queue item removed).

## 1. The wall, verified at instruction level

`docs/RE_1443_JSON_LENGTH.md` and `findings2` §8 stated the mechanism from a Ghidra render
and a partial read. Verified against the raw bytes this pass:

**The inline buffer is the object's own first 34148 bytes.** The builder is initialised
with its end:

```
490354  lw   v0,16(a0)          ; the 34148-byte block
490356  li   s0,34148
490362  addu s0,v0,s0           ; end = block + 34148
49036a  jal  6000dc             ; muse::OutputStream::initStreamWriter(stream, variant, apiKey)
```

`ensureCapacity(this, request)` (`0x509c14`) then uses `this + 34148` as the end
(`li v1,34148` / `addu v1,a0,v1` at `0x509c44`/`0x509c48`) and the state fields sit 16384
bytes further on (`cursor` `+50532`, `end` `+50536`, mode flag `+50556`, overflowed flag
`+50557`), so the first bytes written past the end land in unused object members, not on
the cursor — which is why the walk survives as long as it does.

**The size test is unsigned and the request is under-computed:**

```
509c24  lw   a2,0(s1)      ; cursor
509c26  lw   v0,0(v0)      ; end
509c28  subu v0,a2         ; end - cursor
509c2a  sltu v0,a1         ; unsigned compare against the request
509c2c  bteqz 509c8e       ; enough -> return
...
509c8e  lbu v0,0(s0)       ; returns the overflowed flag
```

and the element writer (`0x509d4c`) requests `2*len(value) + 8`
(`509d5a` `addiu v0,8` / `509d62` `sll a1,v0,1`) for a format that needs
`2*len(name) + len(value) + 6`, passes `maxlen = end - cursor + 1`
(`509d80`/`509d88`), and then advances the cursor by snprintf's **full return**
(`509d94`–`509d98`).

Consequences, in order: a long element is cut at the buffer end; the cursor lands past
`end`; the next `end - cursor` underflows to ~4.29e9, `sltu` fails, the grow branch is
skipped for every later element; and once the overflow flag is set, `ensureCapacity`
returns "ok" unconditionally. When the mode flag is clear the code does not even try to
grow — it logs and sets the overflow flag (`0x509c5e`–`0x509c6c`).

## 2. Reachability bound: no legitimate argument is that long

A real SCDP argument cannot reach 34148 B on this build — every argument the device
receives is capped well below it, measured directly:

| argument | probe | result |
|---|---|---|
| `SystemProperties.StringValue` | 70000 `a` (prior pass) | stored 2047 |
| `SystemProperties.VariableName` | `C`*34148 + `SIG`, then get `C`*34148 | key stored, suffix irrelevant from 2047 up |
| `AVTransport.CurrentURIMetaData` | 40000 B via `:1443`, read back over **direct SOAP** `:1400` (no proxy on the read) | 4095 B raw, `ENDMARK` lost |
| `ContentDirectory.CreateObject.Elements` | 4410 B / 40410 B | `803` / `712` (favourites store rejects first) |

The `:1443` read-back of `CurrentURIMetaData` saturates at 4095 as well
(`recon/jc7/wall_burn.tsv`: 400 B round-trips byte-identical with `ENDMARK`; 5000 … 70000
all come back at 4095 with the mark gone, `rc 0` throughout), so the truncation is real
but invisible in any stored value. **The 34148-byte buffer is 8x larger than anything the
action table can hand it.**

## 3. The reachable trigger is an argument *name*

The proxy forwards argument names it does not know (it is a free-form name/value list) and
the device ignores them, so an unknown name is the only way to put an element past the
wall. Per case one `SetString` carries `[unknown 34000–500000-B name][VariableName=K]
[StringValue=v]`. Whatever the cut lands on, the two later elements are written past the
buffer, and the device tolerates the unclosed cut element and still binds the trailing
pair — which is the proof that the bytes past the buffer were written and parsed.

`recon/jc7/wall_names.py` → `wall_names.tsv` (10 Hz `:1400/status/VERSION` poller):

| name bytes | set | rc | trailing pair bound |
|---|---|---|---|
| 34000 (fits) | 200 | 0 | yes |
| 34130 (first cut) | 200 | 0 | yes |
| 34160 | 200 | 0 | yes |
| 70000 | 200 | 0 | yes |
| 200000 | 200 | 0 | yes |
| 500000 | 200 | 0 | yes |

`BATCH_END 200`, no daemon death. Poller: 269/275 samples `200`; the six `000`s are
consecutive 2 s curl timeouts during the two largest bodies (the poller blocks on its own
sample, so a gap is one slow read, not a death), and a dedicated run measured a single
**1.03 s** `:1400` status latency while a 500 kB body was in flight. So the availability
effect of the walk is a sub-second-to-seconds stall of the status server, then recovery.

An argument-order check: with the unknown element first and the pair after, the pair binds
both at 40000 (measured) and at 500000, i.e. nothing after the cut element is lost.

## 4. Oracle notes for whoever continues

* `SetString`/`GetString` is a *2 KB* oracle, not a 34 kB one.
* `SetAVTransportURI` + `GetMediaInfo.CurrentURIMetaData` is byte-identical up to 4095 B —
  the best small-value round-trip on the box, but useless at the wall.
* `.86` is a satellite: transport mutations need the coordinator. `playerId` in the path
  must be the *local* UDN — another player's UDN answers `ERROR_INVALID_OBJECT_ID` (400).
* `ContentDirectory.CreateObject` on `FV:2` echoes its `Elements` back in `Result`, but
  rewrites `upnp:class` and rejects long payloads at ~4096 B with `803`.
* `Queue.AddURI` is accepted by the coordinator but the device rebuilds the item from the
  URI, so `EnqueuedURIMetaData` is not an echo.

## Verdict

T2 stays **confirmed latent**, now with the bound: the under-request and the underflowed
grow-check are real and reachable, but only through argument *names*; every legitimate
argument is device-capped at 2047–4095 B, so on 86.10-81100 the wall has no visible
consequence (0 faults to 1 MB bodies, status server stalls ≤ ~1 s). It becomes a real
truncation the moment an action carries an argument above ~34130 B.

Regression for T1 stays green: `recon/jc7/escape_contrast.py` exits 0 — proxy 200 with an
identical read-back, direct SOAP 402 with the same raw value.
