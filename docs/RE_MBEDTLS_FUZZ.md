# RE_MBEDTLS_FUZZ — offline x509 harness on the device's own libmbedtls (bead frankensonos-wbz)

Lane: the certificate surface of the 57.23 updater, which was assigned as a pre-signature
surface. Deliverable is an offline harness that runs the **device's own** `libmbedtls.so.14` /
`libmbedx509.so.1` / `libmbedcrypto.so.7`, a deterministic corpus, and any fault reproduced with
a minimal DER.

Everything below is measured unless marked *(static)*, which means it is a reading of the
disassembly and has no runtime observation behind it.

## Verdict (2026-10-27)

1. *(static)* **The type-22 DER table is not parsed before the signature check.** The updater is a state
   machine (`0x100036f4`-`0x10003728` dispatches on a stage value: stage 0 → `0x100034a4`, which
   calls the parse/verify driver `0x10007fa0`; stage 4 → `0x10003514`, the write-sections
   pass). In the parse pass the type-22 handler `0x10007db0` only *reads* the section
   (`0x10006da4`); the type-22 header parse (`ZZ0121`) and its signature verify (`ZZ0124`) live
   in the write pass handler `0x10008890`, which only a later stage reaches after the trailer
   handler `0x10007e98` verified the type-17 signature. A modified bundle therefore needs a
   valid signature to be parsed at all. `docs/RE_CVES.md` §6 ("plaintext and parsed before any
   signature check") is wrong on this point; the three poisoned device bundles were nulls by
   construction.
2. **`MBEDTLS_X509_RSASSA_PSS_SUPPORT` is compiled out of the 57.23 build.** A valid
   PSS/SHA-256 certificate is rejected at `-0x2300` (`X509_INVALID_ALG`) while the same file
   parses on a stock 2.28.10 build. Every PSS fixture in `captures/research/cve-lane/`
   (CVE-2018-0487 family) is therefore inert on this firmware — 2.28 is past that CVE anyway,
   and the verify path is never entered.
3. The library is **2.28.x** (SOVERSION triple 14/7/1), not 2.5.0/2.16.x. `MBEDTLS_ERROR_C`
   is also off (`mbedtls_strerror()` returns an empty string).
4. Against that build the structural and targeted sweeps below produce **no reproducible
   fault**. The x509 parser on 57.23 is clean for the shapes tried.

## What the device actually ships

| fact | evidence |
|---|---|
| mbedTLS 2.28.x | `libmbedtls.so.14`, `libmbedcrypto.so.7`, `libmbedx509.so.1` in `57.23-74170-1-16-rootfs.bin`; `own_buffer` present in the `x509_crt` layout |
| PSS support **off** | `parse_der` of a valid `rsassaPss`/SHA-256 cert → `-0x2300`; same file → `0` on stock 2.28.10 |
| `MBEDTLS_ERROR_C` off | `mbedtls_strerror()` writes an empty string |
| Sonos time override | `mbedtls_x509_set_time` (0x3b3c) stores its argument into a global that the validity checks `blrl` — the argument is a **`time_t (*)(void)`**, not an epoch. Passing an integer segfaults. |
| real API sequence | `anacapad` dlsyms `mbedtls_x509_crt_parse_der_nocopy`, `mbedtls_x509_crt_verify_with_profile`, `mbedtls_x509_set_time` (strings at `0x10004099`/`0x100040bb`/`0x100040e0`) |

`_nocopy` matters: the chain points into the caller's buffer, so the buffer must outlive the
verify. The harness keeps it alive for the whole run.

## Harness

`recon/mbedtls-ppc/` — Dockerfile, `harness.c`, `build.sh`, `gen_corpus.py`, `gen_targeted.py`,
`dertree.py`, `mkbase.py`, `run_corpus.py`, `confirm.py`.

- qemu-ppc user mode, cross glibc (`powerpc-linux-gnu`), **device `.so` files** copied next to
  the binary with a transitive `RPATH` (`$ORIGIN/lib`). The device `.so` only needs
  `GLIBC_2.0`/`GLIBC_2.1.3`, both still exported, so no device libc is involved.
- Modes: `parse`, `nocopy`, `dn` (`mbedtls_x509_dn_gets`), `info` (`mbedtls_x509_crt_info`),
  `verify` (self-trust or a supplied CA), `chain`. A signal is the interesting outcome; the
  runner records exit code and signal per case.
- The clock is pinned through the device's own `mbedtls_x509_set_time` hook, so validity never
  depends on wall time. `HARNESS_NOW` overrides it, `HARNESS_NO_TIME` disables the hook.

Build and run:

```bash
docker build -t fsonos-mbedtls-ppc recon/mbedtls-ppc
docker run --rm \
  -v "$PWD/captures/research/mbedtls-ppc:/w" \
  -v "$PWD/recon/mbedtls-ppc:/src:ro" \
  -v "$PWD/captures/firmware/57.23-74170-1-16/rootfs-extracted:/rfs:ro" \
  fsonos-mbedtls-ppc sh -c 'cp /src/*.py /src/harness.c /src/build.sh /w/ && sh /w/build.sh &&
                            cd /w && python3 run_corpus.py corpus/struct_fixed out/struct_fixed.tsv'
```

`rootfs-extracted` is produced once with
`unsquashfs -d rootfs-extracted 57.23-74170-1-16-rootfs.bin` (git-ignored).

## Corpus

Deterministic, generated from a real parsed DER tree and re-serialized so every enclosing
length is correct — the first pass spliced bytes in place and every case died at the outer
SEQUENCE, which is why it produced no signal.

| corpus | base | cases | families |
|---|---|---|---|
| `struct_fixed` | RSA-2048 self-signed | 1648 | wrong declared length (696), truncation (114), tag swap (780), child duplication (18), emptied value (36), nesting (4) |
| `targeted_fixed` | same | 105 | DN/RDN (36), validity time (25), algorithm id (16), SPKI (5), extensions (13), serial (5), version (5) |
| `ec_struct` | EC P-256 with SAN/BC/KU/EKU/NC | 2848 | same structural families |
| `ec_targeted` | same | 105 | same targeted families |
| `chain_struct` | RSA-2048 leaf signed by a test CA | 1606 | same structural families, verified against the CA |
| `chain_targeted` | same | 105 | same targeted families, verified against the CA |

Targeted detail: empty Name, empty SET, RDN with empty SEQUENCE, OID without value, value
without OID, 1000 RDNs, CN declared `0xffff`, every string type (BMP/Universal/Teletex/
Numeric/Printable/IA5), odd-length BMPString, truncated UTF-8, embedded NUL, 200 nested
SET/SEQ, duplicated RDN; UTCTime/GeneralizedTime malformed forms (no `Z`, month 13, day 00,
leap second, 5-digit year, offset form, embedded NUL, 20-digit year); sig-alg OID swaps
(PSS, MD5, SHA-1, ECDSA, unknown, empty), parameters removed/NULL/300 bytes; BIT STRING
emptied, without the unused-bits byte, unused-bits 7 and 0xff, truncated; duplicate extension,
unknown extension OID, empty OID, empty/`0xffff`/truncated extension value, critical `0xff`,
SAN with zero and with 200 entries, basicConstraints pathLen `0x7f`, keyUsage with extra bits,
nameConstraints, authorityKeyIdentifier; serial empty/zero/negative/40-byte/leading zero;
version 0/1/2/3/0xff/empty.

## Results

Every case runs in all five modes. A hit is re-run with `confirm.py`; the two that appeared did
not reproduce, so they are reported as observations rather than as a primitive.

### Sweeps

| corpus | modes | cases x modes | hits | reproducible |
|---|---|---|---|---|
| `targeted_fixed` (RSA) | 5 | 525 | 0 | — |
| `struct_fixed` (RSA) | 5 | 8240 | 1 | **no** |
| `struct_fixed` (RSA), re-run | 5 | 8240 | 0 | — |
| `ec_targeted` | 5 | 525 | 0 | — |
| `ec_struct` (EC) | 5 | 14240 | 1 | **no** |
| `chain_targeted` (leaf vs CA) | 5 | 525 | 0 | — |
| `chain_struct` (leaf vs CA) | 5 | 8030 | 0 | — |

Two single observations, neither reproducible afterwards:

| case | mode | fault | shape |
|---|---|---|---|
| `struct_fixed/tag_006_13.der` | `info` | SIGSEGV | signature-algorithm OID tag `0x06` → `0x13` |
| `ec_struct/len_043_zero_s4.der` | `nocopy` | SIGSEGV | a SEQUENCE declared length `0x00000000` in 4-byte long form |

Both cases re-run afterwards: 3400/3400 clean each, in fresh containers, with and
without pipes on stdout, with and without ASLR. A full second sweep of `struct_fixed` (all
five modes) also produced zero hits. They are recorded as observations, not as a primitive.

Both files are small and stable, so a future pane can chase them directly:

```bash
qemu-ppc -L /usr/powerpc-linux-gnu ./out/harness info   corpus/struct_fixed/tag_006_13.der
qemu-ppc -L /usr/powerpc-linux-gnu ./out/harness nocopy corpus/ec_struct/len_043_zero_s4.der
```

## Why the device runs so far were nulls

The three poisoned type-22 images in `captures/research/cve-lane/poison/` splice PSS
certificates into the bundle. Two independent reasons they could not fault:

1. the bundle is verified before it is parsed, and the splice breaks the type-22 digest, so
   `upgrade` never installs it and `anacapad` never loads it;
2. even with a valid signature, `rsassaPss` is rejected at the algorithm identifier, so
   `mbedtls_rsa_rsassa_pss_verify_ext` is never reached.

A future device run needs a **non-PSS** certificate and a valid signature. Since the type-22
digest is `SHA-256` over the bundle bytes and the signature is in the type-17 header, the only
way to ship an edited bundle is a vendor key — the same wall the rest of the root lane is at.
The DER surface is therefore a *post-signature* surface, and worth device time only for a
fault that a genuine-signature package could still carry.

## The route that is actually reachable

`anacapad` carries its own bundle path, independent of the updater:

```
/etc/fallback_trusted_roots.rcb            (rootfs, genuine)
/certbundles/v4/trusted_roots.rcb          (cloud fetch)
%s/sys/run/rcb/trusted_roots.rcb.tmp       (install target)
%s/sys/run/rcb/root_certs_metadata.txt
rcbundle
```

strings at `0x104f7f70`, `0x104f8038`, `0x104f805c`, `0x104f7f90`, `0x104f8314`, `0x104fbd94`.

So the daemon fetches a trust bundle from the cloud and installs it. That is the one path where
an attacker-supplied DER can reach `mbedtls_x509_crt_parse_der_nocopy` **without a vendor
signature** — the gate becomes the TLS session, not the package signature. Combined with the
already-proven eMudhra forged leaf (`docs/EMUDHRA_AND_FIRMWARE.md`) this is a coherent chain:
forged chain → serve a hostile `trusted_roots.rcb` → device parses attacker DER in root context.
Whether the daemon verifies the downloaded bundle before installing it is the open question and
is the next thing to read (the fetch/install function around the strings above).

## Reproduction of the PSS finding

```bash
openssl req -x509 -newkey rsa:2048 -keyout pss.key -out pss.pem -days 3650 -nodes \
  -subj "/CN=pss-test" -sha256 -sigopt rsa_padding_mode:pss -sigopt rsa_pss_saltlen:32
openssl x509 -in pss.pem -outform DER -out pss.der
qemu-ppc -L /usr/powerpc-linux-gnu ./out/harness parse pss.der     # -0x2300 X509_INVALID_ALG
qemu-ppc -L /usr/powerpc-linux-gnu ./out/harness_ref parse pss.der # 0 (stock 2.28.10 control)
```
