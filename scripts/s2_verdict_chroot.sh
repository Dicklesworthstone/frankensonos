#!/bin/bash
# S2 (86.10) upgrade-binary verdict harness on css — reproducible setup.
# Reconstructed 2026-11-08 (docs/ROOT_SWARM_STATE.md session 17).
#
# Usage: run on css (ubuntu@209.145.54.164):
#   bash scripts/s2_verdict_chroot.sh <package.upd>
#
# What it does:
#   1. (re)builds /tmp/s86sys: the 86.10 MIPS-BE sysroot + the upgrade binary
#   2. installs the wget-dialect wrapper (Sonos `-qC <report>` → GNU wget)
#   3. writes /var/run/upgradeinfo (Key: [value] format; the URL goes on ARGV)
#   4. serves the package on :8898 and runs the verdict under qemu-mips
#
# Facts baked in (from the Ghidra RE of libsonosminiutils + the child argv):
#   - upgradeinfo lines: 'Key: [value]'; '#' and empty lines skipped;
#     first ':' must be followed by ' ' then '['; a ']' terminates the value
#   - keys parsed by the upgrade binary: Report SonosID HouseholdID
#     CurrentVersion SystemVersion UpdateID Timestamp URL
#     (+ PrevSWGenMinVersion MinVersion Offline SWGenDowngrade on S1 57.23)
#   - the URL must NOT be in the file for the S2 binary — pass it as ARGV
#   - the child wget invocation: `wget -qC /var/run/upgrade_report.txt
#     -T 300 -O - <URL>`; GNU wget has no -C-with-arg → the wrapper drops it
set -euo pipefail

PKG="${1:?usage: s2_verdict_chroot.sh <package.upd>}"
SYS=/tmp/s86sys
SRV=/tmp/s86

# 1. sysroot sanity: the MIPS libs must be there and not x86-contaminated
bad=$(file "$SYS"/lib/*.so.* 2>/dev/null | grep -c x86 || true)
if [ "$bad" != "0" ]; then
  echo "x86 libs inside $SYS/lib — clean them (see session 17 notes)" >&2
  exit 2
fi

# 2. the wget wrapper (Sonos dialect → GNU)
mkdir -p "$SYS/usr/bin" "$SYS/bin" "$SYS/lib64" "$SYS/tmp" "$SYS/var/run"
cp /usr/bin/wget "$SYS/usr/bin/wget.real"
cp /lib64/ld-linux-x86-64.so.2 "$SYS/lib64/" 2>/dev/null || true
for L in $(ldd /usr/bin/wget | awk '/=>/ {print $3}'); do cp "$L" "$SYS/usr/lib/"; done
cp /bin/sh "$SYS/bin/sh"

cat > "$SYS/usr/bin/wget" <<'EOF'
#!/bin/sh
# Sonos wget dialect: -qC <reportfile> -T <t> -O - <URL>  →  GNU: drop -C's arg
q=""; t=""; o=""; url=""
while [ $# -gt 0 ]; do
  case "$1" in
    -q) q="-q" ;;
    -C) shift ;;
    -T) shift; t="-T $1" ;;
    -O) shift; o="-O $1" ;;
    *)  url="$1" ;;
  esac
  shift
done
LD_LIBRARY_PATH=/usr/lib exec /usr/bin/wget.real $q $t $o "$url"
EOF
chmod +x "$SYS/usr/bin/wget"

# 3. the upgradeinfo (the URL goes on ARGV, not in the file)
printf 'CurrentVersion: [86.10-81100]\nMinVersion: [86.0-1]\n' \
  > "$SYS/var/run/upgradeinfo"

# 4. serve + run
if ! curl -s -o /dev/null "http://127.0.0.1:8898/$(basename "$PKG")"; then
  systemd-run --unit=upgsrv --property=WorkingDirectory="$SRV" \
    /usr/bin/python3 -m http.server 8898
  sleep 2
fi

chroot "$SYS" /usr/bin/qemu-mips ./bin/upgrade \
  "http://127.0.0.1:8898/$(basename "$PKG")"
