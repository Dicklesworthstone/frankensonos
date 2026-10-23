# Root runbook — the day a signing key (or a write primitive) lands

Everything here is already proven except the one missing input. Numbers are the exact
offsets/values measured on 57.23-74170 and recorded in `FIRMWARE_PATHS.md`; the local
artifacts are git-ignored under `captures/`.

Read this with `docs/FIRMWARE_PATHS.md` open. Two facts frame it:

- The updater validates the whole image **before** it writes anything or runs the script
  (`bin/upgrade` `0x10007fa0`: fetch → compat → signature → `upgrade_write_sections`
  `0x10008890`, which is what runs `system("/bin/sh /var/upgradescript umountnone")`).
- Every edit that matters is *outside* the signature's control only if you can re-sign, and
  the signature ring cannot carry a key (`ZZ0121`, 33/17/21-byte payloads). So a key is the
  whole game.

## A. With a Sonos signing key for the model

1. Take the running model's `.upd` (for a model-16 Play:5 that is
   `57.23-74170-1-16.upd`, sha256 `e19a7bb2…`), decrypt each `0x886499ca` section with the
   per-model private key (RSA-OAEP(SHA-1) unwrap of `[0x32:0x132]`, AES-128-CBC, IV zero,
   body at `0x13A`), edit `bin/…` or the type-3 script, re-encrypt to the same recipient.
2. Replace the type-3 script with `/jffs/system/Configure.dev` style work: start
   `telnetd -l /bin/sh -p 2323`, write `/tmp/device_unlocked_flag`, then exit 0.
3. Rebuild the type-17 ring over the new content and sign it. Layout: magic `0x621da74d`,
   `u32` total, `u16` version 1, `u32 n<9`, `n` entries of `0x108` bytes
   (`[u32 kind][u32 len][payload]`, kinds 1/2/3 with 33/17/21-byte payloads), then a second
   `u32 m<9` count and its entries.
4. Bump the type-1 version record so the policy gate is satisfied, keep the type-21 patch
   list at `2021_11/2023_01/2023_12/2024_12`, serve it and fire
   `ZoneGroupTopology#BeginSoftwareUpdate`.
5. Watch: full read of the file is the gate passing; a reboot ~20 s later with the version
   changed is the install. `captures/research/re-probe.py --ip <ip> --file <upd>` prints
   that verdict in one line.

## B. Without a key, via a memory primitive

Target order, cheapest first:

1. **HLS segment ring — the one live write primitive.** `SetAVTransportURI` with an
   `hls-radio://` playlist we serve is pre-auth, and any playlist with a forward-advancing
   live window makes the daemon `memcpy` segment entries *backwards* past the array
   (`new_head = 0x10 - (limit - head)`, unchecked) — 1–2 new segments per reload kills in
   7–8 s. The copy is at `0x10192754`, its destination is computed from a constant 16, so
   the goal is to land controlled segment bytes on a structure that matters. It is guarded:
   a backward counter pair and `head == 0` both divert to a reset instead of copying, so the
   ordinary outcome is the one-entry overrun (`dest = base - 0xc48`); forcing
   `limit < head` with `tail >= limit` is what would turn it into a huge signed length, but a
   plain shrink-then-grow window did not reach it (44 s, no death) -- the consumer has to
   actually drain while the window moves. See the `re-hls-kill-cnk` bead for the register
   roles, the branch tree and that negative.
2. Anything else that gives an arbitrary write: the update script is the natural payload
   vehicle once a write exists, because `/jffs/system/Configure.dev` runs as root at boot
   (`rcS` line 20) and also arms dropbear.
3. Failing a write, a full RCE gives the same result directly.

Live entry points that reach a parser **before** verification, on the device (the key is
present there, so these are the only parsers that run):
`POST /setstring` (one request kills the daemon — do not hammer during a run), the type-22
cert bundle, the type-3 script and the type-17 ring. Offline the same parsers cannot be
reached past the bundle without real MDP bytes (`hal_flash_get_mdp2` magic `0xca989b4a` /
`0xfa87b921`, key derived with the `0x4f0`-byte blob at `0x10016ef8`).

## C. Policy levers that work today (no key)

| Lever | Effect | How |
|---|---|---|
| Genuine older image | installs down to the patch-train floor (`57.22-61162`, SSA-2024-0002) | ordinary update, verified by the parallel session |
| `MinVersion: [1.0-1]` in `upgradeinfo` | clears the post-download downgrade verdict | `ExtraOptions` with a real newline: `&#10;MinVersion: [1.0-1]` |
| `Force` (Flags bit 0) | does not override the version gate | — |

The fetch-side abort at exactly `0x20000` is a **separate** gate that `MinVersion` does not
reach.

## D. Turning a 34.16 body into an installable one (format recipe, proven offline)

A genuine 34.16 image aborts on 57.23 because of its trailing `type 0xffffffff` record (the
updater `abort()`s on a mid-stream trailer). Remove only that record, splice a type-21
record (`2021_11_PATCHES:\n2023_01_PATCHES:\n2023_12_PATCHES:\n2024_12_PATCHES:\n`) after the
version record, and inject `MinVersion`; the whole 4.7 MB then parses and compatibility
matches, ending only at `No valid signature`. 34.16 carries no signature section, so those
edits are free — it needs a key or a bypass like anything else.

## E. Safety

- One experiment per unit per minute; `POST /setstring` alone causes a ~80 s daemon outage
  and a repeat can reboot the box.
- A reboot series can drop a unit to its other boot slot, so re-read `/status/VERSION`
  before and after and do not attribute a version change to yourself without checking.
- Nothing here writes flash until the updater decides to; a failed run leaves the version
  untouched.
- An HLS experiment keeps retriggering across restarts. `Pause` with `InstanceID=0` on
  `/MediaRenderer/AVTransport/Control` settles the unit; `Stop` alone did not.
