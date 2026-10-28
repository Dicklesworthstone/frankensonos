# RE_CVES — live lane log

One lane of the 2026-10-24 root swarm. Facts with addresses/commands only; merge into FIRMWARE_PATHS.md when the lane closes.

Window: **2026-09-23 … 2026-10-23** (NVD `pubStartDate`/`pubEndDate`). Anything older that still bites a shipped
version is listed separately as *pre-window, live* — it is not in the window but it is in the device.

## 1. What is actually on the devices (version evidence)

| component | S1 (57.23, PPC) | S2 (86.10, MIPS) | evidence |
|---|---|---|---|
| expat | embedded; **no version string in this build at all** (§5b) — the memo's 2.2.0 came from 34.16, not 57.23 | **2.5.0** | `strings anacapad \| grep expat_` → `expat_2.5.0` on 86.10 only; both builds static, no `libexpat` in either rootfs |
| mbedTLS | **2.28.x** (`libmbedtls.so.14` / `libmbedcrypto.so.7` / `libmbedx509.so.1`) — **corrected 2026-10-25, was 2.5.0** | **3.6.6** | 57.23 rootfs images, both models: `57.23-74170-1-16-rootfs.bin` (squashfs) `lib/`, and `57.23-74170-1-12-rootfs.bin` (cramfs) `lib/`; `sonos-root-local/fw/internals/libmbedtls.so.3.6.6`, NEEDED `libmbedtls.so.21` |
| libcurl | not used by the daemon | **static in `anacapad`**, mbedTLS backend, HTTP/2 present (`nghttp2` ×18, `HTTP/2` ×17, `ALPN` ×15); **version unpinnable** (three approaches, all null — see §5a) | `readelf -d` (no `libcurl` NEEDED); `strings` |
| busybox | **v1.00** (`s1_rootfs/bin/busybox`), **v1.14.1** (`play1_rootfs/bin/busybox`); applets present: `telnetd`, `wget`, **no `httpd`** | not in the captured overlay (base-rootfs binary, not extracted) | `strings busybox \| grep -x httpd` → empty |
| kernel | **2.4.25-LSDK-5.0.1.20** (2004) | not in the captured artifacts (image encrypted) | `captures/research/s1_attack_surface.md:28` |
| HLS / MPEG-TS | — | FFmpeg **5.0.3** (`libavformat.so.59`, `libavcodec.so.59`); **no** `libavformat` NEEDED in `anacapad` and no `avformat`/`hls`/`m3u8` strings in it — decoding is a different process (TBD, consumer binary not in the overlay) | `strings libavformat.so.59` → `FFmpeg version 5.0.3` |
| libsmb2 | absent | **4.0.0** (`libsmb2.so.1`), used by the S2 daemon's SMB server | `strings libsmb2.so.1` → `libsmb2.so.4.0.0` |

Consequence for triage: the odd components are exactly the pre-auth, network-facing ones — expat (SOAP/DIDL/GENA),
libcurl (outbound fetch), libsmb2 (SMB listener on S2), FFmpeg (media). busybox and the kernel are mostly init-side.

## 2. In-window CVEs (2026-09-23 … 2026-10-23)

### expat 2.5.0

| CVE | pub | what | 2.5.0? | reachable? |
|---|---|---|---|---|
| CVE-2026-77214 | 10-07 | heap over-read, `XML_ParseBuffer` trusts caller `len` and walks `m_bufferEnd` past the buffer | yes as a version matter | **UNREACHABLE — settled, §5b.** The daemon calls only `XML_Parse` (six call sites, all 4-argument, all in one wrapper); nothing calls the 3-argument buffer API. `XML_Parse` does `XML_GetBuffer(len)` first, so the unvalidated-add never starts from a short allocation. Impact if it ever were reached: disclosure, CVSS 8.2 C:H. |
| CVE-2026-102633 | 09-29 | integer overflow in `expat_realloc`, 32-bit only | **2.7.2–2.8.5 only — not 2.5.0** | n/a (MIPS/PPC are both 32-bit, so it would have applied if the version matched) |

Pre-window but live on 2.5.0 (verified against NVD text):

| CVE | pub | what | reachability |
|---|---|---|---|
| CVE-2026-66046 | 08-18 (Debian DSA-6566-1: 10-15) | O(N²) in `storeAtts()` — a few MB of XML with many non-normalized attributes burns CPU. No entities, no non-default options needed | **high** — any SOAP/DIDL body we can make the daemon parse. Our 2 k–50 k sibling-property probes are the same shape and were all accepted, so the cost function is real and unbounded by the parser |
| CVE-2026-32776 | — | NULL deref on empty external parameter entity content | `expat_dtd.py` / `expat_poc*.py` already drive this surface; `XML_ExternalEntityParserCreate` is what `docs/FIRMWARE_PATHS.md:626` calls out as unused, so this needs a re-check before it can be claimed |
| CVE-2026-93990 | — | high surrogate without low surrogate accepted (before 2.8.5) | needs the daemon to hand expat UTF-16; an encoding declaration inside a DIDL/SOAP document is attacker-controlled, so this is *probably* live but unmeasured |
| CVE-2026-76956 | 08-20 | getentropy RC misuse → hash flooding | **not 2.5.0** (2.8.2/2.8.3 only) |

### FFmpeg 5.0.3 (S2 HLS/TS path)

| CVE | pub | what | 5.0.3? | reachable? |
|---|---|---|---|---|
| CVE-2026-107695 | 10-08 | **HLS demuxer infinite loop** — `parse_playlist()` accepts Master Playlist tags in a Media Playlist; a self-referencing playlist adds variants forever in `hls_read_header()` | yes (`before 8.1.3`) | **high, but not reproduced** — exactly the `x-rincon-mp3radio://…/x.m3u8` path the hls-kill bead drives; measured self-reference is bounded (measured below) |
| CVE-2026-107697 | 10-08 | HLS demuxer bypasses protocol/`allowed_extensions` for child playlists | yes | high — a master playlist can make the demuxer open non-media local/protocol targets (file read, extra SSRF) |
| CVE-2026-107660 | 10-08 | `tls_open()` in `libavformat/tls_mbedtls.c` skips hostname checks for **IP-literal** hosts | yes (`before 8.1.3`) | medium — needs an `https://IP/…` media URL; any trusted CA-issued cert then intercepts |
| CVE-2026-96611 | 09-23 | signed overflow on HEIF `ispe` w/h in `mov.c` | yes (before 9.0) | low on this fleet (MOV/HEIF only, not the M3U8 path) |
| CVE-2026-107677 / -107678 | 10-08 | DASH infinite loop; recursive free stack exhaustion in `av_encryption_init_info_free()` | yes (through 9.0.2) | low — DASH demuxer and encryption-info only |
| CVE-2026-107696 / -107698 | 10-08 | RTSP 3xx redirect with no limit; RTSP redirect SSRF | yes | low unless `rtsp://` URIs are accepted |

### libcurl (static, HTTP/2 present)

| CVE | pub | what | reachable? |
|---|---|---|---|
| CVE-2026-92392 | 10-14 | UAF of `struct Curl_easy` when an **upload** gets `RST_STREAM` (`HTTP_1_1_REQUIRED` / `REFUSED_STREAM`). Affected 8.12.0–8.22.0 (fixed 8.23.0, released the same day; 8.16.2/8.20.2 in their branches) | **UNPROVEN — closed for now.** Both halves fail: the version cannot be pinned from the binary (§5a) and no upload call site is named. Reopen only if `curl_version_info` is recovered (e.g. from a live `#0`/core, a `/diag` verb that prints it, or an image whose build date straddles 8.23.0). |

### busybox (v1.00 / v1.14.1)

Nineteen CVEs were published 2026-09-23 (CVE-2026-88830…88840). None survive the version+applet filter:

- `88830` (TLS Montgomery overflow) and `88840` (TLS truncated ClientHello read past end): 1.14.1 has **no TLS** in `wget` (`FEATURE_WGET_HTTPS` is documented as not verifying peers either, see rejected `88836`) → n/a.
- `88831` (httpd CIDR deny fails open), `88837` (httpd yescrypt treated as plaintext): **no `httpd` applet** is compiled in.
- `88839` (`passwd`/`group` `tokenize()` stale pointer) needs someone running `passwd`; `88835` (`dpkg` OOB read) needs a `.deb` installed; `88832` needs a crafted romfs mounted — all post-auth or offline.
- `CVE-2026-108119` (tar symlink/hardlink escape on extraction) — only matters if an attacker-supplied tar is unpacked; the updater does not use busybox tar on the `.upd`, it has its own VM.

### kernel

Kernel CVEs in the window are all modern-tree fixes (`kasan` cache shrink race 2026-09-24, SELinux `mprotect` backing-file checks 2026-10-06, …). On **2.4.25 (2004)** almost none of them apply; the productive list there is the historical 2.4/2.6 set, not the last 30 days. No in-window kernel CVE touches this build.

## 3. CVE-2025-1048 / CVE-2025-1049 — the real shapes, and a numbering trap

Both are Pwn2Own 2024, fixed in S1 57.22-61162 / S2 83.1-61240 — every build we own is *above* the fix line, so they are
reproduction targets, not live bugs (see UPDATE_FEED_AND_SIGNATURE.md:964 — recovering them needs the patch-list gate bypassed).

**The Sonos advisory and the CVE records disagree, and the repo docs inherited Sonos's order.** SSA-2024-0002 labels
1048 = "HLS", 1049 = "MPEG-TS", 1050 = "SMB2 UAF". The CVE records are:

- **CVE-2025-1048** = **libsmb2 use-after-free** (ZDI-25-223, ZDI-CAN-25535, Viettel, CWE-416, 8.8 AV:A). Shape:
  an SMB2 message sequence that frees a session/request object and then performs an operation on it — no auth, no
  credentials. Target file is libsmb2, which is on **S2 only** (`libsmb2.so.1`, and S2's SMB server is reachable, see
  bead fy9/c25).
- **CVE-2025-1049** = **ID3 heap overflow in the TS path** (ZDI-25-224, ZDI-CAN-25601, InfoSect, CWE-122, 8.8). Shape:
  an ID3 tag length copied without validating against the heap buffer *before* the copy, inside ID3 processing of
  transport-stream media. That is exactly our EV-13 ID3 mutation set; retest it against 34.16, which predates the fix.
- CVE-2025-1050 = the SMB2 UAF per Sonos, which collides with 1048's CWE — treat 1048/1050 as the same SMB family until
  someone diffs libsmb2 4.0.0 against the fixed tag.

## 4. Ranking by reachability (pre-auth, from the LAN)

1. **CVE-2026-107695** (FFmpeg HLS loop) — one HTTP fetch of a crafted m3u8, hangs the media path. Same door as the open
   daemon-kill bead; cheapest next live test.
2. **CVE-2026-66046** (expat O(N²)) — a single SOAP/DIDL body with thousands of non-normalized attributes; no preconditions.
3. **CVE-2026-107697** (HLS protocol/extension bypass) — chains with (1): the child playlist reaches file/`http` targets.
4. **CVE-2026-1050 / CVE-2025-1048 family** (libsmb2 UAF) — pre-auth SMB on S2, but we are above the fix line.
5. **CVE-2025-1049** (ID3) — pre-auth media on any build; only exploitable on ≤57.22-59130/83.1.
6. **CVE-2026-107660** (FFmpeg TLS hostname check) — needs an IP-literal https media URL; enables MITM, not memory corruption.
7. **CVE-2026-77214** (expat over-read) — unreachable here: `XML_Parse` only, never `XML_ParseBuffer` (§5b).
8. **CVE-2026-92392** (curl h2 upload UAF) — unproven on both counts (version unpinnable, no upload site); closed.
9. **busybox / kernel** — no applicable in-window issue.

### Measured on 86.10 (2026-10-23): the self-referencing playlist does not hang

Served three shapes from a LAN host to `.84` (86.10) as `x-rincon-mp3radio://<ip>/<file>` and counted the
demuxer's fetches:

| shape | requests in 10 s (per path) |
|---|---|
| master playlist, one variant pointing at itself | 4 / 4 `loop.m3u8` |
| media playlist (EXTINF + segment) with an embedded STREAM-INF variant pointing at itself | 9: 7 `loop2.m3u8` + 2 `seg.ts` (then ~1 reload / 5 s, normal HLS polling) |
| master playlist, eight variants all pointing at itself | 8 `loop3.m3u8` (one level, no fan-out) |

So CVE-2026-107695's *outcome* (unbounded variant growth) is not reproduced by self-reference on this build:
recursion is depth-bounded, the variant count does not multiply, and playback opens normally each time.
Whether 5.0.3 has the bug under a nastier shape (distinct URLs per level, or a master nested inside a media
playlist that also carries a segment) is open — this is a starting negative, not a refutation.

## 5. Open measurements

- name an HTTP/2 **upload** call site in `anacapad` and pin `curl_version_info` from a live process (§5a);
- identify the FFmpeg consumer binary and whether `allowed_extensions`/protocol whitelist is configured (decides 107697);
- S2 busybox and S2 kernel: **unverified, and left that way.** Neither is in any captured artifact (the overlay carries `bin/anacapad` only; the rootfs is inside the encrypted image) and neither is a pre-auth surface on S2. Anything written about them would be inference from the S1 rootfs, which is a different userland (busybox v1.00 / v1.14.1) and a different kernel (2.4.25).

## 5b. expat: no version string, static link, and `XML_Parse` only (settled)

Three separate facts, in the order they matter:

1. **No version evidence in 57.23.** `strings` finds eight expat strings and not one of them is a version: `EXPAT_ENTROPY_DEBUG`, `EXPAT_ACCOUNTING_DEBUG`, `EXPAT_ENTITY_DEBUG`, `requested feature requires XML_DTD support in Expat`, and the `expat: Accounting/Entities` format strings plus `xmlparse.c:%d`. The 2.2.0 in the memo was read from **34.16**, and 86.10 carries an explicit `expat_2.5.0`. So 57.23's version is **unpinned**; "2.5.0" for it is inference from the neighbouring builds, and should be written that way.
2. **There is nothing to import.** expat is statically linked in both builds — no `libexpat*.so` in either rootfs, zero `XML_*` entries in `.dynsym`, no `NEEDED`. Asking whether `XML_GetBuffer` is "imported" therefore always answers no and cannot discriminate; the discriminating evidence has to be the call shape.
3. **The call shape says `XML_Parse` only.** `XML_Parse` is at `0x1041ced4` in the 57.23 binary (four arguments — the `cmpwi 7,3,0` prologue, `XML_ERROR_NO_BUFFER`-free, and the `m_parsingStatus`/`m_bufferPtr` checks at +476/+472). It has exactly **six** callers, all inside one daemon function: `0x1038d890`, `0x1038d9b0`, `0x1038da3c`, `0x1038dad8`, `0x1038db30`, `0x1038db78`. Every one is in the four-argument form — e.g. `0x1038d890` is `(parser, "<literal>", 1, 0)` and `0x1038db78` is `(parser, NULL, 0, 1)`, the flush idiom. Nowhere does daemon code pass the three-argument `(parser, len, final)` form that `XML_ParseBuffer` needs.

Consequence: `m_bufferEnd` only ever advances right after a matching `XML_GetBuffer(len)`, so **CVE-2026-77214 is unreachable on 57.23.** 86.10 was not read for this (the MIPS build has no symbol table and the offset probe was inconclusive), so carry it there as "same wrapper, unmeasured" rather than as a measurement. It is a version-level bug we do not have a door for, not a maybe.

## 5a. Why the curl version is unpinnable (six approaches, all null)

1. **Version literal.** `curl_version()` normally carries `"libcurl/" LIBCURL_VERSION`; neither `libcurl/` nor the `libcurl/%d.%d.%d` format is present. Under `-ffunction-sections -Wl,--gc-sections` the whole `curl_version`/`curl_version_info` pair is dropped because the daemon never calls it.
2. **Bare `x.y.z` sweep.** Every string matching a bare version triple in the binary is one of `2.0.0 17.2.7 1.54.1 1.2.11 1.1.0` — no `8.x` anywhere.
3. **Backend / symbol / stamp probes.** The only backend literal is `mbedTLS/%u.%u.%u` (`0x69ee44`), parameterised by the **TLS** library, not curl; there are zero `curl_*` symbols in `.symtab` or `.dynsym` (fully static and stripped) and no `.comment` section.

The only remaining source is runtime (`curl_version_info` out of a live process) or a build date; neither is available, so CVE-2026-92392 is recorded as unproven rather than inferred.

## 6. Poisoned type-22 bundle carrying CVE-2018-0487 (built + fired, 2026-10-23)

The rcbundle inside the type-22 section is **plaintext and parsed before any signature check**, so a
rejected image still drives the certificate parser. Built three variants off the genuine
`57.23-74170-1-16.upd` and fired each at `.202` (57.23-74170, model S5) with `BeginSoftwareUpdate` —
which lives on **ZoneGroupTopology**, not SystemProperties (401 there).

Method: the bundle manifest is `u16 count @rb+0x6a` then `count` × 8-byte `u16 0x07c1 | u32 offset |
u16 length` records, contiguous, and each `length` is the exact DER size of the cert at `offset`. So a
fixture built to the **same length** needs no count, offset or length change — only that entry's bytes
move. Padding is a non-critical filler extension (`1.3.6.1.4.1.4444`) on a PSS/SHA-512 cert, sized until
the DER is exactly the target length (`captures/research/cve-lane/padcert.py`, `mkpss2.py`).

| variant | fixture | entry | splice offset (abs) | len | served | daemon down | ICMP | version after |
|---|---|---|---|---|---|---|---|---|
| A | `pss528`, 442 B | 0 | `0x764` | 442 | **5 606 749 / 5 606 749**, 17 s | 23:27:18 → 23:28:01 (43 s) | 0 % loss | 57.23-74170 |
| B | `pss528` + 12-arc filler OID, 765 B | 25 | `0x40ae` | 765 | 5 606 749 / 5 606 749, 17 s | 23:30:48 → 23:31:15 (27 s) | 0 % loss | 57.23-74170 |
| C | hand-built `pss512` (emLen 64 < hLen 64), 442 B | 0 | `0x764` | 442 | 5 606 749 / 5 606 749, 17 s | 23:28:30 → 23:28:56 (26 s) + second pass 23:29:05 (20 s) | 0 % loss | 57.23-74170 |

**Correction 2026-10-27 (bead wbz):** "parsed before any signature check" is wrong, and the
fixtures are inert twice over. The updater is a stage machine; the type-22 handler in the parse
stage only *reads* the section, and the header parse plus its signature verify live in the
write stage, which runs only after the type-17 signature verified. Separately,
`MBEDTLS_X509_RSASSA_PSS_SUPPORT` is compiled out of the 57.23 build, so `rsassaPss` is
rejected at the algorithm identifier before any verify. See `docs/RE_MBEDTLS_FUZZ.md`.

Run class for all three: **full read, no abort**. Every byte of every variant was delivered and the bundle
therefore parsed; the daemon restart after the transfer is the update path, not the abort point, so the
bytes-served count is the only usable crash discriminator and it never truncated. `.202` came back on its
own each time, version unchanged (stale signature, as expected).

Paths: `captures/research/cve-lane/poison/{A,B,C}-*.upd` (5 606 749 B each, byte-identical to the vendor
package outside the spliced entry), builder `poison/build.py`, serving log `/tmp/upd_access.log`.

## 7. Window refresh 2026-09-25 … 2026-10-25 (2026-10-25)

The window above closed on 10-23; this section carries the two days after it plus the version corrections
that came with a proper look at the 57.23 rootfs images.

### 7a. The S1 mbedTLS row was wrong: it is 2.28.x, not 2.5.0

`captures/firmware/s1_rootfs` is the **34.16** tree, and its `lib/libmbedtls.so.10` is where the 2.5.0 in
§1 came from. The 57.23 images ship a different library: `libmbedtls.so.14`, `libmbedcrypto.so.7`,
`libmbedx509.so.1` — the 2.28 SOVERSION triple — in both `57.23-74170-1-16-rootfs.bin` (squashfs, `lib/`)
and `57.23-74170-1-12-rootfs.bin` (cramfs, `lib/`). Symbol set agrees: `mbedtls_platform_zeroize` (2.7+),
`mbedtls_x509_crt_parse_der_with_ext_cb` (2.16+), `mbedtls_x509_crt_verify_restartable` (2.7+).

Consequence: the 2.5.0-era triage below is superseded for S1. 2.28 is the LTS branch and is patched
through 2.28.10; the rootfs squashfs build stamp is 2026-02-17, so the shipped point release is
2.28.10-or-later and the pre-2.28.10 advisories (CVE-2025-27809 hostname check, CVE-2025-27810
uninitialised Finished) are **not** live on 57.23. S2 is 3.6.6 and is a different question (§7b).

### 7b. mbedTLS 3.6.6 on S2: one in-window advisory, client-side only

| CVE | pub | what | 3.6.6? | reachable? |
|---|---|---|---|---|
| CVE-2026-25832 | 09-14 | TLS 1.3 client accepts a HelloRetryRequest selection it should reject (3.6.x before 3.6.7, 4.1.x before 4.1.2) | yes | **client-side only.** The S2 daemon is a TLS *server* on `:1443` and a TLS *client* for outbound media/cloud fetches. A hostile server on an outbound fetch is the door; the fetch target is not attacker-chosen pre-auth (`OnlineUpdateBaseURL` is devmode-gated), so this is a MITM/rogue-server primitive, not a LAN one. |
| CVE-2026-34876 | 03-xx | OOB read in `mbedtls_ccm_finish()` (3.x before 3.6.6) | **no — fixed at 3.6.6** | n/a |

### 7c. libsmb2 4.0.0 on S2: four in-window CVEs behind the `/getaa` SMB client

Published 2026-10-14 and 2026-10-22, all "through 6.2" / "before 6.3", so 4.0.0 is inside every range.
The trust boundary is the **server reply**, and `/getaa?u=x-file-cifs://<us>:<port>/…` makes the player
our client — no configuration, no credentials.

| CVE | sink | trigger | class |
|---|---|---|---|
| CVE-2026-110712 | `smb2_decode_file_all_info` (`lib/smb2-data-file-info.c:202`) | QUERY_INFO FileAllInformation reply with a `FileNameLength` past the buffer; reached by `smb2_stat`/`smb2_fstat`, which is what the `/getaa` client already issues | heap OOB **read** |
| CVE-2026-110713 | `smb2_decode_file_fs_volume_info` / `_fs_attribute_info` | QUERY_INFO FileFsVolume/FileFsAttribute reply with a missing or oversized name length | heap OOB **read** |
| CVE-2026-110714 | `decode_acl` (`lib/smb2-data-security-descriptor.c:266`) | QUERY_INFO security-descriptor reply with `AclSize < 8`; the `size_t` underflow defeats the ACE bounds checks | heap OOB **read** |
| CVE-2026-115505 | DCE/RPC NDR decoder (`lib/dcerpc.c`) | crafted `NetrShareEnum` reply with a wrapping `actual_count`/`max_count`; needs `smb2_share_enum_async()` | heap OOB **read and write**, CVSS 7.6 |

`RE_SMB_STATE.md`'s matrix mutated only SMB2 **header** words (`OutputBufferOffset`, `OutputBufferLength`,
`DataOffset`, …). The payload fields *inside* the QUERY_INFO reply are the untested axis, and 110712/110713/
110714 all live there. 115505 needs a call site the `/getaa` path does not obviously use — treat it as
"only if share enumeration is wired", not as reachable.

**Proven on 4.0.0, not just 6.2.** The advisories say "through 6.2"; a v4.0.0 checkout built with ASan
aborts on all three of the first CVEs. Reproducer: `captures/research/smb-fuzz/poc_queryinfo_oob.c`
(build recipe in the header). Observed 2026-10-25:

```
case 1  heap-buffer-overflow READ of size 2 in utf16_size (unicode.c:202)
        <- utf16_to_utf8 <- smb2_decode_file_all_info (smb2-data-file-info.c:158)
case 2  same, <- smb2_decode_file_fs_volume_info (smb2-data-filesystem-info.c:63)
case 3  heap-buffer-overflow READ of size 1 in smb2_get_uint8 (pdu.c:246)
        <- decode_ace (smb2-data-security-descriptor.c:111) <- decode_acl (:262)
```

4.0.0's `smb2_stat_async` requests `SMB2_FILE_ALL_INFORMATION` (`lib/libsmb2.c`), so case 1 is the one a
`/getaa` fetch reaches; case 3 needs a security-descriptor QUERY_INFO, which the art path does not
obviously issue.

Writer: `captures/research/smb-fuzz/smb_state_fuzz.py` already serves the right state machine; the change
is to build the QUERY_INFO reply body instead of mutating the header.

**Faulting instruction and margin (2026-10-25).** The read is the code-unit load in `utf16_size`,
`ldrh w12, [x8]` at `0x3daa0` in `utf16_to_utf8` (source `unicode.c:202`, `le16toh(*utf16++)`); the
surrogate trail load `ldrh w12, [x8, #0x2]` at `0x3daf4` (`unicode.c:216`) is the second site. With
`name_len = N` the loop reads exactly `N` bytes past `reply+100`, so the smallest over-read is **2 bytes**
(`N = 2`, one code unit). Handing the decoder an iovec shorter than its allocation steers that read onto a
mapped, controlled neighbour: `poc_utf16_margin.c` case A returns `"Z"` read from the neighbour and case D
decodes a full surrogate pair (`f0 90 80 80`) from outside the iovec, both with no ASan report; cases B
and C are the 2-byte minimal margins ASan still catches (`unicode.c:202` and `:216`).

Live on `.86`, rewriting `FileNameLength` to `buflen - 100 + margin`, fresh server per run, 2-3 shots
per run. The margin is a gradient, and the *large* end is the reliable one:

| margin | runs | outcome |
|---|---|---|
| 2 | 1 | 3/3 shots 200, daemon alive |
| 4096 | 1 | 200, alive |
| 8192 | 2 | 200 once, wedged once |
| 16384 | 4 | wedged 3, survived 1 — borderline |
| 24576 / 32768 / 60000 | 4 | **wedged every run on the first shot** |

A wedged run means `/getaa` returns 000 and `/status/zp` stops answering until the offending socket closes;
the daemon recovers on its own. So the deterministic live trigger is `FileNameLength >= buflen + 24576`,
not the 2-byte floor — the floor is the minimal over-read, the large margin is what leaves the mapped heap.

**The leaked name reaches no output (2026-10-26).** The over-read does land in the decoded name, but
nothing on this build emits it. `fstat_cb_1` (`lib/libsmb2.c:1758-1790`) copies only attributes, size and
timestamps out of `struct smb2_file_all_info` and never touches `fs->name`; the client re-opens the file by
the name from the URL, so both CREATEs on the wire carry `folder.jpg` and not the decoded string. Proved by
differential response: a 4096-byte over-read (name ~4096 chars) against a plain control gives **byte-identical
headers and body** (325 B, same `CONTENT-TYPE`/`CONTENT-LENGTH`). Error bodies are empty (404 and 500), no
response header carries a name, and the `/status/anacapa.log` page returns the empty `ZPSupportInfo` XML on
both `.86` and `.202`, so the log page is not a channel either. The over-read is therefore a heap read with
no exfiltration path — it matters for corruption and the wedge, not for disclosure.

**Reply size grooms the fault (2026-10-26).** The over-read walks linearly, so whether it faults is decided
by what follows the reply allocation — and that is steerable from the server side by the reply *size*. With
the minimal 2-byte margin (`name_len = body - 98`) and a padded QUERY_INFO body, a fresh server per run:

| body size | runs | outcome |
|---|---|---|
| 8192, 10240 | 2 | 200 every shot |
| 11264, 12288 | 3 | 11264 wedged 2/2; 12288 wedged once then survived — noisy edge |
| **16384** | 3 | **6/6 shots wedged** |
| 24576, 32768, 49152, 65536 | 4 | wedged every shot |

So at >= 16 KiB the allocation's end abuts unmapped memory and a **2-byte** over-read faults deterministically
— the size is the grooming knob, and the fault no longer depends on the layout lottery that made the small
margins unreliable. `name_len` is a u16, so bodies above ~65.6 KB need `name_len` clamped to 65535 and the
margin grows with the excess.

**The fault is a crash, not a hung thread (2026-10-26).** The daemon is supervised and restarts: after a
groomed fault the transport drops from `PLAYING` to `STOPPED` and `GetMediaInfo`'s `CurrentURI` comes back
empty (it was `x-rincon:RINCON_…` before), i.e. in-memory state is gone. Reproduced on three consecutive
faults; `/status/zp` returns 200 again within ~12 s each time. So a 2-byte heap over-read in a QUERY_INFO
reply is a remote daemon-restart primitive from one unauthenticated `/getaa` request, with the reply size as
the only knob.

**The trail site is not reliably worse (2026-10-26).** Forcing the surrogate branch — `name_len = buflen - 98`
with the last in-bounds unit set to a lone lead surrogate `0xD800` — makes the loop take the trail load at
`0x3daf4` (`unicode.c:216`) two bytes past the body. Across 3 runs at the same 2-byte margin it wedged once
and survived twice, i.e. the same layout dependence as the code-unit site. It is a distinct instruction and
a distinct trigger, but not a stronger one.


### 7d. `mdputil` and `keyval`: shell-only, no HTTP path

Both are **absent from every binary in the 57.23 rootfs**. `grep -l mdputil bin/* usr/sbin/* opt/bin/*` returns
only `usr/sbin/netconfig.sh`; `opt/bin/anacapad` contains neither string. They are invoked exclusively from
shell:

- `usr/sbin/netconfig.sh:42-44` — `MODEL=`/`SUBMODEL=`/`WIFITYPE=` from `/bin/mdputil | /usr/sbin/keyval ^…`
- `usr/sbin/netconfig.sh:69,122-123,135-136,192,209-214,251` and `wifi/wpaconfig:77-78,85-86` — `keyval`
  over `netsettings.txt`, `ssidlist.txt`, `setmac` output and the WPA config
- `keyval` is a `^key` / `-s key` extractor over stdin or a named file; `mdputil` prints the MDP blob

The `/status` table (§1 of `RE_COPY_AUDIT.md`) has no `mdputil` or `keyval` entry either, so **no HTTP path
reaches them**. They are only usable once you already have shell.

The in-process route is different and does exist: `anacapad` imports `hal_flash_get_mdp` (`0x10546190`),
`hal_flash_get_mdp2` (`0x105465a8`), `hal_flash_get_mdp2_wrapper`, `hal_flash_get_mdp_wrapper`,
`sonosMdpSerialToString` and `hal_flash_mdp_write` (`0x10546ec8`) from `libflash.so.1`. The read/modify/write
is one function, `0x1028d1dc` (frame 4 816, strings `MDP flag update`, `/jffs/settings`, `/opt`,
`Incorrect householdId`), called from the network-settings writer `0x1000894c` (`%s/netsettings.txt`). So an
MDP3 read needs code execution in the daemon, not a request.

## 8. In-window sweep restricted to shipped components (2026-09-25 … 2026-10-25)

Window is the 30 days up to today. Only components that are actually in the images are listed; anything
the fleet does not ship is dropped rather than scored.

### 8a. Inventory, from the images (corrects §1)

`57.23-74170-1-16-rootfs.bin` extracted to `/tmp/sq57` (squashfs, uncompressed). Every ELF, with the
version string where one exists:

| component | version | evidence |
|---|---|---|
| busybox | **1.24.2** — *not* v1.00/v1.14.1, those are the 34.16 and play1 trees | `strings bin/busybox` → `BusyBox v1.24.2` |
| wpa_supplicant | **2.8** | `strings wifi/wpa_supplicant` → `wpa_supplicant v2.8` |
| glibc | **2.3.5** | `strings lib/libc.so.6` → `glibc 2.3.5` |
| zlib | **1.2.11**, static in `anacapad` | `strings opt/bin/anacapad` → `1.2.11` |
| mbedTLS | 2.28.x | §7a |
| libmpg123, libsbc, libuuid | unpinned (no version string; sonames only) | `lib/libmpg123.so.0`, `lib/libsbc.so.1`, `lib/libuuid.so.1` |
| udhcpc | separate binary, not a busybox applet | `file sbin/udhcpc` |

Applets actually compiled into busybox (probed): `find grep ifconfig insmod mount ping route telnetd
wget`. **`tar`, `awk`, `gzip`, `unzip`, `dpkg-deb`, `httpd` are absent.**

### 8b. In-window CVEs, scored

| CVE | pub | component | applies? | reachable? |
|---|---|---|---|---|
| CVE-2026-115506 | 10-22 | expat ≤ 2.9.0 | **yes on both lines** — S2 is 2.5.0; S1 is unpinned but carries the guard's own strings (`expat: Accounting(%p)`, `limit on input amplification factor … breached`), so it is ≥ 2.4.0 and affected | **live, DoS.** Entity amplification detected too late in `accountingDiffTolerated()`. Any SOAP/DIDL body reaches it — the wrapper calls `XML_Parse` on the raw body. Measured below. |
| CVE-2026-110712 / -110713 / -110714 | 10-14 | libsmb2 4.0.0 | **yes** | **proven on 4.0.0**, §7c. `/getaa` QUERY_INFO reply. |
| CVE-2026-115505 | 10-22 | libsmb2 4.0.0 | yes | needs `smb2_share_enum_async`; not shown on the `/getaa` path. |
| CVE-2026-92392 | 10-14 | curl (static, unpinned) | unproven | h2 upload UAF; version cannot be pinned (§5a) and no upload site is named. |
| CVE-2026-107695 / -107697 | 10-08 | FFmpeg 5.0.3 | yes | HLS demuxer; §2 already carries the measured self-reference negative. |
| CVE-2026-78807 | 10-08 | wpa_supplicant 2.8 | yes | **local only** (CVSS AV:L) — driver-based PMKSA selection. Not a LAN door. |
| CVE-2026-71971 / -71972 / -71974 | 09-29 | U-Boot | yes as a version matter | **not reachable here.** 71971 is `__net_defragment` (needs `CONFIG_IP_DEFRAG` and a network-booting U-Boot), 71974 is the Android bootmeth partition read, 71972 is the BMP RLE8 splash decoder. Sonos U-Boot does none of the three. |
| CVE-2026-88833 / -88838 / -88842 / -92205 / -90954 / -108119 | 10-13…10-23 | busybox | yes as a version matter | **not reachable.** Every one is `tar`, `dpkg-deb`, `awk` or libarchive; none of those applets is compiled in (§8a). |
| CVE-2026-8674 | 09-17 | glibc DNS stub resolver | **no** — affects 2.26…2.44, we ship 2.3.5 | n/a |
| CVE-2026-58055 | — | nghttp2 | **no** — it is the `nghttpx` proxy, we ship `libnghttp2` | n/a |
| CVE-2026-76844 | 08-24 (just outside) | zlib 1.2.11 | yes | `gz_write()` only; the daemon uses the raw inflate path, not `gzFile`. |
| mpg123 | — | — | nothing in window | n/a |

### 8c. CVE-2026-115506 measured on expat 2.5.0

Built the exact tag (`R_2_5_0`) and drove it the way the daemon does — `XML_ParserCreate(NULL)` then
`XML_Parse(body, len, 1)`, no `XML_SetParamEntityParsing`. General-entity nesting, `multi=100`,
`depth=40`, `count=300`:

| body | result |
|---|---|
| 747 B | error `limit on input amplification factor … breached`, 9.2 ms |
| 15 KB | same error, 6.0 ms |
| 71 KB | same, 9.3 ms |
| 221 KB | same, 26.3 ms |
| 521 KB | same, 38.6 ms |
| 1.02 MB | same, 75.1 ms |

So 2.5.0 **has** the accounting guard (the `expat: Accounting(%p): …` format string is in the S2 binary)
and the bug is that it fires late: the work is done first, then the parse fails. Cost is ~75 ms per MB,
capped near 100× amplification. A 256 KiB body — a size the `/notify` path is already known to accept —
costs ~19 ms, so four concurrent bodies pin all four worker threads. That is a thread-pinning lever that
stacks with the existing `/notify` stall, not a seconds-long hang on its own; the advisory's "tens of
seconds" needs ~100 MB bodies.

Parameter-entity shapes do **not** help: `%aN;` in content parses clean (`ok=1`) in 0.3 ms at 137 KB,
because the expansion never lands in the document.

### 8c-bis. Correction: the daemon's build has NO amplification guard, and billion laughs is 16 s for 502 bytes

§8c measured the accounting guard on a build that had `XML_DTD` **defined**. The daemon's build has it
**undefined**, and every `accountingDiffTolerated()` call site is inside `#ifdef XML_DTD`
(`xmlparse.c` 2649, 2773, 2838, 4044, 4196, 4288, 4495, 4548, 4717, 5652, …). With `XML_DTD` undefined
the guard is compiled out entirely, so the expansion is never rejected — it runs to completion.

Measured, same harness, only the config differs (`XML_DTD` off, `XML_GE`/`XML_NS` on, matching the
binary):

| body | `XML_DTD` on | `XML_DTD` off (daemon) |
|---|---|---|
| 390 B, 6 levels | guard fires | `ok=1`, 154 ms |
| 446 B, 7 levels | guard fires | `ok=1`, 1.6 s |
| 502 B, 8 levels | guard fires, 100 ms | `ok=1`, **15.5 s** |
| 558 B, 9 levels | guard fires | `ok=1`, **156.2 s** |

Each level is exactly ×10 time (154 ms → 1.6 s → 15.5 s → 156 s) and every one of them *completes*, so
the cost is real work, not a reject. A **558-byte** SOAP or DIDL body therefore holds one worker thread
for 156 s, and four concurrent bodies pin all four (`NumThreads 4`) for the same period — well past the
10 s `TimeOut`, so the requests do not even fail fast. A tenth level is ~26 minutes. That is two orders
of magnitude better per byte than the 75 ms/MB amplification path in §8c, and it needs no large body at
all.

Also live in this configuration, and proven: **CVE-class #780** — `XML_ExternalEntityParserCreate(p, NULL,
NULL)` segfaults in `setContext` (`while (*context != '\0')` with `context == NULL`), because with
`XML_DTD` undefined the `if (context)` guard around the `setContext` call is itself inside `#ifdef
XML_DTD`. ASan: `SEGV on unknown address 0x000000000000`, `#0 setContext xmlparse.c:6636`,
`#1 XML_ExternalEntityParserCreate xmlparse.c:1404`. Fixed by `4eeaf49` in 2.6.0. Reachability is
unresolved: it needs the daemon to call `XML_ExternalEntityParserCreate`, which the wrapper
`FUN_1038d8b0` does not.

### 8d. Is there a write in expat 2.5.0? No — settled

Every expat security fix published after 2.5.0 is denial of service or robustness, not a heap write:

| release | CVE | class |
|---|---|---|
| 2.6.0 | CVE-2023-52425 | quadratic runtime on big tokens (DoS) |
| 2.6.0 | CVE-2023-52426 | billion laughs when built without `XML_DTD` (DoS) |
| 2.7.0 | CVE-2024-8176 | stack overflow from chained entities (DoS) |
| 2.7.2 | CVE-2025-59375 | dynamic-memory amplification to OOM |
| 2.8.0 | CVE-2026-41080 | too little SipHash entropy (hash flooding) |
| 2.8.5 | CVE-2026-93990 | high surrogate accepted (damage downstream, not in expat) |

The one in-window CVE that *is* a write, CVE-2026-102633 (integer overflow in `expat_realloc`, 32-bit
only, heap overflow), is version-gated **2.7.2–2.8.5**: the vulnerable `expat_realloc` shape arrived with
the 2.7.2 alloc tracker. 2.5.0 allocates through `MALLOC`/`REALLOC` at each site instead, and each site is
guarded — `poolGrow` bails on `blockSize < 0`, `storeAtts` on
`(unsigned)m_attsSize > (size_t)(-1) / sizeof(ATTRIBUTE)`, `addBinding` on both
`len > INT_MAX - EXPAND_SPARE` and `(unsigned)(len + EXPAND_SPARE) > (size_t)(-1) / sizeof(XML_Char)`.

Measured, not just read: built `R_2_5_0` with `-fsanitize=address,undefined` and drove eight shapes —
note the first pass silently picked up the macOS SDK's `expat_config.h`, which defines `XML_DTD`; the
whole matrix was re-run afterwards with a config matching the binary (`XML_DTD` off, `XML_GE`/`XML_NS`
on) and stayed clean, so the result does not depend on that mistake. Shapes:
200 k attributes on one element, a 4 MiB attribute value, billion laughs, 200 k entity refs inside one
attribute value, 200 k nested elements, 100 k namespace declarations, a 2 MiB namespace URI, and the
`#812` out-of-order-entity-close reproducer with `XML_StopParser` — whole-buffer and chunked at 64, 128,
256, 512, 1024 and 4096 bytes. **Zero ASan/UBSan reports** on every run that finished, including the
slowest: the 4 MiB single token at 64-byte chunks (quadratic, as expected) completed clean, as did
200 k attributes at 64-byte chunks, and the full matrix is clean in the `XML_DTD`-off configuration too.
The only non-clean outcome was the 200 k-attribute case at 256-byte
chunks, which hit a 300 s wall — time, not a fault.

Two corrections to §5b while measuring:

- The daemon's expat is built **without `XML_DTD`**. The string `requested feature requires XML_DTD
  support in Expat` is present and `XML_DTD` occurs exactly once in the whole binary, inside it. So
  parameter entities and external DTD are out, and CVE-2023-52426 (the no-`XML_DTD` billion laughs)
  applies rather than being excluded.
- The wrapper `FUN_1038d8b0` does **not** hand the body over in one call: it splits at UTF-8 character
  boundaries and calls `XML_Parse(parser, p, len, 0)` per segment, carrying a partial multi-byte
  character in `parser+8`. The chunks are still large, so CVE-2023-52425 stays DoS-only, but "a single
  call to XML_Parse" is not literally true.

CVE-2026-77214 (over-read) stays unreachable: the only expat entry point called anywhere is
`0x1041ced4` (`XML_Parse`), six times, and no 3-argument `XML_ParseBuffer` call site exists.

### 8e. The one non-DoS expat fix after 2.5.0, and why it is not on the path

`c4208e7` (2.6.0, issue #812) guards `entityTrackingOnClose` in `processInternalEntity` with
`parser->m_openInternalEntities->entity == entity`, so an entity closed out of order no longer pops the
wrong node off the open-entity stack. It is the only post-2.5.0 fix that touches parser state rather than
just cost.

Reproduced with the upstream regression shape (`<!DOCTYPE t1[<!ENTITY e1 'angle<'><!ENTITY e2 '&e1;'>]><t1>&e2;`,
single-byte feeding, `XML_StopParser` from the character handler): on `R_2_5_0` it parses clean
(`ok=1`), and under `-fsanitize=address` with LeakSanitizer there is **no leak and no ASan report** — the
run just takes ~60 s in LSan's exit scan where the plain build returns instantly. So the wrong-pop is
benign on this shape; it is not a write.

Built the same repro against `R_2_6_0` for comparison: identical behaviour — `ok=1`, no leak, no ASan
report, and the same LSan exit-scan cost (both builds hit a 120 s wall under load). The fix is therefore
not observable on this input at all, which is what a state-consistency fix that never dereferences the
wrong node looks like.

**Not reachable as the daemon is written.** `XML_StopParser` is what makes the parser suspend mid-entity,
and the wrapper `FUN_1038d8b0` never calls it and never checks for a suspended status: it only splits the
body at UTF-8 character boundaries, buffers a partial multi-byte sequence in `parser+8`, and calls
`XML_Parse(parser, p, len, 0)` on each complete prefix, then `XML_Parse(parser, 0, 0, 1)` to finish. No
handler in the daemon stops the parser, so the out-of-order-close path stays dormant.

The six `XML_Parse` call sites are `0x1038d890`, `0x1038d9b0`, `0x1038da3c`, `0x1038dad8`, `0x1038db30`,
`0x1038db78` (file offsets `0x38d890` …), all inside one wrapper, all four-argument. `XML_Parse` itself is
`0x1041ced4`.

Harness: `/tmp/expat250/h.c` (not committed — throwaway, 20 lines).

## expat 2.5.0: is there a write, not just the amplification DoS? (2026-10-25)

Enumerated every expat CVE published since 2.5.0 (2022-10) and filtered for memory classes. **No heap write
exists for 2.5.0 in that set.**

| CVE | class | 2.5.0? | gate |
|---|---|---|---|
| CVE-2026-76641 | OOB **read** (CWE-125), external entities | yes | needs external-entity parsing |
| CVE-2026-4224 | **stack overflow** (CWE-674) on a deeply nested content model | yes | needs a registered `ElementDeclHandler` **and** an inline DTD |
| CVE-2026-102633 | integer overflow in `expat_realloc` (32-bit) | **no** — 2.7.2–2.8.5 only | — |
| CVE-2026-66046 / 2025-59375 | O(N²) `storeAtts`, large allocations | yes | none — already ranked as DoS |
| CVE-2026-93990 | UTF-16 surrogate accepted | yes | UTF-16 input |
| CVE-2026-7210 | hash-flood entropy (Python binding) | n/a here | — |

So the answer to "is there a write" is **no heap write, one stack-exhaustion bug**: CVE-2026-4224. Its gate is
an `ElementDeclHandler`, which the daemon must register — the binary carries `requested feature requires
XML_DTD support in Expat`, so DTD support is compiled in, but whether a handler is registered is unproven
statically (expat is stripped, no `XML_*` symbols). If a handler is registered, an inline DTD with a deeply
nested content model is a pre-auth stack overflow through any SOAP/DIDL body.

## 9. Is there a write in expat 2.5.0, not just the amplification DoS? — no

Short answer: **no heap write.** Every heap-write expat bug is either already fixed by 2.5.0 or needs an
input class this daemon cannot produce. There is one stack-exhaustion bug (CVE-2026-4224) and it is gated
on an `ElementDeclHandler`; see the sibling section above, which owns that question. The amplification DoS
(§8c) remains the only ungated issue.

### 9a. The write-class CVEs all predate 2.5.0

| CVE | class | fixed in | 2.5.0? |
|---|---|---|---|
| CVE-2022-25314 | integer overflow in `copyString` → heap OOB **write** | 2.4.5 | no |
| CVE-2022-25315 | integer overflow in `storeRawNames` → heap OOB **write** | 2.4.5 | no |
| CVE-2022-40674 | use-after-free in `doContent` | 2.4.9 | no |
| CVE-2022-43680 | use-after-free in `XML_ExternalEntityParserCreate` | **2.5.0 itself** | no |

2.5.0 was released on 2022-10-25 *for* CVE-2022-43680 — its own `Changes` entry reads
`#616 #649 #650  CVE-2022-43680 -- Fix heap use-after-free after overeager destruction of a shared DTD`.
So the version string `expat_2.5.0` in the S2 binary is the post-UAF-fix release.

### 9b. The 2.8.2 batch is write-class but out of reach

2.8.2 (2026-06-25) fixed nine integer overflows and one OOB store. All of them are before 2.5.0's
successors and therefore apply to 2.5.0 as a version matter, but each needs an input this daemon cannot
produce:

| CVE | site | what it needs |
|---|---|---|
| CVE-2026-56132 | OOB **store** into `scaffIndex` in `doProlog` | DTD **sharing across parsers** |
| CVE-2026-56403 | integer overflow in `storeAtts` | 2^31 attributes |
| CVE-2026-56404 | integer overflow in `addBinding` | `len == INT_MAX` |
| CVE-2026-56405 | integer overflow in `getAttributeId` | `i == INT_MAX` |
| CVE-2026-56407 | integer overflow in `textLen` handling | `poolLength > INT_MAX` |
| CVE-2026-56408 | integer overflow in `copyString` | > 2 GB |
| **CVE-2026-25210** | integer overflow in the `doContent` tag-buffer reallocation (`bufSize = (bufEnd - buf) * 2`) | > 2 GB tag — patch adds a `SIZE_MAX / 2` guard |

56132 is the only one that is not a 2 GB threshold, and it is the only one worth testing. Upstream's own
regression test (`test_scaff_index_shared_across_external_entity_parser`) needs
`XML_SetExternalEntityRefHandler` **and** `XML_SetElementDeclHandler` **and** a `%e;` parameter entity
that loads an external subset — the sharing is what makes `scaffIndex` and `m_groupSize` diverge.

Measured on the 2.5.0 build: a plain parser with 40, 100 and 1 000 nested `(` in an element content
model parses clean (`ok=1`, ≤ 0.4 ms). The same document after `XML_ParserReset` parses clean. The
`XML_ExternalEntityParserCreate` variant errors out before the model because the daemon-style setup has
no external-entity handler.

Decisive test: upstream's own trigger document (`<!ELEMENT a (((…b…)))>`, `<!ENTITY % e SYSTEM 'ext'>`,
`%e;`, a second deep model) fed to a bare `XML_ParserCreate(NULL)` with **no handlers at all** — the
daemon's configuration — parses clean on 2.5.0 (`ok=1`, 0.4 ms, 342 bytes). Without the handler the
external subset is skipped, `dtdCopy` never runs, and `scaffIndex` is never shared. So 56132 is **not
reachable**.

CVE-2026-72522 (OOB read plus infinite loop in the `*_toUtf16` functions) needs `XML_UNICODE`, i.e. a
16-bit-character build; the daemon is the narrow build.

### 9c. Consequence

The expat surface is a **thread-pinning DoS only** (§8c: ~75 ms per MB, four concurrent bodies pin all
four workers). Nothing in expat gives a *heap* write on this fleet without a > 2 GB body or an
external-entity handler. The one stack bug (CVE-2026-4224) needs an `ElementDeclHandler`; whether the
daemon registers one is still unproven — expat is stripped and carries no `XML_*` symbols, so the
setter block cannot be located by name.

## 10. The stack path (CVE-2026-4224) is closed: the SOAP parser registers no ElementDeclHandler

The sibling section above left this open because expat is stripped. It can be settled from the call shape
without symbols.

`XML_ParserCreate` is `0x1041c124` (called with a NULL encoding). The daemon's SOAP/DIDL XML context is
built by `0x10387150`, which does exactly:

```
10387170  bl   0x1041bb78        ; XML_ParserFree(old)
10387178  bl   0x1041c124        ; XML_ParserCreate(NULL)
10387180  stw  3, 4(31)          ; ctx+4 = parser  (the feeder 0x1038d810 reads ctx+4)
10387184  bl   0x10419edc        ; setter: stores offsets 0/4
10387194  bl   0x10419f34        ; setter: stores offset 60
103871ac  bl   0x10419f20        ; setter: stores offsets 52 and 56
```

Mapped against the `struct XML_ParserStruct` field order (`m_startElementHandler` at 52, `m_endElementHandler`
56, `m_characterDataHandler` 60, … `m_externalEntityRefHandler` 100, `m_skippedEntityHandler` 108,
`m_unknownEncodingHandler` 112, **`m_elementDeclHandler` 116**), that is user data, character data and the
start/end element pair. **Nothing stores to offset 116.**

That is decisive because `doProlog` gates the whole model builder on the handler:

```c
case XML_ROLE_ELEMENT_NAME:
  if (parser->m_elementDeclHandler) {
    ...
    dtd->in_eldecl = XML_TRUE;      /* only set here */
```

With the handler NULL, `in_eldecl` never becomes true, the scaffold is never built and `build_model`
(`xmlparse.c:5573`, the recursive function) is never called. A deeply nested content model is therefore
just a flat scan — which is what the measurements in §9b already showed (1 000 nested `(` parse clean in
0.0 ms).

Residual: a **second** XML consumer exists at `0x103cb870` and its bulk initializer *does* store offset 116
(`0x103cb9b8`), so it registers an ElementDeclHandler. It sits in a stringless module (`0x103cxxxx`) and
the static call-graph walk from the 19 pre-auth handlers does not reach it within five levels — the
handlers dispatch through vtables. If that consumer ever parses a request body, 4224 becomes live there;
until then the SOAP path is closed.

## 11. In-window sweep with named sinks only (2026-09-26 … 2026-10-26)

Window is the 30 days to today. Rule for this pass: a CVE is listed only if it has a **named sink** in the
component's own code. Anything whose sink is a protocol or API the daemon does not use is dropped with the
reason, so the table is short on purpose.

### 11a. What the daemon actually uses (feature filter, measured)

| feature | used? | evidence |
|---|---|---|
| curl ping-pong protocols (FTP, IMAP, SMTP, POP3, TFTP) | **no** | zero hits for `ftp://` `imap://` `smtp://` `pop3://` `tftp://` in `s2_overlay/bin/anacapad` |
| curl SSH / known_hosts | no | same, plus no `libssh` NEEDED |
| curl MIME API | **yes** | `mime.c` strings: `Cannot rewind mime/post data`, `Mime post already completely uploaded`, `mime_read()` |
| curl HTTP/2 | yes | `nghttp2` ×18, `HTTP/2` ×17, `ALPN` ×15 |
| multipart bodies | yes, but **hand-built** | fixed boundary literals `##123456789###BOUNDARY`, `SONOSMULTIPARTBOUNDARY.BLAHBLAHBLAH`, `multipart/%s; boundary=%s` |
| expat DTD | yes | §8c |
| expat ElementDeclHandler | no | §10 |

### 11b. The table

| CVE | pub | sink | verdict |
|---|---|---|---|
| CVE-2026-92392 | 10-14 | `struct Curl_easy` teardown when an **upload** gets `RST_STREAM` (`HTTP_1_1_REQUIRED` / `REFUSED_STREAM`) | **the one live candidate, and the upload half is now proven.** The `POST` literal (`0x664aa4`) is loaded in the same function as the multipart builder (`0x4f0cdc` next to `0x4f0e8a`), `cr_mime_*` wrappers exist (×6) and `nghttp2`/`ALPN` are linked, so the daemon POSTs a mime body over HTTP/2 and a hostile server can answer `RST_STREAM`. Only the version gate (8.12.0–8.22.0) is open, and it cannot be pinned (§5a). |
| CVE-2026-92391 | 10-14 | `curl_mime_free` recursion | dead: needs thousands of *nested* parts; the daemon's mime trees are built in code, not from a request. |
| CVE-2026-92387 | 10-14 | pingpong response loop (`pingpong` area) | dead: no pingpong protocol strings. |
| CVE-2026-92389 / -92388 | 10-14 | TFTP direction confusion / FTP wildcard write callback | dead: no `tftp://`, no `ftp://`. |
| CVE-2026-87864 / -87865 | 10-14 | IMAP APPEND injection / knownhost callback | dead: no `imap://`, no libssh. |
| CVE-2026-107928 / -87866 | 10-14 | GnuTLS WebSocket info leak / wolfSSL pinning | dead: mbedTLS backend, not GnuTLS or wolfSSL. |
| CVE-2026-92390 / -86410 | 10-14 | curl **tool** only (`-J`, SFTP redirect) | dead: we ship libcurl. |
| CVE-2026-107660 | 10-18 | `tls_open()` in `libavformat/tls_mbedtls.c` | live as a MITM primitive: needs an `https://IP/…` media URL. Already ranked in §2. |
| CVE-2026-113351 | 10-20 | `XML_ParserReset` byte-count accounting | dead: 2.7.2–2.9.0 only, and the daemon frees and recreates the parser rather than resetting it. |
| CVE-2026-110712 / -110713 / -110714 | 10-14 | `smb2_decode_file_all_info`, `_fs_volume_info`, `decode_acl` | **proven on 4.0.0**, §7c, reproducer committed. |
| CVE-2026-115505 | 10-22 | `lib/dcerpc.c` NDR counts | needs `smb2_share_enum_async`; no call site found. |
| CVE-2026-115506 | 10-22 | `accountingDiffTolerated` | live DoS, measured §8c. |
| CVE-2026-78807 | 10-08 | driver-based PMKSA selection | local only (AV:L). |
| CVE-2026-71971 / -71972 / -71974 | 09-29 | U-Boot `__net_defragment` / BMP RLE8 / bootmeth | dead: Sonos U-Boot does none of the three. |
| CVE-2026-88833…92205, -108119 | 10-13…10-23 | busybox tar / dpkg-deb / awk / libarchive | dead: none of those applets is compiled in (§8a). |
| CVE-2026-8674 | 09-17 | glibc DNS stub resolver assertion | dead: affects 2.26–2.44, we ship 2.3.5. |
| CVE-2026-58055 | — | `nghttpx` proxy | dead: we ship `libnghttp2`, not the proxy. |

### 11c. Result

Nothing new is reachable. The in-window set reduces to what §7c and §8c already carry: three proven
libsmb2 over-reads and one measured expat DoS. The only unresolved in-window item is CVE-2026-92392, and
it is unresolved for one reason only — the curl version cannot be pinned from a stripped static binary
(§5a). Pinning it needs a live `curl_version_info` or a build date, not more static work.

Re-swept 2026-10-26: nothing new against any shipped component (libsmb2's last entry is still 10-22). The
window is exhausted; the next useful move on this lane is the version pin, not another pass.

## 9. Window 2026-09-26 … 2026-10-26, named sinks only

Swept NVD per component against our exact versions. Reported only where the advisory names a function or
object we can point at in the binary; everything else in the window is behavioural or consumer-side and is
listed as excluded at the end.

### 9a. libsmb2 4.0.0 — three named decoders, all pre-auth via `/getaa`

All three sinks exist at these offsets in `~/sonos-root-local/fw/internals/libsmb2.so.1`
(`libsmb2.so.4.0.0`, sha256 `ba823cc9…`), and all three are reached by a rogue SMB server with no
credentials:

| CVE | named sink | the unchecked use |
|---|---|---|
| CVE-2026-110712 | `smb2_decode_file_all_info` @`0x20ea4` | `FileNameLength` read at `0x20fac`, then `lhz r4,0x1e(r1)` / `addi r3,r3,0x64` / `srwi r4,r4,1` / `bl utf16_to_utf8` at `0x20fb0`–`0x20fc0`. The only length checks in the function are the fixed `cmplwi r9,0x27` @`0x20ec8` and `cmplwi r9,0x3f` @`0x20f10`. |
| CVE-2026-110713 | `smb2_decode_file_fs_volume_info` @`0x21050` | `LabelLength` at `0x210f8`, source fixed at payload+`0x12` @`0x210fc`, `srwi` @`0x21100`, then `utf16_to_utf8`. No bound. |
| CVE-2026-110714 | `decode_acl` (static, called from `smb2_decode_security_descriptor` @`0x21698`) | `acl_size` clamped into `v.len` at `0x21914`/`0x2191c`, then `addi r9,r9,-8` **unconditional** at `0x2194c`; `acl_size < 8` underflows to ~`SIZE_MAX` and every later `smb2_get_uint*` check is dead. The descriptor offset checks are also 32-bit and wrap (`addi r10,r9,8` + `cmplw` at `0x217a0`/`0x217cc`/`0x21838`). |

### 9a-ter. The 110714 advisory understates its own fix, and the pattern space is now closed

Two things worth having before anyone tries to exploit or triage §9a:

**The advisory names one defect; the fix closes nine.** CVE-2026-110714's text mentions only "AclSize below
8". The commit (`523abe20`) also converts every unchecked `v.len -=` in `decode_ace` to a checked
`DEC_VLEN` — seven sites at `lib/smb2-data-security-descriptor.c` 134, 151, 164, 168, 172, 177, 190 — plus
the two in `decode_acl` (258, 275). Those are not independent bugs: they compound, because once the
`acl_size < 8` underflow has made `v.len` ~`SIZE_MAX` at 258, every later `smb2_get_uint*` bound check in
`decode_ace` is dead and the unchecked decrements keep it dead.

**The offset checks are the wrapping form.** `offset_dacl + 8 <= vec->len` (line 341) and
`offset_owner + 2 + SID_ID_AUTH_LEN < vec->len` (line 328) are 32-bit adds; an offset near `0xFFFFFFFF`
wraps, passes, and yields a wild `v.buf`. The fix rewrites them as `offset < vec->len && vec->len -
offset >= N`.

**No fourth `utf16_to_utf8` sink exists.** All seven call sites in 4.0.0 were checked; only the two the
advisories name are unbounded:

| site | bound |
|---|---|
| `smb2-data-file-info.c:158` (`name_len`) | **none** — CVE-2026-110712 |
| `smb2-data-filesystem-info.c:63` (`volume_label_length`) | **none** — CVE-2026-110713 |
| `smb2-cmd-query-directory.c:79` (`name_len`) | `80 + name_len > vec->len` → error |
| `smb2-data-reparse-point.c:80`, `:96` | `suboffset + sublen + 12 > reparse_data_length`, itself checked against `vec->len` |
| `dcerpc.c:887` | inside the NDR decoder, i.e. the 115505 class |

So the three CVEs cover the whole `utf16_to_utf8` pattern space, and `v.len -=` appears nowhere outside
`smb2-data-security-descriptor.c`.

### 9a-bis. Correction: the SMB parser is not in the daemon process

`anacapad` does not link libsmb2 at all — zero `smb2_` dynamic symbols, zero `libsmb2` strings and no
`DT_NEEDED` entry, in **both** builds. So the `/getaa` client lives in a separate binary or library in the
S2 rootfs, and the three decoder sinks in §9a run outside the daemon.

That matters for triage in two directions. It is still fully pre-auth and still reached by a rogue SMB
server, but a fault in the parser does not take `:1400`/`:1443` down — which is consistent with the
round-1 `DataLength` observation (the request stalls, `/status/zp` keeps answering 200, and the daemon
needs no recovery). Conversely the parser process is a *different* target with its own lifetime, and the
three CVEs are DoS against it rather than against the daemon.

Environment note: the local RE tree `~/sonos-root-local` (which held `fw/internals/libsmb2.so.1` and the
`recon/` harnesses) disappeared from the machine during this round. The offsets in §9a are therefore
doc-only until it is restored; the `captures/firmware/` copies of the daemons are unaffected.

### 9b. busybox `udhcpc` — CVE-2026-88833, and it runs here

Named sink: the `udhcpc` raw-packet path. The client replaces the validated packet length with an
attacker-controlled value from the IP header without re-validating the minimum size, underflows when
subtracting header sizes, then copies with a massively oversized length. CVSS 6.5, `AV:A`.

`udhcpc` is live on both lines: `/opt/log/udhcpc.log` is in both binaries (`0x667149` S2, `0x5049c5` S1)
and the S2 daemon reads `/tmp/udhcpc_resp_mac_addr` (`0x67671d`). Trigger is a rogue DHCP server on the
LAN — no auth, no share, nothing on the device. This is the only in-window item whose sink is a copy with
an attacker-derived length.

### 9c. FFmpeg 5.0.3 (S2 only — no `libav*` in the S1 rootfs)

| CVE | named sink | note |
|---|---|---|
| CVE-2026-107678 | `av_encryption_init_info_free()` (`libavutil/encryption_info.c`) via `mov_read_pssh()` | recursive free of the `AVEncryptionInitInfo` list; tens of thousands of small pssh boxes. Needs an MP4 through the demuxer. |
| CVE-2026-107660 | `tls_open()` (`libavformat/tls_mbedtls.c`) | skips hostname checks for IP-address hosts — the mbedTLS TLS path, i.e. ours. |
| CVE-2026-107696 | `ff_rtsp_connect()` | unbounded RTSP 3xx redirect follow. |
| CVE-2026-107698 | `ff_rtsp_connect()` | SSRF: redirect `Location` not validated. |
| CVE-2026-107676 | `av_dynamic_hdr_plus_to_t35()` | up to three uninitialised payload bytes when `tone_mapping_flag` is 0. |

FFmpeg is statically linked in the S2 daemon and absent from S1, so these are S2-only. The sinks are
named but not verifiable at an offset without the S2 rootfs.

### 9d. Excluded, with the reason

- **CVE-2026-102633** (expat, `expat_realloc()` integer overflow, 32-bit) — named sink, but 2.7.2–2.8.5
  only; we ship 2.5.0.
- **CVE-2026-115505** (libsmb2 DCE/RPC NDR) — names a file (`lib/dcerpc.c`), not a function, and `/getaa`
  never calls `smb2_share_enum_async`.
- **CVE-2026-92392** (curl HTTP/2 upload UAF) — names `struct Curl_easy`, an object rather than a
  function; reachable only if the build is ≥ 8.12.0, which the build date makes likely but does not prove.
- All nine zlib hits are consumer-side (vm2, Nicotine+, libsoup, Thrift, hex_core, undici, adm-zip,
  esaml, hackney); no zlib-core CVE in the window.
- mbedTLS and nghttp2: zero results in the window.

## 12. The real expat DoS: quadratic default-attribute collisions (CVE-2025-66382 / CVE-2026-45186)

§8c ranked the amplification bug at ~75 ms/MB. That is the *weak* one. The quadratic attribute-collision
bug is 40× stronger and needs no amplification trick at all.

### 12a. Sink

`defineAttribute`, `xmlparse.c:6383`:

```c
if (value || isId) {
  int i;
  for (i = 0; i < type->nDefaultAtts; i++)          /* linear scan, per attribute */
    if (attId == type->defaultAtts[i].id)
      return 1;
```

Called from `XML_ROLE_IMPLIED_ATTRIBUTE_VALUE` / `XML_ROLE_REQUIRED_ATTRIBUTE_VALUE` (`xmlparse.c:4964`,
`:5005`), gated only on `dtd->keepProcessing` — **no handler needed**, so the daemon's bare
`XML_ParserCreate` + `XML_Parse` reaches it.

The `value || isId` test is the whole trick: an `#IMPLIED` attribute passes `value = NULL` and `isId = 0`,
so the scan is skipped and the cost is linear. A **default value** (`a1 CDATA "v"`) turns it into O(n²).

### 12b. Measured on 2.5.0

`<!DOCTYPE e [<!ELEMENT e EMPTY><!ATTLIST e a1 CDATA "v" a2 CDATA "v" …>]><e/>`

| attributes | body | time |
|---|---|---|
| 2 000 | 31 KB | 1.0 ms |
| 10 000 | 159 KB | 17.2 ms |
| 40 000 | 669 KB | 230.4 ms |
| 150 000 | 2.23 MB | **3 247.5 ms** |

Ratios 13.4× and 14.1× for 4× and 3.75× the input — n², as the code says. Extrapolating: 4 MB ≈ 13 s,
8 MB ≈ 52 s. The advisory's "~2 MiB → 25–100 s" is this shape on slower hardware than this Mac.

Control, same generator with `#IMPLIED` instead of a default: 40 000 attributes = 11.3 ms, i.e. 20× faster
and linear. That is the discriminator, and it is why the earlier "many attributes" probe looked harmless.

### 12c. Status

- **CVE-2025-66382** (published 2025-11-28, "crafted file ~2 MiB → 25–100 s", through 2.7.3) — live on
  2.5.0, sink named, reproduced.
- **CVE-2026-45186** (published 2026-05-10, "attribute name collision checks", before 2.8.1) — the same
  sink; the 2.8.1 fix replaces the linear scan with a `defaultAttsNames` hash table (`PR #1216`).
- Both are **outside the 30-day window**, so they are listed here as pre-window/live, not as window hits.

This supersedes §8c as the expat DoS of record: one 2 MB SOAP body holds a worker for ~3 s on this
hardware, and four of them pin all four threads for the duration.

## 10. Window 2026-09-26 … 2026-10-26, kernel — and the two S1 kernels

The kernel was treated as unreadable. It is not: the S1 kernel image is gzip from offset `0x5779`
(`57.23-74170-1-16-kernel.uImage`), and it decompresses to 1 511 544 bytes.

**There are two different S1 kernels, per model.**

| package | kernel | evidence |
|---|---|---|
| `57.23-74170-1-16` (Play:5 Gen1 class, type-2 records name models 2 and 5) | **Linux 2.4.22-rincon**, built 2026-02-17 | `Linux version 2.4.22-rincon- (secbuild@aws-bionic-sec-82) (gcc version 3.4.4)` |
| `57.23-74170-1-12` (Play:1) | 2.6.35 | `vermagic=2.6.35 mod_unload MIPS32_R2 32BIT` on every `.ko` in that rootfs |

So "the S1 kernel" is not one thing, and 2.4.22 is two years older than 2.6.35. Any kernel CVE has to be
checked against both, and most fail on the older one for lack of the subsystem.

### 10a. In-window kernel CVEs with a named file, and why they do not land

| CVE | named file | verdict |
|---|---|---|
| 98167, 98169, 98170, 98171 | `fs/smb/client/smb2ops.c` | SMB2/3 only. 2.4.22 has **no SMB2 at all** (zero `smb2`/`SMB2` strings) and only the old `fs/cifs` client (`cifs_mount`, `cifs_open`, …); SMB2 support did not land until 3.7. N/A on both. |
| 98168 | `fs/smb/client/cifssmb.c` (`cifs_query_reparse_point`) | reparse points are SMB2-era. N/A. |
| 98182 | `net/mac80211/cfg.c` | the bug is the monitor-with-no-TXQ path (4.19+). N/A. |
| 98233 | `net/packet/af_packet.c` | the bug is in the virtio `VNET_HDR` error path (2.6.22+). N/A on 2.4.22. |
| 98229, 98230, 98231 | `net/xfrm/*` | no `xfrm` strings in the 2.4.22 kernel. N/A. |
| 98234 | `net/sched/sch_hhf.c` | hhf arrived in 2.6.38. N/A. |
| 98246, 98247 | `net/bluetooth/*` | no bluetooth in the kernel. N/A. |
| 98184–98196 | marvell / microchip / intel / ti / broadcom wifi | the device runs the Atheros SDK driver, not these. N/A. |
| **98255** | `net/ipv4/tcp_input.c` (old ACKs in the fast path) | **the only plausible one** — long-standing code that exists in 2.4.22. Unverifiable at an offset: the kernel is fully stripped and carries no function-name strings (`tcp_ack`, `tcp_rcv_established` both absent). |

Net: the kernel contributes no named sink we can currently point at. The useful product of this sweep is
the version map in §10, not a CVE.

**Independent re-run, and it is not deterministic.** Same harness, same host, fresh server per size:
`UNITS 8100 DELTA 1` (body ~16.3 KiB) wedged **1 of 2**; `UNITS 12200 DELTA 1` (~24.5 KiB) wedged **1 of
3**. The surviving shots answer 404 (the share holds a stub JPEG, so a 404 is the normal non-crash
answer). So reply size *raises* the hit rate but does not yet make the fault reliable, and the earlier
"6/6" and "every shot" should be read as best-case. A deterministic crash is the precondition for
exploitation, so this gap matters.

### 5a-bis. Three more attempts (2026-10-26), still null

4. **The wrapper's own error string.** The daemon logs `failed to set options %d %s` around its
   `curl_easy_setopt` wrapper, so the option numbers are in that function. The string is at vaddr
   `0x67b10c` (file `0x27b10c`), but it has **zero** literal-pool references: MIPS16 loads 32-bit
   constants as a `00HH LLLL` pair (verified against a known case, `POST` at `0x664aa4` referenced from
   `0x4f0cdc`), and `00 67 b1 0c` appears nowhere. The string is dead in this build.
5. **Jump-table hunt.** `curl_easy_setopt` would carry a large switch table. The biggest candidate
   (256 entries at `0x6b9128`) is referenced once, from `0x647818`, whose enclosing function has a
   `sltiu a3,224` bound — but 224 does not match curl's `CURLOPTTYPE_LONG` ceiling (326 at 8.12.0, 327
   from 8.16.0), and the MIPS16 decode there is misaligned, so the match is not established.
6. **LONG-option diff.** If the daemon passed an option added in 8.12.0 the version would be bounded
   from below. Diffing `include/curl/curl.h` 8.11.0 against 8.12.0: **no `CURLOPTTYPE_LONG` option was
   added**, so the LONG ceiling cannot separate them.

Net: the pin needs runtime data. `curl_version_info` from a live process, a core, or a build date.

## 13. Is the URL-span overflow (RE_COPY_AUDIT finding 1) a known CVE? — no, and it is not on S2

Two questions the CVE lane can answer for the exploit work.

### 13a. No published CVE covers it

Searched the Sonos vendor CVE set and the 2026 advisories. Everything published against Sonos hardware is
one of: the `mt_7615.ko` WPA2 four-way-handshake stack overflow (CVE-2023-50809), the Era 300 SMB
response OOB (CVE-2026-4149) and heap overflow (CVE-2025-1051), or `sonos/tract` (a Rust ML library that
is not in this firmware). **Nothing covers a URL/path span copy in `anacapad`.**

Consequence: there is no upstream patch to crib the clobber semantics from, and no vendor write-up to
lean on. The overwrite map has to be derived from the frame itself (SapphireIvy's assignment), and the
finding is genuinely unpublished.

### 13b. The same shape is not at the S2 percent-decode sites

`sonosDecodePercentEncodedStr` is imported on S2 (`R_MIPS_JUMP_SLOT` at `0x6f6520`, PLT stub `0x6dc440`)
and has exactly **two** call sites:

| site | call | bound |
|---|---|---|
| `0x4a9cce` | `sonosDecodePercentEncodedStr(str+3, sp+24, 1025, &n)` after a `strncmp(str, …, 3)` | 1 025 into a 1 056-byte gap (frame 1 080) — fits |
| `0x4cc848` | `sonosDecodePercentEncodedStr(src, sp+13056, 2048, &n)` | 2 048 into a 14 640-byte frame — fits |

Both are the *clamped* form. Neither has the S1 pattern (span computed before the clamp, then `memcpy`
of the raw span). So the bug is **S1-only as far as this scan reaches** — the S2 URI machinery exists
(`file://` ×3, `http://` ×17) but its percent-decode path is bounded.

That matters for scoping: the 0-day is a 57.23 bug, and `.202` is the right and only test unit for it.

## 11. Is the URL-span overflow already a CVE? No — and the nearest published analogue

Asked directly now that it is the lead. Two independent sweeps say it is unpublished:

- NVD `keywordSearch=sonos` over twelve months in 90-day chunks (2025-10-01 … 2026-10-26): **two** hits,
  neither about anacapad — CVE-2026-4149 (Era 300, SMB `DataOffset`) and CVE-2026-75093 (the `sonos` Rust
  crate, `tract`).
- The complete vendor list at OpenCVE (`vendor=sonos`, 22 CVEs back to 2020) contains nothing about a URL,
  path, URI or request-line span. The Sonos CVEs are the ZDI audio-codec and SMB series, the 2023
  wireless-driver pair, and the `tract` crate.

So `0x1007b2c4` is a genuine 0-day, not a rediscovery.

Two published neighbours are worth keeping in view:

- **CVE-2023-27355** (8.8, `AV:A`, root) — "MPEG-TS parser … lack of proper validation of the length of
  user-supplied data prior to copying it to a fixed-length stack-based buffer", ZDI-CAN-19773, tested
  against Sonos One `70.3-35220`. Same class as our finding, same product line, and 57.23 is *older* than
  the version the advisory calls affected, so it should still be live. Caveat: the TS demuxer is not in
  `anacapad` (no `mpegts`/`m3u8`/`EXT-X` strings in either build), so on S1 the component is still
  unidentified — the earlier "naive AFC=3/afelen=0xFF shape plays and does not crash" note means the
  *shape* was wrong, not that the bug is absent.
- **CVE-2026-4149** (9.8, Era 300) — the SMB reply `DataOffset` field. Our S2 build already requires it to
  be exactly `0x50` (see `RE_GETAA_DATALENGTH.md`), so that one is fixed on our line; the S1 line has no
  SMB client in the daemon at all.


### 11c. CVE-2023-27355 does not apply to S1 — there is no TS demuxer here

Followed up the "best second target" and it does not survive contact. S1 57.23 has no HLS or MPEG-TS
handling at all:

- `m3u8`, `EXT-X`, `mpegts`, `mp2t`, `transport stream` — **zero** occurrences in `anacapad` and zero
  anywhere in the extracted 57.23 rootfs (`grep -rl` over the whole tree).
- No `audio/*` MIME strings either; the eight `audio` hits are the websocket subprotocol names
  (`v1.api.smartspeaker.audio`) and event names. The daemon hands URLs to the player firmware rather than
  demuxing.
- `/opt/bin` holds only `anacapad` and `anacapactl` — there is no separate media binary to hold a demuxer.

CVE-2023-27355 was tested against a Sonos One, i.e. an S2 device, where FFmpeg does the demuxing. So on the
S1 line it is not reachable, and the earlier "naive AFC=3/afelen=0xFF shape plays and does not crash 57.23"
note was measuring the player's own firmware, not `anacapad`.

Practical read, corrected: the URL-span overflow is the only pre-auth *stack write* we hold, full stop —
unpublished, located, and S1-only. There is no second published target on this line.

### 11a. Affected scope: S1 only — the S2 handler has no span copy at all

Checked the S2 equivalent before anyone spends device time on `.86`. The 86.10 `x-file-cifs` handler is a
small function (prologue `0x4a9c90`, `save 1080,ra,s0-s1`, frame 1080) that does exactly three things:

```
4a9cac  li   a2, 3
4a9cb4  jal  strncmp            ; strncmp(v1, "x-f", 3)
4a9cce  jal  sonosDecodePercentEncodedStr   ; (v1+3, sp+24, 1025)
```

**Zero `memcpy` and zero `strchr` in the whole function.** The URL is decoded straight into a 1025-byte
stack buffer with the clamp honoured, and there is no second, unclamped span copy. So the `0x1007b2c4`
overflow is **57.23 / S1 only**; 86.10 / S2 does not carry that code path.

That is also the shape of the fix: drop the span copy, decode into the bounded buffer. It means the live
`.202` result (404 up to N≈3900, 408/000 from N≈5300) is the whole affected surface, and `.86` is a
control rather than a second target.

### 11b. Finding 2.1 (`0x101cc878`) — unpublished, and no S2 counterpart found

Same CVE question, same answer: the 22-entry Sonos vendor list has nothing about a long value, a title or a
SavedQueues entry being copied into a fixed buffer, so 2.1 is unpublished too.

Checked outside NVD as well, because NVD keyword search can lag a disclosure and ZDI is the CNA behind
every Sonos CVE: ZDI's **published** index (`/advisories/published/`, 2.5 MB) contains exactly one Sonos
entry — the Era 300 SMB one, CVE-2026-4149 — and its **upcoming** index (2.4 MB) contains none. So neither
finding is sitting in a ZDI queue either.

Scope is *probably* S1-only but **unproven**, unlike the URL span. The S2 build has no `memcpy` whose
length is loaded from an object field inside a large frame — the whole binary yields four large-frame
copies with any field- or slot-loaded length, and the closest (0x4ba8ba, `memcpy(sp+32, s0+340, 3276)`
guarded by a flag byte at `336(s0)`) is a fixed size, not a field. So the 2.1 shape does not obviously
exist on S2, but the object layout differs enough between builds that "not found by this search" is not
"absent". Treat `.86` as untested for 2.1 rather than as a control.

## 14. Is there a newer S1 to diff, and is the Play:1 affected? — no, and unproven

Two questions that decide whether the URL-span bug can be scoped by comparison instead of by testing.

### 14a. No newer S1 binary exists to diff

The newest S1 image in the set is `57.23-74170` (both `-1-12` and `-1-16`). Sonos puts legacy players on a
separate version track from current hardware, so there is no later S1 release to pull and diff — which
means there is **no upstream fix to read** for the intended bound. The overwrite map has to come from the
frame (SapphireIvy) and the primitive from the NUL write (FoggyBirch), not from a vendor patch.

### 14b. The Play:1 build has the URI subsystem but a different dispatcher

`57.23-74170-1-12/anacapad` (MIPS, the Play:1 model):

| marker | PPC `-1-16` | MIPS `-1-12` |
|---|---|---|
| `http://` | 17 | 23 |
| `https://` | present | 6 |
| `file://` | **3** | **0** |
| `sonosDecodePercentEncodedStr` | imported | imported (`R_MIPS_JUMP_SLOT` at `0x543220`) |

So the URI machinery is there on both, but the **`file://` branch is PPC-only**. The S1 splitter is reached
from the dispatcher that owns `http://`/`https://`/`file://`, so the Play:1 either has a different
dispatcher or a different set of branches — either way, **do not assume the Play:1 is affected**. It has to
be tested on hardware.

### 14c. Decoder caveat for the MIPS build

`mips-linux-gnu-objdump -m mips:16` misdecodes this binary: `jal` targets print as bare numbers
(`jal 190`), PLT stubs decode as garbage, and the `.rel.plt` table decodes as instructions. Greps for
`li a2,1025` and `-5344` both return zero, so instruction-level claims on `-1-12` need a fixed decoder
first. This is the same misalignment SapphireIvy hit on the S2 binary, and it is why 14b is stated at
string level only.

## 15. Window refresh 2026-09-26 … 2026-10-26 (uwk, 2026-10-27)

Fresh NVD API sweep (`pubStartDate=2026-09-26`, `pubEndDate=2026-10-27`,
keyword per component) against the MEASURED versions — mbedTLS 2.28.x (57.23,
wbz correction), 3.6.6 (S2), libsmb2 4.0.0, busybox 1.24.2 (57.23 rootfs),
expat 2.5.0 (86.10), curl static-unpinned, glibc 2.3.5, wpa_supplicant 2.8,
zlib 1.2.11. Every in-window hit is in the table with its version verdict;
nothing is summarised away silently.

### 15a. Per-version match table

| CVE | pub | component @measured version | version match? | surface-reachable? |
|---|---|---|---|---|
| CVE-2026-110712 / -110713 / -110714 | 10-14 | libsmb2 4.0.0 | **yes** ("through 6.2"; ASan-proven on the 4.0.0 tag, §7c) | **yes, pre-auth** — rogue SMB server via `/getaa`; but runs outside `anacapad` (§9a-bis). Daemon-restart primitive, not RCE. Carried, unchanged. |
| CVE-2026-115505 | 10-22 | libsmb2 4.0.0 | yes | **no** — DCE/RPC NDR needs `smb2_share_enum_async`, no call site on the `/getaa` path. |
| CVE-2026-115506 | 10-22 | expat 2.5.0 (S2); S1 unpinned-but-affected | yes | **yes, DoS only** — measured twice (§8c/§8c-bis); the daemon build has NO guard (`XML_DTD` compiled out), 558-byte body = 156 s of worker time. |
| CVE-2026-102633 | 09-29 | expat 2.5.0 | **no** — 2.7.2–2.8.5 only | n/a |
| CVE-2026-77214 | 10-07 | expat 2.5.0 | yes as version matter | **no** — `XML_ParseBuffer` never called (§5b, settled at instruction level). |
| CVE-2026-113351 | 10-20 | expat 2.5.0 | **no** — 2.7.2–2.9.0, and the daemon frees+recreates rather than resets | n/a |
| CVE-2026-88833 | 10-13 | busybox **udhcpc** — the 57.23 rootfs ships a SEPARATE `sbin/udhcpc` binary (36,592 B PPC, version string `0.9.8`), not the 1.24.2 applet | version of the standalone client unproven against the advisory range (busybox git udhcpc) | **plausible, pre-auth via rogue DHCP server** — carried from §9b; the standalone binary's lineage is the open check. |
| CVE-2026-88829 / -88834 | 10-13/10-14 | busybox httpd | **n/a** — `strings bin/busybox \| grep httpd` = 0 hits on the 57.23 rootfs; no `httpd` applet (§8a re-verified today) | n/a |
| CVE-2026-88838 / -88842 / -90954 / -92205 / -108119 | 10-13…10-21 | busybox mount/dpkg/dpkg-deb/libarchive/tar | **n/a** — none of those applets compiled in (§8a) | n/a |
| CVE-2026-76014 | 08-19 (pre-window) | busybox wget `-T` NULL deref, ≤1.30.1 | version range covers 1.24.2 | **local-only** (`-T` is a CLI argument; the updater's execl line is fixed, not attacker-controlled) |
| CVE-2026-92392 | 10-14 | curl (static, unpinned) | 8.12.0–8.22.0, **unproven** (§5a: six null probes) | upload half now proven (§11b: mime + h2 linked, POST literal); version still unpinnable offline. **The one live unresolved candidate.** |
| CVE-2026-86414 | 10-20 | curl | 7.10.8–8.22.0 (curl.se) | **no** — needs an HTTP proxy in the chain (`CURLOPT_PROXY`); the daemon talks direct to LAN/cloud endpoints. |
| CVE-2026-86415 | 10-20 | curl | 8.21.0–8.22.0 | **no** — Schannel client-cert cache; the build is mbedTLS-backend. |
| CVE-2026-94266 | 10-20 | curl | 7.62.0–8.22.0 | **no** — `CURLINFO_EFFECTIVE_URL` credential leak; the daemon never calls getinfo (strings: no `CURLINFO_`). |
| CVE-2026-86412 | 10-20 | curl | 8.18.0–8.22.0 | **no** — cookie-jar file rewrite; daemon uses no cookie jar. |
| CVE-2026-107928 / -87865 / -92387…-92389 | 10-14/10-20 | curl | various | **no** — GnuTLS/libssh2/FTP/IMAP/TFTP surfaces absent from the binary (§11a feature filter). |
| CVE-2026-88836 | 10-13 | busybox | **rejected upstream** (Red Hat: not a security vulnerability) | n/a |
| mbedTLS | — | 2.28.x (57.23) / 3.6.6 (S2) | **zero in-window CVEs** — NVD keyword sweep 2026-09-26…10-27 returns 0; the advisory index's latest batch is 2026-07 (CVE-2026-50580/50586/50588 etc., fixed in 3.6.6+/2.28 LTS EOL 2025-03: 2.28.10 is the last 2.28 release and predates those fixes — the 57.23 2.28.x lib is 2.28.10-or-never, so the July batch's 2.28 fixes are **already in or the branch never got them**; none is published in-window either way) | n/a |
| nghttp2 | — | libnghttp2 (static, in curl) | **zero in-window CVEs** (CVE-2026-58055 is the nghttpx proxy; CVE-2026-27135 is May, pre-window) | n/a |
| sqlite | — | not in either rootfs as a server surface | zero relevant in-window hits (all 16 are consumers) | n/a |
| glibc | — | 2.3.5 (57.23) | CVE-2026-8674 (DNS search-domain assert) affects 2.26–2.44 | **no** — version out of range (already §8b). |
| kernel | — | 2.4.22-rincon (S1 -1-16) / 2.6.35 (S1 -1-12) | §10 carries the full named-file sweep | no new in-window sink |

### 15b. Sonos advisory watch (SSA page diffed 2026-10-27)

`https://www.sonos.com/en-us/security-advisory` lists exactly five advisories:
SSA-2024-0001, SSA-2024-0002, SSA-2025-0001, SSA-2025-0002, SSA-2026-0001.
**No new SSA in the window.**

**SSA-2026-0001 (2026-03-25, CVE-2026-4149) — folded in, per the bead comment.**
Advisory text (fetched and read today): "All S2 Systems. Affected versions:
All releases prior to Sonos Systems release v17.7 (build 92.0-72171) … A
vulnerability exists in the SMB protocol client implementation within Sonos S2
systems which allows an attacker to force a protocol downgrade to a vulnerable
version … a specially crafted SMB response … potentially leading to arbitrary
code execution. This issue … does not affect Sonos S1 systems."

- Version window: our S2 fleet is **86.10-81100 < 92.0-72171 → inside the
  affected range.** This is the only vendor-published, version-matching,
  pre-auth RCE-class family for hardware we own.
- Surface: the S2 SMB *client* (libsmb2 4.0.0) driven by a rogue server — the
  same `/getaa?u=x-file-cifs://` door as §7c, plus the downgrade angle the
  advisory names. Note the two CVE families (4149 downgrade-RCE; the
  110712/13/14 read set) are distinct bugs in the same client.
- Caveat measured earlier (§11/RE_GETAA_DATALENGTH.md): our S2 build requires
  SMB reply `DataOffset == 0x50` — that specific 4149 shape may already be
  gated in 86.10 even though the version floor is not met. Re-verify on
  `.86`/`.81` before claiming.

### 15c. Verdict

**Zero NEW version-matching, surface-reachable CVEs in 2026-09-26…2026-10-26**
beyond what §7c/§8/§11 already carry (libsmb2 read set, expat DoS, udhcpc
candidate). The in-window delta this pass adds: four new curl advisories
(86412/86414/86415/94266) — all dead on feature filters — and busybox
CVE-2026-88829/88834 (httpd; applet absent). The mbedTLS and nghttp2 windows
are empty. The falsifier ("zero version-matching surface-reachable CVEs") is
**not** met only because SSA-2026-0001/CVE-2026-4149 remains an open
version-matching RCE candidate pending an on-device downgrade-path check —
that is the bead's surviving item, not a new finding.

Sources: NVD API 2.0 keyword sweeps (exact query in the harness history),
curl.se/docs/security.html + per-CVE pages (fetched 2026-10-27),
mbed-tls.readthedocs.io security-advisories index (2026-03 and 2026-07 batches
are the newest), sonos.com/en-us/security-advisory + SSA-2026-0001 page
(fetched 2026-10-27), OpenCVE vendor=sonos (22 entries, newest
CVE-2026-55832 tract, 2026-09 — Rust ML library, not in firmware).

## 8. Window refresh 2026-09-28 … 2026-10-28 (CalmHawk, 2026-10-28)

Full re-sweep of the last 30 days (NVD vendor=Sonos, TheHackerWire tracker, OpenCVE):

- **No new Sonos-vendor CVEs in the window.** The newest vendor entries remain the tract
  Rust CVEs (CVE-2026-75093 `Tensor::from_raw_dt_align`, CVE-2026-55832), published Aug/Sep
  2026 — §"Recent-CVE check" above already settled both as unreachable on 57.23/86.10 (no
  tract runtime in either build; re-check on any 97.x image).
- **CVE-2026-4149 (ZDI-26-192, ZDI-CAN-28345, Era 300, CVSS 10.0, published 2026-04-10)** —
  surfaced in this sweep because the tracker ranks it the fleet's only critical: an
  unvalidated **`DataOffset` in SMB *responses***, memory access past the allocation,
  leveraged to code execution **in kernel context**. Fixed in `83.1-61240` (2025-02-04 per
  ZeroPath; ZDI says "fixed in 83.1-61240"), which is **below both of our builds**
  (57.23-74170, 86.10-81100) — so it is a reproduction target at best, not a live bug.
  Two things it does establish: (1) Sonos-class memory-corruption bugs concentrate in the
  SMB reply parser — exactly where §7c's libsmb2 4.0.0 CVEs (110712/110713/110714/115505)
  sit on our S2 builds; (2) on Era-300-class hardware the SMB handler runs in kernel
  context, so the same bug class yields more than a userspace crash there. Our S2
  libsmb2 is userspace-root (anacapad), and the QUERY_INFO over-read already gives a
  deterministic restart primitive; the missing step on that lane is corruption → control.
- Community prior art unchanged: NCC "Listen Up" (BH-USA 2024) and the Pwn2Own 2024
  trio (§3) remain the only public Sonos exploit literature; no new public Sonos RE
  releases in the window.

Net for the campaign: the 0-day lanes (busybox wget chunked grammar, :1443 JSON parser,
notify/setstring grammar, mbedtls x509, HLS ring, codec diffs) remain the only paths to a
pre-auth corruption primitive on these builds; the freshest *named* bugs stay the four
libsmb2 ones in §7c.
