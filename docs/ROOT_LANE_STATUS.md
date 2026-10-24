# Root lane status (orchestrator ledger)

Question of record: a **network-only, pre-auth root** on the owner's own S1 (57.23) and
S2 (86.10) players, with no vendor developer key, no manual downgrade flash, no hardware
access. Success = persistent root or an unsigned-image install that survives a reboot.

The signature/transport walls are settled in `UPDATE_FEED_AND_SIGNATURE.md` and
`EMUDHRA_AND_FIRMWARE.md`; do not re-derive them. This file tracks only the live lanes.

## Hard facts that bind every lane

- The type-17 signature hashes the **plaintext stream before the type-17 record**, RSA-2048
  PKCS#1 v1.5 / SHA-256, against two compiled keys. No vendor key, no self-signing.
- The content key is obtainable (MDP3 + OTP, HITB method) but only lets us decrypt/edit.
- The updater's downgrade floor is a compiled **type-21 patch list**; every public S1 RCE
  (CVE-2023-2735x, CVE-2025-1048/1049) is below it. 57.23 already carries the fixes.
- eMudhra `EM DV TLS CA - G3B` is compromised and its key is public; a forged leaf chains to
  a root in the device bundle. That buys cloud impersonation (transport/token/arming), not a
  package signature.
- Post-type-17 bytes are outside the hash. A **plaintext** trailing record fails on device
  (17 s abort, no script) - an enveloped trailing section is the open test.
- Pre-auth surfaces with real parsers: `/devmode` (S1/S2), `/unlock`, `/notify`, `/setstring`,
  `/advconfig`, `/mtmhhsetup` (S2, live handler, 500), `/support/directsubmit`, `SetZoneAttributes`.
- No vendored named-file write is known; `/jffs/system/Configure.dev` remains the persistence
  target (rcS sh's it, existence unlocks telnetd).

## Lanes

| lane | bead | state | last evidence |
|---|---|---|---|
| HLS compaction write | re-hls-kill-cnk | **live** | entry-16 injection + C = capacity-head; sign check due |
| Token inspector / cloud | xnf | **live** | composer mapped; ctx callbacks (slot 12/24) open |
| Updater bundle/type-21 | nyy, 2ba, beginsw | **live** | plaintext tail rejected; enveloped tail + mbedtls 2.5.0 cert next |
| Devmode / S2 copies | 8h3, pfo, c4v, dlt | **live** | statement replay corpus hunt; mtmhhsetup handler |
| Fresh CVE map | fy9, c25, qgv | **live** | component map + ranking in `RE_CVES.md` |
| Write primitives | q57, gko, uboot, ijn | **live** | bit 22 / `/raw` / SSH-key gate |

## Honest state at 2026-10-24 22:30Z

- Two real primitives are in analysis: a heap `memcpy(dst, src, len)` where all three are
  derived from clobbered object words (HLS), and a cloud-impersonation path that can arm the
  updater and mint device tokens (eMudhra). Neither is a reliable root yet.
- Nothing on this fleet is remotely rootable by a *published* CVE at the installed versions;
  the HLS daemon-kill is a regression-shaped, data-dependent crash of the same class as
  CVE-2025-1048 but not yet shown to be that bug.
- No file write with a chosen path exists pre-auth on either generation so far.
- A root primitive must be: initial code exec -> (optional) write
  `/jffs/system/Configure.dev` -> boot. Or an install path that admits unsigned bytes.

## What would change the verdict

1. A controlled write into the HLS ring that lands on a live function pointer or the ring's
   array pointer, with a fixed-address gadget (no PIE).
2. A malicious x509 certificate that crashes or corrupts mbedtls 2.5.0 in the pre-signature
   type-22 parse on S1.
3. A replayable signed devmode statement -> `/unlock`, sshd, `/testenv`.
4. An enveloped trailing section the flasher honors -> unsigned rootfs after the signature
   record (still needs the flasher to be last-wins, not first-wins).
5. A pre-auth request-chosen file write (writer lane) -> Configure.dev directly.
