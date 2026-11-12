#!/bin/sh
# emu_s1_bringup.sh - make the S1 chroot bootable under qemu on css.
#
# Recreates everything learned the hard way (see docs/ROOT_SWARM_STATE.md):
#   /dev/{urandom,random,null,zero} character nodes
#   /proc and /sys mounted (a reboot clears these; they MUST be re-mounted)
#   /proc/driver/audio/{dsp,audioctl} via a tmpfs over /proc/driver
#   /dev/{audioctl,dsp_dev} empty nodes        (sound device)
#   a 16-byte /dev/chk record                  (button event source)
#   settings keys dhcpMac / householdLocationId / hhid
#
# Does NOT start netstartd: it reconfigures the HOST NIC and takes css off the
# network. Start it by hand only if you accept that.
#
# usage: emu_s1_bringup.sh [ROOTFS]   (default /tmp/s23rootfs)

R=${1:-/tmp/s23rootfs}
set -e

mkdir -p "$R/dev" "$R/proc" "$R/sys"

# character devices
[ -e "$R/dev/null" ]    || mknod -m 666 "$R/dev/null"    c 1 3
[ -e "$R/dev/zero" ]    || mknod -m 666 "$R/dev/zero"    c 1 5
[ -e "$R/dev/random" ]  || mknod -m 666 "$R/dev/random"  c 1 8
[ -e "$R/dev/urandom" ] || mknod -m 666 "$R/dev/urandom" c 1 9

# procfs and sysfs (a reboot unmounts these)
mountpoint -q "$R/proc" || mount -t proc  proc "$R/proc"
mountpoint -q "$R/sys"  || mount -t sysfs sys  "$R/sys"

# audio control files the daemon opens
mountpoint -q /proc/driver || mount -t tmpfs tmpfs /proc/driver
mkdir -p /proc/driver/audio
: > /proc/driver/audio/dsp
: > /proc/driver/audio/audioctl
chmod 666 /proc/driver/audio/dsp /proc/driver/audio/audioctl

# sound device nodes and the button event record
: > "$R/dev/audioctl"
: > "$R/dev/dsp_dev"
chmod 666 "$R/dev/audioctl" "$R/dev/dsp_dev"
dd if=/dev/zero of="$R/dev/chk" bs=16 count=1 conv=notrunc status=none

# settings the cloud-registration gate wants
if [ -d "$R/jffs/settings" ]; then
  cat > "$R/jffs/settings/netsettings.json" <<'EOF'
{"fileVersion":0,"lastUpdateDevice":"RINCON_00000000000001400","fileSchemaVersion":2,"dhcpMac":"00-0E-58-80-10-8A","sonosnet":{},"networks":[]}
EOF
  cat > "$R/jffs/settings/householdsettings.json" <<'EOF'
{"fileVersion":1,"lastUpdateDevice":"RINCON_00000000000001400","fileSchemaVersion":2,"householdLocationId":"HHID_111111111111111111111111111","householdSettings":{"restricted-admin":{"settings":{"hhid":"HHID_111111111111111111111111111","explicitContentFiltering":false,"recentlyPlayed":false,"hhSwgenState":"auto:1"}},"restricted":{"settings":{"autoUpdatesEnabled":false}}}}
EOF
fi

echo "bring-up complete for $R (netstartd deliberately NOT started)"
