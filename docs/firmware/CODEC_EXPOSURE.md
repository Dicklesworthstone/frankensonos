# libFLAC 1.3.4 decode-path exposure (defensive assessment)

**Scope:** static analysis of **public, open-source** FLAC history
(`github.com/xiph/flac`) to assess what a decoder pinned at **libFLAC 1.3.4**
is missing on the audio-decode path. Produced as scrubbed prose per the SCOPE.md
reverse-engineering exception (understanding + documentation). No Sonos binary,
firmware image, decompiled code, or device identifier appears here.

## Method

Cloned the upstream repo and read the git history of the decode-path sources
between tag `1.3.4` and `master` (HEAD `e94ff9f6`, 2026-07-19):

```
git log 1.3.4..master -- src/libFLAC/stream_decoder.c \
                         src/libFLAC/bitreader.c \
                         src/libFLAC/format.c \
                         src/libFLAC/metadata_iterators.c
```

91 commits touch those files in that range. Below are the memory-safety /
input-robustness–relevant ones, with the upstream release that first shipped
each fix (`git tag --contains`).

## Fixes a 1.3.4 decoder is missing

| Commit | First released | Class | Hardens |
|---|---|---|---|
| `99cb7046` | 1.4.0 | bitreader state | bitreader left in undefined state on malformed input |
| `4e823662` | 1.4.0 | decode logic | 33-bps constant-subframe mis-decode |
| `46f3f367` | 1.4.0 | null deref | null-pointer deref in `metadata_iterators.c` |
| `cbb039d2` | 1.4.0 | assert/abort | assertion failure in `seek_to_absolute_sample_` |
| `c90b3ea3` | 1.4.0 | integer overflow | overflow in seeking code |
| `bef0c92d` | 1.4.0 | integer underflow | unsigned underflow in seeking code |
| `c65ef589` | 1.4.3 | bounds | continued decoding subframes past invalid data |
| `0e1535cc` | 1.4.3 | validation | invalid blocksize (65536) accepted |
| `1faa4924` | 1.4.3 | validation | 0-track cuesheet treated as valid |
| `ebc2a696` | 1.4.3 | validation | gap in `seektable_is_legal` check |
| `e4f1ee9f` | 1.5.0 | integer overflow | signed overflow in seeking code |

Related, same range: a large set of oss-fuzz-driven hardening and
sanitizer/UB-in-fuzzing commits (`155e206f`, `2b17af19`, `79a164ec`,
`d52921bc`, `63ac1c37`, `9e143766`, `95064f9f`, …) — fuzzing-harness and
UB-annotation changes rather than shipped behavior bugs, listed here only so the
inventory is complete.

## Honest read of the exposure

These are overwhelmingly **robustness / crash-class** defects: undefined
bitreader state, an assertion/abort, integer overflow/underflow in the seeking
path, one null deref, and several "accepted invalid input" validation gaps. The
profile is **denial-of-service / hang on malformed input**, not a demonstrated
remote-code-execution primitive. Nothing in this inventory establishes a write
primitive or control-flow hijack; claiming RCE from this list would be
unsupported.

## Remediation

1. **Update the decoder.** `1.4.3` closes most of the above; `1.5.0` closes the
   remainder. A current libFLAC is the direct fix.
2. **Boundary mitigation** where the bundled decoder can't be updated: do not
   let unauthenticated, unfiltered `SetAVTransportURI` content reach the
   decoder — network segmentation and not exposing the control port to
   untrusted sources. This is the same posture the project already takes toward
   the players (Tailscale fronts the daemon, never the speakers).

## What this assessment deliberately does NOT include

Per SCOPE.md, the RE exception authorizes *understanding and scrubbed-prose
documentation*; it keeps anything that writes to or takes down a device
human-led, with one explicit approval per operation. Accordingly this work
stopped at the assessment above and did **not**:

- craft FLAC files engineered to trigger any of these bugs;
- stage any payload in `smbshare/` or any delivery path;
- deliver anything to, or attempt to crash, a live device.

Those steps are exploit development and a DoS against owner hardware; they are
outside the documentation lane and are the owner's to pursue deliberately and by
hand if ever, not autonomous-agent work.
