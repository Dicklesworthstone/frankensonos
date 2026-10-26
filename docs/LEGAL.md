# Is this legal? (short answer: yes, and it is well-trodden)

FrankenSonos reverse-engineers the owner's own hardware for interoperability. That is the most
protected kind of reverse engineering there is, and it is what every Sonos integration before this one
has done.

## The basis

- **Interoperability reverse engineering is lawful in the US.** *Sega v. Accolade* (9th Cir. 1992) and
  *Sony v. Connectix* (9th Cir. 2000) hold that disassembling a lawfully obtained product to learn the
  interface, and writing a new implementation, is fair use. We copy no Sonos code: the deliverable is
  Rust written from observed behaviour.
- **It is expressly lawful in the EU.** Software Directive 2009/24/EC Art. 6 grants decompilation for
  interoperability without the rightsholder's authorisation.
- **The protocol facts are not protectable expression.** SSDP, UPnP, SOAP, GENA and DIDL-Lite are
  published standards; the Sonos-specific parts are short identifiers and URI shapes.
- **The signature gate is the only anti-circumvention wrinkle.** Defeating a technical protection
  measure is DMCA §1201 territory, but §1201 has an interoperability exemption and we are not selling a
  circumvention product. In practice Sonos has never pursued the SoCo / node-sonos / Home Assistant
  lineage for exactly this.

## What we deliberately do *not* redistribute

- **Firmware images.** `.upd` files are fetched and kept locally; the repo holds no vendor binaries.
- **Keys and certificates.** The two update SPKIs and the devmode public key are *public* halves; we
  record their SHA-1 names, not private material.
- **Site data.** No addresses, serials, household ids or tokens in git (see `SCOPE.md`).

## The real risks are operational, not legal

| Risk | Mitigation |
|---|---|
| Bricking a player | read-only probes first; every crash so far self-heals in seconds; no persistent state written without a plan |
| Voiding warranty | only once we actually flash; the goal is our own firmware, so this is accepted |
| Losing a statement/credential | statements are per-serial and replayable — keep a copy per device before touching it |

If a future step needs something genuinely grey (e.g. redistributing a vendor binary), it goes here
first.
