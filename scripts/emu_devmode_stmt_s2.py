import os, struct
from unicorn import *
from unicorn.ppc_const import *
LIB = os.environ.get("SONOS_S2_CRYPTO", "/Users/jemanuel/sonos-root-local/fw/internals/libsonoscrypto.so.3")
FUNC, PARSER_JUMP, VERIFY = 0xCA14, 0x9E14, 0x201C8
SENT = 0xDEAD0000
MODEL, DEV = 8, b"000E58CADE94"
FP = "28a096f5f6f5578345d1e892dfb2f09ce99f1ad3"
d = open(LIB, "rb").read()
_shoff,=struct.unpack(">I",d[32:36]); _she, _shn, _shstr = struct.unpack(">HHH", d[46:52])
_si=_shstr*_she+_shoff; _str,=struct.unpack(">I", d[_si+16:_si+20])
_rela=_sym=_dynstr=None
for _i in range(_shn):
    _o=_shoff+_i*_she; _no,=struct.unpack(">I", d[_o:_o+4]); _t,_f,_a,_off,_sz=struct.unpack(">IIIII", d[_o+4:_o+24])
    _e=d.index(b"\0", _str+_no); _nm=d[_str+_no:_e].decode()
    if _nm==".rela.plt": _rela=(_off,_sz)
    elif _nm==".dynsym": _sym=(_off,_sz)
    elif _nm==".dynstr": _dynstr=_a
_names=[]
for _i in range(_sym[1]//16):
    _o=_sym[0]+_i*16; _no,=struct.unpack(">I", d[_o:_o+4]); _e=d.index(b"\0", _dynstr+_no); _names.append(d[_dynstr+_no:_e].decode())
_ents=[]
for _i in range(_rela[1]//12):
    _o=_rela[0]+_i*12; _info,=struct.unpack(">I", d[_o+4:_o+8]); _ents.append(_names[_info>>8])
_defined={}
for _i in range(_sym[1]//16):
    _o=_sym[0]+_i*16; _val,=struct.unpack(">I", d[_o+4:_o+8]); _shndx,=struct.unpack(">H", d[_o+14:_o+16])
    if _shndx: _defined[_names[_i]]=_val
H, S = 0x48, 8
PLT = {0x200d0+H+i*S: n for i,n in enumerate(_ents)}
phoff, = struct.unpack(">I", d[28:32]); phentsize, phnum = struct.unpack(">HH", d[42:46])
def build(dev=DEV, magic=0x28721E71, lower=0x55, upper=0x57, ring=True, trunc=None, devid_len=None):
    s = struct.pack(">II", magic, MODEL) + b"\0"*16 + struct.pack(">II", lower, upper)
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
    mu = Uc(UC_ARCH_PPC, UC_MODE_PPC32 | UC_MODE_BIG_ENDIAN)
    for i in range(phnum):
        o = phoff + i*phentsize
        pt, off, va, pa, fsz, msz, fl, al = struct.unpack(">IIIIIIII", d[o:o+32])
        if pt != 1: continue
        base = va & ~0xFFF; top = (va + msz + 0xFFF) & ~0xFFF
        mu.mem_map(base, top-base); mu.mem_write(va, d[off:off+fsz])
    mu.mem_map(0x1F0000, 0x30000); mu.mem_map(0x100000, 0x4000); mu.mem_map(0x220000, 0x1000); mu.mem_map(0x300000, 0x10000)
    mu.mem_write(0x100000, statement); mu.mem_write(0x220000, DEV + b"\0"); mu.mem_write(0x300000, b"\0"*0x10000)
    sp = 0x30F000; outs = 0x301000
    mu.mem_write(sp+8, struct.pack(">III", len(DEV), outs+4, outs+8))
    mu.reg_write(UC_PPC_REG_1, sp); mu.reg_write(UC_PPC_REG_LR, SENT); mu.reg_write(UC_PPC_REG_2, 0x27ED8); mu.reg_write(UC_PPC_REG_30, 0x27ED8)
    mu.reg_write(UC_PPC_REG_3, 0x100000); mu.reg_write(UC_PPC_REG_4, len(statement))
    mu.reg_write(UC_PPC_REG_5, MODEL); mu.reg_write(UC_PPC_REG_6, 0x220000); mu.reg_write(UC_PPC_REG_7, len(DEV))
    mu.reg_write(UC_PPC_REG_8, 0); mu.reg_write(UC_PPC_REG_9, 0x56)
    mu.reg_write(UC_PPC_REG_10, outs)
    st = {"verify": False, "stubs": [], "steps": 0, "ret": None, "last": []}
    def hook(mu, pc, size, ud):
        st["steps"] += 1
        st["last"].append(pc)
        if pc == VERIFY: st["verify"] = True; mu.emu_stop(); return
        if pc in (0xCBC4, 0xCBD0, 0xCBDC, 0xCBE0): mu.reg_write(UC_PPC_REG_PC, pc+4); return
        w, = struct.unpack(">I", mu.mem_read(pc, 4))
        if (w >> 26) == 31 and ((w >> 1) & 0x1F) == 15:   # isel
            rd, ra, rb, crb = (w >> 21) & 0x1F, (w >> 16) & 0x1F, (w >> 11) & 0x1F, (w >> 6) & 0x1F
            cr = mu.reg_read(UC_PPC_REG_CR)
            take = (cr >> (31 - crb)) & 1
            mu.reg_write(UC_PPC_REG_0 + rd, mu.reg_read(UC_PPC_REG_0 + (ra if take else rb)))
            mu.reg_write(UC_PPC_REG_PC, pc + 4); return
        if pc == 0xCAA0: st["ret"] = mu.reg_read(UC_PPC_REG_3); mu.emu_stop(); return
        if 0x200D0 <= pc < 0x20A00:
            name = PLT.get(pc, f"?{pc:x}")
            a0, a1, a2 = (mu.reg_read(getattr(__import__("unicorn.ppc_const", fromlist=[f"UC_PPC_REG_{k}"]), f"UC_PPC_REG_{k}")) for k in (3,4,5))
            st["stubs"].append(name)
            if pc == VERIFY: st["verify"] = True; mu.emu_stop(); return
            if name == "memset":
                mu.mem_write(a0, bytes([a1 & 0xFF])*a2); mu.reg_write(UC_PPC_REG_3, a0)
            elif name == "memcmp":
                x = mu.mem_read(a0, a2); y = mu.mem_read(a1, a2); r = (x > y) - (x < y)
                mu.reg_write(UC_PPC_REG_3, r & 0xFFFFFFFF)
            elif name in ("memcpy", "memmove"):
                mu.mem_write(a0, bytes(mu.mem_read(a1, a2))); mu.reg_write(UC_PPC_REG_3, a0)
            elif name == "strlen":
                v = a0
                while mu.mem_read(v, 1) != b"\0": v += 1
                mu.reg_write(UC_PPC_REG_3, v-a0)
            elif name in _defined:
                mu.reg_write(UC_PPC_REG_PC, _defined[name]); return
            else:
                st["ret"] = f"UNKNOWN {name} args={a0:x},{a1:x},{a2:x}"; mu.emu_stop(); return
            mu.reg_write(UC_PPC_REG_PC, mu.reg_read(UC_PPC_REG_LR)); return
        if st["steps"] > 2_000_000: st["ret"] = "RUNAWAY"; mu.emu_stop()
    def err(mu, acc, addr, size, val, ud):
        st["ucerr"] = f"{acc} @{addr:x} pc={mu.reg_read(UC_PPC_REG_PC):x}"
    mu.hook_add(UC_HOOK_CODE, hook); mu.hook_add(UC_HOOK_MEM_INVALID, err)
    try: mu.emu_start(FUNC, SENT, count=2_000_000)
    except UcError as e: st["ret"] = f"UC:{e} {st.get('ucerr','')}"
    print('  trace:', ' '.join(hex(x) for x in st['last'][-14:]))
    return st
cases = {"valid": build(), "bad-magic": build(magic=0xDEADBEEF), "wrong-devid": build(dev=b"\0"*len(DEV)),
         "bad-devlen": build(devid_len=11), "lo-edge": build(lower=0x57), "hi-edge": build(upper=0x55), "exact": build(lower=0x56, upper=0x56),
         "free-byte": (lambda x: x[:8]+bytes([x[8]^0xFF])+x[9:])(build()),
  "no-ring": build(ring=False), "short-4": build(trunc=4)}
for n, s in cases.items():
    r = run(s); print(f"{n:12} verify={r['verify']} ret={r['ret']} steps={r['steps']} stubs={r['stubs'][:6]}")
