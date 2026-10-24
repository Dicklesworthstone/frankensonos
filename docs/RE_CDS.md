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
