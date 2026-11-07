#!/usr/bin/env python3
"""Structure-aware fuzzer for the Sonos .upd pre-signature parsers.
Generates mutations of a genuine package preserving the section chain structure
(but mutating type-21/type-22 content, lengths, and type fields).
Each mutant is run through the real upgrade binary in the chroot.
A crash/hang/abort = a candidate."""
import struct, subprocess, os, random, sys, hashlib, shutil

MAGIC = bytes.fromhex("497f1635")
BASE = "captures/firmware/57.22-65130-1-16.upd"
CHROOT = "/tmp/65130rootfs"
OUTDIR = "/tmp/s2decode/fuzz"

def parse_sections(d):
    sections = []
    o = 0x1C
    while o + 16 <= len(d):
        magic, t, ln = struct.unpack_from("<III", d, o)
        if d[o:o+4] != MAGIC: break
        sections.append((o, t, ln))
        o += ln
    return sections

def build_mutant(d, sections, mutations):
    """Apply mutations and rebuild the file preserving section order."""
    result = bytearray(d[:0x1C])  # the file header
    for o, t, ln in sections:
        body = bytes(d[o+16:o+ln])
        # apply mutations to this section
        if (t, 'body') in mutations:
            body = mutations[(t, 'body')](body)
        if (t, 'len') in mutations:
            ln = mutations[(t, 'len')](ln)
        result += MAGIC + struct.pack("<III", t, ln, 0) + body
    return bytes(result)

def mutate_bytes(body, rng):
    """Random byte mutations in the body."""
    b = bytearray(body)
    n = rng.randint(1, max(1, len(b)//20))
    for _ in range(n):
        pos = rng.randint(0, len(b)-1)
        b[pos] = rng.randint(0, 255)
    return bytes(b)

def mutate_truncate(body, rng):
    """Truncate the body."""
    cut = rng.randint(1, max(1, len(body)-1))
    return body[:cut]

def mutate_extend(body, rng):
    """Extend the body with random data or 0x00."""
    n = rng.randint(1, 0x1000)
    return body + bytes(rng.randint(0,255) for _ in range(n))

def mutate_len_lie(body, rng):
    """Corrupt length fields inside the body."""
    b = bytearray(body)
    if len(b) > 8:
        pos = rng.randint(0, len(b)-8)
        struct.pack_into("<I", b, pos, rng.choice([0, 0xFFFFFFFF, 0x80000000, 0x7FFFFFFF]))
    return bytes(b)

def run_chroot(pkg_path, timeout=30):
    """Run the upgrade binary in the chroot against this package."""
    dest = os.path.join(CHROOT, "tmp", "fuzz_pkg.upd")
    shutil.copy2(pkg_path, dest)
    cmd = ["sudo", "chroot", CHROOT, "/usr/bin/qemu-ppc", "/bin/upgrade",
           "http://127.0.0.1:8899/fuzz_pkg.upd"]
    try:
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout, input="")
        return r.stdout + r.stderr, r.returncode
    except subprocess.TimeoutExpired:
        return "TIMEOUT", -1

def main():
    d = open(BASE, "rb").read()
    sections = parse_sections(d)
    print(f"base: {len(d)} bytes, {len(sections)} sections: {[(t,l) for _,t,l in sections]}")
    os.makedirs(OUTDIR, exist_ok=True)
    rng = random.Random(42)
    n_fuzz = int(sys.argv[1]) if len(sys.argv) > 1 else 50
    crashes = 0
    for i in range(n_fuzz):
        # pick a mutation strategy
        strategy = rng.choice(["bytes", "truncate", "extend", "len_lie"])
        target_type = rng.choice([21, 22])  # type-21 or type-22
        # find the section
        target_off = None
        for o, t, ln in sections:
            if t == target_type: target_off, target_ln = o, ln; break
        if target_off is None: continue
        body = d[target_off+16:target_off+target_ln]
        # apply the mutation
        mut_fn = {"bytes": mutate_bytes, "truncate": mutate_truncate,
                  "extend": mutate_extend, "len_lie": mutate_len_lie}[strategy]
        new_body = mut_fn(body, rng)
        # build the mutant
        mutations = {(target_type, 'body'): lambda b, nb=new_body: nb}
        mutant = build_mutant(d, sections, mutations)
        # write and test
        pkg = os.path.join(OUTDIR, f"fuzz_{i:04d}.upd")
        open(pkg, "wb").write(mutant)
        output, rc = run_chroot(pkg)
        # detect anomalies
        has_crash = "SIGSEGV" in output or "SIGABRT" in output or "Aborted" in output
        has_unusual = any(k in output for k in ["Segmentation", "core dumped", "assert", "Buffer overflow"])
        if has_crash or has_unusual or rc not in (0, 1, 10, 30):
            crashes += 1
            print(f"*** FUZZ-{i:04d}: {strategy} on type-{target_type} → CRASH/ANOMALY (rc={rc})")
            print(f"    output: {output[-300:]}")
        elif i % 10 == 0:
            # extract the verdict
            verdict = "unknown"
            for line in output.splitlines():
                if "Upgrade failed" in line or "upgrade to version" in line:
                    verdict = line.strip()[-60:]
                    break
            print(f"  fuzz-{i:04d}: {strategy} on type-{target_type} → {verdict}")
    print(f"\ndone: {n_fuzz} mutants, {crashes} crashes/anomalies")

if __name__ == "__main__":
    main()
