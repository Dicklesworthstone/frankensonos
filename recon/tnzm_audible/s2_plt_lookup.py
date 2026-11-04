#!/usr/bin/env python3
"""Locate system/execve PLT stubs in the S2/MIPS anacapad (86.10)."""
import struct
import re
import subprocess

ELF = "captures/firmware/s2_overlay/bin/anacapad"
BASE = 0x400000

blob = open(ELF, "rb").read()

out = subprocess.run(
    ["mips-linux-gnu-readelf", "-S", ELF], capture_output=True, text=True
).stdout
secs = {}
for line in out.splitlines():
    m = re.match(r"\s*\[\s*(\d+)\]\s+(\S+)\s+(\S+)\s+([0-9a-f]+)\s+([0-9a-f]+)\s+([0-9a-f]+)", line)
    if m:
        secs[m.group(2)] = (int(m.group(4), 16), int(m.group(6), 16))

rel_off, rel_sz = secs[".rel.plt"]
dsym_off, _ = secs[".dynsym"]
dstr_off, _ = secs[".dynstr"]
plt_va, plt_sz = secs[".plt"]


def sym_name(idx: int) -> str:
    off = dsym_off + idx * 16
    st_name = struct.unpack(">I", blob[off : off + 4])[0]
    return blob[dstr_off + st_name : dstr_off + st_name + 32].split(b"\0")[0].decode()


for j in range(rel_sz // 8):
    off, info = struct.unpack(">II", blob[rel_off + j * 8 : rel_off + j * 8 + 8])
    si = info >> 8
    nm = sym_name(si)
    if nm in ("system", "execve", "popen", "fork", "execl"):
        # o32 .plt: slot j (after 0 reserved) = plt_va + j*16? The first rel entry
        # maps to plt slot 0; verify by decoding the stub at that address.
        stub = plt_va + j * 16
        raw = blob[stub - BASE : stub - BASE + 16]
        print(f".rel.plt[{j}] {nm}: GOT={off:#x} stub={stub:#x} bytes={raw.hex()}")
