#!/usr/bin/env python3
"""Build the iss string per the decoded 86.10 composer (FUN_006020f8):

  { owner { luid <[ctx+0x48]> } hhid <[ctx+0x30]> network_hash <hash> } }

The network_hash is provider-computed (fnv1a-accumulator over device state,
0x498882 → 0x634214 → 0x6341dc); until its input trace completes, pass it via
--network-hash. The burned 401 shapes (2026-11-03) confirm the compare target
is exactly this string with a LIVE hash value.

Usage:
  build_iss.py --luid <luid> --hhid <hhid> [--network-hash <hash>]
Prints the iss; use with mint_jwt.py:
  mint_jwt.py "{\"iss\": \"$(build_iss.py ...)\", ...}"
"""
import argparse

def build(luid: str, hhid: str, nh: str) -> str:
    return "{ owner { luid %s } hhid %s network_hash %s } }" % (luid, hhid, nh)

if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--luid", required=True)
    ap.add_argument("--hhid", required=True)
    ap.add_argument("--network-hash", default="")
    a = ap.parse_args()
    print(build(a.luid, a.hhid, a.network_hash))
