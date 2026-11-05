#!/usr/bin/env python3
"""PPC 57.23 anacapad: locate system/execve glink stubs."""
import struct

blob = open("captures/firmware/57.23-74170-1-16/anacapad", "rb").read()
rel_off = 0x5ECC
rel_sz = 0x1668
sym_off = 0x1330
str_off = 0x3930
PLT = 0x10545FE8
PLT_SZ = 0x16B0

for j in range(rel_sz // 12):
    off, info, add = struct.unpack(">III", blob[rel_off + j * 12 : rel_off + j * 12 + 12])
    si = info >> 8
    no = struct.unpack(">I", blob[sym_off + si * 16 : sym_off + si * 16 + 4])[0]
    nm = blob[str_off + no : str_off + no + 32].split(b"\0")[0].decode()
    if nm in ("system", "execve", "abort", "exit", "popen"):
        print(f".rel.plt[{j}] {nm}: GOT slot={off:#x}")
        print(f"   glink cand A (plt_end + j*8): {PLT + PLT_SZ + j * 8:#x}")
        print(f"   glink cand B (plt + j*8):     {PLT + j * 8:#x}")
