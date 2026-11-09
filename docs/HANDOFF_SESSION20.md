# HANDOFF — Session 20: COMPLETE THE ROOT SHELL (all goals)

## THE SINGLE GOAL
Interactive persistent root shell on the Sonos units (.84 Play:1 PPC,
.202 Play:5 PPC, .135 Playbar MIPS) via pure network exploitation.
NO devmode portal (legal exposure — owner's explicit instruction).
NO serial console (the project = cutting-edge offensive exploitation only).
NO cloud accounts. Everything through the attack surface.

## WHERE WE ARE (verified live, 2026-11-08)

### Proven and operational
- Root code execution: the .aa overflow → chain → system() fires on all 3
  units (15+ fires, 12+ reboot signatures). The band 0x106c4ef0 is the
  proven working S for .84 (the payload builder: fire84.py).
- Per-unit firmware delivery: BeginSoftwareUpdate via SOAP (no DNS changes)
  — the genuine 65130 install + the 57.23 round-trip PROVEN on .84.
- The offline verdict harness: the S1 57.23 + the S2 86.10 upgrade binaries
  run under qemu on css (/tmp/s23rootfs, /tmp/s86sys).
- The fuzzer: 300 structure-aware mutants, checked-null, 0 crashes.
- All three units HEALTHY: .84/.202 = 57.23-74170, .135 = 86.10-81100.

### The three walls, each proven
1. THE EXEC-ENV: the .aa child (sh -c) never produces effects — 8 exec
   shapes × 2 archs, all 200/200 fires, all observables negative. The
   child's stderr = invisible without the shell (the chicken-and-egg).
2. THE SIGNING KEY: any firmware edit breaks the type-17 RSA signature.
   The vendor's signing key = compiled into their builds; the model-16
   envelope key (held) ≠ the signing key (proven byte-level, the keyring
   SPKIs identical across the 57.23/65130 builds @0xD8B8/0xDAE0/0xDC08/
   0xDED8). The version bump = dead without it.
3. THE HHID (the emulation wall — THE CURRENT FRONTIER): the daemon's
   mod_zp init waits on the MUSE household identity (the cloud-generated
   hhid). Under qemu the main deadlocks in sigsuspend × the linuxthreads
   manager in futex_do_wait. No listener binds → no local .aa repro yet.

## THE BREAKTHROUGH THIS WEEK (the reason session 20 CAN close it)

The qemu-ppc thread wall is BROKEN:
- The linuxthreads clone() EINVAL root-caused and patched (see the libc
  patch below) — threads SPAWN for the first time in the emulation.
- The daemon boots: config parse → mod_zp loaded → audio HAL → the
  sigsuspend event loop. THREE gates solved this week:
  a. the /dev/chk park (0x10007710: the open + ioctl 0x4004F301, the out
     must == 6550) — patched OR shimmed;
  b. the diag-collection abort (the 512× localhost:%d/raw/%s self-fetch
     retry → abort @0x1036bab4) — nop'd;
  c. the clone EINVAL — libc patched (see below).

## THE PRECISE DEADLOCK (decoded from /proc on the live stalled daemon)
- main = sigsuspend (the anacapa signal pump; waits for the init-done signal)
- the ONE spawned thread = futex_do_wait (the linuxthreads manager)
- the listener never binds: the mod_zp init waits on the HHID = the MUSE
  household identity generated at CLOUD REGISTRATION (not MDP2, not
  localsettings — the documented S2 wall, confirmed for the S1).

### THE LEAD (session 20's opening move)
The daemon contains a SYNTHETIC household identity:
- `HHID_111111111111111111111111111` at vaddr 0x104909b4
- the state flag `cloud_rcvd_muse_hhid` at vaddr 0x10491de8
- `urn:sonos:hhid` at 0x10490668
The flag has ONE direct xref: **0x1028631c**, inside the function
**0x102860f0** (stwu r1,-48) — the muse state machine. Session 20:
1. Boot the daemon under the gdb stub (-g 12346, the nopark binary).
2. Break at 0x102860f0 / 0x1028631c; single-step the state check.
3. Force the cloud_rcvd_muse_hhid flag TRUE and/or write the synthetic
   HHID into the state global (the gdb set).
4. The init proceeds → `anacapa is starting on port 1400/1443/1843` →
   the listener binds.
5. THE PAYOFF: fire the .aa at the LOCAL daemon (the fire_local84 recipe
   below) with -strace → the system() child's fork/exec VISIBLE → the
   exec-env failure named → fix the payload → fire on the REAL .84 →
   2323 → persistent root.

## THE EXACT RECIPE (verified working this week)

### css (ubuntu@209.145.54.164, the key ~/.ssh/contabo_new_baremetal_superserver_box.pem)
- The chroot: /tmp/s23rootfs (the S1 57.23 PPC rootfs + the dev fakes)
  - /dev/mtd0: the MDP magic 0xce10e47d @0, the MDP2 magic 0xca989b4a
    @0x200, version 3 @0x204, features 0x1e @0x208, submodel 1 @0x20c
  - /dev/mtd1..8: zero files; /dev/chk, /dev/leds, /dev/audioctl: regular
    files; /dev/mtd/0 + /dev/mtd/1: the older fakes
  - /jffs/system/audio_out_disable: PRESENT (the clean audio skip —
    REQUIRED; without it the audio HAL asserts on the fake submodel)
  - /ramdisk/optlog/: the diag traces (anacapa.trace = the live log;
    the GUEST clock in the timestamps)
  - the anacapa.conf: DiagLevel default=9 (REQUIRED for the init logs)
- /tmp/launchd.py: the daemon fork-launcher (the double-fork, the log to
  /tmp/s23rootfs/ramdisk/final.log). Usage: `sudo python3 /tmp/launchd.py`
- /tmp/s23_anacapad_nopark (md5 bfe23920...): the daemon with BOTH patches
  (the /dev/chk forced + the diag abort nop'd). THIS IS DEPLOYED as
  /tmp/s23rootfs/opt/bin/anacapad.
- /tmp/s23_anacapad_fastdiag (md5 f7ab5625...): + the diag retry 512→2 ×3
  (0x36b9f8/0x36baa0/0x36ba28). NOT deployed — swap in if the diag wait
  is too slow.
- /tmp/s23_libc_v5 (md5 1264992... — ALREADY DEPLOYED as
  /tmp/s23rootfs/lib/libc.so.6): the clone patch:
  - 0xCAE90 (vaddr 0xfb8ee90): `mr r6,r8` → `li r6,0` (0x38C00000) —
    child_tidptr = NULL
  - 0xCAE94 (vaddr 0xfb8ee94): `mr r7,r9` → `oris r3,r3,5` (0x64630005) —
    flags += CLONE_THREAD|CLONE_SYSVSEM
  - ENCODING NOTE: `oris ra,rs,imm` = 0x64000000|(rs<<21)|(ra<<16)|imm —
    the first attempt 0x64E30005 was `oris r7,r3,5` (rs/ra swapped!)
- /tmp/ledshim3.c: the HOST-side ioctl shim (the /dev/chk 6550 + the
  audioctl 12-byte) — NOTE: the LD_PRELOAD does NOT intercept the guest
  ioctl(2) (qemu does them as direct syscalls); only useful for the
  host-libc paths. The sudo STRIPS LD_PRELOAD — use `sudo env
  LD_PRELOAD=...`.
- /tmp/s1k/: the S1 kernel.uImage + rootfs.bin (the qemu-system-ppc
  full-system path — NOT yet attempted to boot; the fallback lane).
- The launch: `sudo python3 /tmp/launchd.py` → the pid prints; the init
  takes 5-10 min of wall time (the emulation is ~50-100× slower; the
  diag-retry phase is the slow part — the fastdiag binary fixes it).

### The startup sequence that WORKS (the 21:36-style full run)
1. `sudo python3 /tmp/launchd.py` on css.
2. Watch: `tail -f /tmp/s23rootfs/ramdisk/optlog/anacapa.trace`
   - the banner → mod_zp loaded → (the audio skipped, the flag) →
     [the diag wait: 2-10 min OR swap in the fastdiag] → THE HHID WAIT
     (the stall — the session 20's work happens HERE, via the gdb).
3. The gdb attach: the daemon must run WITH -g 12346 (the launcher
   doesn't pass it — either edit /tmp/launchd.py to add `-g 12346` after
   `qemu-ppc`, or use the systemd-run pattern with the stub).
4. THE GDB CAVEATS (learned the hard way):
   - the software breakpoints are UNRELIABLE on this mapping (the silent
     misses); the confirmed-entry breakpoints work (0x1028d1dc ALWAYS
     hits; 0x102860f0/0x1028631c = the session 20's targets);
   - the async 'continue&' + the python sleep/interrupt = RACES in the
     batch mode; use the plain 'continue' + the catchpoints
     ('catch signal SIGABRT' works);
   - the qemu-ppc -strace changes the TIMING → the audio assert returns
     under strace; use -strace only for the SHORT captures.

### The .aa fire at the local daemon (once :1400 binds)
- The payload: /tmp/s2decode/JFFSEX_84.aa (the proven 6408-byte chain:
  F=base, A@[6404], B@[428], V@[500], FN=system@plt 0x105464B0 @556,
  the cmd at 504 = ';dd if=/dev/mtd0 of=/jffs/upgrade.log bs=4096 count=1').
- The local fire: /tmp/aa_www/soapfire.py (the SetAVTransportURI SOAP with
  the audio/aac protocolInfo — the .84-verified shape) against
  http://127.0.0.1:1400/AVTransport/Control, the .aa served from
  :8899 (the systemd-run /tmp/aa_www pattern; use the SCRIPT launcher,
  NOT the inline systemd-run --property=WorkingDirectory — it 404s).
- The strace daemon (the -strace variant) = for the child capture: run the
  daemon with `-strace 2>/tmp/s23rootfs/ramdisk/tmp/full.log`, fire, then
  `grep -aE 'clone|execve|fork' full.log`.
- THE TIMING WARNING: the -strace runs abort at the audio (the timing);
  the audio_out_disable flag = REQUIRED for the strace runs too.

### The device-side fire (the REAL .84, after the payload fix)
- /tmp/s2decode/fire84.py <payload.aa> — the two-stage setav + Play fire
  at 192.168.4.84; the band 0x106c4ef0 (the current lifetime).
- The read-backs: /status/jffs/upgrade.log (the dd exfil route — WORKS as
  a route; the file must be written by the child), nc -z 2323.
- The 8 exec shapes are ALL documented in docs/ROOT_SWARM_STATE.md —
  do NOT re-test them blindly; the emulation strace = the discriminator.

## THE KEY vaddrs (the S1 57.23 anacapad, the base 0x10000000 = the file 0)
- 0x10007710: the /dev/chk check (the open + the ioctl 0x4004F301, the out
  == 6550); the nopark patch = `li r3,1; blr` (0x38600001, 0x4E800020)
- 0x100078e4: the park loop (`b 0x100078e4`) — the daemon parks here when
  the check fails; the 100% CPU = NOT a hang
- 0x1036bab4: the diag-collection abort (`bl abort@got.plt`) — nop'd
  (0x60000000)
- 0x1036ba90-bab0: the diag retry loop (the li r0,512 ×2 @0x36b9f8/
  0x36baa0; the manager li r9,512 @0x36ba28) — the fastdiag = 512→2
- 0x1036badc-...: the MDP2 parse: the magic 0xCA989B4A (the version > 3 →
  the field @+1116) OR the magic 0xFA87B921 (the ≤3 → the field @+2592)
- 0x102860f0 / 0x1028631c: the muse state machine / the
  cloud_rcvd_muse_hhid xref — SESSION 20'S BREAKPOINT TARGETS
- 0x104909b4: the synthetic `HHID_111111111111111111111111111`
- 0x10491de8: the `cloud_rcvd_muse_hhid` state-flag string
- 0x105464B0: system@plt; 0x10546420: execve@plt; 0x105462f0: fork@plt
- the error-string table (the S2 86.10 upgrade): 0x417af0, 20 entries
- the customsd route: 0x1052d204 (the handler 0x101516ec) — the pre-auth
  service registration (the csrfToken in the GET; the SMAPI triad =
  stringsUri/presentationMapUri/manifestUri = the untested parse surface
  behind the hhid gate — REACHABLE once the daemon is fully up!)
- /raw + /dropout_triggered//button_triggered: the diag XML upload (the
  S2 403s it; the hhid gate again)

## THE OTHER PROVEN FRAGMENTS (the session 19 sweep)
- /customsd = a PRE-AUTH config-write: the GET serves the form + a fresh
  csrfToken; the POST (the token + the fields) = 'Success!'; the service
  is advertised via the unauthenticated ListAvailableServices SOAP. The
  401/806 codes on GetSessionId/GetAppLink/GetDeviceLinkCode = the hhid
  gate. The form fields: sid (240-253/255), name, uri, secureUri,
  stringsUri, presentationMapUri, manifestUri, authType, pollInterval,
  caps. NO crash at the form layer (the name 200B, the interval -1 = 200).
- /status/<file> routes: the compiled table at 0x1052c60c+ (the pairs
  {route, type, cmd/file}); /status/VERSION works; the exec pages
  (ps/netstat/free/mount/lsmod) = empty UNIT-WIDE (the same exec-env).
- the status-table routes serve the FILES: /jffs/upgrade.log,
  /jffs/watchdog.log, /tmp/upgrade.log, /VERSION, /etc/resolv.conf,
  /tmp/memorylog/log.0-3 — the child-exfil targets (the writes must land;
  /jffs persists).

## THE DEVICE STATE (verified)
- .84 = 57.23-74170, .202 = 57.23-74170, .135 = 86.10-81100. All healthy.
- The band 0x106c4ef0 = the working S for .84 (the fires reboot it).
- The devmode inputs (NOT to be used — the legal): .84 16/000E5880108A,
  .202 16/000E585D8C10, .135 20/949F3EFD9AA8.
- The 86.11-82170-1-12.upd = staged (/tmp/q57-serve/, /tmp/s86 on css) —
  the S2 86.11 install = the alternative base lane (the same hhid gate).

## THE SESSION 20 PLAN (the full path to DONE)
1. The hhid unlock in the emulation (the bp 0x102860f0/0x1028631c, the
   flag force, the synthetic HHID write) → :1400 binds.
2. The local .aa fire + -strace → the child's fork/exec captured → the
   exec-env failure NAMED (the expected candidates: the chroot/jail, the
   fds, the PATH, the seccomp-ish checks, the watchdog fd inheritance).
3. The payload fix per the finding → the local repro PASSES (the effects
   visible in the emulated filesystem).
4. Fire the FIXED payload on the REAL .84 → 2323 → Configure.dev plant →
   the persistent root → replicate to .202/.135.
5. The wrap: the docs, the beads, the commits.
THE FALLBACK if the hhid force stalls: the qemu-system-ppc full-system
(/tmp/s1k/ = the kernel + the rootfs; the mpc8544ds board; the boot not
yet attempted — the raw kernel = the PPC boot stub, no U-Boot header).
THE FALLBACK 2: the S2 86.10 daemon (/tmp/s86) has the SAME hhid wall —
the S2's muse layer = the same fix pattern.
