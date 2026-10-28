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
