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
   envelope), but a patched tree still aborts mid-body; the signature stage is not reached and
   remains the wall.
2. **A real memory primitive exists in the HLS queue**: an unchecked `memcpy(dst, src, n*3152)`
   whose destination and length come from ring counters that an append to slot 16 would overwrite
   with URI text. The emulator shows the shortcut chain (batch >= 17) produces a wrapped ~4 GiB
   copy that faults *in the emulator*; on device no configuration yet reaches that state, because
   slot 16 is gated behind the shortcut. The reachable live band is inside the array, C <= 15.
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
| HLS C/dest map (emulator) | `scripts/hls-queue-emu/` + `docs/RE_HLS_EXPLOIT.md`; live reachability is limited |
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
- Token without a live object: 63+ claim permutations, all one-bit 401.
- `/getrs`: 2229-index scan (bit31 values, negatives, dense ranges, a 2000-value random sample),
  flat 200 and alive throughout - the table reads are benign.
- `AddOAuthAccountX`: 20-case ten-argument matrix, every case 500/402, liveness before and after.
- Rootfs envelope: the toolchain is now correct enough that an *untouched* tree with a zero IV and a
  length-exact envelope completes the whole read; a patched tree then aborts mid-body, so the
  remaining difference is the inserted content, not the envelope. The signature is still the wall.

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
| pre-auth code exec on S1/S2 | NOT ACHIEVED | one primitive (HLS) without control of PC; no second primitive |
| unsigned/durable install | BLOCKED | vendor key or a bug in the pre-signature parser (all tried are clean) |
| unlock / devmode | BLOCKED | device-bound statement needs the portal (owner login) |
| persistence path after root | READY | `Configure.dev` payload built; `/jffs` preserve table recorded in FIRMWARE_PATHS (0x1101) |
| SSH after unlock | MAPPED | statement starts telnetd/dropbear; `/ssh/fingerprints` is read-only |

## Lab capability gained

anacapad now **serves in the css guest** (11 threads, :1400/:1443 listening, GetZoneAttributes answers
200) with the device's own loader, the ABI-patched libflash and the /opt/s16 staging recipe committed
(d4fcd97). The staged build is MIPS, and HLS exists only in the PPC model-16 build (the 57.23-1-12
MIPS anacapad has zero HLS strings), so a PPC guest is the missing instrument for the next session.

## Gate datum (publish readiness)

`cargo clippy --workspace --all-targets -- -D warnings`: PASS, no diagnostics.
`cargo test --workspace`: 137 test binaries, **1 failure** —
`crates/fsonos-core/tests/live_bootstrap_sim.rs:108`, the S2 household not reappearing after the
Living Room player is set back online; it passes 1/1 in isolation (2.12 s), so it is
order/parallelism sensitive rather than a plain red. Two initial gate attempts were refused by the
rch shim (queue_timeout); both recorded runs then went through rch with local fallback enabled.
So the workspace is not green as a whole and must not be called release-ready.

## Next actions, in priority order

1. HLS: finish the shortcut-vs-suppressed live discrimination (bit31 producer), then aim the
   bounded backward copy at a corrupted function pointer; the emulator is the iteration tool.
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
