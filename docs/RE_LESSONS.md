# RE lessons — how to probe these players without wasting a round

Operational method notes from the live RE lane. Facts about the protocols live
in [`PROTOCOL.md`](PROTOCOL.md); this file is about *how* to get answers
without breaking the house or fooling yourself. No site data here: addresses
are written as `192.168.4.x`-style placeholders and uuids as `RINCON_…`.

## Probe discipline

- **Pick the least critical player that has the service.** Bridges for
  headers, a lone Play:5 for transport. Say which device class a result came
  from — "anacapad" behaviour is not uniform across classes (a bridge wedges
  on a SUBSCRIBE that a Play:5 answers instantly).
- **Know the reset vocabulary.** A connection reset with the listener still
  answering is a per-connection crash; a dead port plus dead ICMP is a reboot;
  a closed port that pings is a dead daemon. Record which one happened.
- **Devmode is rate-limited** to roughly one POST per four seconds and the
  limiter answers **400**, which looks exactly like a parse failure. Space
  posts ≥5.5 s, and treat *reset* as the signal, not the status code. Re-run
  any 400/403 that matters at least twice.
- **Body limits differ per path.** `modZPHandleRequestOfType` users
  (`/advconfig`, `/devmode`) refuse ≥0x800 with 413; SOAP bodies stream and
  38 KB / 4800 parameters are fine. A 413 on a crafted statement means the
  case is too large for the *handler*, not that the parser rejected it.
- **Read the status line's actor.** A `400`/`403` from `/devmode` is the
  handler speaking; a bare 404 with no body from `/msprox` is a *handler*
  404, while the same path without a query answers 405 from a different
  gate. Different codes from the same URL mean two different matchers.
- **Never flash to answer a question.** `BeginSoftwareUpdate` against a served
  file costs one reboot; the version/user-agent/abort behaviour is in
  `PROTOCOL.md` §9 and a served file that fails the gate leaves the device
  unharmed.
- **Leave the house quiet**: volume to the floor, transport stopped, and say
  so. Note any queue clearing — `RemoveAllTracksFromQueue` is destructive to
  the owner's queue.

## Static work discipline

- **Check rs, not the annotation.** `lw gp,16(sp)` in
  `modZPHandleRequestOfType` disassembles with an `arg_10h` label and reads
  `s8`, not `sp`; a wrong register reading produced a phantom gp-poisoning
  theory for an hour. Decode `rs` yourself on any load/store that matters.
- **Only one caller is a finding.** `fcn.0x4ebc30` looked like a bogus bound
  until the xref list showed a single caller that uses it as a zero-guard; the
  real copy is a different function (`0x4ebc4c`) with its own check.
- **A readable build is a lower bound.** 34.16 is the only readable anacapad.
  A crash that 34.16 survives can be a 57.23 regression (the devmode overrun
  is); state the build a finding was proven on.
- **Expat version by behaviour, not by string.** A 729× internal-entity
  expansion accepted with `200` proves no amplification limit (pre-2.4.9)
  without trusting a string; the compiled `expat_2.2.0` marker only confirms
  the base image.
- **A binary that computes `gp` is relocated: apply a load bias before
  trusting vaddrs.** `bin/upgrade` opens with `bltzal`/`lui gp,0xfc1` and
  every data access is `lw ...,off(gp)`. Hunting string xrefs by
  `lui`+`addiu` at the ELF vaddr finds nothing, because the runtime address
  is base+offset with a base the linker sets at load. Read the prologue,
  derive the base, then search; a literal-address search on such a file is a
  guaranteed false negative, not evidence of absence.

- **Component presence first.** Fresh NVD hits are worthless without the
  component: no CIFS/SMB client, no use of pupnp, and `0` bytes on a TLS
  port was read as "no TLS client" — wrongly: the fetcher *does* speak
  TLS 1.0 and pulls a full 5.4 MB over it; the empty answer was the
  server's OpenSSL 4 refusing that version. Probe the component with a
  server it can actually negotiate (empty `/status/proc/fs/cifs/DebugData`
  is a real absence) before reading CVE text.

## Where the edges actually are

- **SOAP/XML is hard to move**: 1850-byte arguments, 38 KB bodies, 4800
  parameters, 500-deep nesting, 1 KB names and CDATA all survive; the
  decoder and out-parameter copy clamp to their capacities.
- **Queue metadata is the soft spot**: a multi-KB `dc:title` in
  `AddURIToQueue` kills the daemon 72–97 s later. It is state-dependent —
  always run a cleared-queue control before calling it an overflow.
- **Devmode ring keys from 0x5B0 up reset the connection** on 57.23 and the
  fault is content-dependent; `hash_alg`/`siglen` are read past the copied
  statement. 34.16 returns `0` for the same bytes.
- **GENA**: `/msprox`-style guessing fails; subscribe from a Play:5 or an S2,
  not a bridge. Callback URLs over ~1022 chars are rejected 412.

## If stuck, these are unexplored

- The `submit`/`button=delete` branch of `/devmode`, and `/unlock`'s
  challenge replay with a captured `Challenge:` value.
- `/regcert` and `/device_account*` once unlocked — 403 is a flag gate, not a
  missing handler.
- GENA as a *publisher*: make a player discover a rogue device over SSDP and
  subscribe to it, then feed the notification parser.
- `SetString`/`SetZoneAttributes` value semantics (0x400 locals, exact-fit
  copies) rather than envelope structure.
