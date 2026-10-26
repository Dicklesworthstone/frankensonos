# RE_CDS — ContentDirectory Browse/Search argument shapes

S1 `.202` (57.23) first, then the SortCriteria/Filter subset on S2 `.86` (86.10). `Browse` at
`/MediaServer/ContentDirectory/Control`, unauthenticated. 20 cases on S1, 9 on S2, liveness `/info` before
and after each; bodies under `/tmp/cds_*.bin`; runner `captures/research/cds_fuzz.py`.

## S1, 20 cases

| group | cases | result |
|---|---|---|
| Browse control, `ObjectID` = `SQ:`, `A:`, 4 KB of `A` | 5 | 200 (`A:` returns 2 865 B, the 4 KB id 701) |
| `ObjectID` empty | 1 | 500 / **701** (Invalid ID) |
| SortCriteria `+dc:title`, `-dc:title`, 100 criteria, 4 KB | 4 | 200 |
| Filter `*`(control), 1 000 fields, 4 KB | 3 | 200 |
| Filter garbage `<<>>,,;;` | 1 | 500 / **402** |
| `StartingIndex` 2^31-1, `-1`; `RequestedCount` 2^31-1, `-1` | 4 | 200 |
| Search control, empty criteria | 2 | 500 / **401** |

So the Browse parser is flat: every oversized field (4 KB ObjectID, 4 KB SortCriteria, 1 000-field and 4 KB
Filter) is accepted, and the integer fields tolerate both saturated and negative text. Only an empty or
unknown ObjectID is rejected, with the correct codes.

## Search is not a declared action, and the 401 path does not drain the body

`Search` is **absent from both SCPDs** (`.202` and `.86` both `grep -c "<name>Search"` → 0), so 401 is the
correct answer — but on S1 the 401 arrives *before* the body is consumed, and the unread bytes turn the
close into an RST:

| Search body | S1 | S2 |
|---|---|---|
| 451 B control | 500/401 clean | 500/401 clean |
| 1 450 B (100 SortCriteria) | **RST 2/3** (once clean 401) | clean |
| 4 528 B (4 KB criteria) | **RST 3/3** | clean |

Daemon alive after every RST on both builds, and the next request is served normally — a connection-level
artifact, not a crash. It is the same close-with-unread-data shape as the `/notify` early close, here caused
by action dispatch rather than a read limit, and **S2 differs**: it drains the body before closing even for
an undeclared action.

## S2 subset (SortCriteria/Filter)

Nine cases: sort `+dc:title`, `-dc:title`, 100 criteria, 4 KB; filter 1 000 fields, 4 KB, garbage; Search
control, Search 4 KB. All 200 (or 402 for the garbage filter), 9-24 ms, no RST anywhere.

## Verdict

Both parsers are bounded and benign. Nothing reset, crashed or wedged on either build; the whole matrix is
negative, with the S1-vs-S2 difference confined to how the 401 path handles an unread body.

## 713 is "Restricted parent", and the daemon hard-codes it for `S:`

CreateObject answering 713 for `SQ:`, `SQ:0` and `SQ:1` is not a DIDL problem. CDS:1 defines **713 =
Restricted parent** (`UPnP-av-ContentDirectory-v1-Service.pdf`, the error table repeated for CreateObject,
DestroyObject, ImportResource, ExportResource and TransferResource — `pdftotext` line 2857 etc.).

The daemon agrees, and the check is a literal string compare:

```
101550fc  stwu 1, -16(1)
10155100  lis 4, 4164 / addi 4, 4, 1836      ; 0x1044072C = "S:"
10155114  addi 3, 3, 1041                    ; obj+1041 = the parent id
10155118  bl 0x10546270                      ; strcmp
10155124  li 3, 713                          ; equal -> 713, unconditionally
1015512c  lwz 3, 4(31) / bl 0x101541d0       ; otherwise continue
```

So `S:` — the Saved Queues root — is rejected as a parent by construction. `SQ:` and `SQ:0` do not match
that compare, so their 713 comes from the other two sites (`0x100ffb80`, a status→error mapping table, and
`0x101686b8`, a default error path) and is still a restriction verdict, not a parse failure.

Consequence: stop guessing ids. Browse the parent first and read the `restricted` attribute on each child;
CreateObject only succeeds under a container whose `restricted` is `0`. The Elements DIDL is not the
variable to fix.

## 811 on CreateSavedQueue is an empty-source verdict, and the argument order is in the SCPD

811 is **undocumented**: the community custom-error table for AVTransport stops at 800 (`701`–`718` are
the spec codes, `737`–`739` and `800` the vendor ones; there is no 811).

The device's own service description does give the exact signature, in order
(`opt/htdocs/xml/AVTransport1.xml`):

```
InstanceID  Title  EnqueuedURI  EnqueuedURIMetaData   (in)
NumTracksAdded  NewQueueLength  AssignedObjectID  NewUpdateID   (out)
```

and `A_ARG_TYPE_SavedQueueTitle` is a plain `string` with **no declared max**.

In the binary the handler is `0x101dfdd8` (frame 22 736; it builds a `"%s/%s"` path at `0x101dfe38` and
calls `0x10216acc` with limit 577). The 811 return is:

```
101e02a0  lwz 9, 92(31)          ; a string field
101e02a4  lbz 0, 0(9)            ; first byte
101e02a8  cmpwi 7, 0, 0
101e02ac  bc 13, 30, …           ; non-empty -> skip
101e02b0  lwz 0, 112(31)         ; a flag
101e02b4  li 30, 811             ; empty string + flag set -> 811
```

So 811 is a **content** verdict raised on an empty string field, not a parse or DIDL failure — the same
shape as 713 in that respect. The field is read as the queue source; supplying a real `EnqueuedURI`
alongside the metadata is what clears it. The sibling branch at `0x101e0298` returns 803 for the other
case, so 803 and 811 are distinct verdicts and should not be conflated.

Note the handler is only valid on the **coordinator** (`.84`, not `.202`), which is why `.202` 800s.
