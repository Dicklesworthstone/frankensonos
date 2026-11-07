# MOR_CASE — Sonos root-shell campaign: fastest path to interactive root on ≥1 unit

## Decision contract
- Primary question: Which single next action most likely yields an interactive root shell on ≥1 Sonos unit, given: no serial/physical, no network-wide DNS changes, no eero Plus disablement?
- Decision owner: Jeffrey (approves LAN-adjacent experiments; owns all units)
- Horizon: days (this week)
- Scope: .84/.202 (S1 PPC 57.23-74170), .135 (S2 86.10-81100); excludes .86/.81
- Success: an authenticated shell session as root (uid=0) on any unit
- Loss function: false "this will work" claims waste evenings; missed cheap path delays the whole firmware endgame; bricking is unacceptable
- Constraints: per-unit LAN only; all firmware packages must be validly signed (prefix byte-identical or malleable-after-type-17); owner has NOT flipped eero DNS; eero Plus filters may be active

## Verified evidence (E-IDs)
- E1: FN-call primitive proven on all 3 units (.aa overflow → dispatcher → FN jump); 8+ reboots on .84, 15 canary hits .202, 12+ hits .135 (ROOT_SWARM_STATE, multiple sessions)
- E2: S1 r3=B at FN call (decode + emu walk); S2 a0-at-call = NOT node0 (post-reboot lifetimes; 6-shape exec matrix all negative: direct/;prefix/beacon/loop/copy/setsid — session 10)
- E3: per-unit BSU(ZoneGroupTopology#BeginSoftwareUpdate URL,Flags,ExtraOptions) works — .84/.202 fetched validly-signed 5.6MB packages in full, twice each (http8899.log)
- E4: trailing type-3 must be ENCRYPTED (886499ca envelope, model-16/12 keys held) — plaintext never executes (UPDATE_FEED_AND_SIGNATURE + DeepSeek session-10 verification); pwned-enc.upd built, verify PASS
- E5: offline verdict harness live (chroot + qemu-ppc, css): 57.23 and 65130 upgrade binaries both run; ALL gates pass except fake-proc hw-set (chroot artifact; real .84 bits ⊆ 0x1e per the 10-22 accepted-65130 probe)
- E6: .84 non-install verdict = NOT the compat check; NOT the fetch gate; the MinVersion/deferred/downgrade decision — invisible without upgrade.log (session 11 chroot replay)
- E7: BSU ExtraOptions LF-injection writes MinVersion into upgradeinfo (measured in chroot) but 'inert in practice: writer templates every key afterwards' (FIRMWARE_PATHS measured)
- E8: genuine 86.11-82170-1-12.upd (S2 upgrade, newer, signed) downloadable from the CDN; staged (DeepSeek session-10)
- E9: /devmode 200 + csrfToken on all 3 units; scripts/devmode_submit.py exists; the signed-statement blob was last REJECTED at the ring stage (dad973e7) and the portal-emulator match is untested end-to-end
- E10: /status/zp exposes <LocalUID> + <HouseholdControlID> pre-auth per unit (xnf iss inputs)
- E11: the S2 exec-shape matrix is complete and all-negative externally (6 shapes × watchdog-reboot pattern)
- E12: the arming chain (DNS+rdr+TLS1.0 responder+launchd) is staged but requires the owner's eero DNS flip (which the owner is reluctant to do network-wide)
- E13: the S2 QEMU daemon boots to the hhid wall (muse-layer registration identity, not MDP2)
- E14: the units' BSU handler = ZoneGroupTopology#BeginSoftwareUpdate — Flags 0/1/2/4 all fetch; none install (session-9 verdict matrix)

## Pivotal unknowns (rank by decision impact)
- U1: What EXACTLY refuses the .84 65130 install post-fetch? (E6 — the single binary answer that unblocks the whole no-DNS S1 path)
- U2: Can the MinVersion line survive the writer's templating on hardware? (E7 — if yes, the downgrade verdict passes)
- U3: Is the S2 child-env failure (E11) an fd/env inheritance issue that setsid+nohup+stdio-detach can bypass — and was that variant correctly fired? (the SETSID fire ran but the beacon never confirmed)
- U4: Will the devmode portal accept a correctly-built statement for these serials? (E9 — zero-exploit shell if yes)

## Transient vs structural
- Structural: the signature boundary (type-17); the malleability foothold; the per-unit BSU mechanism
- Transient: band addresses (shift per lifetime); watchdog timing; eero lease timing
