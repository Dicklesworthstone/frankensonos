# S2 root door (SSH key, no listener spawned by us)

`/etc/inittab` already respawns `/etc/scripts/run_sshd.sh` **as root**. That script waits on
`/tmp/device_unlocked_flag` (10 s poll), creates `/jffs/sys/debug/ssh/authorized_keys` if absent and
then `exec /usr/bin/dropbear -R -F`. apacapad runs as `anacapa` with `-C all=eip`
(`CAP_DAC_OVERRIDE`), so one command is enough:

1. write the key + touch the flag (`s2_install.sh`),
2. the vendor's own root dropbear starts on :22,
3. `ssh -i key root@<unit>`.

Key type must be **P-256 ECDSA** — the dropbear build carries only `ecdsa-sha2-nistp256`
(no rsa, no ed25519) and the `/ssh/fingerprints` route hard-requires it. Success oracle:
`/ssh/fingerprints` 403 → 200 and port 22 open (`sonos-exploit probe`).

Driven by `sonos-exploit plant --device <ip>`.
