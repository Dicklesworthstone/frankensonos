# RE_UPDATER — live lane log

One lane of the 2026-10-24 root swarm. Facts with addresses/commands only; merge into FIRMWARE_PATHS.md when the lane closes.

## 2026-10-23 · zero/short signature and record-flag probes (negative, orchestrator)

On .202, prefix byte-identical, whole file fetched (byte-counted server), version unchanged in
all four:

| change | fetch | back up | verdict |
|---|---|---|---|
| type-17 `siglen` 0x100 -> 0 (record + ring lengths fixed, file truncated) | full 5.6 MB | 14.7 s | reject |
| `siglen` -> 1, one signature byte kept | full | ~15 s | reject |
| `siglen` 0 + record flag 0 | full | ~15 s | reject |
| `siglen` 0 + record flag 2 | full | ~15 s | reject |

So a signature is mandatory and the record-side flag word does not waive it. The remaining
malleability question is only the enveloped trailing section (hash boundary is before the
type-17 record; on-device the plaintext tail failed at envelope parse).
