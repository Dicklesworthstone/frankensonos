# Route 4 — rmtShell: registry mapping and the real spawn gate

Answer first: **rmtShell has no in-daemon consumer.** The daemon's only static reference to the
key is the config-dump handler; the thing that spawns a remote shell is a *boot script*, and the
condition is the existence of `/jffs/system/Configure.dev`, not the setting value.

## The actual spawn chain (rootfs)

    /etc/Configure:79   if [ -f /etc/unlocked_build_flag -o -e /jffs/system/Configure.dev ]; then
    /etc/Configure:80       /etc/scripts/run_sshd.sh
    /etc/Configure:81       ln -s /opt/htdocs_locked /tmp/htdocs_locked
    /etc/Configure:82       touch /tmp/device_unlocked_flag
    /etc/Configure:85   fi
    ...
    /etc/Configure:87   if [ -f /jffs/system/Configure.dev ]; then
    /etc/Configure:88       sh /jffs/system/Configure.dev

and `/etc/scripts/run_sshd.sh` is five lines: `#!/bin/sh` … `exec /usr/sbin/telnetd`.

So the root route is: **make `/jffs/system/Configure.dev` exist**, whose *contents* then run as root
at the next boot. No argv, no execve, no signature.

Downstream of the flag the daemon is touched only for the "unlocked" case: the daemon-side starter
is **`0x102a8d28`** (latch byte `0x10545e00`, spawn helper `0x102a89d0`, script literal
`0x10497090`), with a status poller at `0x102a8f00` (`0x1038a950`, compares HTTP 200) and
start/stop writers of the latch at `0x102a8724`, `0x102a8804`, `0x102a8888`, `0x102a8dc8`,
`0x102a8f98`. None of these is constructor-reachable and the whole ssh cluster
(`0x102a87xx`–`0x102a8fxx`) contains **no** settings-API call — it is callback-driven.

## Registry mapping

- Key literal `rmtShell` at `0x1049875c`; its **only** xref is the dump handler `0x102b21e8` at
  `0x102b2518` (`r4 = key` → `0x10372640(ctx, key, buf)` → `0x103728bc(ctx, key, value)`).
- Same handler emits the neighbours in order: `devmode` `0x102b2500`, `rmtShell` `0x102b2518`,
  `unlockedBld` `0x102b252c`, `confdev` `0x102b2578` (value = `0x10546690("/jffs/system/Configure.dev")`,
  i.e. a stat/access result, literal at `0x102b2574`), `console` `0x102b2590`.
- There is **no id table**: no pointer array to these key strings exists in `.rodata`, `.data` or
  `.sdata`; the registry API takes the string (`0x10372640` get, `0x103728bc` pair-write) and the
  string→id assignment happens inside the map ctor `0x103725bc`/`0x1037229c`. So "the registry id"
  is an internal map slot, not a stable constant an attacker or a script can aim at.

## Consequence for the route

`confdev` is not a switch — it is reported from the file system. The one-registry-flip idea
therefore only works if something writes the *file*. What the daemon can already do is write a
file: the statement-save handler (`0x103940b8`, route `0x10394b70`) writes the POST body to the
path held at **`[statement+1072]`**, composed as `sprintf("%s/localsettings.txt", base)` by
`0x10007a90` (setter `0x10007bec`/`0x10007c08`, free+strdup per path). The name tail is fixed, so
the file is `localsettings.txt` — good for settings, not for Configure.dev by itself.
