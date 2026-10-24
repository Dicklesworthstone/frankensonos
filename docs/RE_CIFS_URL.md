# RE_CIFS_URL — `x-file-cifs://` URL shapes behind `/getaa?u=` (S2 .86)

The daemon parses this URL before any SMB packet is sent, so the parser is reachable unauthenticated with one
GET. 15 cases, `/info` liveness before and after each. Control is a **real working mount** (impacket share on
`192.168.4.170:1445/SHARE`, 4 068-byte image) so a 200 is a genuine end-to-end success; the rest are
`http404` with the daemon alive.

| case | URL shape | result |
|---|---|---|
| control | `u:p@host/SHARE/folder.jpg` | **200, 4 068 B** |
| empty user | `:p@host/...` | **200, 4 068 B** |
| empty pass | `u:@host/...` | **200, 4 068 B** |
| both empty | `@host/...` | **200, 4 068 B** |
| 4 KB user | `AAAA…:p@host/...` | 404, 37 ms |
| 4 KB pass | `u:AAAA…@host/...` | 404, 28 ms |
| 4 KB host | `u:p@AAAA…/...` | 404, 440 ms |
| 4 KB share | `u:p@host/AAAA…/folder.jpg` | 404, 89 ms |
| 4 KB path | `u:p@host/SHARE/AAAA….jpg` | 404, 35 ms |
| 100 × `@` in the userinfo | `@@@…@h/SHARE/folder.jpg` | 404, 2 539 ms |
| IPv6 literal | `u:p@[::1]:1445/SHARE/folder.jpg` | 404, 3 626 ms |
| %-escapes | `%2e%2e%2f%00%zz%2F` in the file name | 404, 186 ms |
| embedded `?` | `…/folder.jpg?a=b&c=d` | 404, 233 ms |
| no share | `u:p@host` (no `/share/path`) | 404, 22 ms |
| huge port | `:99999999` | 404, 155 ms |

**No crash, no reset, no 500, liveness 200 before and after every case.**

## Reading

- Empty credentials are accepted (guest mount works, image returned) — all three empty forms succeed, so the
  userinfo parser tolerates `:p@`, `u:@` and a bare `@`.
- Every pathological shape is contained: 4 KB in any one component, a hundred `@`, an IPv6 literal, escapes
  that include `%00` and an invalid `%zz`, a `?` that lands mid-URL, a missing share and an out-of-range port
  all end in a clean 404. The store probe is bounded over the whole component set.

## Limits

`404` conflates parse failure, auth failure and fetch failure, and with the share mounted the only unambiguous
positives are the four 200s. The 4 KB component cases in particular cannot be classified further from here
(the time spread — 28 ms to 440 ms — is consistent with *either* an early parse rejection or a connect attempt
that fails); separating those needs a listener that logs NEGOTIATE/SESSION_SETUP, which the SMB lane already
has. Runner `captures/research/cifs_url_fuzz.py`; the share server used for the control stays up on
`192.168.4.170:1445`.
