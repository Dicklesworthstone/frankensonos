import os, re, struct, sys
from unicorn import *
from unicorn.mips_const import *

LIB = "/Users/jemanuel/projects/frankensonos/captures/firmware/57.23-74170-1-12/libsonoscrypto.so.1"
FUNC = 0xAB54
VERIFY = 0xA920
SENT = 0xDEAD0000
MODEL = 16
DEV = b"000E585D8C10"
FP = "1756174b985da89911df58473514542844f5357a"

d = open(LIB, "rb").read()
# program headers
phoff, = struct.unpack(">I", d[28:32]); phentsize, phnum = struct.unpack(">HH", d[42:46])
# sections for stub range
shoff, = struct.unpack(">I", d[32:36]); shentsize, shnum, shstrndx = struct.unpack(">HHH", d[46:52])
def secname(off):
    nameoff, = struct.unpack(">I", d[off:off+4]); i = shstrndx*shentsize + shoff
    strtab, = struct.unpack(">I", d[i+16:i+20])
    e = d.index(b"\0", strtab+nameoff); return d[strtab+nameoff:e].decode()
stub_lo, stub_hi = None, None
for i in range(shnum):
    o = shoff + i*shentsize
    nm = secname(o)
    if nm == ".MIPS.stubs":
        addr, size = struct.unpack(">II", d[o+12:o+20]); stub_lo, stub_hi = addr, addr+size
# dynsym map for stub addresses
symoff, = struct.unpack(">I", d[shoff + 5*shentsize + 16: shoff + 5*shentsize + 20])
symsize, symstr, = None, None
for i in range(shnum):
    o = shoff + i*shentsize
    if d[o:o+4] == b"\0\0\0\0": pass
    nm = secname(o)
    if nm == ".dynsym": symoff, symsize = struct.unpack(">II", d[o+16:o+24])
    if nm == ".dynstr": symstr = struct.unpack(">I", d[o+16:o+20])[0]
stub2name = {}
if symstr:
    n = symsize // 16
    for k in range(n):
        o = symoff + k*16
        val, = struct.unpack(">I", d[o+4:o+8]); no, = struct.unpack(">I", d[o:o+4])
        if stub_lo <= val < stub_hi and no < symstr:
            e = d.index(b"\0", symstr+no); stub2name[val] = d[symstr+no:e].decode()

def build(dev=DEV, magic=0x28721E71, maxv=0x390017, minv=0x390017, ring=True, trunc=None, devid_len=None):
    s = struct.pack(">II", magic, MODEL) + b"\0"*16 + struct.pack(">II", maxv, minv)
    n = len(dev) if devid_len is None else devid_len
    s += bytes([n]) + dev + b"\0"
    if ring:
        r = struct.pack(">IIH", 0x621DA74D, 0, 1) + b"\0\0" + struct.pack(">II", 0, 0) + b"\x01"
        r += struct.pack(">I", 20) + b"\x03" + bytes.fromhex(FP) + b"\x01" + struct.pack(">I", 1)
        r += struct.pack(">I", 256) + b"\xAA"*256
        r = r[:4] + struct.pack(">I", len(r)) + r[8:]
        s += r
    return s[:trunc] if trunc else s

def run(statement):
    mu = Uc(UC_ARCH_MIPS, UC_MODE_MIPS32 | UC_MODE_BIG_ENDIAN)
    for i in range(phnum):
        o = phoff + i*phentsize
        p_type, off, va, pa, fsz, msz, fl, al = struct.unpack(">IIIIIIII", d[o:o+32])
        if p_type != 1: continue
        base = va & ~0xFFF; top = (va + msz + 0xFFF) & ~0xFFF
        mu.mem_map(base, top-base)
        mu.mem_write(va, d[off:off+fsz])
    mu.mem_map(0x100000, 0x4000); mu.mem_map(0x200000, 0x1000); mu.mem_map(0x300000, 0x8000)
    mu.mem_write(0x100000, statement)
    mu.mem_write(0x200000, DEV + b"\0")
    mu.mem_write(0x300000, b"\0"*0x8000)
    sp = 0x307000
    outs = 0x301000
    mu.mem_write(sp+16, struct.pack(">IIIII", len(DEV), outs, outs+4, outs+8, outs+12))
    mu.reg_write(UC_MIPS_REG_SP, sp); mu.reg_write(UC_MIPS_REG_RA, SENT)
    mu.reg_write(UC_MIPS_REG_A0, 0x100000); mu.reg_write(UC_MIPS_REG_A1, len(statement))
    mu.reg_write(UC_MIPS_REG_A2, MODEL); mu.reg_write(UC_MIPS_REG_A3, 0x200000)
    mu.reg_write(UC_MIPS_REG_T9, FUNC)
    state = {"verify": False, "stubs": [], "steps": 0, "ret": None}
    def hook(mu, pc, size, ud):
        state["steps"] += 1
        if pc == VERIFY: state["verify"] = True; mu.emu_stop(); return
        if pc == SENT: state["ret"] = mu.reg_read(UC_MIPS_REG_V0); mu.emu_stop(); return
        if stub_lo <= pc < stub_hi:
            t8 = mu.reg_read(UC_MIPS_REG_T8); name = stub2name.get(pc, f"idx{t8}")
            state["stubs"].append(name); a0 = mu.reg_read(UC_MIPS_REG_A0); a1 = mu.reg_read(UC_MIPS_REG_A1); a2 = mu.reg_read(UC_MIPS_REG_A2)
            if name == "memcmp":
                x = mu.mem_read(a0, a2); y = mu.mem_read(a1, a2)
                r = (x > y) - (x < y); mu.reg_write(UC_MIPS_REG_V0, r & 0xFFFFFFFF)
            elif name == "memcpy":
                mu.mem_write(a0, bytes(mu.mem_read(a1, a2))); mu.reg_write(UC_MIPS_REG_V0, a0)
            elif name == "memset":
                mu.mem_write(a0, bytes([a1 & 0xFF])*a2); mu.reg_write(UC_MIPS_REG_V0, a0)
            elif name == "strlen":
                v = a0
                while mu.mem_read(v, 1) != b"\0": v += 1
                mu.reg_write(UC_MIPS_REG_V0, v-a0)
            else:
                state["ret"] = ("UNKNOWN_STUB", name); mu.emu_stop(); return
            mu.reg_write(UC_MIPS_REG_PC, mu.reg_read(UC_MIPS_REG_RA)); return
        if state["steps"] > 3_000_000: state["ret"] = "RUNAWAY"; mu.emu_stop()
    mu.hook_add(UC_HOOK_CODE, hook)
    try: mu.emu_start(FUNC, SENT, count=3_000_000)
    except UcError as e: state["ret"] = f"UC:{e}"
    return state

cases = {
  "valid": build(),
  "bad-magic": build(magic=0xDEADBEEF),
  "wrong-devid": build(dev=b"\0"*len(DEV)),
  "bad-devlen": build(devid_len=len(DEV)+1),
  "upper-edge": build(maxv=0x390018),
  "lower-edge": build(minv=0x390016),
  "wrong-fp": build().replace(bytes.fromhex(FP), b"\x00"*20, 1),
  "free-byte": (lambda x: x[:8]+bytes([x[8]^0xFF])+x[9:])(build()),
  "no-ring": build(ring=False),
  "short-4": build(trunc=4),
}
for name, st in cases.items():
    r = run(st)
    print(f"{name:12} verify={r['verify']} ret={r['ret']} steps={r['steps']} stubs={r['stubs']}")
