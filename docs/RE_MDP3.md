# MDP3 / OTP: tool, and what userland actually reaches

Lane CalmTower (bead `frankensonos-lane-calm-tower-mdp3-jfy`). Offline, no
hardware. Content keys live in MDP3 (magic `f079a9cb`, 0x4000 bytes) sealed
with `sha256(OTP[0xD0:0xE0])[:16]`; the model RSA private key is the field at
`0x780` (docs/UPDATE_FEED_AND_SIGNATURE.md, "The content key is obtainable").

## Tool

`captures/research/mdp3/mdp3.py` — port of `sonostool.py`'s `blob_encdec`
without pycryptodome (openssl CLI for AES):

```
mdp3.py fields MDP OTP              # which sealed fields are present/decrypt
mdp3.py decrypt MDP OTP rootfs|jffs|model
mdp3.py upd MDP OTP FILE OUTDIR     # decrypt encrypted .upd sections
mdp3.py upd-pem FILE OUTDIR PEM     # same, with an already-decrypted model key
mdp3.py luks MDP OTP HEX32          # LUKS key derivation
mdp3.py selftest                    # synthetic MDP3 round-trip
```

## Known vector (differential against the published tool)

`captures/research/mdp3/vector_test.py` builds a synthetic MDP3 (jffs, rootfs,
model fields) and OTP, decrypts every field with **both** this tool and
`sonostool.py` at blasty/sonos `27be438`, and asserts byte equality, then
compares the LUKS derivation. Vector hashes: mdp
`84fde277c0c463718188644500770fa62f7eb10dce23108320cfb23c8cae0476`, otp
`d9c76fa34978cb9620dab8c3f46bbe075fddc145eb282b39009141f98d0cfe82`; all
three fields and the LUKS key are identical (`VECTOR OK`).

End-to-end chain, one command: a synthetic MDP3 whose model field is the real
`model16-private.pem` (`model-pem` round-trips it byte-identically) feeds
`verify` on `57.23-74170-1-16.upd`, which reports section 7 `23212f62`, section
8 `48000005`, section 9 `73717368` (squashfs). Synthetic vector hashes: mdp
`b6eb6023…c4b2d0`, otp `4103c4b3…82299b`.

The vector immediately caught a real bug in the port: the per-blob tag is
padded to **8** bytes and XORed **modulo its own length** over the 12-byte IV
(`keymod` in the reference), not zero-padded to 12 as the first version did.
The selftest had round-tripped because both sides shared the wrong rule; only
the differential exposed it.

Verification, all three halves:

- `selftest` builds an MDP3 with two sealed fields and round-trips them
  (`selftest ok`).
- `upd-pem 57.23-74170-1-16.upd … model16-private.pem` decrypts the real
  package: section 7 is the preinstall script (`#!/b`), section 8 the kernel
  (`48000005`), section 9 a squashfs (`73717368` = `hsqs`). Re-run after the
  keymod fix, unchanged.
- the differential vector above covers the MDP3→key half; only a real
  MDP3+OTP dump is still missing.

An encrypted section body is an envelope: `MAGIC_FW`, BE32 header length,
byte recipient length at `0x14`, recipient at `0x16`, four reserved bytes,
BE32 RSA length, the RSA ciphertext, BE32 cipher id (1), BE32 data length,
then the payload at the header length. The recipient is variable-length, so
the fixed `0x32`/`0x132`/`0x13A` offsets only hold for a 20-byte recipient;
`envelope()` parses the general shape (proved with a 32-byte recipient and
against the real package, where it yields `fd88f264…d437`).

Section headers are `{magic, type, size, flag}`; `flag & 4` marks an encrypted
section and those carry `MAGIC_FW` (`886499ca`). Plaintext payload sections
carry `621da74d` instead. The validator reports any flag-4 section that lacks
the FW magic rather than skipping it silently.

## Second layout: fenway (model-8 family)

`trulyspinach/sonos-fenway` `includes/mdp.h` documents a second MDP3 layout for
the MPC8314 family: the same 0x4000 page and the same 0x580/0x680/0x780
offsets, but the key fields are `{magic, len, reserved}` TLVs with CAAM magics
(`0xF0A2ADE7` AES128 black, `0x618C4DE8` AES128 red, `0x19283746` RSAREF priv,
`0xF4CCC68D` PKCS1 priv) in fixed 256/256/2048-byte slots. `--layout=fenway`
parses that shape and bounds `len` against the slot; a synthetic fenway page
round-trips all three fields (`fenway-model` decrypts).

The same header also shows the page is the **third** member of `struct smdp`
(MDP1 512 + MDP2 4096, then MDP3), so a whole-region dump carries it at
`0x1200`; `--smdp` slices exactly that page out before parsing (tested with a
synthetic smdp image). `otp OTP` prints the derived AES key so a dump can be
sanity-checked before use. Only `OTP[0xD0:0xE0]` is ever read (the reference's
single OTP access, `sonostool.py:113`), so a partial dump down to `0xE0` is
enough — the tool accepts it. `--layout=auto` picks the family from the field
magic at `0x580` so an operator does not have to know it.

The same repo's public dumps are kernel/rootfs only, and the
`BookCatKid/sonos-firmware-archive` key-recovery ledger records that no public
MDP3+OTP pair exists (the public paths are the EL3 exploit on a physical
Amlogic device and `mdputil` on a rooted fenway unit). So the tool now covers
both documented layouts and still waits on one dump.

## How the two inputs come off the owner's own hardware

Neither input is in any shipped package, so the dump procedure is part of the
tool's contract:

- **S1 (PPC, 57.23).** `/dev/mdp` is the MDP device (`mdputil` strings at
  `0x3240` and `0x3270`). `mdputil` is statically linked with the whole
  `libupgrade` MDP API (`upgrade_get_mdp3` at `0x74a`, `upgrade_mdp_write` at
  `0x898`) and prints `MDP3VERSION=%u` (`0x2edc`), but its getopt string
  `Aa:whfv:M:m:r:e:s:Ik:b:l:F:C:P:T:i:d:Bx:y:z:L:` (`0x2c6c`) has no dump
  option, so it never emits a key blob. Note `MDP3VERSION=%u` is read from
  **MDP1+0x78** (`lw a2,120(s0)` at 86.10 `mdputil` `0x401644`), not from the
  MDP3 page — the field is a version counter in MDP1. A dump is `dd if=/dev/mdp bs=1
  skip=0x1200 count=0x4000` (the smdp page offset, `--smdp`) or a small helper
  against `upgrade_get_mdp3`.
- **OTP on S1.** No userland tooling: `grep -rl otp s1_rootfs` matches only
  `sbin/udhcpc` and `etc/services`, there is no `/dev/otp`, and the inflated
  kernel (`57.23-74170-1-16-kernel.uImage`, gzip at `0x5779`) has no `efuse`
  string — the OTP is read by the bootloader, so a userland read has to go
  through `devmem` on the eFUSE window.
- **Play:1 / fenway.** `mdputil` is present in that rootfs too; the fenway
  layout applies (`--layout=fenway`).
- **Amlogic (86.10).** The rootfs is public: `86.10-80260-1-12-rootfs.squashfs`
  from `BookCatKid/sonos-firmware-archive` release `firmware-86.10-80260`,
  sha256 `218687383c08648d366d31f5d26bf27b55a8ce7f1f450bc55e5cae1b7e527c1b`
  (matches the repo's receipt). It carries `lib/libflash.so.1` and
  `bin/mdputil`. `libsonos-mdp.so.1` there is only the colour/serial helper
  (`sonosGetColorVariantString`, `sonosMdpSerialToString`) — no flash access.
  `libflash.so.1` has **only** `hal_flash_get_mdp` (512 bytes, `0x18ca`) and
  `hal_flash_get_mdp2` (4096, `0x1a84`); there is **no** `get_mdp3`. The MDP
  offset is not a constant: it is a flash-info field at `+80`, filled in
  `hal_flash_info_init` (`0x15b4`) from the partition table and read back in
  `hal_flash_get_mdp` (`0x18c8`). So an S2 dump is
  `read_from_offset(fd, info+80 + 0x1200, 0x4000)` — the page sits at the smdp
  offset inside the MDP region, which is exactly what `--smdp` slices.

**Nothing at runtime reads the MDP3 page.** 86.10 `anacapad` has no
`read_from_offset` and no `hal_flash_get_mdp3` (only `hal_flash_get_mdp`, 4
refs), and its MDP call sites consume MDP2 device info only. So MDP3+OTP is an
**offline package-decryption** input, not a device-behaviour input: the tool
exists for the former and no daemon path depends on it.

## What userland reaches today

`anacapad` imports `hal_flash_get_mdp`, `hal_flash_get_mdp2`,
`hal_flash_get_mdp_wrapper`, `hal_flash_get_mdp2_wrapper` (S2 also
`hal_flash_mdp_write`) and calls them in five places on 86.10:

| site (86.10) | what it does |
|---|---|
| `0x475bae` | fills a small global (model/vendor) at startup; `RabortIfUnlocked` on failure |
| `0x4a9bc4`, `0x4a9bd0` | reads MDP + MDP2 into 0x4000/0x1000 stack buffers, ORs flag bit 0x20, writes MDP back (`0x656350`) — a migration/repair path |
| `0x4cc944`, `0x4cc962` | same pair in the devmode/mfgunlock area |
| `0x596042`, `0x596054` | MDP/MDP2 → `sonosGetFinalColorVariant` |
| `0x623a86` | MDP2 → global device info: 708-byte `memcpy` (serial/model block), 128 bytes from `+0x2c4` |

None of them copy the sealed key fields out; all consume MDP2 device info.

Exposed over HTTP, measured on the three reachable units:

- `/status/zp` and `/support/review` carry `<MDP2Ver>` and `<MDP3Ver>`
  (`.81` 0/0, `.86` 5/2, `.202` 0/0). Version only.
- No route runs `mdputil` or `keyval` (neither name appears in either daemon).
  `mdputil` (in the rootfs, `-A`-style dump of VENDOR/MODEL/…/MDP3VERSION/
  FUSEVAL) and `keyval` (a text key-value reader, not MDP) are shell-only.
- `upgrade` reads MDP3 for the model key but logs no key material
  (`strings`: only "MDP3 version %u, min version %u" and decrypt errors), so
  the readable `/jffs/upgrade.log` does not leak it either.

## Conclusion

There is **no pre-auth userland path to MDP3 or OTP** on 57.23 or 86.10: the
daemon reads MDP only for device identity and exposes version numbers, and the
tools that could dump the fields need exec. The lane's remaining value is the
tool itself plus the fact that `.86` reports MDP3 v2 — the layout sonostool
targets — so a single shell (or any future exec primitive) converts to the
model key immediately.

## Next

- When exec exists: `mdputil` dump for the non-secret fields, then read the
  MDP3 partition (`/dev/mtd*` via `hal_flash_get_mdp`) and OTP, and run
  `mdp3.py decrypt … model`.
- If a status page is ever made to exec, `mdputil` is the smallest target.
