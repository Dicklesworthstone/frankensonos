# eMudhra and firmware: what the compromise buys

Two independent gates guard a firmware install. The Oct 2026 eMudhra DV
intermediate compromise only solves the second one.

1. **Transport.** The update image is fetched over HTTPS from
   `update-firmware.sonos.com`. A server needs a certificate the device
   accepts. Sonos's own key is not available; a forged leaf under the
   compromised `EM DV TLS CA - G3B` chains to `emSign ECC Root CA - G3`,
   which is present in **both** the 57.23 (S1) and 86.10 (S2) root bundles
   (`type 22` payload, checked byte-for-byte). A leaf with
   `SAN=update-firmware.sonos.com` verifies against that root, so the
   image fetch passes.
2. **Package signature.** Type-17 is verified by **two hardcoded Sonos
   keys** in `libsonoscrypto`, not by a public CA, so eMudhra cannot sign a
   package. It does not need to: the type-17 signature covers **only the
   57-byte list region** (`payload[0x10:0x49]`). Keeping the original
   type-17 and type-22 sections leaves the signature valid while the
   rootfs, kernel, and preinstall change freely.

So the combination is

```
forged TLS leaf (eMudhra chain)  -> transport accepted
original type-17/type-22         -> signature accepted
patched squashfs 3.0 rootfs      -> payload
```

and the manifest does not even need a valid signature (57.23 accepts edited
manifests).

## Durability against revocation

The trust anchor lives in the **installed bundle**, so a revocation only
takes effect through a **new firmware that the device accepts**.

- Install first. Once the rootfs is ours, the device no longer has to take
  Sonos updates at all.
- For durability across updates we control, add **our own CA** (or pin our
  host) to the rootfs fallback bundle `/etc/fallback_trusted_roots.rcb`
  and to the type-22 bundle, so the update path stays trusted even if the
  eMudhra roots are later dropped. The bundle header signature is likely
  advisory in the same way the manifest one is; test it before relying on
  it.
- Keep a second way in (local API / LAN) so a bad update is recoverable
  without the cloud.
- If a Sonos revocation build is accepted **before** we install, the
  eMudhra chain may stop being trusted; re-check the root in the new
  bundle before taking such a build.

## Proven pieces

- Chain verifies: `openssl verify -CAfile emroot.pem -untrusted <EM-DV + emSign root TLS>` → `OK`.
- Leaf served with the full chain on `:12443`; `s_client` against the
  device root returns `Verify return code: 0 (ok)`.
- Root DER present in both shipped bundles (S1 `57.23-74170`, S2
  `86.10-80260`).
- The image URL is a directory base; the updater appends `/update.upd`.
  The back half of the flow already fetched a patched package in 206
  chunks once the update timer fired.

## Remaining blocker

The per-device update scheduler (`updsched`) fires on a **900 s** timer
after `BeginSoftwareUpdate` and only builds work when the household has
update targets. Runs have been accepted (`200`) but the timer often
expires without a fetch, so the install has not yet been observed
end-to-end. That is a scheduling/queue issue, not crypto.
