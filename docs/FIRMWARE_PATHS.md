# Firmware paths per device class (owner-directed, 2026-10-08)

The owner's goal: his own software on his own speakers, so the system works
the way he wants. This file maps what the RE lane has established about
getting there, per device class, with the concrete next action for each.
Two rules hold regardless: **flashing or modifying a device is human-led**
(explicit per-operation approval, `docs/SCOPE.md`), and nothing proprietary
(images, keys, decompiled code) is committed — analysis lives as prose here
and in `docs/PROTOCOL.md` §9.

## Class A: S1-gen MIPS players (Play:5 Gen1, Play:1, Bridges) — OPEN BOOK

State: milestone-era images (34.16) are unencrypted and fully decoded;
rootfs extracted and readable on both platforms. The updaters signature-check
images against an on-device key and carry a vendor debug path
(`allow_policy_bypass`, debug-version flags, no-op mode) whose trigger is not
yet found.

Path to owner software: find/enable the debug update path (`r2` on
State: milestone-era images (34.16) are unencrypted and fully decoded;
rootfs extracted and readable on both platforms. The updaters signature-check
images against an on-device key and carry a vendor debug path
(`allow_policy_bypass`, debug-version flags, no-op mode) whose trigger is not
yet found.

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
