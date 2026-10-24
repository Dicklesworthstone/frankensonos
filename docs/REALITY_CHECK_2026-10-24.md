# Reality check — network-only root lane, 2026-10-24

Question of record: a **pre-auth, network-only root** on the owner's own S1 (57.23) and S2
(86.10) players; no vendor developer key, no manual downgrade flash, no hardware access.
Success = persistent root, or an unsigned/first-party-key-free install that survives a reboot.

Read with `docs/SCOPE.md`, `README.md` and `docs/ROOT_LANE_STATUS.md`. This is the honest check,
not a summary of activity.

## Verdict

**Not root yet.** The lane has moved from "unknown wall" to a small number of *measured* walls and
two primitives, but no state-changing exploit is proven end to end. Claims below are scoped to
what a run actually showed; the audit in the session log flags the ones that are inference.

What changed for the better:

1. **The install path is characterised up to the signature gate, and that gate is closed without the vendor key.**
   Proven by code and by device: the type-17 signature covers the 57-byte list2 region only, but
   that region stores `sha256(file[:type17])` and the updater memcmps the running prefix hash
   against it; any body edit needs both a digest rewrite and a fresh signature. Appended records
   after the type-17 are unsigned, and *every* unused-section variant (types 5/9/10, raw and
   enveloped) is parsed then rejected before pass 2 - the flasher never runs. The rebuilt rootfs
   pipeline is proven end to end on an untouched tree (full read with a zero-IV length-exact
   envelope); a patched tree gets to 5.11/5.61 MB and still aborts, so the last 0.5 MB is a
   squashfs tail difference and the signature remains the wall.
2. **A real memory primitive exists in the HLS queue, and its write side is now measured on the device.** An append after a split writes the whole 3152-byte record over the ring header at `parent+0xc508`, including the array pointer at `parent+0xc528`. The pointer bytes are the record's `name[0x20..0x23]`, the name is percent-decoded before the record is built (proven with encoded pointer bytes that reach `0x40xxxxxx` and crash only on unmapped bytes), and the appended record is stamped at `+4` by a helper that then overwrites the ring cursor with its own return value. Destination arithmetic is `array + cursor*3152`; `array` is the name bytes, but the cursor is the helper's return (`0x101919b4`), which is environment-derived and is the remaining unknown. Aiming the write at `.plt` with two stubs (strstr/strlen slots) is built and fetched but does not fire yet - the destination misses because the live cursor is not the record's `+4`.
   `/` at `name[0x20]` is the only pointer byte that dies; `@`..`w` (mmap) survives. The install lane's
   emulator model reaches slot 16 deterministically on a split; playback continues after the corrupt
   header, so the primitive is silent unless the destination is chosen.
3. **Every pre-signature and runtime parser surface tried is hardened** (see the negative ledger).
   That is a real narrowing: future effort has a short list.

What did *not* happen: no daemon code execution, no unsigned install, no devmode unlock.

## What is proven (evidence)

| fact | evidence |
|---|---|
| genuine URL installs work both ways (control) | 57.22-65130 accepted, 30 s box-down, returns 57.22; 57.23 restored; `docs/RE_UPDATER.md` control section |
| updater loop after a failed run, and its recovery | `docs/ROOT_LANE_STATUS.md` 23:58Z; 20 consecutive 200s after recovery |
| signature scope and prefix digest | `docs/RE_UPDATER.md` "two separate values"; digest reproduced on three builds (the packaged rebuild's own digest is stale - audit) |
| appended sections never reach pass 2 | 7-variant matrix, `docs/RE_UPDATER.md`, `scripts/append_upd_section.py` |
| rootfs package builder | `captures/firmware/57.23-74170-1-16/pwned-q57.upd`; digest-recomputed variant still aborts mid-body (1.5 MiB), so envelope correctness is open |
| HLS pointer offset and encoding | `docs/RE_HLS_EXPLOIT.md`; `name[0x20..0x23]` is the array pointer, percent-encoded bytes are decoded before the record, map probe (`@` survives, `/` dies) |
| HLS write reaches the ring header | slot 16 write after a split: header 0..15 = name bytes, playback continues |
| URI is at record+0 | `strlcpy(record+0, uri, 2049)` at 0x10191b14; a second copy lands at 0x801 and the lane doc records both |
| token fingerprint format | composer is compact JSON; hhid = `{owner:{luid},hhid,network_hash}`; luid/network_hash are the mhhid segments *by inference* (the TXT triple matched slot 12), `docs/RE_TOKEN.md` |
| token issuer needs a live object | nine rodata vtables, trampoline shape, honest stop point |
| devmode statement is device-bound | signature covers the whole clear statement incl. id (`docs/RE_DEVMODE.md`) |
| `/ssh/fingerprints` is a host-key reporter gated by unlock | `docs/RE_TOKEN.md` |
| `/status/ZPLocalSettingsFile/<name>` reads one `..` into `/jffs` | `docs/RE_WRITE_PRIMITIVES.md`; name length bounded by the request line (~4 KiB ok, 8 KiB reset) |

## Negative ledger (do not re-spend)

- Type-21 patch list: no size crash found; every mutation hit the 512 KiB semantic verdict (that is a verdict point, not a proven size bound).
- Type-1 version, type-2 compat: structural bounds found; only semantic verdicts move.
- Type-22 x509 (mbedtls 2.5.0, no PSS support): 2496-case offline sweep, zero crashes (commit eaa20e4, `scripts/mbedtls-harness/`; the ./third of on-device runs that lost the trigger are not in that denominator).
- GENA client responses: 18 malformed shapes, zero (single run each; no control matrix beyond liveness).
- Updater HTTP responses (busybox wget as root): complete frame matrix, two device-confirmed desyncs
  ('-1' and a short uppercase chunk close after ~8 KB/128 KB with rc 0), no memory fault; the argv
  is exactly known (`-qC /var/run/upgrade_report.txt -T 300 -O - <url>`).
- SMB client state machine on S2: 18 applied mutations, zero (apply-verified; warm-up case first).
- `/notify` LastChange: 12 shapes, 128 KiB ceiling, push does not drive state.
- `/status` name length: 4 KiB ok, 8 KiB reset (request line), no join overflow; `/mtmhhsetup`:
  403-gated while locked, so its parser is only reachable after an unlock.
- S2 `SetZoneAttributes` icon URL: stored, never fetched (no GET on any scheme) - a stored-URL surface, not a fetch surface.
- `SetAVTransportURI`/`AddURIToQueue` 20-shape matrix on .202 and on the S2 build: accepted shapes only, no parser fault.
- `/mfgunlock`: unstable on .86 (state-dependent), no usable transition observed.
- Token without a live object: 63+ claim permutations, all one-bit 401.
- `/getrs`: 2229-index scan (bit31 values, negatives, dense ranges, a 2000-value random sample),
  flat 200 and alive throughout - the table reads are benign.
- `AddOAuthAccountX`: 20-case ten-argument matrix, every case 500/402, liveness before and after.
- Rootfs envelope: the toolchain reached its best state: a zero-IV, length-exact, prefix-matched
  patched build now reads 5,111,808 of 5,606,749 bytes before the client closes (the untouched-tree
  control completes). The remaining ~0.5 MB is a squashfs tail/block difference to compare next
  session; the signature is still the wall even after that.

## Bead status (the 20 open at session start)

Advanced with evidence: `re-hls-kill-cnk`, `xnf`, `nyy`, `2ba`, `re-beginsw-fdp`, `8h3`,
`re-s2-devcopy-pfo`, `gko`, `re-uboot-cve-ols`, `ijn`, `re-notify-shapes-qgv`,
`re-long-outage-7hq`, `c25`, `fy9`, `spotify-more-kinds-94i`.
Still genuinely blocked: `q57` (needs an install path), `publish-0-1-0-isb` (deps),
`re-spotify-audio-w21` (scope: needs a streaming credential; evaluation already GO),
`c4v` (gated on a locked unit).
Closures are evidence-bound; three are thin and flagged by the audit: `nyy` (a crash that later proved to be an envelope-shape rejection, with no control row), `2ba` (a negative that omits its ~1/3 lost-trigger denominator in the close text), `xnf` is still open while the live endpoint returns 401. `re-s2-devcopy-pfo` is static-only.

## Gap analysis for the root goal

| goal | state | gap |
|---|---|---|
| pre-auth code exec on S1/S2 | NOT ACHIEVED | HLS gives an 0xc50-byte write to an address taken from decoded name bytes; the cursor half of the address is helper-derived and not yet predicted live; no second primitive |
| unsigned/durable install | BLOCKED | vendor key or a bug in the pre-signature parser (all tried are clean) |
| unlock / devmode | BLOCKED | device-bound statement needs the portal (owner login) |
| persistence path after root | READY | `Configure.dev` payload built; `/jffs` preserve table recorded in FIRMWARE_PATHS (0x1101) |
| SSH after unlock | MAPPED | statement starts telnetd/dropbear; `/ssh/fingerprints` is read-only |

## Lab capability gained

anacapad now **serves in the css guest** (11 threads, :1400/:1443 listening, GetZoneAttributes answers
200) with the device's own loader, the ABI-patched libflash and the /opt/s16 staging recipe committed
(d4fcd97). The staged build is MIPS, and HLS exists only in the PPC model-16 build (the 57.23-1-12
MIPS anacapad has zero HLS strings), so a PPC guest is the missing instrument for the next session.
The PPC guest itself now boots the 1-16 build past module load to the `port 1400/1443/1843` binds and
then SIGABRTs (~40 ms later, before `accept`). Corrections that got it there - `pkill -x`, tmux not
setsid, the absolute `/opt` layout, a `libflash` shim with SYSV hash and soname, and a `/dev/chk`
patch - are in `docs/FIRMWARE_PATHS.md`. The abort is *not* the DCMS/regcert gate (those strings are
absent from this binary); the monitor socket samples NIP and is the way to name it.

## Gate datum (publish readiness)

`cargo clippy --workspace --all-targets -- -D warnings`: PASS, no diagnostics.
`cargo test --workspace`: 137 test binaries, **1 failure** —
`crates/fsonos-core/tests/live_bootstrap_sim.rs:108`, the S2 household not reappearing after the
Living Room player is set back online; it passes 1/1 in isolation (2.12 s), so it is
order/parallelism sensitive rather than a plain red. Two initial gate attempts were refused by the
rch shim (queue_timeout); both recorded runs then went through rch with local fallback enabled.
So the workspace is not green as a whole and must not be called release-ready.

## Next actions, in priority order

1. HLS: solve the destination. The helper at `0x101919b4` returns the value stored as the ring
   cursor; find what its return is derived from (it is not the record's `+4`, and it is invariant to
   name, duration and `+0xc40` in the model), then the `.plt` stub payload already built should fire.
   The emulator is the iteration tool; one device run per candidate array.
2. Owner action: load the browser relay extension and sign in once to the devmode portal; a
   per-serial statement immediately yields telnetd + dropbear (S1) or the SSH reporter (S2).
3. A second memory bug: the S2 registration/devmode handler under a *valid* statement, and the
   `AddOAuthAccountX` ten-argument parser, are the two remaining undeep-fuzzed runtime parsers.
4. Do not retry: signature/prefix splicing, appended-section installs, x509 PSS CVEs.

## Denominator note

Timeboxed session (~6 h). Two live units (`.202` S1, `.84` S1) plus `.81/.86` S2; two units were
lost to network/router drops during the window and recovered. Every device run in the tables has
its control row; runs without a control are marked as such. The one harness bug found mid-session
(`TS_FILE` vs `TSFILE`) invalidated the early "real TS" rows and they were re-run, not reinterpreted.

## Session close

- Head at close: `192da06`; pushed with the usual rebase (no force).
- Units left healthy and idle: `.202` 57.23, `.84` 57.23, `.81`/`.86` 86.10; all four answer
  `/status`. `.202` was wedged by the final update run and recovered on its own before close.
- Open beads: 4 in progress (`xnf`, `re-hls-kill-cnk`, `c25`, `spotify-more-kinds-94i`), 4 blocked
  (`q57`, `publish-0-1-0-isb`, `re-uboot-cve-ols`, `re-spotify-audio-w21`); 138 closed.
- HLS ended at: pointer offset and percent-decoding settled on device, the slot-16 write and its
  header corruption measured, the destination's cursor proven to be helper-derived; the `.plt`
  stub payload is built and fetched but does not fire. Not claimed: code execution.
- Not claimed: any code execution, any unsigned install, any unlock.
