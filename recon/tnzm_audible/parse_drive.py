#!/usr/bin/env python3
"""Drive 0x101152f8 (audible container parse) with fake IO; log TOC reads + data reads."""
import struct, json, sys
from elftools.elf.elffile import ELFFile
from unicorn import *
from unicorn.ppc_const import *

ELF='captures/firmware/57.23-74170-1-16/anacapad'
POCFILE=sys.argv[1] if len(sys.argv)>1 else '/tmp/tnzm_bisect/C_base.aa'

f=open(ELF,'rb'); ef=ELFFile(f)
uc=Uc(UC_ARCH_PPC, UC_MODE_PPC32+UC_MODE_BIG_ENDIAN)
for seg in ef.iter_segments():
    if seg['p_type']!='PT_LOAD': continue
    va=seg['p_vaddr']; sz=seg['p_memsz']
    lo=va&~0xFFF; hi=(va+sz+0xFFF)&~0xFFF
    if hi>lo: uc.mem_map(lo,hi-lo); uc.mem_write(va,seg.data()[:seg['p_filesz']])
plt=json.load(open('/tmp/ppc_plt_map.json'))
EMU=0x1052d000; uc.mem_map(EMU,0xF000)
slotmap={}
for i,off_s in enumerate(sorted(plt, key=lambda s:int(s,16))):
    off=int(off_s,16); tgt=EMU+i*64
    delta=(tgt-off)&0x3FFFFFC
    if delta&0x2000000: delta-=0x4000000
    if abs(delta)>=0x2000000: continue
    uc.mem_write(off,struct.pack('>I',0x48000000|(delta&0x3FFFFFC)))
    slotmap[tgt]=plt[off_s]
SESS=0x52000000; STACK=0x60000000; HEAP=0x54000000; POC=0x55000000; TLS=0x56000000
uc.mem_map(SESS,0x100000); uc.mem_map(STACK,0x400000); uc.mem_map(HEAP,0x1000000)
uc.mem_map(POC,0x10000); uc.mem_map(TLS,0x10000); uc.mem_map(0x0,0x100000)
RET=0x48000000; uc.mem_map(RET,0x1000); uc.mem_write(RET,struct.pack('>I',0x4e800020))
uc.mem_write(STACK,struct.pack('>I',RET)*(0x400000//4))
uc.reg_write(UC_PPC_REG_13,0x105459b0+0x8000); uc.reg_write(UC_PPC_REG_2,0x10545664+0x8000)
poc=open(POCFILE,'rb').read(); uc.mem_write(POC,poc)
STREAM=0x57000000; uc.mem_map(STREAM,0x10000); uc.mem_write(STREAM,struct.pack('>I',1))
FAKELOG=0x57500000; uc.mem_map(FAKELOG,0x1000)
uc.mem_write(FAKELOG,struct.pack('>I',FAKELOG))
uc.mem_write(FAKELOG+0x100,struct.pack('>II',0x38600000,0x4e800020))
uc.mem_write(FAKELOG+8,struct.pack('>I',FAKELOG+0x100))
uc.mem_write(0x10545d68,struct.pack('>I',FAKELOG))
heap=[HEAP+0x1000]
def alloc(n,align=16):
    a=(heap[0]+align-1)&~(align-1); heap[0]=a+n; return a
# TOC cursor: start after 4-byte magic; the 12B read = {u8 type; u32 off; u32 len; u8 pad[3]} = 13?
# aadec: entry = type u8 + off u32 + len u32 = 9 bytes; terminator type=0. Serve 12B window, advance 9.
toc=[4]; events=[]
def hook(uc,addr,size,ud):
    if addr==0x100149a8:
        buf=uc.reg_read(UC_PPC_REG_4)
        p=toc[0]
        chunk=poc[p:p+12]
        uc.mem_write(buf,chunk+b'\0'*(12-len(chunk)))
        etype=poc[p] if p<len(poc) else 0
        events.append(('toc',p,etype))
        toc[0]+=9
        uc.reg_write(UC_PPC_REG_3,12)
        uc.reg_write(UC_PPC_REG_PC,uc.reg_read(UC_PPC_REG_LR)); return
    if addr==0x100a5510:
        uc.reg_write(UC_PPC_REG_3,1); uc.reg_write(UC_PPC_REG_PC,uc.reg_read(UC_PPC_REG_LR)); return
    if addr==0x100a374c:
        uc.reg_write(UC_PPC_REG_3,0); uc.reg_write(UC_PPC_REG_PC,uc.reg_read(UC_PPC_REG_LR)); return
    if addr==0x10340640:
        uc.reg_write(UC_PPC_REG_3,0)
        uc.reg_write(UC_PPC_REG_PC,uc.reg_read(UC_PPC_REG_LR)); return
    if addr==0x10341678:
        uc.reg_write(UC_PPC_REG_3,0)
        uc.reg_write(UC_PPC_REG_PC,uc.reg_read(UC_PPC_REG_LR)); return
    if addr==0x10346d1c:
        regs=[uc.reg_read(r) for r in (UC_PPC_REG_3,UC_PPC_REG_4,UC_PPC_REG_5,UC_PPC_REG_6,UC_PPC_REG_7,UC_PPC_REG_8,UC_PPC_REG_9,UC_PPC_REG_10)]
        events.append(('data_read',)+tuple(hex(x) for x in regs))
        uc.reg_write(UC_PPC_REG_3,0)
        uc.reg_write(UC_PPC_REG_PC,uc.reg_read(UC_PPC_REG_LR)); return
    name=slotmap.get(addr)
    if name:
        a3=uc.reg_read(UC_PPC_REG_3); a4=uc.reg_read(UC_PPC_REG_4); a5=uc.reg_read(UC_PPC_REG_5)
        if name in ('strlcpy','strncpy'):
            src=bytes(uc.mem_read(a4,65536)).split(b'\0')[0]
            uc.mem_write(a3,src[:max(0,a5-1)]+b'\0'); uc.reg_write(UC_PPC_REG_3,a4)
        elif name=='memcpy':
            uc.mem_write(a3,bytes(uc.mem_read(a4,a5))); uc.reg_write(UC_PPC_REG_3,a3)
        elif name=='memset':
            uc.mem_write(a3,bytes([a4&0xFF])*a5); uc.reg_write(UC_PPC_REG_3,a3)
        elif name=='strlen':
            uc.reg_write(UC_PPC_REG_3,len(bytes(uc.mem_read(a3,65536)).split(b'\0')[0]))
        elif name=='malloc':
            uc.reg_write(UC_PPC_REG_3,alloc(a3))
        elif name=='pthread_getspecific':
            uc.reg_write(UC_PPC_REG_3,TLS)
        else:
            uc.reg_write(UC_PPC_REG_3,0)
        uc.reg_write(UC_PPC_REG_PC,uc.reg_read(UC_PPC_REG_LR))
uc.hook_add(UC_HOOK_CODE,hook)
uc.mem_write(SESS+6380,b'http://x/t.aa\0')
uc.mem_write(SESS+16626,b'\0'*1025); uc.mem_write(SESS+17651,b'\0'*4097)
uc.mem_write(SESS+21756,struct.pack('>I',0)); uc.mem_write(SESS+30028,struct.pack('>I',0))
uc.mem_write(SESS+30032,struct.pack('>I',0))
spcell=HEAP+0x800; uc.mem_write(spcell,struct.pack('>I',STREAM))
try:
    for reg,v in ((UC_PPC_REG_3,SESS),(UC_PPC_REG_4,spcell),(UC_PPC_REG_5,SESS+17651),(UC_PPC_REG_6,32)):
        uc.reg_write(reg,v)
    uc.reg_write(UC_PPC_REG_LR,RET); uc.reg_write(UC_PPC_REG_1,STACK+0x300000)
    uc.emu_start(0x101152f8,RET,count=500000)
    status='returned'
except UcError as e:
    status='ERR %s pc=%s lr=%s'%(e,hex(uc.reg_read(UC_PPC_REG_PC)),hex(uc.reg_read(UC_PPC_REG_LR)))
print(POCFILE.split('/')[-1], status, 'events:',len(events))
for e in events[:30]: print(' ',e)
