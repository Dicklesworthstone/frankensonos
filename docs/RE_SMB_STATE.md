# RE_SMB_STATE — SMB2 state machine against the Sonos *client* (libsmb2 4.0.0)

Device `.86` (S2 86.10, Play:1). The box is the SMB **client**: `GET /getaa?u=x-file-cifs://<us>:<port>/S/<file>`
makes it connect and drive NEGOTIATE → SESSION_SETUP (NTLMSSP, 2 round trips) → TREE_CONNECT → CREATE →
QUERY_INFO → READ. `captures/research/smb-fuzz/smb_state_fuzz.py` serves that with impacket's
SimpleSMBServer and mutates exactly one field or header word of the reply to one chosen command.

Harness rules that make results attributable:
- **one fresh server port and one fresh share file per case**, `-sh` warm-up case first (a cold first
  connection 404s before the target op on this client);
- a case counts only when `"applied": true` — the mutation reached the wire;
- liveness is `/status/zp` before, after and 1.4 s later, plus ICMP, and the mutated PDU is written to
  `saves/` for every applied non-control case;
- cap 200 requests; a single `/getaa` costs ~18, so the matrix runs in batches.

## Control

`{"getaa":"http200:4068", "ops":[negotiate, sesssetup, sesssetup, treeconnect, create, queryinfo, create,
queryinfo, create, negotiate, sesssetup, sesssetup, treeconnect, create, queryinfo, create, queryinfo,
read], "pre":"200","post":"200","late":"200","icmp":[true,true]}` — 4 068 bytes, i.e. the real JPEG.

## Matrix (each line applied unless marked)

| case | reply | getaa | daemon | note |
|---|---|---|---|---|
| `negotiate:CreditCharge=0` | header word 6 = 0 | 404 | 200/200/200 | connection abandoned after NEGOTIATE |
| `negotiate:CreditCharge=0xFFFF` | header word 6 = 0xFFFF | 404 | alive | same |
| `negotiate:NextCommand=0xFFFFFFFF` | header word 20 | 404 | alive | **one op then stop**; PDU saved |
| `sesssetup:SecurityBufferOffset` | u32@68 = 0xFFFFFFFF | 200 (3.5 s) | alive | client retried the whole cycle |
| `sesssetup:SecurityBufferLength` | u32@72 = 0xFFFFFFFF | 200 | alive | retried |
| `treeconnect:MaximalAccess` | u32@76 = 0xFFFFFFFF | 200 | alive | ignored |
| `create:CreateContextsOffset` | u32@80 = 0xFFFFFFFF | 200 | alive | retried |
| `create:CreateContextsLength` | u32@84 = 0xFFFFFFFF | 200 | alive | retried |
| `queryinfo:OutputBufferOffset` | u16@66 = 0xFFFF | 200 | alive | retried |
| `queryinfo:OutputBufferLength` | u32@68 = 0xFFFFFFFF | 200 | alive | retried |
| `read:DataOffset` | u8@66 = 0xFF | 200 | alive | retried |
| `read:DataLength` | u32@68 = 0xFFFFFFFF | 200 | alive | retried |
| `read:DataRemaining` | u32@72 = 0xFFFFFFFF | 200 / 404 | alive | flaky, applied once of three |
| `read:CreditCharge=0x100` | header word 6 | 200 | alive | accepted |
| `read:NextCommand=0xFFFFFFFF` | header word 20 | 200 | alive | accepted |
| `read:NextCommand=4` ("chain" into the body) | header word 20 | 200 (2.7 s) | alive | accepted, slow |
| `ioctl:OutputOffset` / `OutputCount` | u32@88 / u32@92 | 200 | alive | ignored / accepted |
| `write:Count` | u32@68 | 200 | alive | write did not appear in a read-only fetch, so this is inert |

**No crash, no wedge, no RST storm, no ICMP loss, no daemon restart in any case.** Every reply the client
rejects produces the same two behaviours: retry the whole connection, or abandon it after the mutated
command (the NEGOTIATE header cases). Everything inside an established session (TREE_CONNECT onwards,
including READ framing) is retried, never fatal.

## Coverage limits

- WRITE and IOCTL are not exercised by a plain `/getaa` image fetch; a share-side or `SetInfo` path would
  be needed to make them meaningful, so `write:Count` is inert here.
- The client is libsmb2 4.0.0. CVE-2025-1048/1050 (libsmb2 UAF family) are SMB *server*-side and above our
  fix line, so this matrix is a field-robustness check, not a CVE reproduction.
- **This matrix only mutates SMB2 header words.** The QUERY_INFO *reply body* is a separate, live axis:
  CVE-2026-110712/110713/110714 (heap OOB reads in the FileAllInformation, FileFsVolume/Attribute and
  ACL decoders) are all present in 4.0.0 and proven there — `poc_queryinfo_oob.c` in this directory, and
  `docs/RE_CVES.md` §7c. Build the reply body, not the header.
- Cold-start flakiness is real: with a fresh port and file, roughly one in four cases 404s before the
  target op and shows `"applied": false`. Re-run those; the table above only records attempts that landed.

## Live: a patched QUERY_INFO body kills anacapad (2026-10-25)

The body axis is now tested on hardware. `recon/smbserv_body.py PORT 96 0xffff` serves the normal
FileAllInformation reply with `NameInformation.FileNameLength` (offset 96) set to `0xffff`, and
`/getaa?u=x-file-cifs://…/SHARE/real.jpg` against it takes the player down: the request never answers
(`curl` 000), `/status/zp` is unreachable for ~5-20 s, then the daemon is back on the same version.
Reproduced 2/2; the control (same harness, unpatched reply) answers 200. The server log shows the
patched reply was actually sent (`PATCHED off=96 val=0xffff buflen=102/116`), so this is the client
decoder, not the harness.

That is CVE-2026-110712's sink (`smb2_decode_file_all_info` → `utf16_to_utf8`) reached over the LAN with
no credentials. It is an OOB **read**, so today it is a remote DoS; the next question is whether the
over-read can be steered into a leak (the decoded name lands in the art URL path) or into a write.

### Boundary measurement

Same harness, varying only `FileNameLength` (the reply's name is ~6 bytes):

| value | bytes read past the name | result |
|---|---|---|
| `0x40` (64) | ~128 | 200, daemon survives |
| `0x100` (256) | ~512 | 200, survives |
| `0x400` (1024) | ~2048 | **crash** (000, dead ~5 s) |
| `0xffff` | ~131 KB | **crash**, 2/2 |

So the over-read is tolerated up to roughly half a kilobyte and faults somewhere
between 512 and 2048 bytes past the buffer — consistent with walking off the heap
chunk. The decoded garbage does not surface in the art response (200 with the
normal image), so today this is a remote DoS with a measured margin, not a leak.

**S1 does not take the same URL.** `/getaa?u=x-file-cifs://…` answers 404 on the 57.23 Play:5
(`.202`) for the encoded, unencoded and `x-smb` forms, and the SMB server sees no connection, so the
crash above is an S2 result until the S1 art path is found.

### Measured outage table (crash_oracle, 2026-10-26)

`recon/smb_outage.sh HOST PORT VALUE` serves one patched reply, polls
`/status/zp` every 100 ms with `recon/crash_oracle.py`, fires the `/getaa` at
t=3 s and keeps polling to time the recovery. Every row is one 22 s run; the
oracle needs two consecutive failed probes before it calls DOWN, so a single
slow probe cannot fake a crash.

| FileNameLength | runs | getaa | crashes | longest outage |
|---|---|---|---|---|
| `0x06` | 1 | 200 | 0 | - |
| `0x20` | 1 | 200 | 0 | - |
| `0x40` | 2 | 000, 200 | 1 | 3.877 s |
| `0x60` | 1 | 200 | 0 | 0.072 s blip |
| `0x80` | 1 | 200 | 0 | - |
| `0x100` | 4 | 000, 000, 200, 200 | 2 | 3.762 s, 3.698 s |
| `0x200` | 2 | 200, 200 | 0 | - |
| `0x400` | 2 | 200, 200 | 0 | - |
| `0x800` | 1 | 200 | 0 | - |
| `0x1000` | 1 | 200 | 0 | - |
| `0xffff` | 2 | 000, 000 | 2 | 3.804 s, 3.689 s |

- **The crash is probabilistic, not a threshold.** `0x100` crashes 2 of 4
  runs, `0xffff` 2 of 2, `0x40` 1 of 2, and `0x200`-`0x1000` never crashed.
  The earlier "0x400 crashes" line was a single sample; `0x400` has since
  survived 2/2.
- Every crash is the same shape: `getaa` 000, `/status/zp` refused for
  3.69-3.88 s, then the daemon answers again on the same version. No restart
  loop, no version change.
- The first case of a session is the least trustworthy: the very first `0x40`
  run crashed, the repeat did not, so warm up with a control before counting.
