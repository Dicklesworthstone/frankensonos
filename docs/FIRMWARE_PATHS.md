# Firmware paths per device class (owner-directed, 2026-10-08)

The owner's goal: his own software on his own speakers, so the system works
the way he wants. This file maps what the RE lane has established about
getting there, per device class, with the concrete next action for each.
Two rules hold regardless: **flashing or modifying a device is human-led**
(explicit per-operation approval, `docs/SCOPE.md`), and nothing proprietary
(images, keys, decompiled code) is committed — analysis lives as prose here
and in `docs/PROTOCOL.md` §9.

## Class A: S1-gen MIPS players (Play:5 Gen1, Play:1, Bridges) — OPEN BOOK

State (2026-10-20, static RE): milestone-era images (34.16) are unencrypted
and fully decoded; rootfs extracted and readable on both platforms (Play:5
Gen1 model 5, Play:1 model 12). The on-device updater signature-checks
images against keys compiled into `libsonoscrypto.so.1` and carries a vendor
debug path (`allow_policy_bypass`, debug-version flags, no-op mode).

The devmode gate is **cryptographic** and now fully mapped: three embedded
2048-bit RSA public keys sit in `libsonoscrypto.so.1` (moduli prefixes
`00ed54d2…`, `00b80399…`, `00e2601d…`; full values kept local-only). `POST
/devmode` base64-decodes a ≤1024-byte `statement` and verifies it as an
RSA-PKCS1v15 signature against one of the three keys (numbered, revocable).
On success the daemon writes `/tmp/device_unlocked_flag`; the updater's
`allow_policy_bypass` `stat()`s it. Forging a statement therefore needs the
matching Sonos **private** key.

**Private-key hunt (2026-10-20):** neither S1-gen controller app carries the
private key. The still-served 47.2 (v9.3) and 90.0 (v17.2.4) macOS DMGs were
downloaded and the full `Sonos.app` binaries scanned: the only "RSA PRIVATE
KEY" strings are mbedtls's PEM parser *label* constants (clustered, no key
data), `developerKey` is a SOAP *parameter name*, and the three moduli do
not appear in the controller. So the devmode signing key is Sonos-issued
(server-side / developer-program), not shipped in the controller. The 90.0
build does carry `SCMockUpdateDebugPage`, but the whole `SCMock*Page` family
is the SwiftUI design-system preview set (mock UI, not a functional update
backdoor). The controller-side levers that DO matter: `BeginSoftwareUpdate
(UpdateURL)` takes the URL as an argument and the controller logs `Override
AnonymousUpdateURL: %s` (the fetch can be redirected), and the update
**manifest** (`.upm`) is signature-verified against a root-CA bundle
(`SCRootCerts` / `RootCACertBundleDownloader`).

**VERIFIED 2026-10-08**: the .upd delivery path is **blocked** by RSA
signature verification on running 57.23 firmware. Live-fire testing proved:
`BeginSoftwareUpdate` accepts arbitrary URLs and the speaker downloads
served files, but modified .upd files (with real kernel+rootfs from the
readable 34.16 base) are rejected — script records never execute, version
stays unchanged, the speaker recovers. The 34.16-era binary showed the
signature check as advisory; 57.23 enforces it. The gap between 34.16 and
57.23 is where Sonos closed it.
(boundary pinned between 34.16 and 57.19). The readable milestone base gives
the platform's layout; decryption needs the device-family key from a device's
install environment (the key is on-device, compiled into the install path).

Path to owner software (S1), in order of tractability:

1. **Downgrade to a pre-enforcement build** (34.16, where the signature
   check is advisory) and install owner software there. The readable 34.16
   base already gives the platform layout. Open questions: how to get a
   57.23 speaker back to 34.16, and whether the 34.16 updater's advisory
   check is actually a hard no-op or just non-fatal.
2. **Devmode bypass** — obtain a legitimately Sonos-signed devmode
   statement (developer-program credential) for the device, `POST /devmode`
   it, and the updater's `allow_policy_bypass` then accepts a modified
   `.upd` (signature skipped / no-op mode).
3. **Forge the statement** — needs the Sonos private key for one of the
   three embedded moduli. Not in the controller; would have to come from a
   Sonos service or a leaked key.

The S1 `.upd` container (34.16, fully decoded) is a record sequence
`[magic=0x35167F49 LE][type u32][total_len u32 (includes the 16-byte
header)][reserved u32][payload]`: type 1 = version header, 2 = 28-byte
sub-record, 3 = shell install script, 4 = CramFS rootfs (custom Sonos
variant, not stock cramfs), 5 = 1-byte marker, 6 = gzip MIPS kernel,
9/10 = 512-byte records, trailer = type `0xFFFFFFFF`. No signature record
in the 34.16 image — the check runs against the on-device keys.

## Class C: Sonos One / modern S2 (Amlogic A113x ARM) — PUBLISHED EXPLOIT ROUTE

The published route (blasty, Synacktiv, NCC Group BH-US 2024 — all public
research, cited in the local runbook archive):

1. **No USB path**: the One's bootrom USB recovery is fused off.
2. **Physical access + PCIe DMA** (Synacktiv method, no flash writes, low
   brick risk): USB3380-class FPGA board on the mini-PCIe WiFi card slot →
   patch `poweroff_cmd`/`vfs_read` in kernel memory → telnet root shell.
3. **EL3 exploit** (blasty's A113x BL31 secure-storage parser overflow) via a
   small kernel module → arbitrary EL3 read/write → dump OTP + bootrom.
4. **Offline**: `sonostool` derives the per-model OTA AES keys and model RSA
   key from the MDP blob + OTP → decrypts any S2 firmware image; one dumped
   device opens fleet-wide update decryption for that model. LUKS keys are
   per-device (CPUID-derived).
5. **S2 `.upd` layout (verified 2026-10-20 against the 86.10 Play:1 and
   97.1 Sonos-One images, both downloaded + walked):** same `0x35167F49`
   record container as S1, but the content is wrapped in encrypted blobs.
   Record types observed: 1 = version header, 2 = sub-records, 21 =
   patch-train list (`2021_11_PATCHES:` …), 22 = certificate bundle (Sonos
   root CAs), 23 = **plain tar overlay** of the rootfs (`bin/anacapad` —
   the ~3 MB zone daemon), and types 3/4/6 (install script / rootfs /
   kernel) as `0x886499ca` encrypted-content blobs. Key/signature blobs
   (types 22/17) use a `0x621da74d` header: an RSA-2048-OAEP-wrapped
   per-blob AES-128-CBC key at `[0x32:0x132]`, the AES-CBC body (IV = 16
   zero bytes) at `0x13A`. Type 17 is the 380-byte `.upd` signature blob.
   `sonostool` decrypts each blob with the model private key (from the MDP
   at offset `0x780`) → AES-CBC → the file.

Costs/risks: ~1-hour physical session with FPGA hardware; firmware-specific
EL3 offsets must be re-derived per build (documented in the runbook);
CVE-2023-50809/50810 patches touched the U-Boot env path but not the BL31
parser per current sources. Full step-by-step runbook with tooling URLs is
archived locally (git-ignored captures/research/) since it's operational
detail, not repo prose.

## Recommendation

Reliability goal is already served by the daemon (stock speakers, no risk).
Firmware replacement research proceeds S1-first (open book, zero hardware
risk) with the Class C physical session as a deliberate, scheduled,
human-in-the-loop event when the owner wants the S2 One opened.
