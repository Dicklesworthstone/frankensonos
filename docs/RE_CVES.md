# RE_CVES — live lane log

One lane of the 2026-10-24 root swarm. Facts with addresses/commands only; merge into FIRMWARE_PATHS.md when the lane closes.

Window: **2026-09-23 … 2026-10-23** (NVD `pubStartDate`/`pubEndDate`). Anything older that still bites a shipped
version is listed separately as *pre-window, live* — it is not in the window but it is in the device.

## 1. What is actually on the devices (version evidence)

| component | S1 (57.23, PPC) | S2 (86.10, MIPS) | evidence |
|---|---|---|---|
| expat | embedded; **no version string in this build at all** (§5b) — the memo's 2.2.0 came from 34.16, not 57.23 | **2.5.0** | `strings anacapad \| grep expat_` → `expat_2.5.0` on 86.10 only; both builds static, no `libexpat` in either rootfs |
| mbedTLS | **2.5.0** (`libmbedtls.so.10`) | **3.6.6** | `captures/firmware/s1_rootfs/lib/libmbedtls.so.10`; `sonos-root-local/fw/internals/libmbedtls.so.3.6.6`, NEEDED `libmbedtls.so.21` |
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
