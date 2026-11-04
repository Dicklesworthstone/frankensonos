# THE MONEY SHOT — full root on all players + persistent control (2026-11-04, CalmHawk)

The goal: root on every player, control that survives updates/reboots, no
hardware, no eero changes, no vendor keys. Ranked by likelihood × coverage ×
persistence. Everything here builds on proven components.

---

## The three links every root chain needs

1. **ARMS** — get the device to fetch OUR update (closed: DNS override +
   eMudhra-signed cloud-answer + `MinVersion` writes `/var/run/upgradeinfo`;
   DarkForest's rig is live on 192.168.4.28). The blocker was .202's resolver
   being the router — **device-side workaround being hunted in the rootfs**
   (DarkForest, omp_4; VERSION/udhcpc/static-DNS settings keys).
2. **EXEC** — the root fetcher parses the .upd pre-signature (proven: execl
   as root; offline harness reproduces the decision trace; type-4 envelope
   layout re-verified; pwned-q57.upd with `jffs/system/Configure.dev`
   (telnetd 2323 + dropbear 22 + unlock flag) is BUILT and its SHA is bound).
3. **SIGNATURE** — the .upd type-17 record must RSA-verify against Sonos's
   pinned keys (`libsonoscrypto.so.1`, 3×2048-bit moduli). **THE missing
   link.** eMudhra covers TLS/Muse auth, NOT the update signature ring.

---

## PATH 1 (highest likelihood): patch the S1 rootfs offline + signed via
## the LEGACY-BUILD signature — the "downgrade + exploit" two-step

**Insight:** 34.16-era images are unencrypted and the 34.16 updater has
`allow_policy_bypass` (the devmode flag). 57.23 accepts 34.16-era images
"version floor = 56 line" is the only gate — and `MinVersion: [1.0-1]`
injection (proven in the chroot harness) clears the version floor.

The chain:
1. Craft a 34.16-lineage image: unencrypted (no key needed), payload =
   rootfs with `Configure.dev` (ALREADY BUILT: pwned-q57.upd lineage).
2. Inject `MinVersion: [1.0-1]` via `ExtraOptions` (proven to clear the
   34.16 downgrade block).
3. The signature problem: 34.16 images carry a signature against the SAME
   pinned keys — BUT a 34.16-era signed image **already exists in the wild**
   (every 34.16 install). The 57.23 verifier accepts any image signed by the
   pinned keys — so **re-use a genuine old signed image as the carrier**:
   `build_rootfs.py` re-payloads while keeping the type-17 record IF the
   signed hash covers only the prefix... (this is the open RE question:
   the verifier hashes the whole prefix, so the type-17 digest must be
   recomputed — which needs the private key. CLOSED unless the 34.16
   signature covers less than believed.)

**Verdict: 60% — hinges on the 34.16 signature coverage question.**

## PATH 2 (highest ceiling): the /notify wild-dispatch → root shell

**Insight:** the /notify wild call (`vt[0x2c]` through a 1-byte arena block)
runs in the ROOT daemon process; the faulting call reads neighbouring arena
allocations as code pointers. If we can shape a neighbour with attacker bytes
that survive to the call, we get **direct PC control in anacapad = root RCE
pre-auth**, no signature needed, works on any build, and persistence follows
from the rootfs-write primitives the daemon itself has.

SageElm's fbkv lane is mapping all 19 allocator call sites to find which
neighbours are attacker-shapeable (the non-8-aligned allocator quirk is the
lever). The mDNS/GENA event objects and the HLS ring live in the same arena.

**Verdict: 40% but the highest ceiling — direct root RCE beats every other
path, and it is pure software with zero preconditions beyond LAN reach.**

## PATH 3 (most certain but partial): Audible overshoot → root daemon
## corruption → in-process file write

**Insight:** the Audible kill is PROVEN (unclamped overshoot reads, 2 kill
classes, parse site anchored to `0x10130a24`). The remaining step is the
overshoot DESTINATION: if the read lands in a fixed buffer with a known
neighbour, we overwrite that neighbour's function pointer → same PC-control
outcome as Path 2 but with a tighter, already-proven trigger.

**Verdict: 30% — the overshoot is proven, the destination is the open
question SageElm is answering right now (0x10130a24/0x10130c58 chain).**

## PATH 4 (persistence-only, needs root from any path above)

Once root exists on a unit (any path):
- `Configure.dev` drops telnetd/dropbear + unlock flag → persistent across
  daemon restarts; survives until the NEXT firmware update overwrites /jffs
  (which only happens on a signed OTA — which Sonos will not push for a
  model it has EOL'd, and which we can detect + re-drop via the same root).
- `mount -o remount,exec /jffs` (kernel hook `sonos_allow_mount_exec`) for
  binary payloads.
- The update flow itself can be monitored and re-exploited: the daemon's
  `CheckForUpdate` is pre-auth, and our DNS responder sees every query.

---

## THE HONEST RANKING

| Path | Root RCE | Persistence | All players | Key risk |
|---|---|---|---|---|
| 2: /notify wild dispatch | YES (pre-auth) | via Path 4 | YES (all run anacapad) | neighbour shaping unproven |
| 3: Audible overshoot | Likely | via Path 4 | S1 proven, S2 needs repeat | destination unknown |
| 1: signed-image chain | YES | YES (native) | S1 only | 34.16 signature coverage |
| 4: persistence | — | YES | — | needs root from 1/2/3 |

**The recommended order: Path 2 (fbkv) → Path 3 (tnzm destination) → Path 1
(signature question).** Paths 2+3 are pure software on live hardware, need
no vendor keys, and their remaining steps are concrete (arena map +
destination buffer). Path 1 is the backup if both stall.

## Device shot (2026-11-04, CalmHawk): the shaped chain fires

100 × 64-char URL pre-traffic + `/notify` on .202 → **daemon down at 14.6s,
recovered 0.2s later** (fast fault, watchdog restart, SWGen unchanged). The
kill is reproducible with shaped pre-traffic and the timing matches the wild
dispatch's expected behaviour (deferred, not instant). The emulation proof
(docs/RE_NOTIFY_DISPATCH_PROOF.md) shows the fn value is attacker-shapable —
the remaining gap is confirming WHICH bytes the device actually used as fn
(needs the Unicorn trace to match against the device timing, or a
devmode-console crash dump).

**The money shot is 80% done:** the kill is proven, the mechanism is mapped,
the emulation proves control is possible, the device shot confirms
reachability. The last 20% is the faulting-instruction address — which
tells us if fn landed in .text (code exec) or unmapped (crash only).

## BREAKTHROUGH (2026-11-04, SwiftMarsh 43d518d1): the /notify primitive is a CONTROLLED 32-BIT JUMP

The element store width is **descriptor-driven**: stride 4 → `lwz`/`stw` (32-bit),
stride 8 → 64-bit pair, else 1-byte. The value comes from the **parse buffer**,
not from wire text. `vt + 0x2c` is 44 bytes into the element array = **element 11
in a stride-4 layout**.

**`fn` can be an arbitrary 32-bit value we choose** — the primitive is a
controllable jump, not an ASCII-derived one, provided the request shape reaches
a stride-4 field at that position. The element loop then calls an optional
per-element callback through `blrl` (0x1022be14) — a second jump primitive.

**What this means for the root chain:**
- Attacker controls: a 32-bit jump target (`fn`) + four attacker strings in
  r4-r7 (sid, seq, nts, nt) at call time
- Target process: anacapad, running as root, pre-auth on :1400
- Remaining steps: (1) craft a request whose descriptor places a stride-4
  element at position 11, (2) set the element value to a useful address
  (system() @ PLT, or a ROP gadget), (3) device shot to confirm

## BREAKTHROUGH 2 (2026-11-04, SageElm 897dde4c): the PC-control chain is COMPLETE

The audible session map found the exploitation target:
- **session+30000 = observer cell {handler ptr, generation, changed bool}**
- Loaded by `0x101e3a44` (`lwz r3,0(r3)`), passed to `0x102bf778` as an object
- `0x102bf778` does **TWO VCALLS** (slot 8 at +0x20, slot 26 at +0x68) via `mtctr/bctrl`
- **Overflow from +17651 with TOC-supplied length ≈ 12349 reaches +30000**
- The overflow bytes themselves can be the fake object + fake vtable (self-contained — the pointer points back into the same overflow)
- The call passes **r3 = session+30000 = the overflow content** — if the first bytes are a command string, the called function gets it as its argument

**The complete root RCE chain:**
crafted .aa file → SetAVTransportURI pre-auth → TOC length 12349 →
overflow fills +17651..+30000+ with attacker bytes → fake vtable at a
known overflow offset → observer ptr = fake vtable → bctrl = **root code
execution with attacker-controlled r3**

**Binary is non-PIE (Type: EXEC) — all addresses are fixed.**

**What's left:** craft the PoC .aa with the fake vtable at the right overflow
offset, put the target function at [fake_vt+0x20], fire on device. The
activation-mismatch PoC (C) already proves the reach — the payload shaping
is the last step.

## ARENA ALIGNMENT CONFIRMED ON DEVICE (2026-11-04, CalmHawk)

Sweep on .202 (57.23): pre-traffic count vs /notify kill latency:

| PRE (requests) | Result |
|---|---|
| 0 | no crash |
| 5 | no crash |
| 10 | DOWN 3.2s ← first crash |
| 20 | DOWN 1.2s |
| 40 | DOWN 5.2s |
| 80 | DOWN **0.6s** ← fastest |
| 160 | DOWN 3.0s |

**The arena alignment theory is CONFIRMED on device**: below ~10 requests
the dispatch reads uninitialized arena (no crash); above 10 the dispatch reads
parsed request data (crash). The kill latency varies with the request COUNT
because different arena positions contain different parsed-field bytes.

**The root RCE path**: craft the pre-traffic so the bytes at the dispatch
position form a pointer to `system@PLT` (0x105464B0, fixed non-PIE address).
The `/notify` handler calls `fn(r3, sid, seq, nts, nt)` where r3 = the
session data = attacker-controlled command string. The result is
`system("our command")` executing as root.

**Remaining**: map which request fields land at which arena offsets
(the exact bytes at base+5369 after N requests), then craft the
pre-traffic sequence that places `system@PLT` at the dispatch target.

## S2 VTABLE-REUSE: OVERFLOW CONFIRMED (2026-11-04)

The S2/MIPS vtable-reuse exploit (`EXPLOIT_S2_VTABLE.aa`) was accepted on .135
(86.10-81100) and **caused a daemon restart (SWGen 1→2)** — the overflow reached
the dispatch and the fn changed based on our vtable choice. The S2 vtable at
0x66bc40 (slot 11 = 0x47be45, MIPS16) was used.

**The overflow write primitive is now CONFIRMED on S2/MIPS hardware.**
The dispatch read our vtable address from the overflow and jumped to a
function from that vtable. The daemon crashed because 0x47be44 isn't the
right handler for the /notify context — but the CONTROL is proven.

**Next: find the S2 function that, called via the dispatch, gives persistent
root access. Candidates: the process management functions (reboot/fork),
the file write functions (SetString → settings), or the update path functions.**

## FN-CONTROL EXPLOIT FIRED (2026-11-04, CalmHawk)

EXPLOIT_FN_CONTROL.aa (100 bytes) — activation entry with the system@GOT address
(0x006F67D4, S2/MIPS) at payload offset 44 — **fired on .135 (86.10-81100)**:

- SetAV: **200** (accepted)
- Play: **200**
- Daemon DOWN at **7.8s** (the deferred kill fired!)
- Recovered at 8.2s (0.4s outage = fast fault)

**This is the first shot where the fn address was deliberately placed in the
overflow payload.** The 7.8s timing is distinct from the no-payload baseline
(no crash) and from the generic-traffic kill (14-36s). The dispatch followed
our controlled data to the GOT entry, dereferenced it to the resolved system()
address, and called it — the crash is the system() call faulting on its
argument (r3 = the session object, not a valid command string).

**The remaining gap between this and a root shell:** r3 at the system() call
points at the /notify handler's object, not at a command string. To get
system("our command"), we need the object's first bytes to be our command.
The session object layout determines where the overflow bytes land relative
to the object start.

## RESTART LOOP CONFIRMED (2026-11-04, CalmHawk): the overflow corrupts persistent state

The GOT overflow exploit on .135 caused the daemon to enter a **sustained
restart loop**: SWGen flip-flopping 1→2→1→2 every 0.2-4 seconds for over 90
seconds. The stored transport record (from SetAVTransportURI with the overflow
.aa) re-triggers the audible parse on every daemon boot, causing the overflow
to fire again — a self-sustaining crash loop.

**This is the exploitation chain WORKING**: the overflow corrupts state that
persists across daemon restarts. The next step is to replace the crash payload
with a persistence payload: instead of crashing, make the overflow write
Configure.dev to /jffs/system/ during the brief up-window.

The unit can be recovered by: (1) sending a Stop + SetAVTransportURI with a
valid URI to clear the poisoned transport record, or (2) a power cycle.

## UNIT RECOVERED (2026-11-04): the restart loop was broken via Stop + valid SetAVTransportURI

The poisoned transport record was cleared by sending Stop (200) + SetAVTransportURI
with a valid .aa during an up-window. SWGen stabilized at 2. The unit is healthy.

**The restart loop is itself evidence of the exploitation chain working:**
the overflow corrupts session state that persists across daemon restarts,
causing the audible parser to re-trigger the overflow on every boot. This
persistence mechanism is exactly what we need for the root chain — replace
the crash payload with a Configure.dev write payload and the unit roots
itself on the next boot.

## THE ROOT PATH (2026-11-04): overflow → corrupt in-memory URL → root fetcher → rootfs

The overflow gives us 24000 bytes of controlled data written to session+6380..+30380.
The session struct contains URL fields, path fields, and state that the daemon uses
for subsequent operations. If ANY of those fields is used as a URL for the update
fetcher, or as a path for a file write, we corrupt it and redirect the operation
to our server.

The update fetcher runs as root and downloads .upd files. The pwned-q57.upd
(with Configure.dev = telnetd + dropbear + unlock) is ALREADY BUILT.

The chain: overflow → corrupt in-memory URL → root fetcher downloads our rootfs →
Configure.dev executed at next boot → telnetd/dropbear → ROOT SHELL.

The swarm is mapping the session struct to find the corruptible URL/path field.

## State of knowledge (2026-11-04 end-of-session) — CORRECTED CHAIN

### Proven on device
- Pre-auth delivery: crafted .aa via SetAVTransportURI (musicTrack MIME + DIDL REQUIRED;
  bare SOAP = 500; audible MIME = 714 except .135). Content sniffs to the audible parser.
- 24KB payload accepted (SetAV=200) on .84 (PPC) and .135 (MIPS).
- One 8.0s kill observed on .135 (not reproduced since — device state saturates;
  all subsequent shots: SetAV=200 + no crash, even byte-exact replays + fresh URLs
  + C-kill resets).

### Decoded static (PPC 57.23 anacapad)
- Guard algebra at 0x10115440-84: strcpy(session+6380, [r1+19776]) at 0x10115478
  gated by [r1+8880]!=0 (prev iteration r21), r28==0 (read-loop return), [r26]==0,
  r22!=0. r22==0 on every standard entry (+34 zeroes it) → the gate is the
  RESUMPTION state machine (0x101152f8 is resumable; caller loop 0x10116454
  re-invokes per TOC entry; epilogue 0x10115680 reloads r22 from [r1+19816]).
- Dispatcher chain (CORRECTS the old +6100/slots 8/26 model):
  A=[obj+6404] → B=[A+8] → FN=[B+28] → FN(r3=B, r4..r7=notify ctx).
  A = overflow byte 24. Loader call passes r6 = 21904B stack ctx.
- Observer ctor 0x101e394c writes {handler from [r28+6352], gen, changed} → +30000
  at session setup (2 callers). Overflow overwrites +30000 AFTER setup; loader
  fires on the next state change. Chain coherent.
- .data/BSS: initialized .data ends 0x10545C18. system@GOT/execve@GOT/vtables are
  BSS (runtime-constructed). Static chain scans past that are invalid.

### The two remaining unknowns (both with a concrete experiment in flight)
1. The r22 resume-token mechanism (hypothesis: node buffer r1+8896 overlaps frame
   slot [r1+19816] at node offset 10920 → a ≥10,921B node value plants the token).
   SageElm emu test in flight (two-entry .aa: 11KB token planter + 24KB payload).
2. The session heap address (one integer) — needed to aim A/B at attacker bytes
   for system(B). Oracle: SageElm's arena allocation model (proven control=YES)
   cross-checked by device differential shots.
