#!/usr/bin/env python3
"""Decrypt sonoscrypto 886499ca envelopes from the 86.10-80260-1-12.upd."""
import struct, subprocess, sys, os

MAGIC=b"\x49\x7f\x16\x35"; ENV=bytes.fromhex("886499ca")
d=open("captures/firmware/86.10-80260-1-12.upd","rb").read()

def sections(d):
    o=0
    while o<len(d):
        if d[o:o+4]!=MAGIC: break
        t,tot,_=struct.unpack_from("<III",d,o+4)
        if tot<16: break
        yield t,o,tot
        o+=tot

secs=list(sections(d))
for t,o,tot in secs: print("section",t,"@",hex(o),"len",hex(tot))

def unwrap(payload, keypem):
    # per build_rootfs doc: ENV, u32be hdr_len, u32be 0x10000, 8 zeros, u8 rlen, u8 3, recipient,
    # u32be 1, u32be rsa_len, OAEP key, u32be 1, u32be data_len, IV(16)+ct, AES-128-CBC zero IV
    assert payload[:4]==ENV
    o=0x14
    rlen=payload[o]; o+=1
    tag=payload[o]; o+=1
    recip=payload[o:o+rlen]; o+=rlen
    one,rsa_len=struct.unpack(">II",payload[o:o+8]); o+=8
    rsa=payload[o:o+rsa_len]; o+=rsa_len
    one2,data_len=struct.unpack(">II",payload[o:o+8]); o+=8
    blob=payload[o:o+data_len]
    iv,ct=blob[:16],blob[16:]
    open("/tmp/s2decode/rsa.bin","wb").write(rsa)
    r=subprocess.run(["openssl","pkeyutl","-decrypt","-inkey",keypem,"-pkeyopt","rsa_padding_mode:oaep","-pkeyopt","rsa_oaep_md:sha1","-in","/tmp/s2decode/rsa.bin"],capture_output=True)
    if r.returncode!=0:
        print("rsa decrypt fail:",r.stderr.decode()[:200]); return None
    aes=r.stdout
    p=subprocess.run(["openssl","enc","-d","-aes-128-cbc","-K",aes.hex(),"-iv",iv.hex()],input=ct,capture_output=True)
    if p.returncode!=0:
        print("aes fail:",p.stderr.decode()[:200]); return None
    return p.stdout

key="captures/firmware/86.10-80260-1-12/model12-private.pem"
for t,o,tot in secs:
    p=d[o+16:o+tot]
    if p[:4]!=ENV: continue
    out=unwrap(p,key)
    if out:
        fn=f"/tmp/s2decode/sect_{t}.bin"
        open(fn,"wb").write(out)
        print(f"section {t}: decrypted {len(out)} bytes -> {fn}; head={out[:20].hex()}")
