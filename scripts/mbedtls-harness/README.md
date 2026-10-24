# mbedtls 2.5.0 x509 parser harness (offline)

Runs the **device's own** `libmbedtls.so.10` / `libmbedx509.so.0` /
`libmbedcrypto.so.0` out of an extracted S1 34.16 rootfs, with nothing but
qemu-user in between, so certificate mutations iterate without a player reboot.
It links the real library and calls the real
`mbedtls_x509_crt_parse_der`, optionally followed by
`mbedtls_x509_crt_verify`, and prints a canary around the certificate object so
an overwrite is visible even when the parser "succeeds".

## Build

Needs clang (MIPS target), `mips-linux-gnu-ld`, the mbedtls **2.5.0** headers
(tag `mbedtls-2.5.0`; verify with `MBEDTLS_VERSION_STRING`), and an extracted
34.16 rootfs for the libraries and the uClibc loader.

```sh
MBEDTLS_INC=/path/to/mbedtls-2.5.0/include ROOTFS=/path/to/s1_rootfs ./build.sh
```

`stubs/` holds the handful of libc declarations the harness needs; the rootfs
ships no headers. `--hash-style=sysv` is required (uClibc 0.9.26 only knows
`DT_HASH` and traps on an empty `.gnu.hash`).

## Run

`SYSROOT` must contain `lib/ld.so.1` (the rootfs `ld-uClibc.so.0`),
`lib/libc.so.0`, and the three mbedtls libs — the same files that were linked.

```sh
SYSROOT=/tmp/rt ./run.sh leaf.der            # parse only
SYSROOT=/tmp/rt ./run.sh leaf.der ca.der      # parse + verify against ca
```

Exit is always 0 with the binary running; read `parse_ret` (`0` = accepted),
`canary: CLOBBERED`, and `verify:` / `flags:`.

## What was run (2026-10-24)

Base certificates: a 2048-bit RSA/SHA-256 self-signed cert, a 512-bit
RSA/SHA-256 one (emLen 64 == SHA-512 hLen, the CVE-2018-0487 shape), and the
lane's two `captures/research/cve-lane/pss-*-sha512.crt` RSASSA-PSS fixtures.

| sweep | mutations | crashes | clobbered |
|---|---|---|---|
| parse, every truncation + header byte flips + `0x30/0x02/0x06/0x03` length forcings | ~2 500 | 0 | 0 |
| verify, signature TLV length/overflow + 300 tail bit flips + truncations | ~1 000 | 0 | 0 |

Return codes seen: `-0x2180` (most mutations, incl. every PSS cert), `-0x2400`,
`-0x23E2`, `-0x3C80`, `-0x262E`; `verify -9984` with flag `0x10000` for
tampered/self-trust cases. `good.der` self-verified with `verify: 0 flags:0x0`.

## Result that matters: the build has no RSASSA-PSS in x509

Every PSS fixture fails at **parse** with `-0x2180`, while a 512-bit
RSA/SHA-256 cert of the same modulus length parses `0` (the code alone is not a
discriminator — a junk file also returns `-0x2180` — the control pair is). `libmbedx509.so.0`
contains no `pss` string and no `1.2.840.113549.1.1.10` OID, i.e. it was built
without `MBEDTLS_X509_RSASSA_PSS_SUPPORT`. Consequently **CVE-2018-0487 cannot
be reached through the bundle parser on 34.16** — the PSS verification code is
not compiled in — which matches the live poison-image runs on `.202` (daemon
restarts, no crash signature).

## Caveats

- Headers are stock 2.5.0; the library was built with its own config. The
  canary proves the struct size used here is not smaller than the library's.
- `-msoft-float`, `-march=mips32`; the device binaries are MIPS-I and run the
  same code paths under qemu.
- `libc.so.0` is the device uClibc, so the whole chain (loader, libc, mbedtls)
  comes from the image under test.
