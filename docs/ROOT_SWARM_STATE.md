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

- **frankensonos-s20 closed 2026-10-28**: 2139-case chunked-grammar corpus clean
  on the device busybox 1.24.2 under qemu-ppc (0 signals — falsifier branch (b)
  satisfied); device verdicts D1/D2/D3 on .202; D2 persistence = ~2 min
  self-recovering pre-auth outage. No code-exec primitive on this build;
  documented, not claimed. Commits 6a23235e..e1323b67,
  `docs/RE_WGET_FUZZ.md`, `recon/wgetfuzz/*`.
- Stale-agent cleanup: beads s20/jc7/wbz/d7vi/v8z/81v reassigned from dead
  session agents (PearlMink, SapphireIvy, IndigoStone, RusticBarn, PinkMill) to
  live panes 2026-10-28 ~22:35Z.

## CVE sweep (2026-10-28)

No new Sonos-vendor CVEs in the last 30 days. CVE-2026-4149 (Era 300 SMB
DataOffset OOB, CVSS 10.0, kernel context, fixed 83.1-61240) is below both our
builds — reproduction target only. Details in `docs/RE_CVES.md` §8.
