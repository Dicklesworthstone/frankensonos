#!/usr/bin/env python3
"""Run libsonoscrypto ZZ0058 (SHA-256 of the header value) under Unicorn PPC.

Proves the API-key check is plain SHA-256: the output equals hashlib.sha256
for the same input. Needs the PPC lib from the 1-16 rootfs.
"""
import struct, hashlib
from unicorn import *
from unicorn.ppc_const import *
LIB="/Users/jemanuel/sonos-root-local/fw/internals/libsonoscrypto.so.3"
d=open(LIB,'rb').read()
phoff,=struct.unpack(">I",d[28:32]); pe,pn=struct.unpack(">HH",d[42:46])
shoff,=struct.unpack(">I",d[32:36]); se,sn,ss=struct.unpack(">HHH",d[46:52]); si=ss*se+shoff; st,=struct.unpack(">I",d[si+16:si+20])
rel=sym=ds=rd=None
for i in range(sn):
    o=shoff+i*se; no,=struct.unpack(">I",d[o:o+4]); t,f,a,off,sz=struct.unpack(">IIIII",d[o+4:o+24]); e=d.index(b'\0',st+no); nm=d[st+no:e].decode()
    if nm=='.rela.plt': rel=(off,sz)
    elif nm=='.dynsym': sym=(off,sz)
    elif nm=='.dynstr': ds=a
    elif nm=='.rela.dyn': rd=(off,sz)
names=[]
for i in range(sym[1]//16):
    o=sym[0]+i*16; no,=struct.unpack(">I",d[o:o+4]); e=d.index(b'\0',ds+no); names.append(d[ds+no:e].decode())
ents=[]
for i in range(rel[1]//12):
    o=rel[0]+i*12; info,=struct.unpack(">I",d[o+4:o+8]); ents.append(names[info>>8])
H,S=0x48,8
defined={}
for i in range(sym[1]//16):
    o=sym[0]+i*16; val,=struct.unpack(">I",d[o+4:o+8]); shndx,=struct.unpack(">H",d[o+14:o+16])
    if shndx: defined[names[i]]=val
PLT={0x200d0+H+i*S:n for i,n in enumerate(ents)}
def run(key):
    mu=Uc(UC_ARCH_PPC, UC_MODE_PPC32|UC_MODE_BIG_ENDIAN)
    for i in range(pn):
        o=phoff+i*pe; pt,off,va,pa,fs,ms,fl,al=struct.unpack(">IIIIIIII",d[o:o+32])
        if pt!=1: continue
        base=va&~0xFFF; top=(va+ms+0xFFF)&~0xFFF; mu.mem_map(base,top-base); mu.mem_write(va,d[off:off+fs])
    mu.mem_map(0x300000,0x20000); mu.mem_map(0x400000,0x2000)
    mu.mem_write(0x400000,key+b"\0"); out=0x300000
    sp=0x31F000; mu.reg_write(UC_PPC_REG_1,sp); mu.reg_write(UC_PPC_REG_LR,0xDEAD0000)
    mu.reg_write(UC_PPC_REG_2,0x27ED8); mu.reg_write(UC_PPC_REG_30,0x27ED8)
    mu.reg_write(UC_PPC_REG_3,1); mu.reg_write(UC_PPC_REG_4,0x400000); mu.reg_write(UC_PPC_REG_5,len(key)); mu.reg_write(UC_PPC_REG_6,out)
    stt={"done":False}; bufs={}
    def hook(mu,pc,size,ud):
        if pc==0xDEAD0000: stt["done"]=True; mu.emu_stop(); return
        if 0x200d0<=pc<0x20a00:
            nm=PLT.get(pc)
            a0,a1,a2=(mu.reg_read(UC_PPC_REG_0+k) for k in (3,4,5))
            if nm=="memset": mu.mem_write(a0,bytes([a1&0xff])*a2); mu.reg_write(UC_PPC_REG_3,a0)
            elif nm=="mbedtls_sha256_init": mu.mem_write(a0,b"\0"*112); bufs[a0]=bytearray()
            elif nm in ("mbedtls_sha256_starts","mbedtls_sha256_starts_ret"): mu.reg_write(UC_PPC_REG_3,0)
            elif nm=="mbedtls_sha256_update": bufs.setdefault(a0,bytearray()).extend(bytes(mu.mem_read(a1,a2)))
            elif nm=="mbedtls_sha256_finish":
                mu.mem_write(a1,hashlib.sha256(bytes(bufs.get(a0,b""))).digest()); mu.reg_write(UC_PPC_REG_3,0)
            elif nm=="mbedtls_sha256_free": bufs.pop(a0,None)
            elif nm in defined: mu.reg_write(UC_PPC_REG_PC,defined[nm]); return
            else: stt["err"]=nm; mu.emu_stop(); return
            mu.reg_write(UC_PPC_REG_PC,mu.reg_read(UC_PPC_REG_LR)); return
    mu.hook_add(UC_HOOK_CODE,hook)
    try: mu.emu_start(0x6ce8,0xDEAD0000,count=2000000)
    except UcError as e: return None,f"UC:{e}"
    return bytes(mu.mem_read(out,32)), (stt.get("err"), mu.reg_read(UC_PPC_REG_3), stt["done"])
import hashlib,sys
for k in (b"123e4567-e89b-12d3-a456-426655440000", b"Sonos"):
    got,info=run(k); print(k[:12], got.hex()[:16], info, "py", hashlib.sha256(k).hexdigest()[:16], "eq", got==hashlib.sha256(k).digest())
