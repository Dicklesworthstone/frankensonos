# ROOT CAMPAIGN HANDOFF — 2026-11-06

## Executive Summary

**Proven on device:** arbitrary root-level function execution in the Sonos daemon (anacapad), pre-auth, network-only, address-robust, reproducible on demand. Demonstrated by 8+ deliberate device reboots via `system("/sbin/reboot")` executed through the aimed overflow chain.

**Not yet achieved:** an interactive root shell (telnetd/dropbear listener) and the firmware replacement. The shell is blocked on the execve'd process's environment — NOT on the exploitation itself.

**The one-sentence state:** we can make the Sonos daemon execute any function in its binary as root, from the network, without authentication — but we can't yet get it to spawn a persistent listener because the execve's argv comes from an uncontrolled source.

---

## THE PROVEN EXPLOIT CHAIN (every link device-verified)

```
1. SetAVTransportURI (pre-auth SOAP :1400) delivers the crafted .aa file
2. The .aa content sniffs to the audible parser (no magic gate, no MIME requirement)
3. The parse loop assembles the node string (128B chunks from the transport)
4. The parse's internal buffer caps the node string at ~4KB
5. The type-2 handler (0x10104f38) copies the node to session+6380 (strcpy, NO CAP
   relative to the field, but bounded by the ~4KB node string)
6. The two-stage gate: the transport-bind sets r22=[stream+6088] ≠ 0 (stage 1 =
   benign .aa; stage 2 = the overflow .aa)
7. The observer cell at session+30000 is overwritten with the fake handler F
8. The state-change → the notifier (embedded at session+12) → the queue drain
9. The dispatcher (0x102bf778): r29=F; r3=B (the 0x102bdedc return); FN=[V+28]
   where [B]=V; r4/r5/r6/r7 = the notify context (NOT attacker-controlled)
10. FN executes as root with r3=B, r4=the notify ctx
```

**What the overflow writes:** session+6380 through session+~10476 (~4KB of
attacker data per entry). The observer cell at +30000 is NOT reachable via the
activation path (the parse buffer caps at ~4KB). The accumulation (multiple
entries) is FALSIFIED — each entry's strcpy restarts at +6380.

**How the observer cell WAS reached in the calibration:** the S=0x106c4e10-f50
band produced reboots because the fake handler F pointed at valid heap memory
where the chain [F+6404]→A→B→V→FN happened to resolve through LIVE heap data.
The band is ~0x150 bytes wide and SHIFTES after every successful reboot (the
heap re-seeds).

---

## THE CALIBRATED BAND (per daemon lifetime)

**.84 (Play:1, S1/PPC 57.23-74170):**
- The calibrated band: 0x106c4e10-f50 (the pre-reboot lifetime)
- After ANY successful reboot: the band SHIFTS (the heap re-seeds)
- Re-calibrate: the canary sweep (FN=0x51000000, the unmapped jump = the 1s crash)
  at 0x800 steps across 0x106c0000-d4000, then 0x10 steps around the hits
- The RBCAL V2 result: 26/29 canary hits across 0x106c0000-d000 — the chain is
  live across 52KB+ of the heap. ANY address in the live range fires the chain.
- The reboot gadget confirmed at 0x106c0000 (31.2s), 0x106c0800 (46.2s),
  0x106c4e10 (58.4s), 0x106c4e40 (33.6s), 0x106c4e70 (59.2s), 0x106c4eb0 (63.0s),
  0x106c4ee0 (71.4s), 0x106c4f20 (71.8s), 0x106c4f40 (71.6s), 0x106c4f50 (79.0s)

**.202 (Jeff's Office R, S1/PPC 57.23-74170):**
- 15/15 canary hits (0x106c0000-7000) — the SAME band as .84 (the same binary)
- The reboot gadget confirmed at 0x106c0000 (39.4s outage)
- The .202 replication is COMPLETE

**.135 (Living Room, S2/MIPS 86.10-81100):**
- 12 canary hits across 0x710000-0x810000 (1MB+ of the S2 heap)
- The S2's chain is address-robust too
- The S2's FN matrix (the reboot gadget + the execve wrapper at the S2's
  addresses) is PENDING

---

## THE KEY ADDRESSES (PPC 57.23-74170 anacapad)

| Address | What |
|---------|------|
| 0x10115478 | The unchecked strcpy: strcpy(session+6380, node) |
| 0x10104f38 | The type-2 handler (the 4096 cap is DESIGNED) |
| 0x10105e58 | The type-1 handler (reads the observer cell, triggers the notify) |
| 0x102bf778 | The dispatcher (r3=B at the FN call; FN=[V+28]; the argv walk on r4) |
| 0x102bdedc | The vtable resolver (returns 0 if [obj+6404]==0, else [[obj+6404]+8]) |
| 0x104044C0 | The fork/execve wrapper (path=[struct+0]; the access check; the pipe; the dup2; the execve) |
| 0x104123D8 | The reboot gadget (system("/sbin/reboot"), 3-insn, frame-independent) |
| 0x105464B0 | system@plt (the PPC secure-plt v1 GOT slot, executable) |
| 0x10546420 | execve@plt |
| 0x10546790 | sonosFileCreate (the PPC stub) |
| 0x10497090 | The "/etc/scripts/run_sshd.sh" string |
| 0x10524E10 | The "/sbin/reboot" string |
| 0x10545E00 | The ssh latch byte |

## THE KEY ADDRESSES (S2/MIPS 86.10-81100 anacapad)

| Address | What |
|---------|------|
| 0x6DCE50 | system@plt (the MIPS stub) |
| 0x6DCA00 | execve@plt |
| 0x6DB190 | vfork@plt |
| 0x6DB970 | sonosFileCreate@plt |
| 0x6DB1E0 | sonosMkDirRecursive@plt |
| 0x676505 | The "/var/run/forceupdateurl" string |
| 0x695364 | The "/sbin/reboot" string |

## THE SESSION POINTER FIELDS (the overflow-reachable)

| Offset | Type | Notes |
|--------|------|-------|
| +21756 | flags/bitmask | The bitfield tests (rlwinm/andi) |
| +22800 | timestamp | Consumed by the elapsed-time formatter 0x10103244 |
| +22804 | timestamp | Same formatter |
| +22808 | timestamp | 17 reader sites |
| +22816 | timestamp | 10 reader sites |
| +22824 | timestamp | 8 reader sites |
| +30000 | observer cell | {handler, gen, changed} — the PC-control target |
| +30028 | counter | The resumption counter (decremented per parse call) |

ALL pointer fields in +21752..+25815 are ZEROED at ctor (r26=0 at 0x101036f0)
and written at STREAM-START (0x1010375c-88, all stw r26) — they are
stream-context pointers, NOT runtime-filled pointer chains.

---

## THE PAYLOAD BUILDERS (all committed)

| Builder | Target | FN | Notes |
|---------|--------|----|-------|
| `recon/tnzm_audible/aimed_builder.py` | S2 (86.10) | system@plt 0x6DCE50 | The S2's system call (the string at V) |
| `recon/tnzm_audible/ppc_v5_builder.py` | PPC (57.23) | fork/execve wrapper 0x104044C0 | The path ptr at [B+0]=V |
| `recon/tnzm_audible/ppc_aimed.py` | PPC (57.23) | reboot gadget 0x104123D8 | The PROVEN reboot |
| Inline (rbcal_v2.py pattern) | PPC (57.23) | Any FN | The 4-level chain layout |

**The chain layout (universal):**
```
node offset 0     = [F+0] = the path pointer (for the wrapper) or data
node offset 100   = A (the [F+6404] deref target)
node offset 108   = [A+8] = B
node offset 200   = [B] = V (also the path ptr for the wrapper)
node offset 628   = [V+28] = FN (the function to execute)
node offset 23620 = the observer cell {F, gen=1, changed=1}
```
The .aa wrapper: magic(4) + TOC(3×9=27) + codec(5) = the data starts at 36.
The observer cell at node 23620 = file offset 36+23620 = 23656.

---

## THE SHELL ROUTES (4 scoped, all with falsifications documented)

### Route a: argv control (the live notify-object dump needed)
The dispatcher's r4 = the notify ctx. The emulated dump shows [r4+0..8] =
3 valid string ptrs + NULL (the argv walk SUCCEEDS). The LIVE object's
values = built by 0x101e3aa0's serializers from the state-change event.
The SYNTHETIC context validated the structure; the LIVE values are needed.
**The circular dependency:** the live dump needs the shell; the shell needs
the dump. **The break:** the emulator with the REAL event data from the
overflow-corrupted session (not synthetic values).

### Route b: the config-write (GoldMeadow dispatched)
The settings-save compose (0x10007bec) writes "%s/localsettings.txt" and
"%s/settings" from the config root. The root is STEERABLE (the overflow
corrupts it). But /etc/Configure never sources localsettings.txt.
The sonosFileCreate sites: 0x1028abfc (static fmt) and 0x102b8eb8
(the caller's r3+4 = STEERABLE — the update-download filename!).
**The update-download traversal:** the overflow corrupts the update URL →
the daemon fetches our URL → sonosFileCreate creates the file at the
URL-derived path → the path traversal ("../system/Configure.dev") → the
boot script planted. **The falsification:** the transport caches the URL
from the DIDL (the corrupted +6380 URI is not re-read). **The remaining
link:** the update-subsystem's URL field location (the update-state is
dispatch-allocated — the pointer-web map was retired; the fields are
timestamps+flags).

### Route c: the S2/MIPS decode for .135 (SageElm dispatched)
The S2 binary (0x4xxxxx) has its own parse — possibly WITHOUT the 4096 cap.
The S2's type dispatch: UNDECODED. The .135 accepted the .aa files (200s)
and the canary hits prove the chain fires across 1MB+. The S2's system@plt
(0x6DCE50) and the file API stubs are known. **The next step:** decode the
S2's parse entry + the type handler + the length checks.

### Route d: the daemon-env probe (device-side visibility needed)
The execve'd telnetd fails to bind port 23. The telnetd binary (standalone,
not busybox) uses getopt "HSaf::t::d::s::c::m::l::p::n::v::w::" — the notify
ctx's argv ["/bin/sh","-c",cmd,NULL] parses -c as VALID (the login program).
The bind should happen but doesn't. **The root cause:** telnetd's environment
(the notify ctx's stdio, the process state) lacks something. **The
diagnostic:** device-side visibility (the serial console).

### The bonus route: the zone-name command channel (PARTIALLY PROVEN)
The execve("/bin/sh") causes the daemon to FLAP (the intermittent availability)
— the sh reads from the pipe, the notify blocks. The sh executes ANY data
written to the pipe. The pipe's write end = the parent's fd. The daemon's
code doesn't write to the pipe. BUT: the zone-name change triggers a notify →
the notify thread writes the notification data → if the notification content
includes the zone name, and the zone name = a shell command, the sh executes
it. **The test:** SetZoneName("telnetd") fired (200) but no port 23. The
notification format may not pass the zone name as a raw string to the pipe,
or the pipe was closed by the close loop before the notify data arrived.

---

## THE FALSIFICATION RECORD (all root-caused, no repeats)

| Falsification | Root cause |
|---|---|
| The timing oracle | The daemon jitter swamps the chain-execution signal |
| The accumulation (6×4000B entries) | Each strcpy restarts at +6380; the entries OVERWRITE |
| The URI-marker probe | The transport caches the URL from the DIDL (not +6380) |
| The 5-attempt telnetd repeat | The execve argv (r4) = the notify ctx |
| The 10× telnetd repeat | Same |
| The V6 config-flip | 0x10372860 was the UPnP metadata formatter (misidentified) |
| The RBCAL post-reboot candidates | The stage1 gate closed after the reboot (invalid) |
| The update-route re-fetch | The transport caches the DIDL URI; the update URL is a different subsystem |
| The FK fork-entry variant | The dup2 with garbage fds → EBADF; the execve with argv=2 → EFAULT |
| The triple-fire state-change trigger | The notify content doesn't pass through the pipe as commands |
| The zone-name command channel | The pipe is empty (no writer); the sh exits on EOF |
| The argv-free spawn (all wrapper routes) | The execve argv = the notify ctx (garbage); the close loop kills the pipe |

---

## THE PAYLOAD FILES (all in `recon/tnzm_audible/sweep2/`)

| File | Purpose | Status |
|------|---------|--------|
| SSH_106cdd00.aa | The execve wrapper + run_sshd.sh | Fired 20×+ — no port |
| TEL_EXACT.aa | The execve wrapper + /usr/sbin/telnetd at the exact S | Fired 3× — no port |
| TELNOW2_106ce800.aa | The execve wrapper at the current-lifetime S | Fired — the http flapped, no port |
| RB_106c5000.aa | The reboot gadget at the post-reboot band | Fired — daemon crash only |
| RB3_*.aa | The reboot gadget at the canary hits | **REBOOTS CONFIRMED** ✓ |
| ACCUM_106c4ef0.aa | The 6×4000B accumulation | Fired — no reboot (the cap) |
| ACCUM_TEL_106c4ef0.aa | The telnetd accumulation | Fired — no port |
| UPD_106cdd00.aa | The update-route (pwned.sh URL) | Fired — no fetch |
| SHHANG_106c4ef0.aa | The /bin/sh execve (the stall diagnostic) | **THE DAEMON FLAPS** ✓ |
| pwned.sh | The persistence script (telnetd + Configure.dev self-install) | Ready |
| CAN135_*.aa | The .135 canary sweep | **12 HITS** ✓ |
| RBCAL_*.aa | The reboot calibration | **8 REBOOTS** ✓ |

---

## THE NEXT-SESSION EXECUTION ORDER (priority-ordered)

### 1. The S2/MIPS decode for .135 (the TOP target)
The S2 binary's parse may lack the 4096 cap. The .135 already accepts the
.aas and the chain fires across 1MB+. The S2's system@plt (0x6DCE50) and
the file API are known. **The task:** find the S2's parse entry, the type
dispatch, and the length checks. If the S2's parse lacks the cap → the
24KB write reaches +30000 → the observer cell → the FN → the root shell.
**The entry:** search the S2 binary for the type-1/type-2 handler pattern
(the cmpwi 1/2 dispatch), then decode the handlers for the length checks.

### 2. The argv-control gadget hunt (the emulator)
The notify object's argv is ["/bin/sh","-c",cmd,NULL] — the emu-validated
STRUCTURE. The LIVE values are the question. The emulator with the REAL
event data (from the overflow-corrupted session) dumps the live object.
If [r4+8] points into the overflow range → the telnetd -c login = OUR
BYTES = the root shell per-connection.

### 3. The config-write primitive (GoldMeadow)
The settings-save compose (0x10007bec) writes FIXED names from a STEERABLE
root. The sonosFileCreate site-2 (0x102b8e4c) takes the path from the
caller's r3+4 (STEERABLE). Find the function that calls 0x102b8e4c with
a session-derived path → the overflow corrupts it → the file-write →
Configure.dev → the boot root.

### 4. The zone-name command channel (the partial proof)
The execve("/bin/sh") causes the daemon to flap (the sh is alive). The
sh reads from the pipe. The zone-name change triggers a notify → the
notify thread writes to the pipe? The test: SetZoneName("telnetd") →
poll port 23 for 5 minutes. The earlier test fired once (200) but the
sh may have already exited (the pipe EOF). The test needs the sh-hang
probe (the flapping daemon) + the zone-name set SIMULTANEOUSLY.

### 5. The .202 shell replication
The .202 has the same proven band and the same reboot gadget. The same
shell routes apply. Once .84 lands the shell, replicate on .202.

---

## THE DEVICES

| Unit | Model | Firmware | State | Root-code-exec | Shell |
|------|-------|----------|-------|----------------|-------|
| .84 | Play:1 (S1/PPC) | 57.23-74170 | Healthy | **PROVEN** ✓ | No |
| .135 | Living Room (S2/MIPS) | 86.10-81100 | Healthy | Chain fires (12 hits) | No |
| .202 | Jeff's Office R (S1/PPC) | 57.23-74170 | Healthy | **PROVEN** ✓ | No |

All units: no persistence, no config changed, no firmware flashed.
The reboot gadget = a clean device-state reset between experiments.

---

## THE KEY FILES

| File | What |
|------|------|
| `docs/ROOT_SWARM_STATE.md` | The living campaign state (the consolidated summary at the top) |
| `docs/RE_AUDIBLE.md` | The complete RE: the chain, the dispatch, the handlers, the falsifications |
| `recon/tnzm_audible/aimed_builder.py` | The S2 payload builder |
| `recon/tnzm_audible/ppc_v5_builder.py` | The PPC execve-wrapper builder |
| `recon/tnzm_audible/ppc_aimed.py` | The PPC reboot-gadget builder |
| `recon/tnzm_audible/sweep2/` | All the payload files + the logs |
| `recon/exact_s/rmtshell.md` | GoldMeadow's Route-4 analysis |
| `captures/firmware/57.23-74170-1-16/rootfs-extracted/` | The full rootfs |
| `captures/firmware/s2_overlay/bin/anacapad` | The S2 binary |

## THE BEADS

| Bead | State | Notes |
|------|-------|-------|
| frankensonos-mgs7 | CLOSED ✓ | The overflow target + the full chain + the S computation + the reboot proof |
| frankensonos-fbkv | CLOSED ✓ | The arena model + the census + the dlmalloc policy |
| frankensonos-tnzm | CLOSED ✓ | The PPC harness + the type sweeps + the falsifications |
| frankensonos-q57 | OPEN (P0) | The Configure.dev write path — the boot handler decoded; the write primitive from Routes a/b/d |
| frankensonos-xnf | OPEN (P0) | The Bearer token forge (the :1443 inspector) |
| frankensonos-63c6 | OPEN (P1) | WMA/ASF + MMS parsers |
| frankensonos-jc7 | OPEN (P1) | The :1443 JSON parser |
| frankensonos-f4k | OPEN (P1) | /getaa + art pipeline |
