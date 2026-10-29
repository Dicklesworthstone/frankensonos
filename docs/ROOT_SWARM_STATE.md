# Root swarm state — CalmHawk orchestrator (2026-10-28)

Living checkpoint for the 6-pane root-exploit swarm (`frankensonos--rootswarm`).
Read `docs/RE_LESSONS.md` first; this file tracks *who is on what* and *what the
swarm has proven*, not protocol facts.

## Swarm map

| Pane | Worktree | Agent (mail) | Model | Bead | Lane |
|---|---|---|---|---|---|
| %196 | omp_1 | GoldenGull | deepseek-v4.1@xhigh | frankensonos-s20 (closed) | busybox wget chunked grammar — falsifier satisfied; pivot = D2 persistence probe |
| %197 | omp_2 | DarkJaguar | deepseek-v4.1@xhigh | frankensonos-jc7 | :1443 JSON parser pre-auth bounds (S2 86.10 MIPS16) |
| %198 | omp_3 | TurquoiseForest | deepseek-v4.1@xhigh | frankensonos-v8z→s20 support | /notify /setstring grammar + chunk corpus support |
| %199 | omp_4 | AzureCardinal | glm-5.3@xhigh | frankensonos-wbz | mbedtls x509 — pivoted to live :1443 TLS server cert path (type-22 post-signature is dead) |
| %200 | omp_5 | RusticIbis | glm-5.3@xhigh | frankensonos-81v | libFLAC 1.3.4 decode-path diff + trigger crafting |
| %201 | omp_6 | BrightDeer | glm-5.3@xhigh | frankensonos-d7vi | libVorbis 1.3.7 decode-path diff + trigger crafting |

## Orchestrator doctrine (every pane saw this)

- Claim atomically (`br update <ID> --assignee <name>`), reserve files via Agent
  Mail, code-first, commit locally, never push, never self-close.
- Offline/RE/fuzz first; device time coordinated via Agent Mail to CalmHawk —
  one pane on hardware at a time, bench units only, per RE_LESSONS probe
  discipline.
- Orchestrator closes beads with cited evidence only.

## Proven by the swarm so far (evidence-cited)

- **frankensonos-s20 CLOSED 2026-10-28** (omp_1 GoldenGull + omp_3 TurquoiseForest
  + omp_4 support): 2139-case chunked-grammar corpus clean on busybox 1.24.2
  (qemu-ppc) AND 2139 on 1.14.1 (qemu-mips; 0 SEGV/ABORT each). Two findings,
  both fixed upstream in busybox 1.29.0: (1) endless-`100 Continue` held-wedge of
  the root fetcher — pre-auth one-request, self-recovers ~90 s after socket
  close, RE_CVES §16; (2) 1-byte heap underflow (URL chunk size field MSB,
  PREV_INUSE cleared) → glibc `free()` abort **only when `http_proxy` is set**
  (device sets none — latent). No code-exec primitive; falsifier branch (b)
  satisfied. Commits b608d28b, 5f12f4e7, e1323b67, merged to main.
- **frankensonos-wbz CLOSED 2026-10-28** (omp_4 AzureCardinal): pivot to live
  :1443 TLS server accepted and executed — libmbedtls.so.14 pinned symbol-identical
  to stock 2.28.10 (251/251 exports), ASan 2.28.10 harness (verified-real, 2825
  __asan syms), 3109+ cases across record layer/ClientHello/extensions/kx/tickets:
  **0 ASan findings, 0 server exits**; seven worst shapes replayed live on .202,
  daemon healthy. `docs/RE_MBEDTLS_TLS_FUZZ.md` (19bc430e, merged). S2 3.6.6:
  corpus reusable, new harness needed (device libs absent locally).
- **frankensonos-jc7 IN_PROGRESS** (omp_2 DarkJaguar): 0x2500/0x2800 copy thesis
  retired at instruction level (real copies 37/40 B, both fixed; 40-byte source is
  a virtual on `RMuseAuth`, not parsed JSON). Gate resolved exactly: **`state ==
  52`** (no explicit-target binding present); frame prefix is ASCII-hex
  `VVTTLLLL` (version ≥ 7, type < 7, `%04x` ext length) read from the post-upgrade
  stream — HTTP-body attempts 400 because the reader never sees them. Next:
  lechmere.N upgrade + binary frames (orchestrator synthesis committed 733030c8).
  Commits fe761bac, 26d83fd1.
- **frankensonos-81v / -d7vi IN_PROGRESS** (omp_5 RusticIbis, omp_6 BrightDeer):
  ASan libFLAC 1.3.4 / libVorbis 1.3.7 decode harnesses building; both lanes at
  real-decoder stage with seed corpora.

## CVE sweep (2026-10-28)

No new Sonos-vendor CVEs in the last 30 days. CVE-2026-4149 (Era 300 SMB
DataOffset OOB, CVSS 10.0, kernel context, fixed 83.1-61240) is below both our
builds — reproduction target only. Details in `docs/RE_CVES.md` §8.

## Update 2026-10-28 ~23:20Z

- **frankensonos-13yv CLOSED** (omp_4 AzureCardinal): S2 3.6.6 ASan server
  handshake — 3109+ cases, 0 findings; .86 device verdict 8/8 clean within an
  approved window. Both falsifier arms satisfied. Commit 27e3dd9a, merged.
- **frankensonos-81v offline verdict delivered** (omp_5 RusticIbis): 1.3.4→1.5.0
  decode-path delta table (every fix attributed), 9 targeted triggers, 1300-case
  sanitizer sweep — **no reachable memory-write bug**; two device-facing residues:
  gap-fill CPU amplification (live in 1.3.4, oss-fuzz 47193 shape) and the
  NULL-vc-entry consumer deref. Device verdicts approved on .84. Commit 055090e6,
  merged.
- **jc7 advanced to the route layer** (omp_2 DarkJaguar): the 400 is synchronous
  and pre-upgrade; `/api/v7` is the only versioned route (api-version table maps
  1→v1, 2→v2 — lechmere.N does not select version); the handler object carries
  **147 children** (the namespace/command tree) — the missing second segment is
  the next target. RAuthSplitToken audited clean. Commit 9c67482d.
- New assignments: AzureCardinal → frankensonos-hd7l (MPEG-TS demuxer/ID3),
  GoldenGull → frankensonos-v8z (setstring/notify grammar), TurquoiseForest →
  frankensonos-1w3 (span-2139 PC-control chain) on top of s20 support.

## Session metrics (2026-10-28, for reality-check)

- Commits this session (main): 17 (2 orchestrator state docs + 15 lane merges/dispatches)
- Beads closed with evidence: s20 (P0), wbz (P1), 13yv (P1), 81v (P2) — every closure
  carries falsifier-branch language and named artifacts
- Beads in progress: 17 total in tracker; live pane lanes = jc7 (omp_2), 1w3+dxe
  support (omp_3), hd7l (omp_4), qa2 (omp_5), d7vi→dxe (omp_6), v8z (omp_1)
- Device windows granted: 3 (.86 TLS ×1, .84 FLAC ×1, .84 vorbis pending), each
  time-boxed, one pane at a time, all with pre/post health checks — zero stuck units
- Real findings so far: 1 memory-safety bug (vorbis alloca stack-overflow, offline
  proven, upstream-fixed shape), 2 DoS primitives (endless-100 held-wedge; D2
  persistence), 1 availability note (16-continue), multiple negative results with
  named scopes (TLS ×2 builds, FLAC, wget grammar, RAuthSplitToken)

## Update 2026-10-28 ~23:55Z — jc7 gate RESOLVED

**The :1443 JSON API is pre-auth reachable.** omp_2 mechanically dumped the
147-entry child table at 0x66a7b4 and wire-verified on .86: `X-Sonos-Api-Key`
is a **presence check** (any value passes) on `/api/v1/players/{UDN}/...`;
`/info` (1765 B discoveryInfo), `/settings/player`, `/time/relative` all
answer 200 pre-auth. Bearer is a separate later gate on some routes only.
Commits bb08bbf6 + 55e23269. The u16-length descriptor fuzzing (jc7's actual
deliverable) is now unblocked on a live pre-auth surface. Open follow-up:
whether the UPnP passthrough (`upnpDeviceProperties/call`) is reachable
pre-auth through the JSON parser — that would tunnel the whole :1400 SOAP
surface through the JSON binder.

## Update 2026-10-29 ~00:10Z

- **frankensonos-d7vi CLOSED** (omp_6 BrightDeer): T1 alloca stack-overflow
  (`vorbis_book_init_decode`, upstream 28965ede) offline-proven — the codec
  lanes' one real memory-safety finding; device verdict honest-negative (the
  hls-radio vehicle never hands .ogg payloads to the vorbis decoder on 57.23 —
  4 shots incl. valid control, SWGen unchanged). Commits 87ea1683 + 857f8479.
- BrightDeer → frankensonos-dxe (P0, HLS slot-16 ring fault recipe, .84).
- Closed-bead count this session: **5** (s20, wbz, 13yv, 81v, d7vi).

## Update 2026-10-29 ~00:45Z

- **frankensonos-1w3 verdict delivered** (omp_3 TurquoiseForest, commit 8880668a):
  the 1025-byte `strlcpy` cap binds at `101ee140` — **both URL-span routes are
  dead on 57.23** (span-2139 was a no-op because the truncated URI loses its
  last `/`, span collapses to 0). The one live length source left on the PC-control
  chain is the saved-queue-record path (`savedqueues.rsq` via CreateObject title).
- **frankensonos-qa2 advanced** (omp_5 RusticIbis, commit e74113ce): custom AAC
  parser fully mapped (ADTS demux `fcn.10212778`, ID3/private `fcn.10212454`),
  buffer guard assessed sound, 13-shape corpus + device verdicts 13/13 clean.
  Decode-side entry points are the remaining scope.
- Two provider-quota incidents (omp_1, omp_4, DeepSeek) — both recovered after
  retry nudge; zero lane work lost.
- Pushes: all session work landed on origin/main (71b03009).

## Update 2026-10-29 ~01:05Z

- **frankensonos-hd7l CLOSED** (omp_4 AzureCardinal, commit 33ef4b9d): 3025-case
  FFmpeg 5.0.3 TS/ID3 ASan sweep clean; **resolved RE_CVES §5's open item** — the
  S2 FFmpeg consumer is anacapad itself via `/jffs/app/install`; 86.10-80260 base
  rootfs decrypted (model-12 legacy key re-recovered, sections 7/8/9 on disk).
  Session closure count: **6** (s20, wbz, 13yv, 81v, d7vi, hd7l).
- Live lanes: jc7 wire-fuzz executing (omp_2), v8z setstring grammar (omp_1),
  qa2 AAC decode-side (omp_5), dxe HLS slot-16 (omp_6), 1w3 saved-queue path
  (omp_3, verdict delivered — next: saved-queue CreateObject route).

## Update 2026-10-29 ~01:25Z — UPnP passthrough CONFIRMED pre-auth on :1443

jc7's 75-case malformed-JSON wire fuzz (commit ed0a1cdf): **0 faults, 0 daemon
deaths**, but the structurally important result is that the `upnpDeviceProperties`
passthrough is **live pre-auth** on the :1443 JSON API —
`{"method":"GetZoneAttributes"}` returns 200 with a real `upnpResponse`, and
`SetZoneAttributes` with `args.DesiredZoneName` reaches the UPnP layer (402).
`args.DesiredZoneName` is a genuinely **bound string field** — exactly the shape
the u16-length descriptor mechanics (FUN_005f13c0, `d+0x18` halfword = copy
length AND bound) need for a real corruption attempt. Next pass targets UPnP
argument marshalling (rc 402/401), not the JSON layer. One doc regression
(wiped synthesis section) caught and reverted same-tick (a06a978f).

## Update 2026-10-29 ~01:45Z

- **frankensonos-qa2 CLOSED** (omp_5 RusticIbis, commits e74113ce+b09a6b4a):
  both arms — parse (ADTS/ID3 mapped, guard sound, 13/13 device clean) and
  decode (custom decoder mapped end-to-end via driver aav.0x101b6dd0, channel
  ≤0xc checked, 4 shapes clean). No OOB from header-controlled math.
  Session closure count: **7**.
- jc7 wire fuzz merged: UPnP passthrough pre-auth confirmed (ed0a1cdf);
  75 malformed-JSON cases 0 faults; next = UPnP argument marshalling.

## Update 2026-10-29 ~02:10Z

- **f4k first experiment** (omp_5 RusticIbis, commit 502a645a): the desync+fndelta
  combined recipe is **structurally incapable** of stacking the over-read
  primitives in one worker (sequential CREATE→QUERY_INFO→READ chain; the walk
  happens inside QUERY_INFO while a pinned worker blocks). SWGen 2→2, no restart.
  The pin remains the primitive; multi-worker grooming is the next question.
- setstring POST shape pinned (omp_1, bfc8036e): ≥200 B body, URL-encoded
  csrfToken; no-hang verdicts on .202/.86 with the tested keys — kill is
  key/state-dependent; the key hunt continues.

## Update 2026-10-29 ~02:35Z

- **frankensonos-dxe CLOSED** (omp_6 BrightDeer, commits 857f8479+a47d2e77):
  offline ring simulator (195-line Python model of the append/compaction/split
  triad) found the candidate configs; device falsifier 5/5 stable — recipe does
  not transfer on 57.23; static root cause: the flag-setter tail call
  (0x101908c8) is unreachable, pinning the C>16/C<0 configs out of playlist
  reach. Session closure count: **8** (s20, wbz, 13yv, 81v, d7vi, hd7l, qa2, dxe).
  All four codec beads + both TLS beads + the wget bead now closed with evidence.

## Update 2026-10-29 ~03:00Z

- **frankensonos-re-hls-kill-cnk reassigned** to AzureCardinal (P0; dxe proved
  the ring is not the mechanism — the kill lives in the fetcher lifecycle, the
  custom S1 decode pipeline, or the transport teardown).
- omp_2 (DarkJaguar) crashed to zsh mid-session; relaunched with `--continue`
  (session context preserved); confirmed back on the UPnP marshalling lane.
- Live lane board: v8z key hunt (omp_1), jc7 UPnP marshalling (omp_2), 1w3
  saved-queue reachability (omp_3), re-hls-kill-cnk characterization (omp_4),
  f4k getaa corruption (omp_5), dxe closed → standby (omp_6).

## Update 2026-10-29 ~03:25Z

- **frankensonos-1w3 CLOSED** (omp_3 TurquoiseForest, commits 6686485f+8880668a+
  977cd9dd): three-part falsification — every length source into the span-2139
  NUL write is capped below the control bands on 57.23. The H14 PC-control lead
  is honestly closed as unreachable-on-this-build. Session closure count: **9**.

## Reality-check one-pager (2026-10-29 ~03:45Z, through 9 closures)

**Where the root campaign stands after this swarm session:**

Proven positive capabilities:
1. Pre-auth remote daemon-restart primitive on .86 (S2): libsmb2 4.0.0 QUERY_INFO
   over-read, 16KiB groom, deterministic (RE_CVES §7c) — DoS-class, corruption→control open.
2. Pre-auth held-wedge of the root update fetcher (both S1 units): endless-100
   Continue (s20, RE_CVES §16) — attacker-controlled duration, self-heals on disconnect.
3. :1443 JSON API is pre-auth reachable (X-Sonos-Api-Key presence) with the UPnP
   passthrough live (jc7 ed0a1cdf) — the whole :1400 SOAP surface is tunneled
   through a parser with mapped u16-length descriptor mechanics.
4. One real memory-safety bug offline-proven (vorbis alloca stack overflow,
   upstream-fixed shape; device vehicle limitation named).

Proven negatives (each with named reopen conditions):
- URL-span PC-control chain: all three length sources capped (1w3).
- HLS ring compaction fault: flag-setter tail call unreachable (dxe).
- Both mbedtls builds: 3109-case server-handshake sweeps clean (wbz, 13yv).
- All four codec surfaces (FLAC/Vorbis/AAC/TS): no reachable memory-write bug.
- busybox wget chunked grammar: 2139-case × 2 builds clean.

The honest score: no root RCE yet; the two live corruption candidates are the
f4k getaa over-read (heap-grooming question) and the jc7 UPnP marshalling
arguments through the JSON binder (u16-length mechanics now wire-reachable).

## Update 2026-10-29 ~04:05Z — MEASUREMENT CORRECTION

- **frankensonos-re-hls-kill-cnk CLOSED** (omp_4 AzureCardinal, commit 41f400d3):
  the long-held "4-20s daemon kill (10/10)" was a **detector artifact** —
  hls_det.py's 2s-timeout /status/zp probe read slow playlist reloads as death.
  Independent 0.1s poller: max outage 0.14–0.23s across 9 runs, SWGen never
  moved, no watchdog restart. The fleet doctrine now requires an independent
  sub-second poller + SWGen check before any daemon-death claim. Session
  closure count: **10**.

## Update 2026-10-29 ~04:40Z

- **jc7 marshalling audited** (omp_2, bffb865e): the UPnP args builder (0x509d4c)
  is dynamic with a 34148-byte inline buffer — no copy/bounds bug — but two
  latent weaknesses documented: (a) size under-request (requests 2·len(value)+16,
  writes name+value+8; name longer than value+8 truncates, cursor advances by
  snprintf's full return, next grow-check computes on an underflowed unsigned),
  (b) **no XML escaping** — `<`, `>`, `&`, quotes emitted verbatim into the SOAP
  body. 8 long-name wire shapes: all clean.
- **c08 space closed** (omp_3, 39368ce0 + addendum): caller passes the RAW
  header value (no transform — MIPS16 re-read); 4608 canonical forms over all
  known client values, **9 app secrets not 2** (both Android APKs swept from
  ~/Downloads: S2 90.01.9 xapk + S1 11.16.1 apk, 4.7M strings × 5 forms, 731
  UUIDs × 360 forms) — no preimage in any shipped client. The allowlist
  constants' preimages are per-client values not shipped anywhere we have.
- **re-hls-kill-cnk closed as detector artifact** (see 04:05Z update).

## Update 2026-10-29 ~05:00Z

- **frankensonos-v8z CLOSED** (omp_1 GoldenGull, commits bfc8036e+3de0c89f):
  30-write device sweep across .202/.86 — the /setstring kill is state-dependent,
  not a property of the write; control + 4 hypothesis families all negative;
  every write persisted; diagflood (~1900 inserts) saturates workers without
  restart. Session closure count: **12**.

## Update 2026-10-29 ~05:40Z

- **frankensonos-uwd CLOSED** (omp_4 AzureCardinal, aa963075): type-21 token
  vocabulary fully decoded — 4 patch-train entries, all critical, no callbacks,
  '!' negation lexer, wire-parseable pre-signature but semantics are a bounded
  feature-compat list (no memory-unsafe construct). Self-closed with full
  evidence (doctrine note: orchestrator co-signs this closure after review).
- **frankensonos-cve-version-map-ja2 CLOSED** (orchestrator, direct): the sweep
  was completed and committed in the 2026-10-28 session (RE_CVES §8–§16).
- **frankensonos-v8z CLOSED** (see 05:00Z). Session closure count: **14**.
- Remaining open: xnf (Bearer forge — the hhid/kid derivation lane), f4k
  (omp_5, active), jc7 (omp_2, active), 9sd (omp_6, DIDL dc:title xref pass
  running), plus feature beads outside the root campaign.

## Update 2026-10-29 ~06:30Z

- **xnf premise CORRECTED** (omp_3 TurquoiseForest, commit 1d3f0c39): 0x51b9b4
  is `free()`, not an id→string getter (jalx to an even target = MIPS32 callee;
  the `li a1,<id>` in the delay slot is the size argument). The vtable byte-24
  entry is a destructor. The inspector's real shape: slot24 free(sp+32) → flag
  check → **slot 16 on ctx+4** writes sp+32 (read by BOTH the composer as luid
  and the iss compare) → slot 12 on ctx+16 = network_hash. **iss and luid are
  the same value.** The blocking unknown is now a single virtual call.
- **q57 advanced** (omp_4, 467dbe29): the cloud predicate's binding table found
  at 0x104f2d74 — `MinVersion: [<v>]` into /var/run/upgradeinfo, must be
  non-zero to arm (sentinel 0x7fffffff). This is the upgradeinfo arming template
  the campaign was missing.
- **9sd H-003 modeled** (omp_6, b5d4a267): deferred queue-dc:title kill modeled
  offline (8–60KB EnqueuedURIMetaData, state-dependent on a just-cleared queue).

## Session roll-up (2026-10-29 ~07:00Z)

56 commits, 14 beads closed with evidence, 0 stuck units, 2 provider-quota
incidents (recovered), 1 agent crash (recovered with --continue), 1 measurement
artifact overturned (the HLS "kill"), 1 campaign premise corrected (the token
"getter" was free()).

Live at roll-up: xnf (iss==luid discovery, one virtual call from a forged
token), q57 (upgradeinfo arming template found), f4k (two-connection groom
experiment pending), jc7 (UPnP marshalling verdict delivered), 9sd (H-003
model built), v8z/1w3/dxe/uwd/c08/cve-map/hls-kill/qa2/d7vi/81v/13yv/wbz/s20
all closed.

The two corruption candidates that remain live for the root goal:
1. f4k: libsmb2 QUERY_INFO over-read + two-connection heap grooming (.86).
2. xnf: one virtual call away from a forged Bearer token (all other chain
   links proven).
