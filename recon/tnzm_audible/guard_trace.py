#!/usr/bin/env python3
"""Guard-set trace for the 0x10115478 unchecked-strcpy path (mgs7 re-arm task).

Emulates 0x101152f8 (57.23 audible container parse) with fake IO per the
session-4 harness, tracing the guard chain CalmHawk decoded (commit 9ad85b19):
  guards: 0x10115444/4C/60/64/6C/74
  milestones: 0x10115478 (UNCHECKED strcpy), 0x10115494 (capped), 0x101156B0 (far), 0x1011564C
Conditions under test: [r1+8880]!=0, r28==0, [r26]==0, r22==0.
"""
import struct, json, sys
from elftools.elf.elffile import ELFFile
from unicorn import *
from unicorn.ppc_const import *

ELF='captures/firmware/57.23-74170-1-16/anacapad'
SESS=0x52000000; STACK=0x62000000; HEAP=0x54000000; POC=0x55000000; TLS=0x56000000
EMU=0x1052d000; RET=0x48000000
GUARDS={0x10115444:'cmpwi cr4,r28,0', 0x1011544C:'bne cr4 -> 0x10115458',
        0x10115460:'cmpwi [r26],0 / bne->0x101156B0', 0x10115464:'beq cr4 -> 0x101154F8',
        0x1011546C:'cmpwi cr4,r22,0 / beq->0x1011564C', 0x10115474:'bne cr4 -> 0x10115504'}
MILE={0x10115478:'UNCHECKED strcpy(session+6380, node)', 0x10115494:'CAPPED path (+7405, 0x400 cap)',
      0x101156B0:'FAR path', 0x1011564C:'epilogue branch'}

def build(poc):
    f=open(ELF,'rb'); ef=ELFFile(f)
    uc=Uc(UC_ARCH_PPC, UC_MODE_PPC32+UC_MODE_BIG_ENDIAN)
    for seg in ef.iter_segments():
        if seg['p_type']!='PT_LOAD': continue
        va=seg['p_vaddr']; sz=seg['p_memsz']
        lo=va&~0xFFF; hi=(va+sz+0xFFF)&~0xFFF
        if hi>lo: uc.mem_map(lo,hi-lo); uc.mem_write(va,seg.data()[:seg['p_filesz']])
    plt=json.load(open('/tmp/ppc_plt_map.json'))
    uc.mem_map(EMU,0xF000)
    slotmap={}
    for i,off_s in enumerate(sorted(plt, key=lambda s:int(s,16))):
        off=int(off_s,16); tgt=EMU+i*64
        delta=(tgt-off)&0x3FFFFFC
        if delta&0x2000000: delta-=0x4000000
        if abs(delta)>=0x2000000: continue
        uc.mem_write(off,struct.pack('>I',0x48000000|(delta&0x3FFFFFC)))
        slotmap[tgt]=plt[off_s]
    uc.mem_map(SESS,0x100000); uc.mem_map(STACK,0x800000); uc.mem_map(HEAP,0x1000000)
    uc.mem_map(POC,0x20000); uc.mem_map(TLS,0x10000); uc.mem_map(0x0,0x100000)
    uc.mem_map(RET,0x1000); uc.mem_write(RET,struct.pack('>I',0x4e800020))
    uc.mem_write(STACK,struct.pack('>I',RET)*(0x800000//4))
    uc.reg_write(UC_PPC_REG_13,0x105459b0+0x8000); uc.reg_write(UC_PPC_REG_2,0x10545664+0x8000)
    uc.mem_write(POC,poc)
    STREAM=0x57000000; uc.mem_map(STREAM,0x40000); uc.mem_write(STREAM,struct.pack('>I',1))
    FAKELOG=0x57500000; uc.mem_map(FAKELOG,0x1000)
    uc.mem_write(FAKELOG,struct.pack('>I',FAKELOG))
    uc.mem_write(FAKELOG+0x100,struct.pack('>II',0x38600000,0x4e800020))
    uc.mem_write(FAKELOG+8,struct.pack('>I',FAKELOG+0x100))
    uc.mem_write(0x10545d68,struct.pack('>I',FAKELOG))
    return uc,slotmap

def run(pocfile):
    poc=open(pocfile,'rb').read()
    uc,slotmap=build(poc)
    heap=[HEAP+0x1000]; toc=[4]; events=[]
    def alloc(n,align=16):
        a=(heap[0]+align-1)&~(align-1); heap[0]=a+n; return a
    def hook(uc,addr,size,ud):
        if addr in GUARDS:
            r1=uc.reg_read(UC_PPC_REG_1)
            flag=uc.mem_read(r1+8880,1)[0]
            events.append(('guard',hex(addr),GUARDS[addr],
                           'r28=%d r22=%d [r26]=%d flag8880=%d'%(
                               uc.reg_read(UC_PPC_REG_28),uc.reg_read(UC_PPC_REG_22),
                               uc.mem_read(uc.reg_read(UC_PPC_REG_26),1)[0],flag)))
            return
        if addr in MILE:
            a3=uc.reg_read(UC_PPC_REG_3); a4=uc.reg_read(UC_PPC_REG_4)
            src=bytes(uc.mem_read(a4,131072)).split(b'\0')[0] if a4>0x1000 else b''
            events.append(('MILE',hex(addr),MILE[addr],'dst=%s srclen=%d'%(hex(a3),len(src))))
            if addr==0x10115478:
                uc.mem_write(a3,src+b'\0')
            uc.reg_write(UC_PPC_REG_PC,uc.reg_read(UC_PPC_REG_LR))
            return
        if addr==0x100149a8:
            buf=uc.reg_read(UC_PPC_REG_4); p=toc[0]
            chunk=poc[p:p+12]
            uc.mem_write(buf,chunk+b'\0'*(12-len(chunk)))
            events.append(('toc',p,poc[p] if p<len(poc) else 0))
            toc[0]+=9
            uc.reg_write(UC_PPC_REG_3,12)
            uc.reg_write(UC_PPC_REG_PC,uc.reg_read(UC_PPC_REG_LR)); return
        if addr==0x100a5510:
            uc.reg_write(UC_PPC_REG_3,1); uc.reg_write(UC_PPC_REG_PC,uc.reg_read(UC_PPC_REG_LR)); return
        if addr==0x100a374c:
            uc.reg_write(UC_PPC_REG_3,0); uc.reg_write(UC_PPC_REG_PC,uc.reg_read(UC_PPC_REG_LR)); return
        if addr in (0x101a8f20,0x101a946c,0x101a999c):
            uc.reg_write(UC_PPC_REG_3,0)
            uc.reg_write(UC_PPC_REG_PC,uc.reg_read(UC_PPC_REG_LR)); return
        if addr==0x10346d1c:
            events.append(('data_read',hex(uc.reg_read(UC_PPC_REG_4))))
            uc.reg_write(UC_PPC_REG_3,0)
            uc.reg_write(UC_PPC_REG_PC,uc.reg_read(UC_PPC_REG_LR)); return
        name=slotmap.get(addr)
        if name:
            a3=uc.reg_read(UC_PPC_REG_3); a4=uc.reg_read(UC_PPC_REG_4); a5=uc.reg_read(UC_PPC_REG_5)
            if name in ('strlcpy','strncpy'):
                src=bytes(uc.mem_read(a4,131072)).split(b'\0')[0]
                uc.mem_write(a3,src[:max(0,a5-1)]+b'\0'); uc.reg_write(UC_PPC_REG_3,a4)
                events.append(('strlcpy',hex(a3),len(src),a5))
            elif name=='strcpy':
                src=bytes(uc.mem_read(a4,131072)).split(b'\0')[0]
                uc.mem_write(a3,src+b'\0'); uc.reg_write(UC_PPC_REG_3,a3)
                events.append(('strcpy',hex(a3),len(src)))
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
    spcell=HEAP+0x800; uc.mem_write(spcell,struct.pack('>I',0x57000000))
    try:
        for reg,v in ((UC_PPC_REG_3,SESS),(UC_PPC_REG_4,spcell),(UC_PPC_REG_5,SESS+17651),(UC_PPC_REG_6,32)):
            uc.reg_write(reg,v)
        uc.reg_write(UC_PPC_REG_LR,RET); uc.reg_write(UC_PPC_REG_1,STACK+0x700000)
        uc.emu_start(0x101152f8,RET,count=3000000)
        status='returned'
    except UcError as e:
        status='ERR %s pc=%s'%(e,hex(uc.reg_read(UC_PPC_REG_PC)))
    return status,events

if __name__=='__main__':
    for f in sys.argv[1:]:
        st,ev=run(f)
        print('='*70)
        print(f,'->',st,'events:',len(ev))
        for e in ev:
            if e[0] in ('guard','MILE','toc'): print('  ',e)
        hits=[e for e in ev if e[0]=='MILE' and 'UNCHECKED' in e[2]]
        print('  UNCHECKED strcpy fired:',bool(hits))
