#!/usr/bin/env python3
"""Unicorn PPC32 harness for the 57.23-74170-1-16 HLS queue internals.

Targets (anacapad, base 0x10000000):
  append    0x10191fbc      compactor 0x10192674      split 0x1019278c
Ring fields on the parent: +0x00 pending byte (gate), +0x04 item index,
+0x08/+0x0c two ring counters, +0x20 slot buffer, +0xc508 C, +0xc50c,
+0xc510, +0xc514 limit.

Runs one scenario per process (Unicorn is not re-entrant across objects):
    ./hls_emu.py compact <pending> <h> <lim>
    ./hls_emu.py split   <pending> <q4> <q8> <limit>
"""
import struct, sys
from unicorn import *
from unicorn.ppc_const import *

ELF = 'captures/firmware/57.23-74170-1-16/anacapad'
BASE = 0x10000000
OBJ = 0x20000000
SLOTS = 0x20080000
STACK = 0x70000000
MEMCPY = 0x10546ad8

APPEND, COMPACT, SPLIT = 0x10191fbc, 0x10192674, 0x1019278c

uc = Uc(UC_ARCH_PPC, UC_MODE_PPC32 + UC_MODE_BIG_ENDIAN)
with open(ELF, 'rb') as f:
    data = f.read()

# --- load PT_LOAD segments
e_phoff, = struct.unpack_from('>I', data, 28)
e_phnum, = struct.unpack_from('>H', data, 44)
for i in range(e_phnum):
    p_type, p_off, p_va, p_pa, p_fsz, p_msz = struct.unpack_from('>IIIIII', data, e_phoff + i * 32)
    if p_type != 1:
        continue
    lo = p_va & ~0xFFF
    hi = (p_va + p_msz + 0xFFF) & ~0xFFF
    uc.mem_map(lo, hi - lo)
    uc.mem_write(p_va, data[p_off:p_off + p_fsz])

uc.mem_map(OBJ - 0x80000, 0x100000)
uc.mem_map(SLOTS, 0x40000)
uc.mem_map(STACK, 0x20000)
STOP = 0x22000000
uc.mem_map(STOP, 0x1000)
uc.mem_write(STOP, struct.pack('>I', 0x4e800020))       # blr

SP = STACK + 0x18000
LOG = []

def rd(a, n):
    return bytes(uc.mem_read(a, n))

def u32(a):
    return struct.unpack('>I', rd(a, 4))[0]

def st(a, v):
    uc.mem_write(a, struct.pack('>I', v & 0xffffffff))

def fields():
    return dict(P=rd(OBJ, 1)[0], C=u32(OBJ + 0xc508), x50c=u32(OBJ + 0xc50c),
                x510=u32(OBJ + 0xc510), lim=u32(OBJ + 0xc514),
                idx=u32(OBJ + 4), n8=u32(OBJ + 8), nc=u32(OBJ + 0xc))

def show(tag):
    f = fields()
    LOG.append((tag, f))
    print('%-22s P=%d C=%d(+0xc508) x50c=%d x510=%d lim=%d idx=%d +8=%d +c=%d'
          % (tag, f['P'], f['C'], f['x50c'], f['x510'], f['lim'], f['idx'], f['n8'], f['nc']))

body = {}
def code_hook(uc, addr, size, ud):
    if addr == MEMCPY:
        dst, src, n = uc.reg_read(UC_PPC_REG_3), uc.reg_read(UC_PPC_REG_4), uc.reg_read(UC_PPC_REG_5)
        n &= 0xffffffff
        if body.get('log_memcpy') and body.get('memcpy_seen', 0) < 3:
            body['memcpy_seen'] = body.get('memcpy_seen', 0) + 1
            print('   memcpy dst=%#x src=%#x len=%d (%#x)' % (dst, src, n, n))
        if 0 < n <= 0x200000:
            uc.mem_write(dst, rd(src, n))
        elif n > 0x200000:
            off = 0
            try:
                while off < n:
                    k = min(0x10000, n - off)
                    uc.mem_write(dst + off, rd(src + off, k))
                    off += k
            except UcError as e:
                print('   MEMCPY FAULT after %d of %d bytes at %#x' % (off, n, dst + off))
                body['memcpy_fault'] = off
                uc.reg_write(UC_PPC_REG_PC, uc.reg_read(UC_PPC_REG_LR))
                return
        uc.reg_write(UC_PPC_REG_PC, uc.reg_read(UC_PPC_REG_LR))
        return
    if addr == COMPACT + 0xe0:            # body entry 0x10192754
        h, lim = u32(OBJ + 0xc508), u32(OBJ + 0xc514)
        n = (lim - h) & 0xffffffff
        print('   BODY h=%d lim=%d n=%d(%#x) C_would_be=%d(%#x)'
              % (h, lim, n, n, (16 - n) & 0xffffffff, (16 - n) & 0xffffffff))
        body['log_memcpy'] = True
    if addr == 0x10192784:                # stw 29,C
        show('after-compact')

uc.hook_add(UC_HOOK_CODE, code_hook)

def setup(pending=0, h=0, lim=16, q=None, x510=0, x50c=0):
    uc.mem_write(OBJ - 0x40000, b'\0' * 0x60000)
    uc.mem_write(SLOTS, bytes((i & 0xff) for i in range(0x40000)))
    st(OBJ + 0x20, SLOTS)
    uc.mem_write(OBJ + 0, bytes([pending & 0xff]))
    st(OBJ + 0xc508, h)
    st(OBJ + 0xc50c, x50c)
    st(OBJ + 0xc510, x510)
    st(OBJ + 0xc514, lim)
    if q:
        st(OBJ + 0xcd48, q[0]); st(OBJ + 0xcd4c, q[1])
        st(OBJ + 0xcd50, q[2]); st(OBJ + 0xcd54, q[3])

def call(fn, a0=OBJ, a1=0):
    uc.reg_write(UC_PPC_REG_1, SP)
    uc.reg_write(UC_PPC_REG_3, a0)
    uc.reg_write(UC_PPC_REG_4, a1)
    uc.reg_write(UC_PPC_REG_LR, STOP)
    uc.reg_write(UC_PPC_REG_PC, fn)
    try:
        uc.emu_start(fn, STOP, count=200000)
    except UcError as e:
        print('   FAULT', e, 'pc=%#x' % uc.reg_read(UC_PPC_REG_PC))
        return False
    return True

mode = sys.argv[1]
if mode == 'compact':
    p, h, lim = int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4])
    x510 = int(sys.argv[5]) if len(sys.argv) > 5 else 0
    setup(p, h, lim, x510=x510)
    show('before')
    call(COMPACT)
    show('after')
elif mode == 'split':
    p, q4, q50, q54, lim = (int(x) for x in sys.argv[2:7])
    setup(p, 0, lim, q=(q4, q4, q50, q54))
    show('before')
    call(SPLIT)
    show('after')
elif mode == 'split-append-compact':
    limit = int(sys.argv[2])
    setup(1, 0, limit, q=(0, 0, 5, 5), x510=5)
    call(SPLIT);  show('after-split')          # cursor := limit
    idx = fields()['C']
    st(OBJ + 4, idx)
    uri = OBJ + 0x10000 - 0xc50
    uc.mem_write(uri, b'http://192.0.2.1:1400/x/' + b'A' * 32 + b'\0')
    call(APPEND, a0=OBJ, a1=uri); show('append@%d' % idx)
    uc.mem_write(OBJ + 0, b'\x01')
    call(COMPACT);  show('after-compact')
elif mode == 'shortcut-chain4':
    limit = int(sys.argv[2])
    setup(1, 0, limit, q=(0, 0, 5, 5), x510=0xffffffff, x50c=0)
    show('cursor=0 cap=0 lim=%d' % limit)
    call(COMPACT);  show('shortcut')            # cap := lim, lim := 16
    idx = fields()['x50c']                      # append at the new cursor
    st(OBJ + 4, idx)
    uri = OBJ + 0x10000 - 0xc50
    uc.mem_write(uri, b'http://192.0.2.1:1400/x/' + b'A' * 32 + b'\0')
    call(APPEND, a0=OBJ, a1=uri); show('append@%d' % idx)
    uc.mem_write(OBJ + 0, b'\x01')
    call(COMPACT);  show('after-compact')
elif mode == 'shortcut-chain3':
    limit = int(sys.argv[2])
    keep = len(sys.argv) > 3 and sys.argv[3] == 'keep'
    setup(1, 0, limit, q=(0, 0, 5, 5), x510=0xffffffff, x50c=0)
    show('cursor=0 cap=0 lim=%d' % limit)
    call(COMPACT);  show('shortcut')            # cursor := lim, cap := 16, lim := 16
    if not keep:
        st(OBJ + 0xc508, 16); st(OBJ + 4, 16)   # instruction: set cursor to 16
    idx = fields()['C']
    st(OBJ + 4, idx)
    uri = OBJ + 0x10000 - 0xc50
    uc.mem_write(uri, b'http://192.0.2.1:1400/x/' + b'A' * 32 + b'\0')
    call(APPEND, a0=OBJ, a1=uri); show('append@%d' % idx)
    uc.mem_write(OBJ + 0, b'\x01')
    call(COMPACT);  show('after-compact')
elif mode == 'shortcut-chain2':
    cap = int(sys.argv[2])
    setup(1, 0, 16, q=(0, 0, 5, 5), x510=5, x50c=cap)
    show('cursor=0 cap=%d' % cap)
    call(COMPACT);  show('shortcut')          # cursor := cap, cap := 16
    idx = fields()['C']
    st(OBJ + 4, idx)
    uri = OBJ + 0x10000 - 0xc50
    uc.mem_write(uri, b'http://192.0.2.1:1400/x/' + b'A' * 32 + b'\0')
    call(APPEND, a0=OBJ, a1=uri); show('append@%d' % idx)
    uc.mem_write(OBJ + 0, b'\x01')
    call(COMPACT);  show('after-compact')
elif mode == 'shortcut-chain':
    limit = int(sys.argv[2])
    setup(1, 0, limit, q=(0, 0, 5, 5))
    show('before')
    call(SPLIT);        show('after-split')
    st(OBJ + 0xc508, 0)                      # record consumed -> head back at 0
    uc.mem_write(OBJ + 0, b'\x01')
    call(COMPACT);      show('shortcut')     # non-copy shortcut, h==0
    st(OBJ + 0xc50c, 16); st(OBJ + 4, 16)    # cursor := 16
    show('cursor=16')
    uri = OBJ + 0x10000 - 0xc50              # a URI-shaped 0xc50 record
    uc.mem_write(uri, b'http://192.0.2.1:1400/x/' + b'A' * 32 + b'\0')
    call(APPEND, a0=OBJ);                     # r4 defaults to 0; patch below
    show('after-append')
    st(OBJ + 0xc510, 5); uc.mem_write(OBJ + 0, b'\x01')
    call(COMPACT);      show('after-compact')
elif mode == 'split-then-compact':
    q4, q50, q54, lim = (int(x) for x in sys.argv[2:6])
    setup(1, 0, lim, q=(q4, q4, q50, q54))
    show('before')
    call(SPLIT);   show('after-split')
    uc.mem_write(OBJ + 0, b'\x01')          # writer re-arms the pending byte
    show('re-armed')
    call(COMPACT); show('after-compact')
elif mode == 'chain':
    # append index 0..n then compact, to see the real field progression
    p = int(sys.argv[2])
    setup(p, 0, 16)
    show('before')
    for i in range(3):
        st(OBJ + 4, i)
        call(APPEND)
        show('append idx=%d' % i)
    st(OBJ, b'\0')
    call(COMPACT)
    show('after')
