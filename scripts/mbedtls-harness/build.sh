#!/bin/sh
# Cross-build xmparse for the device's own mbedtls 2.5.0 (MIPS-I uClibc userland).
#
# Needs: clang with the MIPS target, mips-linux-gnu binutils, the mbedtls 2.5.0
# headers (tag mbedtls-2.5.0), and an extracted S1 rootfs for the libs/loader.
#
#   MBEDTLS_INC=/path/to/mbedtls-2.5.0/include ROOTFS=/path/to/s1_rootfs ./build.sh
#
# Produces ./out/{xmparse,start.o} and prints the exact ld line used. The binary
# links dynamically against the device libs; run it with run.sh.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
: "${MBEDTLS_INC:?set MBEDTLS_INC to the mbedtls-2.5.0 include dir}"
: "${ROOTFS:?set ROOTFS to an extracted S1 rootfs}"
OUT=${OUT:-$here/out}
mkdir -p "$OUT"
CF="-target mips-linux-gnu -mabi=32 -march=mips32 -msoft-float -O1 -ffreestanding -nostdinc"
CF="$CF -isystem $here/stubs -isystem $(clang -print-resource-dir)/include -I$MBEDTLS_INC"

clang $CF -c "$here/start.S"     -o "$OUT/start.o"
clang $CF -c "$here/xmparse.c"   -o "$OUT/xmparse.o"

# --hash-style=sysv is mandatory: uClibc 0.9.26's loader only knows DT_HASH and
# divides by zero on an empty .gnu.hash.
set -x
mips-linux-gnu-ld -m elf32btsmip --hash-style=sysv -e __start -o "$OUT/xmparse" \
    "$OUT/start.o" "$OUT/xmparse.o" \
    "$ROOTFS/lib/libmbedtls.so.10" "$ROOTFS/lib/libmbedx509.so.0" \
    "$ROOTFS/lib/libmbedcrypto.so.0" "$ROOTFS/lib/libc.so.0" \
    --dynamic-linker=/lib/ld.so.1 --allow-shlib-undefined
