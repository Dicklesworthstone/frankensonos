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

## Update 2026-10-23 23:10Z (supersedes the "what would change the verdict" list)

- Verified statically: the type-17 signature is over the 57-byte **list2** region only, but that
  region contains `sha256(file[:type17])` and the verifier compares the running prefix hash to it,
  so a body (rootfs) edit breaks verification at the end even though the RSA signature is intact.
- **New foothold: section types 5, 11, 12, 23 are absent from genuine images and carry flags=0
  (not hashed).** Only a repeated type is fatal; 0x11 is the only repeatable one. Handler
  0x10007db0 receives a partition code per type (6/7/8/0xc) and is the only caller into
  `hal_flash_reprogram_flash`. An appended unused type therefore reaches the flasher unsigned.
- The rootfs builder exists and is verified (`captures/firmware/57.23-74170-1-16/pwned-q57.upd`,
  SHA-256 73587fac...): Configure.dev 0644 + `/PWNED-MARKER`, only type-4 replaced. First runs
  abort at ~4 MiB, most likely a declared-length/envelope builder mismatch (reenc-only aborts too).
- Devmode: statement is device-bound on both generations; payload length is u8 (the 256-byte copy
  is exact, not short). Without the portal, no live unlock.
- Token: hhid claim solved from the mDK TXT triple (`{owner,luid,hhid,network_hash}` JSON); still
  401 because the *issuer* comes from the provider object's real strings, not a stub.

## Update 2026-10-23 23:25Z — appended sections do not reach the flasher

Tested on .202 (full fetch each, version unchanged): type-5 with 0x00 payload, same
with header field 1 and 4, type-5 with an enveloped body, type-9 and type-10 with
their 34.16 512-byte payloads. The type-5 runs correlate with a 0.4-0.5 s ping blip
the baseline lacks, so the appended record is reached, but **no second reboot and no
install in any variant**. Types 5/11/12/23 being unused and unhashed is therefore not
a flash-write foothold: pass 2 seems to walk the compiled type table, not the parsed
stream.

Consequence: **every install path needs the vendor key** (prefix digest + list2
signature), confirmed from three directions now. The root lane's live hope is a
pre-signature parser bug.

Two concrete chains are ready for a future unlock: a valid devmode statement (portal,
device-bound) -> `/ssh/fingerprints` installs a `pubkey` JSON entry into the dropbear
authorized-keys path -> root shell; the route and its filter
(`R_ALLOW_SSH_PUBKEY_INSTALL`, not persisted) are mapped.

## Update 2026-10-23 23:58Z — accepted-install control proven; recovery recipe

The positive control finally ran: serving the genuine 57.22-65130-1-16 package put the unit on
57.22 (full 5,602,653-byte read, box reboot, automatic return), and a genuine 57.23 package
restored it. So URL installs of *genuine* images work in both directions and every "reject"
classification above is meaningful against that control.

Operational hazard found: a failed/looping updater leaves `/var/run/upgradeinfo` pointing at a
stale URL and the daemon restart-loops every 20-50 s with no fetch. Recovery (measured): serve a
genuine image, retry `BeginSoftwareUpdate` until one trigger is accepted, wait for the full read
and reboot, then require 20 consecutive `/status` 200s. Do not change the URL; 200-no-fetch just
means the daemon was mid-restart.

Pre-signature parser results so far are negative but clean: type-21 mutations all stop at the
512 KiB compat verdict (bounded parse), 18 malformed GENA-client responses survive, and three
poisoned type-22 cert bundles read to the end without a device-side fault (offline mbedtls
harness pending).
