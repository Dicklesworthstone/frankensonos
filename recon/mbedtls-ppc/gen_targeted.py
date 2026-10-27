#!/usr/bin/env python3
"""Targeted DER corpus: DN, validity, extensions, algorithm and key surgery.

Companion to gen_corpus.py. Locates TBSCertificate fields by position (stable for any cert)
and mutates the fields the parsers actually walk. Every case is re-serialized from the tree,
so enclosing lengths stay correct.

usage: gen_targeted.py <base.der> <outdir>
"""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import dertree  # noqa: E402
from dertree import Node  # noqa: E402

OID_RSA = bytes.fromhex("2a864886f70d010101")
OID_SHA256RSA = bytes.fromhex("2a864886f70d01010b")
OID_PSS = bytes.fromhex("2a864886f70d01010a")
OID_MD5RSA = bytes.fromhex("2a864886f70d010104")
OID_SHA1RSA = bytes.fromhex("2a864886f70d010105")
OID_ECDSA_SHA256 = bytes.fromhex("2a8648ce3d040302")
OID_UNKNOWN = bytes.fromhex("2a864886f70d0101ff")
OID_CN = bytes.fromhex("550403")
OID_SAN = bytes.fromhex("551d11")
OID_BC = bytes.fromhex("551d13")
OID_KU = bytes.fromhex("551d0f")
OID_NC = bytes.fromhex("551d1e")
OID_AKI = bytes.fromhex("551d23")
OID_UNKNOWN_EXT = bytes.fromhex("2b060104018f4f0d")


def seq(*kids):
    return Node(0x30, list(kids))


def oid(v):
    return Node(0x06, v)


def main():
    if len(sys.argv) != 3:
        print(__doc__)
        return 2
    base = open(sys.argv[1], "rb").read()
    outdir = sys.argv[2]
    os.makedirs(outdir, exist_ok=True)

    root = dertree.parse_one(base)
    cert = root
    tbs = dertree.find(cert, 0x30, depth=1)[0]
    f = tbs.value
    idx = 1 if f and f[0].tag == 0xA0 else 0
    serial, sigalg, issuer, validity, subject, spki = f[idx:idx + 6]
    exts = [t for t in f if t.tag == 0xA3]

    cases = []

    def emit(name, family, note, data):
        if data == base:
            return
        with open(os.path.join(outdir, name + ".der"), "wb") as fh:
            fh.write(data)
        cases.append((name, family, note))

    def mutated():
        return root.clone()

    def node_at(m, path):
        n = m
        for i in path:
            n = n.value[i]
        return n

    # ---- Name / RDN ------------------------------------------------------------
    for label, name in (("issuer", issuer), ("subject", subject)):
        path = [0, idx + (2 if label == "issuer" else 4)]

        m = mutated(); node_at(m, path).value = []
        emit(f"dn_{label}_empty", "dn", f"{label} Name emptied", m.encode())

        m = mutated(); node_at(m, path).value = [Node(0x31, [])]
        emit(f"dn_{label}_empty_set", "dn", f"{label} single empty SET", m.encode())

        m = mutated(); node_at(m, path).value = [Node(0x31, [Node(0x30, [])])]
        emit(f"dn_{label}_set_empty_seq", "dn", f"{label} RDN with empty SEQUENCE", m.encode())

        m = mutated(); node_at(m, path).value = [Node(0x31, [seq(oid(OID_CN), Node(0x0C, b""))])]
        emit(f"dn_{label}_oid_no_value", "dn", f"{label} RDN OID with empty value", m.encode())

        m = mutated(); node_at(m, path).value = [Node(0x31, [seq(Node(0x0C, b"x"))])]
        emit(f"dn_{label}_value_no_oid", "dn", f"{label} RDN value with no OID", m.encode())

        m = mutated()
        node_at(m, path).value = [Node(0x31, [seq(oid(OID_CN), Node(0x0C, b"a"))])] * 1000
        emit(f"dn_{label}_many_rdns", "dn", f"{label} 1000 RDNs", m.encode())

        m = mutated()
        n = node_at(m, path).value[0].value[0].value[1]
        n.len_override = b"\x82\xff\xff"
        emit(f"dn_{label}_long_value", "dn", f"{label} CN value declared 0xffff", m.encode())

        for st, sl in (("bmp", 0x1E), ("universal", 0x1C), ("teletex", 0x14),
                       ("numeric", 0x12), ("printable", 0x13), ("ia5", 0x16)):
            m = mutated()
            node_at(m, path).value = [Node(0x31, [seq(oid(OID_CN), Node(sl, b"A\x00B"))])]
            emit(f"dn_{label}_str_{st}", "dn", f"{label} CN as {st}", m.encode())

        m = mutated()
        node_at(m, path).value = [Node(0x31, [seq(oid(OID_CN), Node(0x1E, b"A"))])]
        emit(f"dn_{label}_bmp_odd", "dn", f"{label} BMPString odd length", m.encode())

        m = mutated()
        node_at(m, path).value = [Node(0x31, [seq(oid(OID_CN), Node(0x0C, b"\xf0\x9f"))])]
        emit(f"dn_{label}_utf8_bad", "dn", f"{label} UTF-8 truncated sequence", m.encode())

        m = mutated()
        node_at(m, path).value = [Node(0x31, [seq(oid(OID_CN), Node(0x0C, b"a\x00b"))])]
        emit(f"dn_{label}_nul", "dn", f"{label} CN with embedded NUL", m.encode())

        m = mutated()
        node_at(m, path).value = [Node(0x31, [Node(0x30, [])])] * 200
        emit(f"dn_{label}_deep", "dn", f"{label} 200 nested SET/SEQ", m.encode())

        m = mutated()
        first = node_at(m, path).value[0]
        node_at(m, path).value = [first.clone(), first.clone()]
        emit(f"dn_{label}_dup_rdn", "dn", f"{label} first RDN duplicated", m.encode())

    # ---- validity / time -------------------------------------------------------
    vpath = [0, idx + 3]
    for i, t in enumerate(validity.value):
        for label, val in (("empty", b""), ("no_z", b"250101000000"), ("short", b"25"),
                           ("month13", b"251301000000Z"), ("day00", b"250100000000Z"),
                           ("leap", b"251231235960Z"), ("year5", b"2512312359Z"),
                           ("gen", b"20251231235959Z"), ("gen_short", b"2025"),
                           ("offset", b"251231235959+0100"), ("nul", b"2512312359\x0031Z"),
                           ("huge", b"99999999999999999999Z")):
            m = mutated()
            node_at(m, vpath).value[i].value = val
            emit(f"time_{i}_{label}", "time", f"validity[{i}] -> {val!r}", m.encode())

    m = mutated(); node_at(m, vpath).value[0].tag = 0x18
    emit("time_utc_to_gen", "time", "notBefore UTCTime->GeneralizedTime", m.encode())
    m = mutated(); node_at(m, vpath).value[0].tag = 0x17
    emit("time_gen_to_utc", "time", "notBefore GeneralizedTime->UTCTime", m.encode())

    # ---- algorithm identifiers -------------------------------------------------
    for label, path in (("sigalg", [0, idx + 1]), ("spki_alg", [0, idx + 5, 0])):
        for label2, val in (("pss", OID_PSS), ("md5", OID_MD5RSA), ("sha1", OID_SHA1RSA),
                            ("ecdsa", OID_ECDSA_SHA256), ("unknown", OID_UNKNOWN), ("empty", b"")):
            m = mutated()
            node_at(m, path).value[0].value = val
            emit(f"alg_{label}_{label2}", "alg", f"{label} OID -> {label2}", m.encode())
        m = mutated(); node_at(m, path).value = [node_at(m, path).value[0]]
        emit(f"alg_{label}_no_params", "alg", f"{label} parameters removed", m.encode())
        m = mutated(); node_at(m, path).value = [node_at(m, path).value[0], Node(0x05, b"")]
        emit(f"alg_{label}_null_params", "alg", f"{label} NULL parameters", m.encode())
        m = mutated()
        node_at(m, path).value = [node_at(m, path).value[0], Node(0x30, [Node(0x0C, b"\x00" * 300)])]
        emit(f"alg_{label}_long_params", "alg", f"{label} 300-byte parameter blob", m.encode())

    # ---- SubjectPublicKeyInfo --------------------------------------------------
    kpath = [0, idx + 5, 1]
    for label, val in (("empty", b""), ("no_unused", b""), ("unused7", b"\x07"),
                       ("unused_ff", b"\xff"), ("truncated", b"\x00")):
        m = mutated()
        b = node_at(m, kpath)
        if label == "empty":
            b.value = b""
        elif label == "no_unused":
            b.value = b.value[1:]
        elif label == "truncated":
            b.value = b"\x00" + b.value[1:9]
        else:
            b.value = val + b.value[1:]
        emit(f"spki_bitstr_{label}", "key", f"subjectPublicKey BIT STRING {label}", m.encode())

    # ---- extensions ------------------------------------------------------------
    for ext_wrap in exts:
        epath = [0, idx + 6]
        elist = ext_wrap.value
        if not elist:
            continue
        e = elist[0]

        m = mutated()
        node_at(m, epath).value = [elist[0].clone(), elist[0].clone()]
        emit("ext_dup_first", "ext", "first extension duplicated", m.encode())

        m = mutated(); node_at(m, epath).value[0].value[0].value = OID_UNKNOWN_EXT
        emit("ext_unknown_oid", "ext", "first extension OID -> unknown", m.encode())

        m = mutated(); node_at(m, epath).value[0].value[0].value = b""
        emit("ext_empty_oid", "ext", "first extension OID emptied", m.encode())

        m = mutated(); node_at(m, epath).value[0].value[1].value = b""
        emit("ext_value_empty", "ext", "first extension value emptied", m.encode())

        m = mutated(); node_at(m, epath).value[0].value[1].len_override = b"\x82\xff\xff"
        emit("ext_value_declared_ffff", "ext", "first extension value declared 0xffff", m.encode())

        m = mutated(); node_at(m, epath).value[0].value[1].value = b"\x30"
        emit("ext_value_truncated", "ext", "first extension value cut to 1 byte", m.encode())

        m = mutated(); node_at(m, epath).value[0].value.insert(0, Node(0x01, b"\xff"))
        emit("ext_critical_ff", "ext", "first extension critical byte 0xff", m.encode())

        m = mutated()
        node_at(m, epath).value.append(seq(oid(OID_SAN), Node(0x04, [seq()])))
        emit("ext_san_empty", "ext", "SAN extension with zero entries", m.encode())

        m = mutated()
        node_at(m, epath).value.append(
            seq(oid(OID_SAN), Node(0x04, [seq(*[Node(0x82, b"a.com")] * 200)])))
        emit("ext_san_many", "ext", "SAN with 200 dNSNames", m.encode())

        m = mutated()
        node_at(m, epath).value.append(
            seq(oid(OID_BC), Node(0x01, b"\xff"),
                Node(0x04, [seq(Node(0x01, b"\xff"), Node(0x02, b"\x7f"))])))
        emit("ext_bc_pathlen7f", "ext", "basicConstraints CA pathLen 0x7f", m.encode())

        m = mutated()
        node_at(m, epath).value.append(seq(oid(OID_KU), Node(0x04, [Node(0x03, b"\x07\xff\xff")])))
        emit("ext_ku_extra_bits", "ext", "keyUsage with 2 value bytes", m.encode())

        m = mutated()
        node_at(m, epath).value.append(
            seq(oid(OID_NC), Node(0x04, [seq(seq(Node(0x82, b"a.com")))])))
        emit("ext_nc_permitted", "ext", "nameConstraints permitted subtree", m.encode())

        m = mutated()
        node_at(m, epath).value.append(seq(oid(OID_AKI), Node(0x04, [seq(Node(0x80, b"\x00" * 20))])))
        emit("ext_aki_keyid", "ext", "authorityKeyIdentifier keyIdentifier", m.encode())

    # ---- serial / version ------------------------------------------------------
    spath = [0, idx]
    for label, val in (("empty", b""), ("zero", b"\x00"), ("negative", b"\xff\x01"),
                       ("huge", b"\x7f" + b"\xff" * 40), ("leading_zero", b"\x00\x01")):
        m = mutated(); node_at(m, spath).value = val
        emit(f"serial_{label}", "serial", f"serialNumber -> {val.hex()}", m.encode())

    if f and f[0].tag == 0xA0:
        for label, val in (("0", b"\x00"), ("1", b"\x01"), ("2", b"\x02"), ("3", b"\x03"),
                           ("ff", b"\xff"), ("empty", b"")):
            m = mutated(); node_at(m, [0, 0, 0]).value = val
            emit(f"version_{label}", "version", f"version -> {val.hex()}", m.encode())

    with open(os.path.join(outdir, "manifest.tsv"), "w") as fh:
        for name, family, note in cases:
            fh.write(f"{name}\t{family}\t{note}\n")
    fams = {}
    for _, family, _ in cases:
        fams[family] = fams.get(family, 0) + 1
    print(f"{len(cases)} cases -> {outdir}  {fams}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
