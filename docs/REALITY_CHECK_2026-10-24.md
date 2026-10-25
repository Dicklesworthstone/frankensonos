# Reality check — network-only root lane, 2026-10-24

Question of record: a **pre-auth, network-only root** on the owner's own S1 (57.23) and S2
(86.10) players; no vendor developer key, no manual downgrade flash, no hardware access.
Success = persistent root, or an unsigned/first-party-key-free install that survives a reboot.

Read with `docs/SCOPE.md`, `README.md` and `docs/ROOT_LANE_STATUS.md`. This is the honest check,
not a summary of activity.

## Verdict

**Not root yet.** The lane has moved from "unknown wall" to a small number of *measured* walls and
two primitives, but no state-changing exploit is proven end to end. Claims below are scoped to
what a run actually showed; the audit in the session log flags the ones that are inference.

What changed for the better:

1. **The install path is characterised up to the signature gate, and that gate is closed without the vendor key.**
   Proven by code and by device: the type-17 signature covers the 57-byte list2 region only, but
   that region stores `sha256(file[:type17])` and the updater memcmps the running prefix hash
   against it; any body edit needs both a digest rewrite and a fresh signature. Appended records
   after the type-17 are unsigned, and *every* unused-section variant (types 5/9/10, raw and
   enveloped) is parsed then rejected before pass 2 - the flasher never runs. The rebuilt rootfs
   pipeline is proven end to end on an untouched tree (full read with a zero-IV length-exact
   envelope); a patched tree gets to 5.11/5.61 MB and still aborts, so the last 0.5 MB is a
   squashfs tail difference and the signature remains the wall.
2. **A real memory primitive exists in the HLS queue; it is proven end to end in the guest and its live trigger is gated on the unlock.** An append after a split writes the whole 3152-byte record over the ring header (`parent+0xC508`), including the array pointer at `+0x20`; the pointer bytes are the record's `name[0x20..0x23]` and the name is percent-decoded before the record is built. Destination arithmetic is `array + ((name[4..7]+1) mod name[12..15])*0xC50`, all name-derived. Forced in the guest with cursor 16 and limit 0: the first append lands on the header, the header's array field becomes the attacker pointer, the next append writes at `pointer + index*0xC50`. The only live trigger is `GET /audio_tap?tap=...`, gated on `/tmp/device_unlocked_flag`; nothing else calls the advance and nothing else in the image creates that flag, so the devmode statement is the single precondition for the whole write.
3. **Every pre-signature and runtime parser surface tried is hardened** (see the negative ledger).
   That is a real narrowing: future effort has a short list.

What did *not* happen: no daemon code execution, no unsigned install, no devmode unlock.

## What is proven (evidence)

| fact | evidence |
|---|---|
| genuine URL installs work both ways (control) | 57.22-65130 accepted, 30 s box-down, returns 57.22; 57.23 restored; `docs/RE_UPDATER.md` control section |
| updater loop after a failed run, and its recovery | `docs/ROOT_LANE_STATUS.md` 23:58Z; 20 consecutive 200s after recovery |
| signature scope and prefix digest | `docs/RE_UPDATER.md` "two separate values"; digest reproduced on three builds (the packaged rebuild's own digest is stale - audit) |
| appended sections never reach pass 2 | 7-variant matrix, `docs/RE_UPDATER.md`, `scripts/append_upd_section.py` |
| rootfs package builder | `captures/firmware/57.23-74170-1-16/pwned-q57.upd`; digest-recomputed variant still aborts mid-body (1.5 MiB), so envelope correctness is open |
| HLS pointer offset and encoding | `docs/RE_HLS_EXPLOIT.md`; `name[0x20..0x23]` is the array pointer, percent-encoded bytes are decoded before the record, map probe (`@` survives, `/` dies) |
| HLS write reaches the ring header | slot 16 write after a split: header 0..15 = name bytes, playback continues |
| URI is at record+0 | `strlcpy(record+0, uri, 2049)` at 0x10191b14; a second copy lands at 0x801 and the lane doc records both |
| token fingerprint format | composer is compact JSON; hhid = `{owner:{luid},hhid,network_hash}`; luid/network_hash are the mhhid segments *by inference* (the TXT triple matched slot 12), `docs/RE_TOKEN.md` |
| token issuer needs a live object | nine rodata vtables, trampoline shape, honest stop point |
| devmode statement is device-bound | signature covers the whole clear statement incl. id (`docs/RE_DEVMODE.md`) |
| `/ssh/fingerprints` is a host-key reporter gated by unlock | `docs/RE_TOKEN.md` |
| `/status/ZPLocalSettingsFile/<name>` reads one `..` into `/jffs` | `docs/RE_WRITE_PRIMITIVES.md`; name length bounded by the request line (~4 KiB ok, 8 KiB reset) |

## Negative ledger (do not re-spend)

- Type-21 patch list: no size crash found; every mutation hit the 512 KiB semantic verdict (that is a verdict point, not a proven size bound).
- Type-1 version, type-2 compat: structural bounds found; only semantic verdicts move.
- Type-22 x509 (mbedtls 2.5.0, no PSS support): 2496-case offline sweep, zero crashes (commit eaa20e4, `scripts/mbedtls-harness/`; the ./third of on-device runs that lost the trigger are not in that denominator).
- GENA client responses: 18 malformed shapes, zero (single run each; no control matrix beyond liveness).
- Updater HTTP responses (busybox wget as root): complete frame matrix, two device-confirmed desyncs
  ('-1' and a short uppercase chunk close after ~8 KB/128 KB with rc 0), no memory fault; the argv
  is exactly known (`-qC /var/run/upgrade_report.txt -T 300 -O - <url>`).
- SMB client state machine on S2: 18 applied mutations, zero (apply-verified; warm-up case first).
- `/notify` LastChange: 12 shapes, 128 KiB ceiling, push does not drive state.
- `/status` name length: 4 KiB ok, 8 KiB reset (request line), no join overflow; `/mtmhhsetup`:
  403-gated while locked, so its parser is only reachable after an unlock.
- S2 `SetZoneAttributes` icon URL: stored, never fetched (no GET on any scheme) - a stored-URL surface, not a fetch surface.
- `SetAVTransportURI`/`AddURIToQueue` 20-shape matrix on .202 and on the S2 build: accepted shapes only, no parser fault.
- `/mfgunlock`: unstable on .86 (state-dependent), no usable transition observed.
- Token without a live object: 63+ claim permutations, all one-bit 401.
- `/getrs`: 2229-index scan (bit31 values, negatives, dense ranges, a 2000-value random sample),
  flat 200 and alive throughout - the table reads are benign.
- `AddOAuthAccountX`: 20-case ten-argument matrix, every case 500/402, liveness before and after.
- Rootfs envelope: the toolchain reached its best state: a zero-IV, length-exact, prefix-matched
  patched build now reads 5,111,808 of 5,606,749 bytes before the client closes (the untouched-tree
  control completes). The remaining ~0.5 MB is a squashfs tail/block difference to compare next
  session; the signature is still the wall even after that.

## Bead status (the 20 open at session start)

Advanced with evidence: `re-hls-kill-cnk`, `xnf`, `nyy`, `2ba`, `re-beginsw-fdp`, `8h3`,
`re-s2-devcopy-pfo`, `gko`, `re-uboot-cve-ols`, `ijn`, `re-notify-shapes-qgv`,
`re-long-outage-7hq`, `c25`, `fy9`, `spotify-more-kinds-94i`.
Still genuinely blocked: `q57` (needs an install path), `publish-0-1-0-isb` (deps),
`re-spotify-audio-w21` (scope: needs a streaming credential; evaluation already GO),
`c4v` (gated on a locked unit).
Closures are evidence-bound; three are thin and flagged by the audit: `nyy` (a crash that later proved to be an envelope-shape rejection, with no control row), `2ba` (a negative that omits its ~1/3 lost-trigger denominator in the close text), `xnf` is still open while the live endpoint returns 401. `re-s2-devcopy-pfo` is static-only.

## Gap analysis for the root goal

| goal | state | gap |
|---|---|---|
| pre-auth code exec on S1/S2 | NOT ACHIEVED | HLS gives a proven 0xc50-byte write to an attacker address, but its only live trigger (`/audio_tap`) needs the unlock; no second primitive |
| unsigned/durable install | BLOCKED | vendor key or a bug in the pre-signature parser (all tried are clean) |
| unlock / devmode | BLOCKED | device-bound statement needs the portal (owner login) |
| persistence path after root | READY | `Configure.dev` payload built; `/jffs` preserve table recorded in FIRMWARE_PATHS (0x1101) |
| SSH after unlock | MAPPED | statement starts telnetd/dropbear; `/ssh/fingerprints` is read-only |

## Lab capability gained

anacapad now **serves in the css guest** (11 threads, :1400/:1443 listening, GetZoneAttributes answers
200) with the device's own loader, the ABI-patched libflash and the /opt/s16 staging recipe committed
(d4fcd97). The staged build is MIPS, and HLS exists only in the PPC model-16 build (the 57.23-1-12
MIPS anacapad has zero HLS strings), so a PPC guest is the missing instrument for the next session.
The PPC guest now **serves**: the abort was a model switch with no default arm, and `GCModel` in the
conf suppresses it - `GetZoneAttributes` answers 200 with the zone name. With `/tmp/device_unlocked_flag`
present, `/audio_tap` answers 404 instead of 403, i.e. the unlock gate the HLS write needs is passed in
the lab. The remaining guest blocker is the model: it resolves to ZP100, whose service set has no
AVTransport, so nothing can be played; libflash is never dlopened (the MDP shim is moot). The service set is a count at `obj+0x140` and an
index array at `obj+0x770`; the 11-entry inventory that array feeds is complete and correct (AVTransport
is in it), but a breakpoint proves that lookup is **not on the request path** - the request dispatcher
itself is still unfound. Three dead items are now documented in the guest image: the advance path, the flag setter
`0x1005dab8` and the service table `0x1052cd18` - all present, all unreferenced. Three structural
edits (description file, forcing `obj+0x170`, adding the entry to the live table) left the 405
unchanged, so the live dispatcher is not driven by either table. **The most important tooling finding of the session:** the qemu gdbstub exposes only CPU#0, so every
breakpoint was following one thread while the daemon serves on another (it has 11). That retroactively
explains roughly a dozen "not hit" results - emitter, formatter, handler, strtok_r, recv, recvfrom - and
means the structural conclusions drawn from them must be re-checked once threading is solved, not
trusted as-is. Fixes: per-thread breakpoints on the serving thread, or a stub that enumerates threads. Worse, even
`read` - a syscall that certainly runs - traps once and then stops while the daemon keeps serving, so
single-hit anchors cannot support a walk. The guest tooling is the binding constraint now, not the
code. Single-threaded TCG (`-accel tcg,thread=single`) plus `set scheduler-locking on` finally let the
request path be walked once - socket read -> buffer handling -> URI normaliser - and it yielded the live
route structure: a 16-bit count at `obj+0x144`, 16-byte entries with the handler at `entry+0x10`, and the
dispatcher at `0x10414618`. The static tables were never the live data, which is why editing them did
nothing. The dispatcher's own breakpoint then missed - and so did the same breakpoint on a *registered* route
(`/status/VERSION`), which proves the address is the problem, not the route. The live structure is
real (read from the walk) but **empty, and empty all the way up**: the dispatcher's count is 0, the
array it copies from (`obj+0x2C`) has count 0 at `obj+0x40` too, and the copy at `0x10242b0c` correctly
copies nothing. A runtime write of the handler and count into the singleton still answered 405, so the converged root
cause is one level higher: **the request never reaches the dispatcher at all** - it is diverted to the
static-file handler, which answers 405. The empty singleton and the dead population path are
downstream of that diversion. The next session starts at the diversion, reading the frames between
`0x10406944` and `0x104143ec` for the branch that skips the dispatcher.

**Root cause found:** the dispatcher is reached, but a gate global `0x10545fac` is `0` and its only
setter (`0x10413f0c`) is dead - a fifth dead item. Setting the gate alone still answers 405; gate and
populated entries are both required, so the static fix is a gate store plus a pre-built entry. The
no-route path (`0x10411a14` -> `0x10418788`) is an allocator, so with an empty table the dispatcher
always falls through and the **static layer** rejects the POST - that is the 405. The route table is
never populated because its population path is dead. The request path is now mapped end to end except
one step: read -> line loop -> whitespace skip -> **classification** -> dispatcher. **The 405 mechanism is now fully explained:** `obj+0x200` is a *per-request* route container whose
entries each carry a pattern list; an entry matches only when a pattern string-equals the path, and no
match falls through to the static layer. The persistent `obj+0x140` table was a red herring. The fix is
precise: populate `obj+0x200` with an entry whose `+0x30` points at a pattern node holding
`/MediaRenderer/AVTransport/Control` - no model change needed. A runtime install at the dispatcher was
overwritten, and the consumer (`0x104171a8`) is cold too, so the request is classified as static
*before* any container is walked; the install point must be after initialisation, not at either
breakpoint.
**Correction from a static read of the loop:** `0x104145e0` loads the count
and, when it is zero, branches past the loop body - so `0x10414618` (the loop's call return address) is
cold *by construction* if the array is empty, which is not a stub failure. Hardware breakpoints
(`hbreak`) are cold after the first hit too, so the stub still has a real one-trap limit, but some of
the earlier "cold" results may be the empty array rather than the stub. `finish` from
`read` is the only technique that still works, which makes each remaining frame a full boot - and
`finish` itself stops after six frames (`0x10411c6c` is the last reachable one, the same blocking
behaviour as the dispatcher). The 405 writer sits above a blocking frame, so it needs a technique
beyond `finish`. Bypassing the located method gate still returns 405, so the emitter at `0x10410630`
is off-path too and a second, unlocated emitter answers - every static address found so far is
off-path for this request. The guest's HTTP stack has at least two parallel implementations and the
one answering `/audio_tap` is the one never found; the next session should break on the listening
accept loop and follow one connection end to end from `accept` - except that `accept` is **cold too**
(`31bc513`). A server that answers must accept, so the stub, not the addresses, is the limit: it is
missing executions outright. Replacing the debugger is the prerequisite for any further progress. The
recommendation is to stop investing in this stub: rebuild qemu with a working gdbstub, or use a
different emulator for the guest - a working debugger would finish the route question in a handful of
runs. Corrections that got it there - `pkill -x`, tmux not
setsid, the absolute `/opt` layout, a `libflash` shim with SYSV hash and soname, and a `/dev/chk`
patch - are in `docs/FIRMWARE_PATHS.md`. The abort is *not* the DCMS/regcert gate (those strings are
absent from this binary); the monitor socket samples NIP and is the way to name it.

## Gate datum (publish readiness)

`cargo clippy --workspace --all-targets -- -D warnings`: PASS.
`cargo test --workspace`: **green** - 213 `test result: ok` lines, 0 `FAILED`, after two flakes were
fixed rather than retried: the live bootstrap sims now serialize and wait 120 s (they starve under a
loaded workspace run), and the Serve probe's one-second deadline became five (its fast case spawns a
shell). The workspace is green as a whole.

The release pipeline is proven end to end: `dsr build frankensonos --target darwin/arm64` succeeded
twice (1/1, 682 s and 696 s, manifest `publishable`), the tarball extracts, the binary answers
`--version`/`--help`, and a simulator smoke passed (`fsonos sim` then `zones` listed both S1 players
and `volume Office 22` read back 22). dsr builds the **tag**, not HEAD: both artifacts are from
`9d3e1df` (`v0.1.0`), while `main` is `fec9149`. Publishing HEAD therefore needs a new tag after
authorization, then one more build.

The build host refused at first: dsr errors above 95% disk. Freeing 32 GiB of regenerable scratch
(`/private/tmp/claude-501/-Users-jemanuel-projects`, `...-frankenterm`, and the repo `target/`) took
the volume from 96% to 95% and the build went through. Note `/Volumes/USB_NVME` is not mounted, so
`CARGO_TARGET_DIR` falls back to the boot volume.

## Next actions, in priority order

1. HLS: the primitive is done; the trigger is `GET /audio_tap?tap=<name>` and it is gated on
   `/tmp/device_unlocked_flag`. So the next step is the unlock, not more HLS: a devmode statement
   (portal login) or any way to create that flag. Once unlocked, the `.plt` payload already built
   fires on the first tap during playback.
2. Owner action: load the browser relay extension and sign in once to the devmode portal; a
   per-serial statement immediately yields telnetd + dropbear (S1) or the SSH reporter (S2).
3. A second memory bug: the S2 registration/devmode handler under a *valid* statement, and the
   `AddOAuthAccountX` ten-argument parser, are the two remaining undeep-fuzzed runtime parsers.
4. Do not retry: signature/prefix splicing, appended-section installs, x509 PSS CVEs.

## Denominator note

Timeboxed session (~6 h). Two live units (`.202` S1, `.84` S1) plus `.81/.86` S2; two units were
lost to network/router drops during the window and recovered. Every device run in the tables has
its control row; runs without a control are marked as such. The one harness bug found mid-session
(`TS_FILE` vs `TSFILE`) invalidated the early "real TS" rows and they were re-run, not reinterpreted.

## Session close

- Head at close: the tip of `main` including this file's commit, pushed without force.
- Units left healthy and idle: `.202` 57.23, `.84` 57.23, `.81`/`.86` 86.10; all four answer
  `/status`. `.202` was wedged by the final update run and recovered on its own before close.
- Open beads: 4 in progress (`xnf`, `re-hls-kill-cnk`, `c25`, `spotify-more-kinds-94i`), 4 blocked
  (`q57`, `publish-0-1-0-isb`, `re-uboot-cve-ols`, `re-spotify-audio-w21`); 138 closed.
- HLS ended at: pointer offset and percent-decoding settled on device, the slot-16 write and its
  header corruption measured, the destination's cursor proven to be helper-derived; the `.plt`
  stub payload is built and fetched but does not fire. Not claimed: code execution.
- Not claimed: any code execution, any unsigned install, any unlock.
