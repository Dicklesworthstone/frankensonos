# WIZARD_SCORES_GLM_ON_DEEPSEEK — adversarial scoring of the DeepSeek plan

Scorer: GLM (author of `WIZARD_IDEAS_GLM.md`). Every load-bearing factual claim
was re-verified against the repo, the staged artifacts, and the live devices
before scoring; verification results are cited inline. Scale 0–1000.

## The headline: DeepSeek out-measured me, and it matters

DeepSeek's session probes (plaintext trailing type-3, the live `/devmode`
200s, the staged 86.11 package, the empty `upgradeinfo`, the S2 0x6DCEA0
correction) are all REAL — I re-verified every one:

| DeepSeek claim | Verified how | Verdict |
|---|---|---|
| trailing type-3 in `pwned-65130.upd` is PLAINTEXT | walked records; last type-3 payload magic = `23212f62` = `#!/b` | TRUE |
| `pwned-enc.upd` (encrypted, 458 B envelope) already built + verify PASS | ran `verify_upd.py` → `type-17 signature: ok (key #2), verdict: PASS`; decrypted the envelope with `model16-private.pem` → beacon+telnetd+`touch /jffs/system/Configure.dev` | TRUE |
| recipient cloned from the genuine record | genuine 57.23-74170-1-16 type-3 recipient = `fd88f2642a9c89a44747c7b4…`; pwned-enc = same | TRUE |
| 86.11-82170-1-12.upd staged + pwned-135 variant | both in `/tmp/q57-serve/`; record walk = 21/2/2/22/23/3/6/4/17/3; trailing type-3 = real `886499ca` envelope, recipient `e35f7c21…` = genuine model-12 id; prefix SHA256-identical to the pristine base | TRUE |
| `/status/upgradeinfo` empty on all 3 units | live curl just now: empty envelope ×3 | TRUE |
| `/devmode` 200 + csrfToken on .84/.202/.135 | live curl just now: 200 ×3 with fresh tokens | TRUE |
| S2 `system@plt` = 0x6DCEA0, 0x6DCE50 = `kill@` | `full16.asm`: `0x4ab64c: jal 6dcea0 <system@mips16plt>`; `006dce50 <kill@mips16plt>` | TRUE |
| `run_sshd.sh` = `exec /usr/sbin/telnetd` (no args) | read the file in the extracted 57.23 rootfs | TRUE |
| model16-private.pem is a real recovered key | it decrypts the GENUINE 57.23-74170-1-16 type-3 (region-setting script) | TRUE |
| no newer S1 build (57.23-80060-1-16 → 404) | live CDN check | TRUE |
| 86.11 CDN URL live | HEAD → 200, 7493709 B, CloudFront | TRUE |
| its idea-2 sed already applied | `/tmp/s2decode/sweepshell.py` line 64: `rebuild(hit,0x6DCEA0, b"wget -qO- …/S2SYS135;/usr/sbin/telnetd …")` | TRUE — the fire is staged, not just described |

This is the strongest duelist output I have seen on this campaign: it did
device work during ideation. Scores reflect that.

---

## Idea-by-idea scores

### 1. S1: FN = system@plt (0x105464B0), r3=B, `;`-prefix — **890**

The one-fire shell. DeepSeek's version improves mine in three concrete ways:
(a) it puts the string at `node200+4` with the `;` prefix — same as mine, but
it also flags `(S+600) NUL/meta-free` as an S-selection constraint, which I
waved at; (b) it warns that `0x105464b0` reads as zeros on disk (secure-plt
v1 slot filled by the loader) — a trap I did not flag and that would have
wasted a debug cycle; (c) its branch table names the follow-up gadget
(`addi r3,r3,4`) for each failure mode. Deductions: the `57907536` header
reconstruction and the `b[23624]=1; b[23628]=1` observer fields are
hand-rolled — the proven builders (`aimed_builder.py`/`ppc_v5_builder.py`)
should generate this payload, and any hand-encoding error is
indistinguishable from a real negative. Also "telnetd double-forks so
system() returns immediately" is asserted, not proven for busybox telnetd on
57.23 (busybox telnetd does fork per-connection, but the daemonizing claim
should be verified with `strings`/`--help` in the rootfs before trusting the
no-watchdog-reboot property). Still: highest value-per-fire in either plan.

### 2. S2: FN = system@plt 0x6DCEA0, a0=node0 — **920**

Better than its S1 twin because the discriminator is stronger: the wget
beacon converts "did our string execute" into an HTTP log line, which is the
exact evidence the S2 lane has lacked since the killall correction (my plan
proposed the same A/B but bolted it on as a second step; DeepSeek's single
payload answers arg-control AND shell in one fire). The corrected stub
address is verified, and — decisively — the patched runner is already staged
in `/tmp/s2decode/sweepshell.py` with a saved baseline. Risk I flagged that
it under-weights: a0-at-entry for the S2 dispatcher has never been live-read
(after the 0x4ab64c lesson, "a0=node0" rests on the matrix-era inference);
if entry-a0 is something else, the beacon dies silently and we learn only
"no" — the 0.2 s-poll cadence and a canary at the same S are mandatory to
distinguish "wrong a0" from "dead string". Even so: cheapest possible path to
the S2 shell, fully staged. This is the best idea in either plan.

### 3. .135: genuine 86.11 upgrade + attached preinstall — **810**

The discovery of the session: a genuine, signed, NEWER S2 build exists and
the B1 downgrade verdict structurally cannot apply. The augmented package is
built correctly (I verified the envelope, the recipient clone, and the
byte-identical prefix myself) and the keyring caveat on `verify_upd.py`
MISMATCH is honestly disclosed and correctly read (the pristine base
mismatches identically). Two real risks it prices in but under-solves:
(a) this is the FIRST live .135 firmware change of the campaign — the
`~90 s` outage claim is optimistic (S1 install cycles observed 60–100 s just
for the BSU wedge; a full flash+reboot is longer) and the Living Room unit
serves the household; the owner should get a heads-up line before the fire.
(b) The S2 flasher honoring a trailing type-3 is still the open question this
experiment answers — good — but if the S2 verifier enforces section order
strictly (type-22-before-type-3 gating is documented on the S1 line and the
S2 86.10 order differs), the trailing record may be ignored, which is
information but not a shell. Run it after idea 2 fails, as it says.

### 4. Devmode statement per serial → /unlock — **760**

Verified live: all three units answer `GET /devmode` 200 with fresh
csrfTokens, `scripts/devmode_submit.py` exists with exactly the described
mechanics (offline validation against a Unicorn emulation of the real
ZZ0137, 3 s limiter, post-200 unlock probes), and `docs/DEVMODE_FORMS.md`
maps all 21 ring checks with a minimal passing ring. This is a
zero-exploit shell if the owner spends 5 minutes in the developer portal —
the only idea in either plan whose remaining blocker is a human form fill,
not a binary. Why not higher: the campaign has had this tooling since Oct 26
and the statement is still missing, which suggests the owner-action
dependency is stickier than "5 min"; also `dad973e7` shows the last real
blob was rejected at the ring stage — the emulator passes but the portal
statement format must match exactly what the portal emits, and that has
never been tested end-to-end with a real portal blob. Still: on both my
list and DeepSeek's this deserves to run the moment the owner is available.

### 5. Encrypt the trailing type-3 (stop shipping plaintext) — **880**

The single most important correction to MY plan. I read "plaintext type-3
never executes" in `UPDATE_FEED_AND_SIGNATURE.md` (lines 442-443) and still
proposed "clone the encrypted format" as future work without checking what
was actually in the staged packages — the staged pwned-65130.upd carries a
PLAINTEXT script (`23 21 2f 62` = `#!/b` at the record payload start), which
means every exec verdict ever drawn from those packages is void. DeepSeek
caught it, and the fix is already built, verified (PASS), and content-
checked (I decrypted it: beacon + telnetd + Configure.dev touch). Deduction:
the idea as written is a *fix*, not a *path* — it still needs a delivery
fire to matter, and its own caveat is right that if the encrypted trailing
record still 48s at the reader, position (after type-17) is the problem, not
the envelope. But as a prerequisite it converts ideas 3 and my 5 from
probably-dead to testable. Where were these bytes checked three sessions
ago?

### 6. Make B1 visible: poll `/status/tmp/upgrade.log` across a forced restart — **700**

The reasoning is right and the mechanism is documented
(`FIRMWARE_PATHS.md:1947`: "the spawned updater's stdout is reachable there
until it rotates") — but I deducted hard for two things. First, the log is
registered only while an update runs, and the documented BSU behavior
(`RE_UPDATER.md:733`) is a 60–100 s daemon wedge with the updater possibly
never spawning a logger at all on this path — the 240 s poll window may
capture nothing, and the script's own 0.2 s loop hammering three endpoints
through the wedge adds load precisely when the daemon is dying. Second, the
`SetString` kill it leans on is documented as a *daemon* death (~25 s delay,
~80 s recovery) that "does not walk the boot slot" — whether the updater
re-spawns with a fresh log on a daemon restart (vs a full boot) is exactly
the unproven link. The idea is correct that this read converts the verdict
mystery into a literal string; its expected-observable section even names
the codes. It just has a real chance of a null result, and my idea 6 (the
30-second `/status/upgradeinfo` read, which I verified live and which
DeepSeek also lists as measured-empty) already answered half of what this
idea chases: the MinVersion line is NOT there now — the injection is dead or
was consumed.

### 7. Decode the two upgradeinfo writers (FUN_1029c34c + BSU handler) — **720**

Sound, cheap, and the right offline complement: the decode directly answers
"can the LF-injected MinVersion line ever survive the templater" and "which
Flags bit routes the deferred path" — the exact questions sessions 8–11
punted on. The Ghidra kit (`DecFn.java`, ppcproj with both binaries) exists;
the chroot replay harness can then test each candidate file shape. Why not
higher: it is investigation, not a shell, and its actionable output
(survived=yes/no per key) only *routes* to ideas 3/8 — if the templater
rewrites unconditionally, the idea's own conclusion is "native arming or
bust," which idea 8 already assumes. Well-scoped ballast for a css session.

### 8. Native arming via pf + ARP gateway hijack — **640**

Substantively the same as my idea 3 (ARP + pf rdr + the existing eMudhra
responder + `settings_forge.py`), so agreement here is convergence, not
novelty. DeepSeek's version is better on one point (the read-only first step
— `tcpdump` the unit's actual resolver behavior before touching anything —
is exactly the discipline the q57 no-DNS findings demand, since `.202` was
measured to never query our responder) and worse on another (it re-derives
the whole interception stack when the LaunchDaemon-responder half is already
persisted and only the ARP/pf half is new). Hard deduction for operational
risk understatement: ARP-spoofing the owner's LAN gateway from a Mac that
other agents are using, against a unit (`%84`) that fetched packages four
times tonight, can black-hole the whole unit's traffic if the spoofer dies
mid-run — the kill switch needs to be a supervised loop, not a background
`&`. And the settings job may fire only on the deferred-shutdown path
(the kr9e open question), which neither version of this idea can force from
outside. Strategic reserve, correctly ranked below the one-fire ideas.

### 9. S2: FN = vfork/execve wrapper 0x656978 with argv at node0 — **650**

The decode is verified-adjacent (the wrapper's `access/vfork/dup2×3/
close 3..255/execve` shape matches the session-3 note; a0=argv array from
the caller struct, a1 = out-struct). Building the argv array in the node
buffer is clever and drops the 107-byte budget — my plan's idea 9 flagged
the same wrapper but as "register control needed"; DeepSeek's insight that
**a0 IS ours** (it points into our buffer) makes the argv array buildable.
Two real problems: the wrapper dups PIPES over fd 0/1/2 from `a1` — a1 is
not ours, and a garbage a1 means `dup2(garbage_fd,…)` fails or the parent
`poll()`s a garbage fd set; and `sonos_auth_add_ambient("all")` in the
wrapper may touch session state we have not groomed. Its own honesty about
"restart plus listener = pass" is the right test criterion. Rank it after
idea 2's system fire: same evidence channel, strictly more unknowns.

### 10. Persistent self-reporting loop (run_sshd.sh + fetched-script retry) — **620**

Right instinct, wrong rank. As a *first-line* idea it burns the campaign's
cleanest unknown (r3=B live) on a payload whose success criterion (a
15 s-repeat beacon) proves liveness but not arg control — if r3 is not B,
the loop never starts and we have learned only what the reboot gadget
already told us. As a *follow-up* after idea 1's branch table, it is
excellent: the `run_sshd.sh` variant doubles as a persistence-path test
(the exact program the Configure.dev boot hook runs), and the repeat-beacon
discriminator cleanly separates "no exec" from "child killed later" — the
last unknown in B2. The `setsid`/subshell-reaping caveat is real and
correctly flagged. Should be idea 1's explicitly-paired second fire, not a
standalone lane.

---

## Where DeepSeek corrected or contradicted me — the honest ledger

1. **My idea 5 (upgrade path) is half-dead on S1 and I did not check.**
   DeepSeek measured the feed: no newer S1 build exists (80060 has no
   model-16 image; 404 verified). My "if the feed still serves 57.23, this
   dies — but the check is 10 minutes" hedge was exactly right and exactly
   the homework I skipped. Its S2 86.11 discovery is the productive
   mirror-image I missed entirely.
2. **The plaintext type-3 in the staged packages.** I proposed "clone the
   encrypted envelope" without checking the staged bytes; DeepSeek found the
   staged packages carry plaintext, built the encrypted fix, verified PASS,
   and staged it. My idea 5's malleability framing survives only because
   their fix exists.
3. **The `/devmode` lane (my plan omitted it entirely).** Verified live: all
   three units are sitting at 200 with fresh tokens, tooling exists, ring
   checks mapped. A zero-exploit shell path that I ranked nowhere. Genuine
   blind spot.
4. **Its S2 idea-2 correction is superior to my idea 2's framing.** I
   hedged with "or a mid-function jump"; DeepSeek just fired the stub
   address and staged the runner. Cleaner.
5. **Where I hold ground:** my idea 6 (the 30-second upgradeinfo read as the
   campaign-rerouter) is subsumed by its probes but was framed as a
   decision gate, not a data point; my idea 7 (fetch-filename source decode,
   0x102b8e4c→0x102b8eb8) is absent from DeepSeek's list and remains the
   only named-write route that composes with the PROVEN fetch; my idea 8
   (qemu-ppc r3/r4 dump) is absent from DeepSeek's list and is the only way
   to de-risk ideas 1/10 without device fires. Its plan is device-heavy;
   mine had the offline rig. Both lists needed both halves.

## Score summary

| # | Idea | Score | One-line verdict |
|---|---|---|---|
| 2 | S2 system@plt fire (staged) | 920 | Cheapest S2 shell; run first |
| 1 | S1 system@plt `;`-prefix | 890 | One fire from the S1 shell |
| 5 | Encrypted type-3 (already built) | 880 | Invalidates my plan's packaging; mandatory fix |
| 3 | Genuine 86.11 upgrade + preinstall | 810 | Real signed upgrade; owner heads-up first |
| 4 | Devmode statement → /unlock | 760 | Zero-exploit shell; blocked on owner's 5 min |
| 7 | Decode the two upgradeinfo writers | 720 | Cheap offline ballast; routes 3/8 |
| 6 | upgrade.log poll across restart | 700 | Right oracle, real null-result risk |
| 9 | S2 execve wrapper argv array | 650 | Clever; a1/pipe unknowns; after idea 2 |
| 8 | pf + ARP native arming | 640 | Same as my 3; operational risk underpriced |
| 10 | Persistent beacon loop | 620 | Pair with idea 1 as fire #2, not standalone |

**Consensus zone (both lists, high rank):** the system@plt fires (my 1/2 ≡
its 1/2 — full convergence on the one-fire shell), the per-unit
interception arming (my 3 ≡ its 8), the encrypted type-3 fix (its 5, my
omission), and read-the-oracle-first (my 6 ≡ its 6's upgradeinfo half).

**Its unique high-value additions:** the 86.11 upgrade lane, the /devmode
lane, the plaintext-package discovery, and the already-staged fires.

**My unique items it should adopt:** the fetch-filename decode (my 7) and
the qemu-ppc register-dump rig (my 8) — both are the offline de-risking
half its plan lacks.
