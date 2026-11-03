# jc7 — the two latent weaknesses, fired (and the arg-plumbing premise corrected)

Pane omp_2, 2026-11-03. Target `.86`, 86.10-81100. Runner `recon/jc7/weak_fire.sh`
(+`weak_fire2.sh`, `weak_fire4.sh`), logs `weak_fire{,2,3,4}.tsv`. 3 s spacing, an
independent 0.1 s `:1400/status/VERSION` poller, never `hls_det`.

## 0. The premise was partly wrong: arguments DO reach SOAP

`docs/FIRMWARE_PATHS.md` and the earlier passes concluded the passthrough was
read-only because every arg-carrying action answered UPnP 402. That was an **arg-name**
error, not a plumbing gap:

* `GetString`'s in-argument is **`VariableName`**, not `StringVariable`
  (`/xml/SystemProperties1.xml`, action `GetString` = `[VariableName in, StringValue out]`).
* With the right name the whole chain works and is visible end to end:

```
POST .../upnpSystemProperties
{"method":"SetString","input":[{"name":"VariableName","value":"jc7z"},
                               {"name":"StringValue","value":"ok"}]}   -> 200 result 0
{"method":"GetString","input":[{"name":"VariableName","value":"jc7z"}]} -> 200 value "ok"
```

* The element shape is `input: [{name,value}]` (a map, a bare list and a typed `name` all
  fail: `Parsing terminated:input.name`). Unknown keys (`args`, `params`, `arguments`) are
  tolerated and silently dropped — which is why they all still answered 402.
* Adjacent evidence that the args arrive: `SetString` with no `input`, with only
  `VariableName`, or with a bogus name all answer **402**; with both valid names it answers
  **200**.

## 1. T1 — XML escaping: the proxy DOES escape (the static reading was wrong)

The marshaller at `0x509d4c` genuinely formats `<%s>%s</%s>` with raw `%s`, but the value is
escaped before it gets there. Proof is the direct/proxy contrast on the **device's own
parser**, which is strict:

| path | `StringValue=<x>` | `StringValue=a&b` |
|---|---|---|
| direct SOAP `:1400/SystemProperties/Control`, raw | **402** | **402** |
| `:1443` `input[].value` | 200, read-back `<x>` | 200, read-back `a&b` |

12 metacharacter values were round-tripped through `SetString`→`GetString`; every one came
back byte-identical, rc `0`, liveness 200 after each. Sent as JSON `input[].value`:
`<x>`, `a&b`, `a&b;c`, `"q"`, `'q'`, `<a/>`, `]]>`, `&#0;`, `<`, `a<b>c&d"e`, `a<TAB>b`,
plus `plain`.

Escape-specific results that pin the encoding:

| value | read-back | reading |
|---|---|---|
| `&` | `&` | single escape, no double-encoding |
| `&amp;` | `&amp;` | escaped to `&amp;amp;`, device decodes once → `&amp;` |
| `</StringValue><StringValue>injected` | identical literal | element injection does **not** break out |
| `a<TAB>b` | `a b` | space-folded — but the **direct** path folds it too, so it is the property store, not the proxy |

**No second-order `:1400` parser hit exists.** Escaping is complete for text content. (Gap
that remains, untested because it needs a 35 kB argument: element *names* are identifiers and
are not escaped, and the character data is only as safe as the escaper — the truncation in §2
is the one way a value can end up split.)

## 2. T2 — size under-request / grow-check underflow: real, exercised, still latent

The request is `2*len(value)+16` while the element needs `2*len(name)+len(value)+6`, and the
cursor is advanced by `snprintf`'s **full** return. `ensureCapacity` (`0x509c14`) tests
`end - cursor` unsigned, so once the cursor is past the end the subtraction underflows to
~4.29e9, `sltu` fails, and the grow path is skipped. The inline buffer is 34148 B
(`li v1,34148` at `0x509c44`), so live writes walk outside the object.

Exercised on `.86` (all answered UPnP **402**, which is the correct verdict for unknown
element names and does prove the action resolved and the body parsed):

| shape | body | cursor walked |
|---|---|---|
| 70000-B name + 1-B value × 1/2/3/5/10/20/50/100 | 140 kB … 7.0 MB | 140 kB … 14 MB |
| 500000-B name + 1-B value × 1/20 | 0.5 / 10 MB | 0.5 / 10 MB |
| 200000-B name × 5 (earlier pass, re-run) | 1 MB | 1 MB |
| short-name controls × 1/5 | tiny | tiny |

Result: **0 faults, 0 daemon deaths.** The 10 Hz poller stayed 200 throughout the worst
case (47/47 samples across the 100-arg 7 MB body — reproduced standalone, since the poll
file of the first two batches came out empty, a harness bug); every batch also ends with an
explicit liveness probe, all 200. After every batch `GetZoneAttributes` still returned
live values and a normal `SetString`/`GetString` round-trip still worked.

The one *visible* consequence of the same bug is on the **first** element only: its
`snprintf` maxlen is `end-cursor+1` = 34149, so a first element taller than 34148 B is
truncated. It is masked here because the SystemProperties store caps a value at **2047 B**
(70000 `a`, 70000 `<` and 200000 `<` all stored exactly 2047 B), so a 70000-B value never
reaches the 34 kB wall. A consumer with >34 kB arguments would see it.

## Verdict

* T1: **closed as non-finding** — escaping is present and complete; contrast with the strict
  direct parser is the proof.
* T2: **confirmed latent** — the under-request and the underflowed grow-check are real and
  let the writer run megabytes outside a 34 kB inline buffer; on this build nothing breaks.
  It becomes a bug the moment an action uses an argument larger than the inline buffer.
* Correction to the record: the passthrough is **not** argument-blind; `input:[{name,value}]`
  with SCDP names works, `GetString` needs `VariableName`.
