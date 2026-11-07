#!/usr/bin/env python3
"""Structure-aware fuzzer v2: targeted mutations on the type-21/22 pre-signature parsers."""
import struct, subprocess, os, random, sys, shutil, hashlib

MAGIC = bytes.fromhex("497f1635")
BASE = "/tmp/65130_base.upd"
CHROOT = "/tmp/65130rootfs"
OUTDIR = "/tmp/fuzz_out"

def parse_sections(d):
    sections = []
    o = 0x1C
    while o + 16 <= len(d):
        magic, t, ln = struct.unpack_from("<III", d, o)
        if d[o:o+4] != MAGIC: break
        sections.append((o, t, ln))
        o += ln
    return sections

def build(base_data, sections, overrides):
    """Rebuild the file with section overrides: {index: new_body} or {index: new_len}."""
    result = bytearray(base_data[:0x1C])
    for i, (o, t, ln) in enumerate(sections):
        if i in overrides:
            body = overrides[i]
            result += MAGIC + struct.pack("<III", t, 16+len(body), 0) + body
        else:
            result += MAGIC + struct.pack("<III", t, ln, 0) + bytes(base_data[o+16:o+ln])
    return bytes(result)

def run_chroot(mutant_data, i):
    dest = os.path.join(CHROOT, "root", f"fuzz_v2_{i}.upd")
    with open(dest, "wb") as f:
        f.write(mutant_data)
    url = f"http://127.0.0.1:8899/fuzz_v2_{i}.upd"
    cmd = ["sudo", "chroot", CHROOT, "/usr/bin/qemu-ppc", "/bin/upgrade", url]
    try:
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=30, input="")
        return r.stdout + r.stderr
    except subprocess.TimeoutExpired:
        return "TIMEOUT"
    finally:
        os.unlink(dest)

def main():
    d = open(BASE, "rb").read()
    sections = parse_sections(d)
    print(f"base: {len(d)} bytes, sections: {[(t,l) for _,t,l in sections]}")
    rng = random.Random(1337)
    n = int(sys.argv[1]) if len(sys.argv) > 1 else 200
    crashes = 0
    anomalies = 0
    for i in range(n):
        strategy = rng.randint(0, 7)
        target_idx = rng.choice([0, 4])  # type-21 (idx 0) or type-22 (idx 4)
        o, t, ln = sections[target_idx]
        body = bytearray(d[o+16:o+ln])
        if strategy == 0:
            # 128KB boundary: corrupt bytes near offset 0x20000 (131072)
            pos = rng.randint(0x20000-64, 0x20000+64)
            if pos < len(body): body[pos] ^= rng.randint(1, 255)
        elif strategy == 1:
            # length field lies in the type-21/22 header
            pos = rng.randint(0, min(32, len(body)-4))
            struct.pack_into("<I", body, pos, rng.choice([0, 0xFFFFFFFF, 0x7FFF, 0x10000, len(body)*2]))
        elif strategy == 2:
            # type-21 series count corruption
            if t == 21 and len(body) > 32:
                pos = rng.randint(0, len(body)-32)
                # find and corrupt a PATCHES header
                for j in range(pos, len(body)-8):
                    if body[j:j+8] == b"_PATCHES":
                        struct.pack_into("<I", body, max(0,j-4), rng.choice([0, 0xFFFF, 0x8000]))
                        break
        elif strategy == 3:
            # type-22 header version/format corruption
            if t == 22 and len(body) > 16:
                pos = rng.randint(0, 32)
                body[pos] ^= rng.randint(1, 255)
        elif strategy == 4:
            # truncate the type-22 body (the 128KB reassembly boundary test)
            cut = rng.randint(0x20000-0x100, 0x20000+0x100)
            body = body[:cut]
        elif strategy == 5:
            # extend the type-22 past the reassembly buffer
            body = body + bytes(0x10000 + rng.randint(0, 0x1000))
        elif strategy == 6:
            # mutate the section TYPE field (change type-21 to something else)
            pass  # handled by the build function
        elif strategy == 7:
            # double-fetch: corrupt the body AND truncate
            if len(body) > 0x20000:
                body[0x20000] ^= 0xFF
                body = body[:0x20000+rng.randint(-64, 64)]
        mutant = build(d, sections, {target_idx: bytes(body)})
        output = run_chroot(mutant, i)
        has_crash = any(k in output for k in ["SIGSEGV", "SIGABRT", "Aborted", "Segmentation", "assert"])
        has_unusual = any(k in output for k in ["Buffer overflow", "corrupt", "invalid", "error: (2", "error: (4"])
        if has_crash:
            crashes += 1
            print(f"*** CRASH fuzz-{i:04d} strategy={strategy} type={t}: {output[-200:]}", flush=True)
        elif has_unusual:
            anomalies += 1
            print(f"  ~ anomaly fuzz-{i:04d} strategy={strategy} type={t}: {output[-150:]}", flush=True)
        if i % 50 == 0:
            print(f"  progress: {i}/{n} crashes={crashes} anomalies={anomalies}", flush=True)
    print(f"\ndone: {n} mutants, {crashes} crashes, {anomalies} anomalies", flush=True)

if __name__ == "__main__":
    main()
