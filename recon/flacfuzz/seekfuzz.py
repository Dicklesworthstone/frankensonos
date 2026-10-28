import json, os, random, subprocess, sys
DEC="/tmp/seek134"; OUT=os.environ.get("OUT","/tmp/seek81v.jsonl")
seeds=[open(p,"rb").read() for p in sys.argv[1:]]
rng=random.Random(4242)
counts={}
with open(OUT,"w") as sink:
    for case in range(300):
        src=bytearray(seeds[case%len(seeds)])
        for _ in range(rng.randint(1,10)):
            op=rng.randint(0,3); pos=rng.randrange(len(src)) if src else 0
            if op==0 and src: src[pos]^=1<<rng.randint(0,7)
            elif op==1 and src: src[pos]=rng.choice([0,0xFF,0x7F,0x80,1,0xFE])
            elif op==2 and len(src)>40: del src[pos+1:]
            else: src[pos:pos]=src[pos:min(len(src),pos+rng.randint(1,32))]
        path="/tmp/sf_%d.flac"%case
        open(path,"wb").write(bytes(src))
        target=rng.choice([0,1,50,100,255,256,257,1000,2**63,2**63-1,2**64-1])
        try:
            r=subprocess.run([DEC,path,str(target)],capture_output=True,timeout=8)
            rc,err=r.returncode,r.stderr.decode("latin-1")
        except subprocess.TimeoutExpired:
            rc,err=124,""
        cls = "TIMEOUT" if rc==124 else ("BAD_rc%d"%rc if (rc not in (0,) or "Sanitizer" in err or "runtime error" in err) else "OK")
        counts[cls]=counts.get(cls,0)+1
        if cls!="OK":
            rec={"case":case,"target":target,"class":cls,"err":err[-260:]}
            sink.write(json.dumps(rec)+"\n"); sink.flush()
            print(case,cls,target,err[-160:].replace("\n"," | "),flush=True)
        os.unlink(path)
print(counts)
