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

## 5a. Why the curl version is unpinnable (three approaches, all null)

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

Short answer: **no reachable write.** Every write-class expat bug is either already fixed by 2.5.0 or
needs an input class this daemon cannot produce. The amplification DoS (§8c) is the whole of it.

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

56132 is the only one that is not a 2 GB threshold, and it is the only one worth testing. Upstream's own
regression test (`test_scaff_index_shared_across_external_entity_parser`) needs
`XML_SetExternalEntityRefHandler` **and** `XML_SetElementDeclHandler` **and** a `%e;` parameter entity
that loads an external subset — the sharing is what makes `scaffIndex` and `m_groupSize` diverge.

Measured on the 2.5.0 build: a plain parser with 40, 100 and 1 000 nested `(` in an element content
model parses clean (`ok=1`, ≤ 0.4 ms). The same document after `XML_ParserReset` parses clean. The
`XML_ExternalEntityParserCreate` variant errors out before the model because the daemon-style setup has
no external-entity handler. The daemon has neither setter (its only expat entry points are the six
`XML_Parse` call sites in §8c), so 56132 is **not reachable**.

CVE-2026-72522 (OOB read plus infinite loop in the `*_toUtf16` functions) needs `XML_UNICODE`, i.e. a
16-bit-character build; the daemon is the narrow build.

### 9c. Consequence

The expat surface is a **thread-pinning DoS only** (§8c: ~75 ms per MB, four concurrent bodies pin all
four workers). Nothing in expat gives a write primitive on this fleet without a > 2 GB body or an
external-entity handler the daemon never installs.
