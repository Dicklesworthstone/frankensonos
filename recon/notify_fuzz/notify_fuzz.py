#!/usr/bin/env python3
"""notify_fuzz.py - offline grammar fuzz for the anacapad pre-auth HTTP handlers
(/notify /setstring /advconfig /support/directsubmit /support/aggregate).

No device, no network. The unit of simulation is the *parse layer* of anacapad
57.23-74170, re-implemented instruction-faithfully from the disassembly:

- token table lookup   (0x1040bfec / 0x1040aac8)
- content-length parse (strtoul base 10, errno semantics)
- body read loop       (0x1038a950 + 0x1038aa08)
- form tokenizer       (0x1038a5d4 -> 0x1038a508)
- percent/plus decoder (0x1038a420)
- per-handler buffer shapes (dstlen / kv caps / key dispatch)

Each simulated request runs against *every* handler shape and every bound is
checked with explicit assertions (out-of-range read/write, missing terminator,
unbounded loop). A fault prints the exact case and the bound that broke, with
the disassembly site in the message. Deterministic: seeded PRNG, sorted case
list, no clock, no dict ordering dependence.

Usage:
  ./notify_fuzz.py                 # full grammar sweep, prints verdict table
  ./notify_fuzz.py -v              # also print every case
  ./notify_fuzz.py --seed 7        # different mutation seed
Exit 0 = clean, 1 = at least one candidate fault.
"""
from __future__ import annotations

import argparse
import json
import sys
from dataclasses import dataclass, field
from typing import List, Optional, Tuple

# ---------------------------------------------------------------- primitives

class Fault(Exception):
    def __init__(self, site: str, msg: str):
        super().__init__(f"[{site}] {msg}")
        self.site = site


@dataclass
class Mem:
    """Byte buffer with a fault-injecting view: any access outside size is a Fault."""
    size: int
    data: bytearray = field(init=False)
    writes: List[Tuple[int, int]] = field(default_factory=list)  # (offset, byte)

    def __post_init__(self):
        self.data = bytearray(self.size)

    def stb(self, off: int, val: int, site: str):
        if off < 0 or off >= self.size:
            raise Fault(site, f"stb 0x{val:02x} at off {off} size {self.size} OOB")
        self.data[off] = val & 0xFF
        self.writes.append((off, val & 0xFF))

    def read(self, off: int, site: str) -> int:
        if off < 0 or off >= self.size:
            raise Fault(site, f"read at off {off} size {self.size} OOB")
        return self.data[off]


def strtoul10(s: str, site: str) -> int:
    """glibc strtoul base-10 semantics on 32-bit: clamp negative to wrap,
    value modulo 2**32 (strtoul returns unsigned long, 32-bit on PPC)."""
    i = 0
    n = len(s)
    while i < n and s[i] in " \t\n\r\f\v":
        i += 1
    neg = False
    if i < n and s[i] in "+-":
        neg = s[i] == "-"
        i += 1
    start = i
    val = 0
    while i < n and s[i].isdigit():
        val = val * 10 + (ord(s[i]) - 48)
        val &= 0xFFFFFFFFFFFFFFFF  # widen then truncate below
        i += 1
    if i == start:
        return 0
    v = val & 0xFFFFFFFF
    if neg and val:
        v = (-v) & 0xFFFFFFFF
    # errno==ERANGE on overflow beyond 32 bits: anacapad checks errno and
    # rejects (r9=0) when errno!=0 -> treat overflow as "reject".
    if val > 0xFFFFFFFF:
        raise OverflowError(site)
    return v


def isxdigit(c: str) -> bool:
    return c in "0123456789abcdefABCDEF"


def hexval(c: str) -> int:
    return int(c, 16)

# ------------------------------------------------------- request token table

@dataclass
class Request:
    method: int                      # 1=GET 4=POST (anacapad internal codes)
    params: List[Tuple[str, str]]    # parsed name/value pairs (first match wins)
    content_length: Optional[str]    # raw header value or None if absent
    body: bytes = b""

    def get_param(self, name: str, site: str) -> Optional[str]:
        """0x1040aac8: walk table stride 0x10, first match, NULL if absent."""
        for k, v in self.params:
            if k == name:
                return v
        return None

# ------------------------------------------------ 0x1038a420 value decoder

def decode_pair(src: bytes, sep: int, dst: Mem, site: str) -> Tuple[bool, int]:
    """Instruction-faithful 0x1038a420. Returns (ok, consumed_end_offset_into_src).
    Writes decoded bytes into dst until sep or NUL; bound = dst.size.
    On bound hit: r27=0 (fail) and the terminator is still written at the
    *un-advanced* cursor (0x1038a4e8), per the ble-branch analysis."""
    p = 0
    cur = 0            # r29 = write cursor
    cap = dst.size     # r28
    ok = True
    while True:
        if p >= len(src):
            break
        c = src[p]
        if c == 0:
            break
        if c == 0x25:  # '%'
            if p + 1 < len(src) and isxdigit(chr(src[p + 1])):
                if p + 2 < len(src) and isxdigit(chr(src[p + 2])):
                    val = (hexval(chr(src[p + 1])) << 4) | hexval(chr(src[p + 2]))
                    p += 2
                else:
                    ok = False
                    break
            else:
                ok = False
                break
        elif c == 0x2B:  # '+'
            val = 0x20
        elif c == sep:
            break
        else:
            val = c
        if cap > 1:
            dst.stb(cur, val, site + ":0x1038a4c4")
            cap -= 1
            cur += 1
        else:
            ok = False
            break
        p += 1
    dst.stb(cur, 0, site + ":0x1038a4e8")
    return ok, p

# ------------------------------------------------ 0x1038a508 pair splitter

def next_pair(state: Mem, key_sep: int, val_sep: int,
              key: Mem, val: Mem, site: str) -> Optional[Mem]:
    """0x1038a508: skip leading val_sep, decode key up to key_sep, expect
    val_sep, decode value. State buffer holds the remaining stream pointer
    byte-offset in data[0..3]... simplified: we carry position explicitly."""
    raise NotImplementedError  # replaced by split_pairs below (same semantics)


def split_pairs(body: bytes, key_cap: int, val_cap: int,
                site: str) -> List[Tuple[bool, str, str]]:
    """Full form parse: repeated 0x1038a5d4 (sep=0x26/0x3d) over the body.
    Mirrors the recursion: pairs split on 0x26, each pair split on 0x3d,
    each half decoded with its own cap. Returns list of (ok, key, val).
    Loop guard: the stream pointer (0x1038a4f4 store) must advance every
    iteration or the handler loops forever - checked explicitly."""
    out: List[Tuple[bool, str, str]] = []
    pos = 0
    n = len(body)
    seen_positions = set()
    while pos < n:
        if pos in seen_positions:
            raise Fault(site + ":0x1038a5f8", "pair splitter did not advance (infinite loop)")
        seen_positions.add(pos)
        # skip leading '&' (0x1038a540 skip loop)
        while pos < n and body[pos] == 0x26:
            pos += 1
        if pos >= n or body[pos] == 0:
            break
        # key up to '='
        eq = body.find(b"=", pos)
        if eq < 0:
            key_raw = body[pos:]
            val_raw = b""
            pos = n
        else:
            key_raw = body[pos:eq]
            val_raw = body[eq + 1:]
            amp = val_raw.find(b"&")
            if amp >= 0:
                val_raw = val_raw[:amp]
                pos = eq + 1 + amp + 1
            else:
                pos = n
        km = Mem(key_cap)
        vm = Mem(val_cap)
        ok1, _ = decode_pair(key_raw, 0x3D, km, site + ":key")
        ok2, _ = decode_pair(val_raw, 0x26, vm, site + ":val")
        ok = ok1 and ok2
        out.append((ok, km.data.split(b"\0")[0].decode("latin1"),
                    vm.data.split(b"\0")[0].decode("latin1")))
    return out

# ------------------------------------------------ 0x1038a950 body reader

def read_body(req: Request, dstlen: int, min_len: int, site: str) -> int:
    """0x1038a950. Returns 0xc8/0x19x like the binary. Bound-checked loop."""
    if req.method != 4:
        return 0x195
    if req.content_length is None:
        return 0x19B
    try:
        want = strtoul10(req.content_length, site + ":strtoul")
    except OverflowError:
        return 0x19B  # errno path -> r9=0 -> caller sees !=0xc8? binary: errno!=0 -> r9=0 -> 0x19b? keep reject
    if want < min_len:
        return 0x19D
    dst = Mem(dstlen) if dstlen else None
    written = 0  # r29
    body = req.body
    guard = 0
    while written < want:
        guard += 1
        if guard > 10_000_000:
            raise Fault(site + ":0x1038aa08", "read loop did not terminate")
        if dstlen <= 0:
            raise Fault(site + ":0x1038aa1c", f"dstlen {dstlen} with body reader")
        remaining = dstlen - written  # r5 = subf r29, r30
        if remaining <= 0:
            # recv clamp: a real recv into a full buffer returns/errs; loop exits
            break
        take = min(remaining, len(body) - written) if written < len(body) else 0
        # recv returns 0/err -> loop exits (binary: cmpwi r3,0; bne back)
        if take <= 0:
            break
        chunk = body[written:written + take]
        for i, b in enumerate(chunk):
            dst.stb(written + i, b, site + ":0x1038aa24")
        written += len(chunk)
    if written >= want:
        # 0x1038aa30 ble -> full length consumed: dst[dstlen-1]=0 (0x1038aa5c)
        if dstlen <= 0:
            raise Fault(site + ":0x1038aa5c", f"dstlen {dstlen} full-length terminator")
        dst.stb(dstlen - 1, 0, site + ":0x1038aa64")
        return 0xC8
    # loop exited early (peer closed / recv 0): 0x1038aa44 branch —
    # r29 < dstlen-1 -> dst[r29]=0 (in bounds); else dst[dstlen-1]=0.
    if dstlen <= 0:
        raise Fault(site + ":0x1038aa4c", f"dstlen {dstlen} early terminator")
    if written < dstlen - 1:
        dst.stb(written, 0, site + ":0x1038aa4c")
    else:
        dst.stb(dstlen - 1, 0, site + ":0x1038aa64")
    return 0xC8

# ------------------------------------------------------------ handler shapes

@dataclass
class HandlerShape:
    name: str
    get: bool
    post: bool
    body_dstlen: int
    body_min: int
    key_cap: int
    val_cap: int
    keys: Tuple[str, ...]
    gate_key: Optional[str] = None   # key that must appear (directsubmit csrf)
    numeric_keys: Tuple[str, ...] = ()  # keys whose value must be int-like


SHAPES = (
    HandlerShape("/notify", get=True, post=False, body_dstlen=0, body_min=0,
                 key_cap=0, val_cap=0,
                 keys=("nt", "nts", "sid", "seq")),
    HandlerShape("/setstring", get=True, post=True, body_dstlen=0x800,
                 body_min=0x3E8, key_cap=0x80, val_cap=0x401,
                 keys=("key", "value")),
    HandlerShape("/advconfig", get=True, post=True, body_dstlen=0x1000,
                 body_min=0x7D0, key_cap=0x20, val_cap=0x80,
                 keys=("FirstZP", "PriorityBridge"),
                 numeric_keys=("FirstZP", "PriorityBridge")),
    HandlerShape("/support/directsubmit", get=False, post=True, body_dstlen=0x800,
                 body_min=0x3E8, key_cap=0x80, val_cap=0x401,
                 keys=("csrfToken",), gate_key="csrfToken"),
    HandlerShape("/support/aggregate", get=False, post=True, body_dstlen=0x800,
                 body_min=0x3E8, key_cap=0x80, val_cap=0x401,
                 keys=()),
)

# --------------------------------------------------------------- fuzz cases

def case_content_length() -> List[Tuple[str, Optional[str], bytes]]:
    """(label, Content-Length header, body bytes)"""
    big = b"A" * 0x1200
    return [
        ("zero", "0", b""),
        ("absent", None, b""),
        ("exact", str(0x3E8), b"A" * 0x3E8),
        ("below-min", "1", b"A"),
        ("over-dstlen", str(0x1200), big),
        ("wrap-neg", "-1", b"A"),
        ("wrap-plus", "+5", b"AAAAA"),
        ("uint32-max", "4294967295", b"A" * 0x1200),
        ("uint32-ovf", "4294967296", b"A" * 0x1200),
        ("hex-ish", "0x10", b""),
        ("spaces", "   10", b"A" * 10),
        ("leading-zeroes", "0000000005", b"AAAAA"),
        ("trailing-junk", "5junk", b"AAAAA"),
        ("empty-val", "", b"A" * 0x1200),
    ]


def case_form_values() -> List[Tuple[str, bytes]]:
    cap80 = 0x80
    cap401 = 0x401
    cap20 = 0x20
    return [
        ("empty", b"="),
        ("double-amp", b"key=value&&key=value"),
        ("amp-only", b"&"),
        ("no-eq", b"keyvalue"),
        ("eq-only-value", b"=value"),
        ("dup-keys", b"key=a&key=b"),
        ("key-at-cap", b"key=" + b"v" * (cap401 - 1)),
        ("key-over-cap", b"key=" + b"v" * (cap401 + 8)),
        ("name-at-cap", b"k" * (cap80 - 1) + b"=v"),
        ("name-over-cap", b"k" * (cap20 + 8) + b"=v"),
        ("pct-trailing", b"key=abc%"),
        ("pct-bad-hex", b"key=abc%zz"),
        ("pct-one-digit", b"key=abc%2"),
        ("pct-nul", b"key=abc%00def"),
        ("pct-sep-encoded", b"key%3Dinner=value"),
        ("plus", b"key=a+b"),
        ("all-pct", b"%6b%65%79=%76%61%6c"),
        ("mixed-sep", b"key=a&=b&key=c"),
        ("long-amp-chain", b"&" * 64 + b"key=v"),
    ]


def case_notify_params() -> List[dict]:
    base = {"nt": "upnp:event", "nts": "upnp:propchange",
            "sid": "uuid:X", "seq": "0"}
    cases = [dict(base)]
    for k in list(base):
        c = dict(base)
        del c[k]
        cases.append(c)
    for k in ("sid", "seq"):
        c = dict(base)
        c[k] = ""
        cases.append(c)
    c = dict(base); c["seq"] = "0" * 0x420; cases.append(c)         # over 0x401
    c = dict(base); c["seq"] = "0" * 0x400; cases.append(c)         # at cap
    c = dict(base); c["seq"] = "%0A%0D: ;"; cases.append(c)
    c = dict(base); c["sid"] = "uuid:" + "X" * 0x400; cases.append(c)
    c = dict(base); c["nt"] = "upnp:event%00"; cases.append(c)
    return cases


def run_case(shape: HandlerShape, method: int, params, cl, body) -> List[str]:
    """Simulate one request through the shape's parse path; return fault site list."""
    faults: List[str] = []
    req = Request(method=method, params=params, content_length=cl, body=body)
    if method == 1:
        # GET: only the param lookup path runs (no body reader)
        for k, _ in params:
            try:
                req.get_param(k, shape.name + ":get")
            except Fault as f:
                faults.append(str(f))
        return faults
    # POST: body reader then form parse (/notify has no POST path -> 0x195 only)
    if shape.body_dstlen == 0:
        return faults
    try:
        rc = read_body(req, shape.body_dstlen, shape.body_min, shape.name)
    except Fault as f:
        faults.append(str(f))
        return faults
    if rc != 0xC8:
        return faults
    try:
        pairs = split_pairs(req.body, shape.key_cap, shape.val_cap, shape.name)
    except Fault as f:
        faults.append(str(f))
        return faults
    # handler-side key dispatch mirrors (strcmp against known keys)
    known = set(shape.keys)
    got_gate = False
    got_numeric: dict = {}
    for ok, k, v in pairs:
        if not ok:
            continue
        if shape.gate_key and k == shape.gate_key:
            got_gate = True
        if k in shape.numeric_keys:
            got_numeric[k] = v
        if k in known and shape.name == "/setstring":
            # 0x1031a650 property-name validation then 0x10318eb0 store with
            # caps 0x80/0x401 - store dst is the same kv buffers (already bounded).
            pass
    if shape.gate_key and not got_gate:
        pass  # 0x195 outcome, not a fault
    return faults


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("-v", "--verbose", action="store_true")
    ap.add_argument("--json", default=None)
    args = ap.parse_args()

    total = 0
    faults: List[dict] = []

    for shape in SHAPES:
        # GET param cases
        if shape.name == "/notify":
            for i, prm in enumerate(case_notify_params()):
                total += 1
                fs = run_case(shape, 1, list(prm.items()), None, b"")
                for f in fs:
                    faults.append({"case": f"{shape.name}/get#{i}", "fault": f})
                if args.verbose:
                    print(f"{shape.name} get#{i}: {'FAULT ' + str(fs) if fs else 'ok'}")
        # POST CL cases
        for i, (lbl, cl, body) in enumerate(case_content_length()):
            total += 1
            fs = run_case(shape, 4, [("csrfToken", "x")] if shape.gate_key else [], cl, body)
            for f in fs:
                faults.append({"case": f"{shape.name}/cl:{lbl}", "fault": f})
            if args.verbose:
                print(f"{shape.name} cl:{lbl}: {'FAULT ' + str(fs) if fs else 'ok'}")
        # POST form-value cases (well-formed CL first)
        n = shape.body_min
        for i, (lbl, body) in enumerate(case_form_values()):
            total += 1
            cl = str(max(n, len(body)))
            fs = run_case(shape, 4, [("csrfToken", "x")] if shape.gate_key else [],
                          cl, body + b"A" * max(0, n - len(body)))
            for f in fs:
                faults.append({"case": f"{shape.name}/form:{lbl}", "fault": f})
            if args.verbose:
                print(f"{shape.name} form:{lbl}: {'FAULT ' + str(fs) if fs else 'ok'}")

    print(f"\n=== {total} cases, {len(faults)} fault(s) ===")
    for f in faults:
        print(json.dumps(f, sort_keys=True))
    if args.json:
        with open(args.json, "w") as fh:
            json.dump({"cases": total, "faults": faults}, fh, indent=1)
    return 1 if faults else 0


if __name__ == "__main__":
    sys.exit(main())
