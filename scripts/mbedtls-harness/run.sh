#!/bin/sh
# Run xmparse against a prepared sysroot tree, under qemu-user.
#
#   SYSROOT=/tmp/hb/root ./run.sh leaf.der [ca.der] [--loop N]
#
# SYSROOT must contain the device loader as lib/ld.so.1, libc.so.0 and the three
# mbedtls libs in lib/ (see README). The guest/qemu needs no kernel.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
: "${SYSROOT:?set SYSROOT to the prepared tree}"
exec qemu-mips -L "$SYSROOT" -E LD_LIBRARY_PATH="$SYSROOT/lib" "$here/out/xmparse" "$@"
