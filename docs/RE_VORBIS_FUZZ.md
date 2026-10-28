# RE_VORBIS_FUZZ — libVorbis 1.3.7 decode path (bead frankensonos-d7vi)

Target: libVorbis 1.3.7 ("Xiph.Org libVorbis I 20200704", confirmed by anacapad
strings) statically linked into the 57.23 anacapad. Decode path is pre-auth
reachable: `SetAVTransportURI` + `Play` with an attacker-chosen stream URL
(delivery vehicle proven in `RE_HLS_EXPLOIT.md` / h14 lane; the same HLS/HTTP
source path). This lane is OFFLINE FIRST: host ASan harness on libvorbis 1.3.7
exact source; device verdicts are a later, gated step.

## Method

1. **Upstream diff** (`~/repos-vorbis-d7vi/vorbis`, v1.3.7..main, 23 commits
   touching `lib/`): the decode-path gaps in 1.3.7 that upstream later fixed:
   - **T1 — `vorbis_book_init_decode` alloca storm** (upstream 28965ede):
     `alloca(sizeof(ptr)*used_entries)` + `alloca(n*4)` sized by used_entries,
     which is attacker-controlled. The only bound is
     `ov_ilog(dim)+ov_ilog(entries) <= 24` (dim*entries shape), which does NOT
     bound used_entries alone: dim=1 allows up to 2^22 used entries →
     ~12 MB of alloca per codebook on the thread stack.
   - **T2 — `_vp_psy_init` half-octave off-by-one read** (upstream 315da9cc):
     `halfoc` clamped to `P_BANDS-1` but interpolation indexes `P_BANDS`
     (weight 0 — an over-read of one float at the array end). Needs high
     sample rates.
   - **T3 — codebook fastpath hint under-shift** (upstream e3c9861f): the
     firsttable holds 15-bit lo/hi hints; with >32k used entries the hints
     under-shift and the binary search runs on a wrong range (upstream treats
     it as a perf bug; checked for OOB anyway).
   - Also from the diff: NULL-check gaps on `_ogg_malloc` in
     `vorbis_book_init_decode` (a629068d — OOM-only), `ov_pcm_page_seek` UAF
     (1b75110b — vorbisfile seek path, not used by an in-process decode),
     large-alloca in `ov_crosslap` (7ef6726d — vorbisfile, not the
     anacapad-style push decode).
2. **Harness**: `vorbis_decode_asan.c` — the same call sequence anacapad runs
   (`vorbis_synthesis_headerin` x3 → `vorbis_synthesis_init` →
   `vorbis_synthesis`/`blockin`/`pcmout`), linked against libvorbis **1.3.7
   built from source with ASan** (`~/repos-vorbis-d7vi/inst-asan-apple`,
   Apple clang; the Homebrew llvm clang's ASan runtime deadlocks at init on
   this box — use `/usr/bin/clang`).
3. **Corpus**: `gen_corpus.py` — bit-exact Vorbis header construction (LSB-first
   bitpacker, correct Ogg CRCs, correct page layouts). 77 cases: the three
   targets above, grammar cases (absurd channels/rates/blocksizes, truncated
   headers), 60 seeded mutations of a valid stream. Building syntactically
   valid setup headers took iteration: sparse-flag second bit, class_subbook
   byte read even when class_subs=0, book count stored as count-1, the
   `ov_ilog(dim)+ov_ilog(entries)<=24` bound (dim=1 caps entries at 2^22).

## Results (`results.jsonl`, ASan, 2026-10-28)

**81 CLEAN, 2 ASAN_CRASH — both T1, both confirmed upstream-fixed bugs.**

| case | entries (dim=1) | verdict |
|---|---|---|
| t1_alloca_e219 | 524288 (~6.3 MB alloca) | CLEAN |
| **t1_alloca_e220** | 1048576 (~12.6 MB alloca) | **stack-overflow** |
| **t1_alloca_e222** | 4194304 (~50 MB alloca) | **stack-overflow** |

```
#0 __asan_alloca_poison
#1 _vds_shared_init block.c:240   <- vorbis_book_init_decode call
#2 vorbis_synthesis_init block.c:709
```

Boundary measured on the host (8 MB main stack, ASan overhead): <=2^19 used
entries clean, >=2^20 overflows. On-device the decode context differs (PPC
32-bit, smaller pthread stacks, no ASan) — a 2 MB+ alloca per codebook on a
smaller stack is expected to fault, and multiple huge codebooks in one setup
header multiply the demand. **This is the device-verdict candidate: a single
~100-byte `.ogg` whose setup header declares one dim=1, 2^22-entry codebook
drives a multi-MB alloca in the pre-auth decode path.**

- **T2**: 96k/192k/384k Hz streams reach full decode CLEAN. The off-by-one
  needs the psy tables reached through a mode with specific blocksize/rate
  combinations; not yet triggered. The upstream fix is a one-index clamp with
  zero weight — impact is a 4-byte over-read at worst even when hit.
- **T3**: 32769/65536/100000-entry codebooks decode CLEAN under ASan (no OOB
  in the hint-search path). Confirms upstream's "perf, not memory safety"
  classification for the non-hint-shifted search.

## Honest assessment

- T1 is a real, upstream-acknowledged stack-exhaustion/stack-clash-shape bug in
  the 1.3.7 decode init path, reachable pre-auth with a tiny file. It is a
  **crash/DoS candidate**, not (yet) a controlled PC primitive: alloca in
  libVorbis doesn't write attacker data to the overflowed region (ASan poisons;
  without ASan the behavior is platform-dependent stack clash).
- The falsifier branch ("100+ cases, no daemon crash distinct from known
  behavior") is NOT yet satisfied device-side — offline we HAVE a distinct
  crash class. The device step (serve t1_alloca_e222.ogg through the HLS
  vehicle, watch SWGen/transport) is the next gated action.

## Reuse

```
# one-time (this host): build ASan libvorbis 1.3.7
cd ~/repos-vorbis-d7vi/v137 && cmake -DCMAKE_POLICY_VERSION_MINIMUM=3.5 \
  -DCMAKE_C_COMPILER=/usr/bin/clang -DCMAKE_C_FLAGS="-g -O1 -fsanitize=address" \
  -DBUILD_SHARED_LIBS=OFF -B build-asan && make -C build-asan -j8 install

python3 recon/vorbisfuzz/gen_corpus.py corpus
python3 recon/vorbisfuzz/run_corpus.py            # -> results.jsonl
```

Device-verdict candidate file: `corpus/t1_alloca_e222.ogg` (94 bytes of setup,
~12-50 MB of alloca demand in `vorbis_synthesis_init`).
