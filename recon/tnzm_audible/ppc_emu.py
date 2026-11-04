#!/usr/bin/env python3
"""Unicorn PPC32-BE harness for the 57.23 anacapad Audible parse path.

Modeled on captures/research/hhid_emu.py (the S2 MIPS16 harness). Maps the
PPC 57.23 anacapad PT_LOADs, resolves the 478 DT_JMPREL PLT slots by name
(r_info sym = info>>8, ELF32 PPC), patches each .plt slot with a `b` into an
emulation region placed in the 0x1052d000 gap (within the PPC ±32 MB branch
range), and serves libc/PLT calls from Python hooks.

Usage: python3 ppc_emu.py            # runs the audible handler smoke path
"""
import struct, json, sys
from elftools.elf.elffile import ELFFile
from unicorn import Uc, UC_ARCH_PPC, UC_MODE_PPC32, UC_MODE_BIG_ENDIAN, UC_HOOK_CODE
from unicorn import UcError
from unicorn.ppc_const import *

ELF = 'captures/firmware/57.23-74170-1-16/anacapad'
EMU = 0x1052d000            # free gap 0x1052d000..0x1053c000 (LOAD1 page-end .. LOAD2 page-start)
THIS = 0x52000000           # synthetic audible session object
SCRATCH = 0x53000000
STACK = 0x60000000
HEAP = 0x54000000
POC = 0x55000000            # PoC .aa mapped here
TLSBLOCK = 0x56000000
RET = 0x48000000


def load(uc):
    f = open(ELF, 'rb'); ef = ELFFile(f)
    for seg in ef.iter_segments():
        if seg['p_type'] != 'PT_LOAD':
            continue
        va, sz = seg['p_vaddr'], seg['p_memsz']
        lo, hi = va & ~0xFFF, (va + sz + 0xFFF) & ~0xFFF
        if hi > lo:
            uc.mem_map(lo, hi - lo)
            uc.mem_write(va, seg.data()[:seg['p_filesz']])


def plt_slots():
    f = open(ELF, 'rb')
    f.seek(0x5ecc); d = f.read(0x1668)          # DT_JMPREL (DT_PLTRELSZ)
    f.seek(0x1330); sym = f.read(0x2600)        # .dynsym
    f.seek(0x3930); st = f.read(0x1ef3)         # .dynstr
    slots = {}
    for j in range(0x1668 // 12):
        off, info, add = struct.unpack('>III', d[j*12:j*12+12])
        si = info >> 8                          # ELF32: r_info = sym<<8 | type
        no = struct.unpack('>I', sym[si*16:si*16+4])[0]
        slots[off] = st[no:].split(b'\0')[0].decode('latin1')
    return slots


def patch_plt(uc):
    slotmap = {}
    for i, (off_s, name) in enumerate(sorted(plt_slots().items(), key=lambda kv: kv[0])):
        off = off_s
        tgt = EMU + i*64
        delta = (tgt - off) & 0x3FFFFFC
        if delta & 0x2000000:
            delta -= 0x4000000
        if abs(delta) >= 0x2000000:
            continue
        uc.mem_write(off, struct.pack('>I', 0x48000000 | (delta & 0x3FFFFFC)))
        slotmap[tgt] = name
    return slotmap


def make_env(uc, poc_bytes):
    uc.mem_map(EMU, 0xF000)
    uc.mem_map(THIS, 0x400000)
    uc.mem_map(SCRATCH, 0x100000)
    uc.mem_map(STACK, 0x200000)
    uc.mem_map(HEAP, 0x1000000)
    uc.mem_map(POC, 0x10000)
    uc.mem_map(TLSBLOCK, 0x10000)
    uc.mem_map(0x0, 0x100000)   # low page shadow: stray small pointers read as zeros
    uc.mem_write(POC, poc_bytes)
    uc.mem_map(RET, 0x1000)
    uc.mem_write(RET, struct.pack('>I', 0x4e800020))          # blr
    uc.mem_write(STACK, struct.pack('>I', RET) * (0x200000 // 4))
    uc.reg_write(UC_PPC_REG_13, 0x105459b0 + 0x8000)          # SDA base
    uc.reg_write(UC_PPC_REG_2, 0x10545664 + 0x8000)           # TOC base


def dbg_mem(uc, access, address, size, value, ud):
    print('MEMFAULT', hex(address), access, 'pc=', hex(uc.reg_read(UC_PPC_REG_PC)))
    return False


def main():
    uc = Uc(UC_ARCH_PPC, UC_MODE_PPC32 + UC_MODE_BIG_ENDIAN)
    load(uc)
    slotmap = patch_plt(uc)
    poc = open(sys.argv[1] if len(sys.argv) > 1 else '/tmp/tnzm_bisect/C_base.aa', 'rb').read()
    make_env(uc, poc)
    heap = [HEAP + 0x1000]
    calls = []

    def alloc(n, align=16):
        a = (heap[0] + align - 1) & ~(align - 1)
        heap[0] = a + n
        return a

    def hook(uc, addr, size, ud):
        name = slotmap.get(addr)
        if name is None:
            return
        calls.append(name)
        a3 = uc.reg_read(UC_PPC_REG_3); a4 = uc.reg_read(UC_PPC_REG_4)
        a5 = uc.reg_read(UC_PPC_REG_5)
        if name in ('strlcpy', 'strncpy'):
            src = bytes(uc.mem_read(a4, 65536)).split(b'\0')[0]
            uc.mem_write(a3, src[:max(0, a5-1)] + b'\0')
            uc.reg_write(UC_PPC_REG_3, a4)
        elif name == 'memcpy':
            uc.mem_write(a3, bytes(uc.mem_read(a4, a5)))
            uc.reg_write(UC_PPC_REG_3, a3)
        elif name == 'memset':
            uc.mem_write(a3, bytes([a4 & 0xFF]) * a5)
            uc.reg_write(UC_PPC_REG_3, a3)
        elif name == 'strlen':
            n = len(bytes(uc.mem_read(a3, 65536)).split(b'\0')[0])
            uc.reg_write(UC_PPC_REG_3, n)
        elif name == 'malloc':
            uc.reg_write(UC_PPC_REG_3, alloc(a3))
        elif name == 'free':
            uc.reg_write(UC_PPC_REG_3, 0)
        elif name == 'pthread_getspecific':
            uc.reg_write(UC_PPC_REG_3, TLSBLOCK)   # synthetic per-thread struct (zeroed)
        else:
            uc.reg_write(UC_PPC_REG_3, 0)
        uc.reg_write(UC_PPC_REG_PC, uc.reg_read(UC_PPC_REG_LR))

    from unicorn import UC_HOOK_MEM_READ_UNMAPPED, UC_HOOK_MEM_WRITE_UNMAPPED, UC_HOOK_MEM_FETCH_UNMAPPED
    uc.hook_add(UC_HOOK_MEM_READ_UNMAPPED | UC_HOOK_MEM_WRITE_UNMAPPED | UC_HOOK_MEM_FETCH_UNMAPPED, dbg_mem)
    uc.hook_add(UC_HOOK_CODE, hook)
    try:
        uc.reg_write(UC_PPC_REG_3, THIS)
        uc.reg_write(UC_PPC_REG_4, SCRATCH)
        uc.reg_write(UC_PPC_REG_5, len(poc))
        uc.reg_write(UC_PPC_REG_LR, RET)
        uc.reg_write(UC_PPC_REG_1, STACK + 0x180000)
        uc.emu_start(0x10118ad8, RET, count=300000)
        print('returned cleanly; PLT calls:', len(calls))
    except UcError as e:
        print('ERR', e, 'pc=', hex(uc.reg_read(UC_PPC_REG_PC)),
              'lr=', hex(uc.reg_read(UC_PPC_REG_LR)), 'calls:', len(calls))
        print('last:', calls[-12:])
    return uc, slotmap, calls


if __name__ == '__main__':
    main()
