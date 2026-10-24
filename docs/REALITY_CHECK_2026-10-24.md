# Reality check — network-only root lane, 2026-10-24

Question of record: a **pre-auth, network-only root** on the owner's own S1 (57.23) and S2
(86.10) players; no vendor developer key, no manual downgrade flash, no hardware access.
Success = persistent root, or an unsigned/first-party-key-free install that survives a reboot.

Read with `docs/SCOPE.md`, `README.md` and `docs/ROOT_LANE_STATUS.md`. This is the honest check,
not a summary of activity.

## Verdict

**Not root yet.** The lane has moved from "unknown wall" to a small number of *measured* walls and
two real primitives, but no state-changing exploit is proven end to end.

What changed for the better:

1. **The install path is now fully characterised, and it is closed without the vendor key.**
   Proven by code and by device: the type-17 signature covers the 57-byte list2 region only, but
   that region stores `sha256(file[:type17])` and the updater memcmps the running prefix hash
   against it; any body edit needs both a digest rewrite and a fresh signature. Appended records
   after the type-17 are unsigned, and *every* unused-section variant (types 5/9/10, raw and
   enveloped) is parsed then rejected before pass 2 - the flasher never runs. A rebuilt rootfs
   package is correct and aborts exactly at the prefix digest.
2. **A real, controllable memory primitive exists in the HLS queue**: an unchecked
   `memcpy(dst, src, n*3152)` whose destination and length come from ring counters that an append
   to slot 16 overwrites with the **URI text**. An emulator maps C one-to-one to the enqueue batch
   and shows a 4 GiB wrapped copy that faults from batch 17 (shortcut chain), while a bounded
   backward copy of 28-40 slots is available in the non-shortcut chain.
3. **Every pre-signature and runtime parser surface tried is hardened** (see the negative ledger).
   That is a real narrowing: future effort has a short list.

What did *not* happen: no daemon code execution, no unsigned install, no devmode unlock.

## What is proven (evidence)

| fact | evidence |
|---|---|
| genuine URL installs work both ways (control) | 57.22-65130 accepted, 30 s box-down, returns 57.22; 57.23 restored; `docs/RE_UPDATER.md` control section |
| updater loop after a failed run, and its recovery | `docs/ROOT_LANE_STATUS.md` 23:58Z; 20 consecutive 200s after recovery |
| signature scope and prefix digest | `docs/RE_UPDATER.md` "two separate values"; digest reproduced on three builds |
| appended sections never reach pass 2 | 7-variant matrix, `docs/RE_UPDATER.md`, `scripts/append_upd_section.py` |
| rewound rootfs package builder | `captures/firmware/57.23-74170-1-16/pwned-q57.upd`, SHA-256 `73587fac…`, aborts at the digest |
| HLS C/dest/fault map | `scripts/hls-queue-emu/` + `docs/RE_HLS_EXPLOIT.md` |
| URI is the injected counter word | `strlcpy(record+0, uri, 2049)` at 0x10191b14 |
| token fingerprint format | composer is compact JSON; hhid = `{owner:{luid},hhid,network_hash}` from mDNS TXT; `docs/RE_TOKEN.md` |
| token issuer needs a live object | nine rodata vtables, trampoline shape, honest stop point |
| devmode statement is device-bound | signature covers the whole clear statement incl. id (`docs/RE_DEVMODE.md`) |
| `/ssh/fingerprints` is a host-key reporter gated by unlock | `docs/RE_TOKEN.md` |
| `/status/ZPLocalSettingsFile/<name>` reads one `..` into `/jffs` | `docs/RE_WRITE_PRIMITIVES.md` |

## Negative ledger (do not re-spend)

- Type-21 patch list: bounded; all mutations fail the 512 KiB semantic verdict.
- Type-1 version, type-2 compat: structural bounds found; only semantic verdicts move.
- Type-22 x509 (mbedtls 2.5.0, no PSS support): 2496-case offline sweep, zero crashes.
- GENA client responses: 18 malformed shapes, zero.
- Updater HTTP responses (busybox wget as root): chunked/redirect/header matrix, desyncs but no
  crash; the client is `wget` and the argv is known.
- SMB client state machine on S2: 18 applied mutations, zero.
- `/notify` LastChange: 12 shapes, 128 KiB ceiling, push does not drive state.
- `/status` name length: bounded; `/mtmhhsetup`: 403-gated while locked.
- Token without a live object: 63+ claim permutations, all one-bit 401.

## Bead status (the 20 open at session start)

Advanced with evidence: `re-hls-kill-cnk`, `xnf`, `nyy`, `2ba`, `re-beginsw-fdp`, `8h3`,
`re-s2-devcopy-pfo`, `gko`, `re-uboot-cve-ols`, `ijn`, `re-notify-shapes-qgv`,
`re-long-outage-7hq`, `c25`, `fy9`, `spotify-more-kinds-94i`.
Still genuinely blocked: `q57` (needs an install path), `publish-0-1-0-isb` (deps),
`re-spotify-audio-w21` (scope: needs a streaming credential; evaluation already GO),
`c4v` (gated on a locked unit).
No bead was closed merely for a refusal; refusals are labelled.

## Gap analysis for the root goal

| goal | state | gap |
|---|---|---|
| pre-auth code exec on S1/S2 | NOT ACHIEVED | one primitive (HLS) without control of PC; no second primitive |
| unsigned/durable install | BLOCKED | vendor key or a bug in the pre-signature parser (all tried are clean) |
| unlock / devmode | BLOCKED | device-bound statement needs the portal (owner login) |
| persistence path after root | READY | `Configure.dev` payload built; `/jffs` preserve table verified |
| SSH after unlock | MAPPED | statement starts telnetd/dropbear; `/ssh/fingerprints` is read-only |

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
