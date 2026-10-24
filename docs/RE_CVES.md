# RE_CVES — live lane log

One lane of the 2026-10-24 root swarm. Facts with addresses/commands only; merge into FIRMWARE_PATHS.md when the lane closes.

Window: **2026-09-23 … 2026-10-23** (NVD `pubStartDate`/`pubEndDate`). Anything older that still bites a shipped
version is listed separately as *pre-window, live* — it is not in the window but it is in the device.

## 1. What is actually on the devices (version evidence)

| component | S1 (57.23, PPC) | S2 (86.10, MIPS) | evidence |
|---|---|---|---|
| expat | embedded, version string not emitted (sibling builds: 34.16 = 2.2.0, 86.10 = **2.5.0**) | **2.5.0** | `strings anacapad \| grep expat_` → `expat_2.5.0`, plus `EXPAT_*_DEBUG`; static, no symbol table entries |
| mbedTLS | **2.5.0** (`libmbedtls.so.10`) | **3.6.6** | `captures/firmware/s1_rootfs/lib/libmbedtls.so.10`; `sonos-root-local/fw/internals/libmbedtls.so.3.6.6`, NEEDED `libmbedtls.so.21` |
| libcurl | not used by the daemon | **static in `anacapad`**, mbedTLS backend, HTTP/2 present (`nghttp2` ×18, `HTTP/2` ×17, `ALPN` ×15); **version not emitted** — no bare `8.x.y` string anywhere, `curl_version()` builds it from macros. Any 2026 build is ≥ 8.12.0 (released 2025-02); 8.23.0 only shipped **2026-10-14**, and the fleet's build date for 81100 is unknown | `readelf -d` (no `libcurl` NEEDED); `strings` |
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
| CVE-2026-77214 | 10-07 | heap over-read, `XML_ParseBuffer` trusts caller `len` and walks `m_bufferEnd` past the buffer | **yes** | **only via the parse-buffer API.** Prior read (FIRMWARE_PATHS.md:626) says the daemon uses `XML_Parse` only; `XML_Parse` calls `XML_GetBuffer(len)` first so the invariant holds. Not re-verified in this lane (expat is fully stripped — no `XML_*` in `.symtab`/`.dynsym`, and the version/encoding strings are materialised with `addiu rX,pc,imm`, so literal-pool xref search finds nothing). Impact when reached: disclosure (CVSS 8.2 C:H). |
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
| CVE-2026-107695 | 10-08 | **HLS demuxer infinite loop** — `parse_playlist()` accepts Master Playlist tags in a Media Playlist; a self-referencing playlist adds variants forever in `hls_read_header()` | yes (`before 8.1.3`) | **high** — exactly the `x-rincon-mp3radio://…/x.m3u8` path the hls-kill bead drives. DoS = *hang*, which is a different signature from our crash |
| CVE-2026-107697 | 10-08 | HLS demuxer bypasses protocol/`allowed_extensions` for child playlists | yes | high — a master playlist can make the demuxer open non-media local/protocol targets (file read, extra SSRF) |
| CVE-2026-107660 | 10-08 | `tls_open()` in `libavformat/tls_mbedtls.c` skips hostname checks for **IP-literal** hosts | yes (`before 8.1.3`) | medium — needs an `https://IP/…` media URL; any trusted CA-issued cert then intercepts |
| CVE-2026-96611 | 09-23 | signed overflow on HEIF `ispe` w/h in `mov.c` | yes (before 9.0) | low on this fleet (MOV/HEIF only, not the M3U8 path) |
| CVE-2026-107677 / -107678 | 10-08 | DASH infinite loop; recursive free stack exhaustion in `av_encryption_init_info_free()` | yes (through 9.0.2) | low — DASH demuxer and encryption-info only |
| CVE-2026-107696 / -107698 | 10-08 | RTSP 3xx redirect with no limit; RTSP redirect SSRF | yes | low unless `rtsp://` URIs are accepted |

### libcurl (static, HTTP/2 present)

| CVE | pub | what | reachable? |
|---|---|---|---|
| CVE-2026-92392 | 10-14 | UAF of `struct Curl_easy` when an **upload** gets `RST_STREAM` (`HTTP_1_1_REQUIRED` / `REFUSED_STREAM`). Affected 8.12.0–8.22.0 (fixed 8.23.0, released the same day; 8.16.2/8.20.2 in their branches). RCE needs a fully known build/layout — ours is static and known | **conditional on an upload path only**: the version half is close to certain (any 2026 build is ≥ 8.12.0, and 8.23.0 shipped on the CVE's own release day), but nothing shows the daemon *uploading* with a body to a URL we can point at our own h2 server. Do not rank it above the DoS items until an upload call site is named. |

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
7. **CVE-2026-77214** (expat over-read) — gated on `XML_ParseBuffer`, which we believe is unused; disclosure only.
8. **CVE-2026-92392** (curl h2 upload UAF) — version is very likely in range; the missing half is an upload path with a body.
9. **busybox / kernel** — no applicable in-window issue.

### Measured on 86.10 (2026-10-23): the self-referencing playlist does not hang

Served three shapes from a LAN host to `.84` (86.10) as `x-rincon-mp3radio://<ip>/<file>` and counted the
demuxer's fetches:

| shape | fetches |
|---|---|
| master playlist, one variant pointing at itself | 4 in 10 s, then idle |
| media playlist (EXTINF + segment) with an embedded STREAM-INF variant pointing at itself | 15 in 12 s, then ~1 reload / 5 s (normal HLS polling) |
| master playlist, eight variants all pointing at itself | 27 in 10 s |

So CVE-2026-107695's *outcome* (unbounded variant growth) is not reproduced by naive self-reference on this
build: recursion is depth-bounded and the fetch profile stays linear. The demuxer still opened and played
normally each time. Whether 5.0.3 has the bug under a nastier shape (distinct URLs per level, or a master
inside a media playlist that also carries a segment) is open — this is a starting negative, not a refutation.

## 5. Open measurements

- name an HTTP/2 **upload** call site in `anacapad` (the version is not recoverable as a literal; 8.23.0 only exists from 2026-10-14, so treat the binary as affected until proven otherwise);
- decide the expat API question for certain (`XML_Parse` vs `XML_ParseBuffer`) — or just test 77214 live;
- identify the FFmpeg consumer binary and whether `allowed_extensions`/protocol whitelist is configured (decides 107697);
- S2 busybox version and S2 kernel version (both missing from the captured artifacts).
