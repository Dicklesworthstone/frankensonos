# RE_MBEDTLS_TLS_FUZZ — ASan server-handshake fuzz of the :1443 TLS surface (bead frankensonos-wbz, pivot)

The type-22 x509 surface was closed as **post-signature** (see
`RE_MBEDTLS_FUZZ.md`): a modified bundle is never parsed without a valid
type-17 signature. This doc is the pivot the orchestrator directed: fuzz the
**live pre-auth TLS server handshake** — the outermost parse a LAN attacker
reaches on `:1443` before any credential, signature, or HTTP state.

Everything below is measured unless marked *(static)*.

## Targets

| line | surface | TLS stack | evidence |
|---|---|---|---|
| S1 57.23 | `anacapad` `:1443` server, `.202` live | mbedtls 2.28.x (`libmbedtls.so.14`) | cert CN=`000E585D8C10`, issuer *Sonos Device Authentication Root CA*, TLSv1.2/AES128-GCM-SHA256 (curl, 2026-10-28); TLS1.0/1.1 → `protocol version` alert |
| S2 86.10 | `anacapad` `:1443` server | mbedtls 3.6.6 | `.86` strings `ssl_conf_own_cert/authmode/ca_chain/read_timeout/ticket_setup`, `net_recv_timeout` — server role *(static, from imports)* |

**Version pin (S1):** the device's `libmbedtls.so.14` exports exactly the same
251 `T` symbols as a stock **2.28.10** build — zero diff, both directions. The
pinned source (`2fc8413`, `mbedtls-2.28.10rc0-pr`, in
`captures/research/mbedtls-ppc/mbedtls/`) is therefore a faithful harness
target.

**S2 3.6.6 device libs are not on disk:** the RE tree `~/sonos-root-local`
that carried `fw/internals/libmbedtls.so.3.6.6` disappeared (documented at
`RE_CVES.md` §9a note). `captures/firmware/s2_overlay/bin/anacapad` links the
symbols but the `.so`s live inside the encrypted S2 rootfs. A stock 3.6.6
source build is the S2 harness baseline when that lane proceeds; the corpus
below is version-agnostic (wire format, not library internals).

## Harness

`captures/research/mbedtls-tls-fuzz/` (git-ignored, like the other fuzz
trees):

- `mbedtls-src/` — 2.28.10 source, configured `-DUSE_SHARED_MBEDTLS_LIBRARY=ON`,
  built with **ASan** (`-DCMAKE_C_FLAGS="-fsanitize=address -g -O1"`, shared
  and exe linker flags too; verified: 2825 `__asan` symbols in
  `libmbedcrypto.7.dylib`). CMake needs
  `-DCMAKE_POLICY_VERSION_MINIMUM=3.5` (cmake 4 vs old `cmake_minimum_required`).
- `keys/` — self-signed RSA-2048 server cert made with the build's own
  `cert_write`/`gen_key` (note: 2.28 `cert_write` selfsign requires
  `issuer_key=`).
- `tlsfuzz.py` — driver: starts `ssl_server2` (min/max TLS1.2, RSA suite
  class), replays each case as a TCP byte sequence, detects
  `ERROR: AddressSanitizer` in the server log, and **re-runs any crash on a
  fresh server for determinism**. Also detects server death (`SRV_EXIT`).
  Valid hellos must carry a `signature_algorithms` extension
  (`DEFAULT_SIGALGS`), otherwise 2.28 answers `-0x6980` (no usable suite) and
  the handshake never gets past suite negotiation.
- `cases_tls/0*.py` — corpus modules (below). A case's `build()` returns a
  list of TCP sends (a list of lists is flattened; byte-at-a-time fragmentation
  is expressible).

Run:

```bash
cd captures/research/mbedtls-tls-fuzz
cmake -S mbedtls-src -B mbedtls-src/build-asan -DCMAKE_POLICY_VERSION_MINIMUM=3.5 \
  -DCMAKE_C_FLAGS="-fsanitize=address -g -O1" -DCMAKE_EXE_LINKER_FLAGS="-fsanitize=address" \
  -DCMAKE_SHARED_LINKER_FLAGS="-fsanitize=address" -DENABLE_PROGRAMS=ON -DENABLE_TESTING=OFF \
  -DUSE_SHARED_MBEDTLS_LIBRARY=ON -DUSE_STATIC_MBEDTLS_LIBRARY=OFF
cmake --build mbedtls-src/build-asan -j8
python3 tlsfuzz.py --out out_tls_asan.tsv
```

## Corpus (10 modules, 3109+ cases)

| module | cases | covers |
|---|---|---|
| `010_control_chello` | 1 | valid RSA ClientHello (control: ServerHello+Certificate flow) |
| `020_sid_len` | 4 | session-id declared-vs-sent mismatches, 0/32/255 |
| `030_record_layer` | 11 | zero-length records, handshake-header overruns, unknown types/versions, fragmentation, byte-at-a-time, CCS/alert before handshake |
| `040_extensions` | 30 | SNI (empty/oversize/mismatch/wrong-type), ALPN, supported_groups, sig_algs, ec_point_formats, session ticket, EMS, max_fragment_length 0-5, unknown ext 512 B, duplicate exts, ext-length overrun, 200-ext flood |
| `050_framing` | 26 | truncations at offsets 0-8, double hello, junk after hello, length-field edits, TLS-version-field variants (3.0/FFFF/3.4/0.0), compression-list abuse, cipher-list abuse (empty/odd/overdeclared) |
| `060_keyexchange` | 24 | RSA/ECDHE/DHE ClientKeyExchange size classes (0/1/128/255/256/512, declared 0x8000 vs 64 sent), EC point forms, premature Finished, premature CCS, alert-after-hello, triple kx, renegotiation-info abuse |
| `070_certrequest` | 4 | client-cert chain length abuse (post-CertificateRequest) |
| `080_recordstress` | 9 | 100 empty records, declared-0xFFFF record, drip-feed, record-version cycling, DTLS-shaped hello on TCP |
| `090_mutate` | 3000 | seeded (1337) structural mutations of the valid hello: byte flips, 1-4-byte deletes/duplicates, 16-bit length bumps |
| `100_ticket_renego` | 23 | session-ticket size classes 0..4096, ticket+session-id resume conflict, renegotiation-info shapes, duplicate ticket exts |

## Results

**0 findings.** Every case `ok`; no ASan report, no server exit, in the full
sweep (`out_tls_asan.tsv`, 10/10 modules). The two single-SIGSEGV
observations from the x509 phase did not have counterparts here; no fault
exists to minimize.

Mechanistic reasons the 2.28.10 server handshake is boring (all *(static)*,
from `library/ssl_srv.c` / `ssl_msg.c` reads during corpus calibration):

- every ClientHello length field is checked against `msg_len` with
  `+2/+1` arithmetic before use (`ssl_srv.c:1750`-`1760` ciphersuite list, the
  sig_algs check at `ssl_srv.c:246` requires `list_size + 2 == len` and even);
- record assembly is `fetch_input`-driven: a record declared longer than the
  stream simply waits; there is no read-into-fixed-buffer on the network path;
- extensions are parsed length-first (`ssl_tls.c` `parse_client_hello` walks
  `ext_len` bounds), and unknown extensions are skipped, not copied.

## Device verdict (2026-10-28, `.202`, 57.23-74170)

The seven worst corpus shapes replayed live against `:1443` over `nc` (Python
raw sockets cannot reach the 192.168.4.0/24 host from this machine — curl/nc
can; same quirk the re-probe harness works around):

| shape | device reply | daemon after |
|---|---|---|
| sid declared 255 / 32 sent | 7 B alert (`15 03 03 00 02 …`) | healthy |
| ext-length overrun (decl 0x100) | 0 B, closed | healthy |
| record declared 0xFFFF, 100 B sent | 0 B, closed | healthy |
| double ClientHello in one record | 0 B, closed | healthy |
| CCS before any handshake | 0 B, closed | healthy |
| alert-only record | 0 B, closed | healthy |
| byte-at-a-time hello (74 records) | 0 B, closed | healthy |

Post-run health: `:1443/status` 200, `:1400/status/VERSION` 200, a follow-up
TLS connect answered. No daemon restart, no wedge, no recovery action needed.
So the device build tolerates the same shapes the ASan reference build does —
consistent with the S1 lib being stock 2.28.10.

Note: raw-socket tests from this host get `Errno 65` for `.202` while curl/nc
succeed on the same interface; the corpus driver is unaffected (localhost).

## Scope honesty

- The harness exercises the **mbedtls** server handshake. On the device, the
  listener is anacapad's; its own framing *around* mbedtls (the muse websocket
  gate in `RE_1443_PARSER.md`) is a different parse and not covered here.
- S2 3.6.6: corpus is reusable, harness not yet built (no device libs); 3.6.6
  has its own handshake parser (client hello restructured around
  `MBEDTLS_SSL_TLS_1_3`), so a rerun there is a new question, not a copy.
- 2.28.10 is post-LTS-EOL upstream; "no findings" matches the empty in-window
  CVE sweep (`RE_CVES.md` §15) — the surface is calm, the value of this lane
  is the negative proof plus the reusable rig.

## §S2 — mbedtls 3.6.6 server handshake (bead frankensonos-13yv, 2026-10-28)

The S2 arm of the same falsifier: 3.6.6 built from upstream (the device's own
`libmbedtls.so.21`/3.6.6 is not extractable locally — see the
`~/sonos-root-local` note in `RE_CVES.md` §9a), same corpus, same driver.

### Harness deltas vs the 2.28.10 rig

- Source: upstream `v3.6.6` tarball + the `mbedtls-framework` submodule
  (main; the tarball has no `.git` so no submodule pin — the framework is
  scripts/tests only and does not affect library code). CMake needs
  `PYTHONPATH=framework/scripts` for `scripts/config.py` and
  `-DMBEDTLS_FATAL_WARNINGS=OFF` (Apple clang deprecation `-Werror` on
  `sprintf` in `programs/hash/generic_sum.c`). Built with the same ASan flags;
  `libmbedtls.21.dylib` verified instrumented (3214 `__asan` symbols).
- Driver: `MBEDTLS_VER=3.6.6` selects `mbedtls-3.6.6-src/build-asan` +
  `keys366/` (own key/cert made by the 3.6.6 `gen_key`/`cert_write`).
- Crash detection hardened for both rigs: a crash report found only in the
  final flush (mac libc fully buffers the server's stdout to the log file) is
  now caught by a re-scan after each case's server-exit and a final
  whole-log scan (`FINAL-SCAN` row).
- Corpus unchanged. The same 2.28-style hello (RSA suite + sig_algs) gets a
  full ServerHello+Certificate flow out of 3.6.6 — no API-shape rework was
  needed for the wire corpus.

### Offline sweep

`out366_v2.tsv`: 10/10 modules, 3109+ cases, **0 findings** — no ASan report,
no server exit, nothing to minimize. (First pass `out366_full.tsv` + hardened
re-run `out366_v2.tsv`; the 2.28.10 rig was re-run once with the same hardened
detection for symmetry, also clean.)

### Device verdict (`.86`, 86.10-81100, 2026-10-28 ~22:58Z)

CalmHawk-approved window (Agent Mail thread 8908), ~1 minute wall, one TCP
connect per shape, 3-5 s spacing, transport state untouched. Replay via `nc`
(the Python raw-socket Errno-65 quirk to LAN hosts; curl/nc unaffected).

| shape | `.86` reply | `/status/zp` after |
|---|---|---|
| control: valid TLS1.2 ClientHello | 7 B alert `15 03 03 00 02 …` | 200 |
| sid declared 255 / 32 sent | 7 B alert | 200 |
| ext-length overrun (declared 0x100) | 7 B alert | 200 |
| record declared 0xFFFF / 100 B sent | 0 B, closed | 200 |
| double ClientHello in one record | 7 B alert | 200 |
| CCS before any handshake | 0 B, closed | 200 |
| alert-only record | 0 B, closed | 200 |
| byte-at-a-time hello (74 records) | 0 B, closed | 200 |

8/8 clean. `/status/zp` 200 after every shape, `:1400` healthy, version
unchanged, no restart. Behavioral note: the 3.6.6 device answers
sid-overrun/double-hello with a protocol-level alert (7 B) where stock
2.28.10 mostly closed the connection silently — a more talkative alert path,
same benign outcome.

### Falsifier status

Both arms fired: 3000+ ASan cases clean on 3.6.6, seven worst shapes clean on
`.86`. No pre-auth record/handshake parsing flaw found by this corpus on
either line. Scope remains: mbedtls server handshake only — the muse
websocket gate behind TLS is the jc7/`RE_1443_PARSER.md` surface, and S2's
client-side surfaces (`/getaa` x-file-cifs, SSA-2026-0001 downgrade path) are
separate lanes.
