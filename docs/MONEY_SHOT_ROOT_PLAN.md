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
