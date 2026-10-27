#!/bin/sh
# Build the PPC harness against the DEVICE's own mbedtls shared objects.
# Run inside the fsonos-mbedtls-ppc image with:
#   -v <repo>/captures/firmware/57.23-74170-1-16/rootfs-extracted:/rfs:ro
#   -v <repo>/captures/research/mbedtls-ppc:/w
set -e

RFS=${RFS:-/rfs}
W=${W:-/w}
SRC=${SRC:-/w/mbedtls}          # headers only (2.28.10, layout-compatible)
OUT=$W/out

mkdir -p "$OUT/lib"
for so in libmbedtls.so.14 libmbedx509.so.1 libmbedcrypto.so.7; do
    cp "$RFS/lib/$so" "$OUT/lib/$so"
done

powerpc-linux-gnu-gcc -O1 -g -Wall -Wextra \
    -I"$SRC/include" \
    -o "$OUT/harness" /w/harness.c \
    -L"$OUT/lib" -l:libmbedx509.so.1 -l:libmbedtls.so.14 -l:libmbedcrypto.so.7 \
    -Wl,--disable-new-dtags -Wl,-rpath,'$ORIGIN/lib' -Wl,--allow-shlib-undefined

echo "built $OUT/harness"
