# A1 — Deduction + abduction: what exactly refuses the .84 65130 install after the full 5.6 MB fetch?

Date: 2026-11-07. Inputs: `docs/UPDATE_FEED_AND_SIGNATURE.md` (downgrade-floor, MinVersion, malleability sections), `docs/FIRMWARE_PATHS.md` (BSU/upgradeinfo/update-gate sections), q57 bead notes (SESSION-8/9 verdict matrix). Unit tags only (.84/.202/.164/.185), no site data.

Premises taken as given (operator + bead notes):
- P1: the 5.6 MB fetch completes, twice (`.84` x2, `.202` x3, Flags 0/1/2/4 all fetch).
- P2: compat check passes on real hardware (package feature bits 0x1e ⊆ unit bits; chroot failure was the fake-/proc artifact only).
- P3: the type-21 patch-series floor matches (4 entries, `…2024_12` — the same list that made genuine 57.22-65130/67080 install on 57.23 units).
- P4: the MinVersion LF/`&#10;` injection was measured working in the chroot, but "the writer templates every key afterwards" on hardware.
- P5: outcome at .84/.202: full fetch + no install + no reboot + version unchanged.

RESULT_STATUS: MATERIAL_INCREMENT

---

## 1. The gate chain between "fetch complete" and "flash write", in execution order

Two layers, because the documented architecture is two-phase: the daemon layer arms a file, `/etc/runanacapa` spawns `/bin/upgrade` on the next daemon start, and the updater does fetch → parse → compat → signature → write. Which layer performs the observed fetch is the pivotal ambiguity (§3).

### Layer D — daemon (`anacapad`), before the updater runs

| # | Gate | Mechanism (documented) | Armed on .84? | MinVersion reach |
|---|------|------------------------|---------------|------------------|
| D1 | BSU writes `/var/run/upgradeinfo` | `BeginSoftwareUpdate(URL, Flags, ExtraOptions)` writes the file; `ExtraOptions` verbatim (UPD:327-331). Contradicted by the 10-22 "not this action" reading (FP:1290s); the later "Update-metering facts" section and q57 practice side with D1 being real | Yes (implied by P1 if the updater fetched; unproven if the daemon fetched) | Delivered here (verbatim append) |
| D2 | Deferred shutdown → `runanacapa` | Accepted BSU → daemon drops ~3 s later; `/etc/runanacapa` runs; **no box reboot is involved** in this flow (FP:1043-1046, controlled on both generations) | Yes | n/a |
| D3 | `runanacapa` guard `[ -f /var/run/upgradeinfo ]` | File must exist at daemon start; `/var/run` is tmpfs → same-boot constraint (FP:1064-1066) | Unknown (see §3) | n/a |
| D4 | Writer rewrite `FUN_1029c34c` | Periodic worker (~10 000-unit delta cadence), synchronous HTTPS cloud settings query; on success re-opens the file `"w"` and re-templates Force/Ignore/Report/CurrentVersion/SystemVersion/URL — **MinVersion/PrevSWGenMinVersion/OriginClient are not in the template** (FP:1049-1062; UPD:330-331) | Runs, but see the race refinement in §4 | **Clobbers** the injection if its tick lands between D1 and the updater's read |

### Layer U — `/bin/upgrade` (root), after spawn — order from the decrypted 57.23 build (`0x10007fa0`, FP:706-717)

| # | Gate | Evidence status | Armed on .84? | MinVersion reach |
|---|------|-----------------|---------------|------------------|
| U1 | Read `upgradeinfo` key table (`0x100178c4..f4`), last-wins | Documented key list incl. MinVersion/PrevSWGenMinVersion/OriginClient (FP:850-855) | Presumed yes | **Read here** — the only place the injected line can act |
| U2 | `wget_fork` fetch from URL | The observed 5.6 MB full reads | **Yes** (P1) | n/a |
| U3 | Record-stream parse | Trailing type-3 after type-17 parses and reaches the normal `(48)` stop (VM-measured, UPD:996-1000); 128 KB per-record reassembly cap not hit (small trailer) | Yes | n/a |
| U4 | Per-record compat: type-1 version band (MinCompatible 56.0), type-2 model/submodel, hardware feature set | P2: passes on hardware | **Yes** | No |
| U4b | Type-21 patch-series floor vs compiled `PATCHES` list → `Upgrade blocked.` **error 10** class | P3: matches (4 entries) — this is the gate the 59130-vs-65130 experiment isolated (FP:1189-1201) | **Yes** | No — error 10 is the patch-floor verdict; MinVersion clears **error 9**, not error 10 |
| U4c | Downgrade policy: version < current → `allow_policy_bypass()` (`/tmp/device_unlocked_flag` or `/jffs/VERSION`), else `Downgrade blocked.` **error 9**; 57.23 variant strings `…deactivated_state` / `…deactivation_pending_state.json` | Bypass consulted at exactly two sites (FP:723-727); **genuine 65130 installed on 57.23 units with no injection** (10-22, FP:1195-1196) ⇒ error 9 does not fire for 65130 | Yes (no bypass file needed) | **Yes — the only gate it reaches** (chroot-proven, FP:860-867). For 65130 it is moot (see §4) |
| U5 | Signature: type-22 rcbundle parse; type-17 SHA-256(prefix) + RSA verify | Prefix byte-identical to genuine 65130 ⇒ verify PASS (chroot: verify PASS with the 65130 keyring) | **Yes** | No |
| U6 | **Post-signature package-state check — the trailing-record handling.** After type-17 verifies, the walker/flasher state machine must accept the stream state. The second `allow_policy_bypass` consultation site sits exactly before the `Invalid update state` refusal (FP:725-727). Open question on record since 10-23: "whether the flasher honours a trailing type-3/4" (UPD:1002-1006) | **Un-armed — no key, flag, or injection reaches it** | **No** | **No — unreachable by ExtraOptions** |
| U7 | `upgrade_write_sections`: type-3 script → `/var/upgradescript`, `system("/bin/sh /var/upgradescript umountnone")`, abort on non-zero | Documented order (FP:714-717) | Not reached (no install) | No |
| U8 | `reprogram_flash` | The flash write itself | Not reached | No |

Side-exit: `/jffs/VERSION` present ⇒ "debug build" no-op write (validates, skips flash). Not applicable to .84.

---

## 2. Eliminative pass — which gates can still be failing?

Given P1–P5:

- U2/U3/U4/U4b/U5: **passed** (P1, P2, P3, byte-identical prefix). Not candidates.
- U4c: **passed without injection** — the 10-22 genuine-65130 install on 57.23 units is the control; error 9 does not fire for a floor-matching 65130. Not the blocker (and see §4: the injection could not have helped even if it did fire).
- U7/U8: **cannot be the refusal point.** If control reached script dispatch, then either the flasher takes first type-3 (ours ignored ⇒ genuine install proceeds ⇒ contradicts P5) or last type-3 (our payload becomes the script; a printf-only script exits 0 under `umountnone` ⇒ flash proceeds ⇒ contradicts P5). Since neither happened, control never reached U7. The refusal is **at or before the U6 state check**.
- D2: **passed by construction** — the flow "no box reboot" is the documented signature of exactly this deferred-shutdown path (FP:1043-1046); P5's "no reboot" matches it, it does not contradict it.
- D3: **alive as an alternative** only under the reading that the daemon, not the updater, performed the fetch (§3).
- **U6 is the surviving gate**: it is the only step between the verified signature and the script dispatch that (a) can refuse cleanly, (b) produces no flash, no reboot, no version change, and (c) has zero armed inputs — no ExtraOptions key, no Flags bit, no bypass file.

---

## 3. The pivotal ambiguity: who performed the 5.6 MB fetch

The q57 notes attribute the fetches to "the unit's own root updater (http8899.log)". If that attribution is right, D3 is armed (the updater only runs after the guard) and U6 is the blocker — the main line above. If the daemon fetched (its fetcher appends `cmaj/cmin/.../householdid` with a `Wget` UA; `wget_fork`'s shape is similar), then D3 never armed and the whole U-layer is unreachable — the arm gate (writer cloud-predicate + ~3 s shutdown window vs ~10 s tick) becomes the blocker instead.

**Zero-device discriminator already on disk:** `http8899.log` — compare the request's UA and query-string parameter set against the two documented fetcher shapes (daemon: `Wget` UA + the 9-param set, FP:1206-1208; updater `wget_fork`: its own param/UA shape from the decrypted binary). This single log read arbitrates H1 (arm gate) vs H2 (U6 state gate) without touching a device.

---

## 4. What the MinVersion line actually reaches (atomic refinements)

- A1. The injected `MinVersion` reaches exactly one verdict: the post-download **error 9** downgrade policy inside `/bin/upgrade` (U4c), read via the U1 key table. Chroot-proven (FP:860-867).
- A2. It does **not** reach the fetch-side sniff gate (measured: same image, same injection, still cut at `0x20000` — two distinct version gates, FP:864-867).
- A3. It does **not** reach U6 (the `Invalid update state` branch) — different refusal site, no key maps to it; `PrevSWGenMinVersion`/`OriginClient` are parsed but unmapped to any behaviour (FP:867-868).
- A4. On hardware the writer's template rewrite (`FUN_1029c34c`) erases the injection **only if its tick lands inside the BSU→read window (~3 s shutdown vs ~10 s cadence)** — the clobber is a race the injection wins ~70 % of the time, not a certainty. The blanket "inert in practice" (UPD:331) overstates it.
- A5. **The injection is strategically moot pre-key**: floor-passing packages (65130) don't need it (error 9 doesn't fire — 10-22 control), and floor-failing packages need a type-21 edit that breaks the signature (FP:1201-1204). Its only future value is post-key or together with a bypass file (`/tmp/device_unlocked_flag`, `/jffs/VERSION` — both require the file-write primitive this campaign is hunting).
- A6. Injection encoding is build-dependent: bare LF is normalised to a space on 57.23 (use the `&#10;` character-reference form, FP:855-859); the S1 34.16-era note records a literal newline working (UPD:329).

---

## 5. Abduction — the single un-armed gate

**The post-signature package-state check (U6): the trailing type-3 record appended after the type-17 signature flips the updater's stream-state machine into its clean refusal branch — the `Invalid update state` class, the second of exactly two `allow_policy_bypass` consultation sites — before `upgrade_write_sections` is ever reached.**

Why it uniquely fits all of P1–P5:

1. It is the **only delta** between the known-good control (genuine 65130 installs on 57.23 units, 10-22) and the .84/.202 failures (same bytes plus the trailing record).
2. A clean refusal at U6 produces precisely the observed outcome: full fetch, no section written, no script run, **no box reboot** (the controlled BSU flow's failure mode is a daemon-level restart only — FP:1043-1046), version unchanged.
3. Every gate that could otherwise refuse has independent evidence of passing (P2, P3, prefix-identical signature, error-9 control).
4. Nothing in our kit arms it: the waiver for that branch is `allow_policy_bypass` = a bypass **file** (`/tmp/device_unlocked_flag` or `/jffs/VERSION`), and the writer-inventory audit proves no shipped pre-auth surface can create either (FP:890-905). It is un-armed in the strict sense: the same primitive that ends this campaign (one arbitrary-content file write) is what arms it.
5. The eliminative argument in §2 rules out everything downstream of it.

It also explains why the chroot harness never saw it: the chroot verdict for pwned-65130 stopped at the hardware-feature artifact **before** the signature stage — the state check lives after the signature, which the chroot run with fake /proc never reached. The "EITHER MinVersion-delivery OR trailing-type-3" fork in the q57 notes collapses: the MinVersion horn is empty (A1–A5), leaving the trailing-record state gate as the sole survivor.

---

## 6. Atomic claims (each independently checkable)

- C1. Between signature verification and script dispatch there exists a package-state gate whose refusal text is `Invalid update state` and whose only documented waiver is `allow_policy_bypass` (FP:723-727).
- C2. `allow_policy_bypass` returns true only for `/tmp/device_unlocked_flag` existing or a 13-char `/jffs/...` path compare (`/jffs/VERSION`); both require a file-write primitive no pre-auth surface provides (FP:727-731, 890-905).
- C3. A trailing type-3 after type-17 parses to the normal `(48)` stop offline (UPD:996-1000) — parse-level acceptance is proven; write-level semantics are the open question on record since 10-23 (UPD:1002-1006).
- C4. Genuine 57.22-65130 installed over the air on 57.23 model-16 units with no MinVersion injection (10-22 control, FP:1195-1196) — therefore error 9 does not fire for 65130 and U4c is not the .84 blocker.
- C5. The controlled BSU→deferred-shutdown→`runanacapa` flow involves **no box reboot** on either generation (FP:1043-1046) — "no reboot" in P5 is consistent with the updater running and refusing cleanly, and does not imply the updater never ran.
- C6. The 10-22 inference "empty `/status/tmp/upgrade.log` ⇒ the `[ -f ]` guard is false ⇒ never armed" (FP:1046-1048) is **unsound as measured**: the same doc records that `/status` does not follow the `/tmp` symlink reliably and returns empty documents for existing files (FP:749-752). The empty-log reads cannot discriminate armed-vs-not. This reopens the D3 question and is why §3's log discriminator matters.
- C7. The writer clobber of injected keys is a race (tick cadence ~10 s vs ~3 s shutdown window), not a certainty (A4).
- C8. Same-version packages take a documented silent "nothing to do" short-circuit (q57 SESSION-8) — the same clean, no-reboot, no-log shape as the abducted refusal; shape evidence alone cannot separate them, the byte-level control in §7.2 can.

## 7. Kill-tests (ordered, cheapest first, no new tooling)

1. **Read `http8899.log`** UA + query params → arbitrates who fetched (H1 arm gate vs H2 U6 state gate). Zero device work.
2. **Serve byte-exact genuine 65130 (no trailing record) to .84 once.** Install expected (it is the 10-22 control). If it also fails on .84, the gate is unit-state-side (deactivation-policy variant of U4c — `Downgrade blocked (deactivated).`, FP:728-731 — or the D3 arm), not the trailing record; if it installs, U6 is confirmed as the blocker.
3. **Trailing-record type A/B:** same prefix + a trailing duplicate **type-1** vs the trailing **type-3**. A type-specific refusal maps the state check; an any-trailing refusal confirms C1's state machine fires on stream shape, not record semantics.
4. Only if 2 implicates unit state: read the deactivation/policy state via `/support/review` (pre-auth, FP:1133-1135) on .84 vs a bench unit.

## 8. Consequence for the campaign

If U6 is confirmed: the trailing-type-3 delivery vehicle dies **as an installer**, and the campaign's payload vehicle moves to what C2 already names — the first arbitrary-content file write (the q57 Configure.dev route) is simultaneously the shell AND the U6 bypass (`/tmp/device_unlocked_flag`), i.e. the upgrade lane and the shell lane converge on one primitive. The malleability foothold (bytes after type-17 are unhashed, UPD:989-1006) remains valid for **parser** work; it is the *flasher state machine*, not the hash, that refuses the installer role.
