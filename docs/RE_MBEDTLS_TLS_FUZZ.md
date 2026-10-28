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
