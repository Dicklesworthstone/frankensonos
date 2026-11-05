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

## Follow-up: who can create Configure.dev

- The daemon never writes it: the only daemon xref is the `stat` at `0x102b2574`/`0x102b2578`
  that populates the `confdev` value. `confdev` is therefore **reported, not a switch**, and the
  "one registry flip arms the boot shell" idea does not hold on 57.23.
- The daemon's save path can only produce the names the setter knows. Formats at `0x1043de1c`ff:
  `%s/shadow2`, `%s/localsettings.txt`, `%s/settings`, `%s/netsettings.txt`, `%s/rinconradio.xml`
  (setter `0x10007a90`, compose at `0x10007bec`, store via free+strdup `0x10007c08`). The base is
  the config root; the tail is fixed, so a redirected base still lands as `…/localsettings.txt`
  and `/etc/Configure` never sources that file (checked: no script reads it).
- The string `system/Configure.dev` also lives in `lib/libsonosutils.so.1` (`0x9a34`, next to
  `Configure` `0x98e4`, `debug/devmode.bin` `0x99e8`), which exports the file API the daemon
  imports: `sonosFileCreate` (`anacapad` stub `0x10546790`, sites `0x1028abfc`, `0x102b8eb8`),
  `sonosFileCreateEx` (stub `0x10546d88`, site `0x100e6744`), plus `sonosCopyFile`,
  `sonosMkDirRecursive`, `sonosFileWriteInt`. Neither stub site passes a Configure.dev path
  (0x102b8eb8 builds its path via `0x102b8c4c`), so the creator of Configure.dev is still open —
  the lib string has no static xref either (PIC, runtime-composed).

So the live question is narrowed to: which route reaches `sonosFileCreate` with a
`system/Configure.dev` argument (or with a path we can steer).
