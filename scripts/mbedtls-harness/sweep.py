#!/usr/bin/env python3
"""Structural sweep of the device mbedtls 2.5.0 x509 parser.

Every case is one DER file handed to ../out/xmparse under qemu-user. A case is
a crash if qemu reports a signal (rc != 0) or the harness canary is clobbered.
The first three crashes are kept and analysed (qemu gdbstub + gdb-multiarch
backtrace), then the sweep stops.

Usage: ./sweep.py <good.der> [--budget N]
"""
import os, random, struct, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
HB = os.environ.get('HB', '/tmp/hb')
ROOT = os.path.join(HB, 'root')
BIN = os.path.join(HB, 'xm2')
WORK = os.path.join(HB, 'sweep')
CRASH = os.path.join(HERE, 'crashes')

# ---------------------------------------------------------------- ASN.1 bits
def tlv(tag, val):
    n = len(val)
    if n < 0x80:
        return bytes([tag, n]) + val
    nb = n.to_bytes((n.bit_length() + 7) // 8, 'big')
    return bytes([tag, 0x80 | len(nb)]) + nb + val

def seq(*items):
    return tlv(0x30, b''.join(items))

def walk(d, off=0, end=None):
    """Yield (offset_of_length_byte, header_len, declared_len) for every TLV."""
    end = len(d) if end is None else end
    while off < end:
        if off + 2 > end:
            return
        tag = d[off]
        i = off + 1
        if i >= end:
            return
        l0 = d[i]
        if l0 & 0x80:
            nb = l0 & 0x7f
            if nb == 0 or i + 1 + nb > end:
                return
            ln = int.from_bytes(d[i + 1:i + 1 + nb], 'big')
            hdr = 2 + nb
        else:
            ln = l0
            hdr = 2
        yield i, hdr, ln
        if tag & 0x20:                      # constructed: descend
            yield from walk(d, off + hdr, min(off + hdr + ln, end))
        off += hdr + ln

def split(d):
    """Return the reusable pieces of the good cert."""
    # Certificate ::= SEQ { tbs, sigalg, sig }
    _, h = 0, None
    body = d[2:] if d[1] < 0x80 else d[2 + (d[1] & 0x7f):]
    parts, off = [], 0
    while off < len(body):
        tag = body[off]
        i, nb = off + 1, body[off + 1] & 0x7f if body[off + 1] & 0x80 else 0
        ln = int.from_bytes(body[i + 1:i + 1 + nb], 'big') if nb else body[off + 1]
        hdr = 2 + nb
        parts.append(body[off:off + hdr + ln])
        off += hdr + ln
    tbs, sigalg, sig = parts[0], parts[1], parts[2]
    # inside tbs: [0]ver, serial, sigalg, issuer, validity, subject, spki, [3]ext
    b = tbs[2:] if tbs[1] < 0x80 else tbs[2 + (tbs[1] & 0x7f):]
    comps, off = [], 0
    while off < len(b):
        nb = b[off + 1] & 0x7f if b[off + 1] & 0x80 else 0
        ln = int.from_bytes(b[off + 2:off + 2 + nb], 'big') if nb else b[off + 1]
        hdr = 2 + nb
        comps.append(b[off:off + hdr + ln])
        off += hdr + ln
    return {'ver': comps[0], 'serial': comps[1], 'sigalg': comps[2],
            'issuer': comps[3], 'validity': comps[4], 'subject': comps[5],
            'spki': comps[6], 'exts': comps[7] if len(comps) > 7 else b'',
            'extlist': [], 'sig': sig}

def ext_block(*exts):
    return tlv(0xa3, seq(*exts)) if exts else b''

def cert(p, serial=None, subject=None, exts=None, sigalg=None, sig=None):
    s = seq(p['ver'], serial or p['serial'], sigalg or p['sigalg'], p['issuer'],
            p['validity'], subject or p['subject'], p['spki'],
            ext_block(*exts) if exts is not None else p['exts'])
    return seq(s, sigalg or p['sigalg'], sig or p['sig'])

def ext(oid, value, critical=False):
    parts = [tlv(0x06, oid)] + ([tlv(0x01, b'\xff')] if critical else [])
    return seq(*parts, tlv(0x04, value))

# ---------------------------------------------------------------- runner
def run(blob):
    path = os.path.join(WORK, 'cur.der')
    with open(path, 'wb') as f:
        f.write(blob)
    try:
        r = subprocess.run(['qemu-mips', '-L', ROOT, '-E',
                            'LD_LIBRARY_PATH=' + ROOT + '/lib', BIN, path],
                           capture_output=True, timeout=25)
    except subprocess.TimeoutExpired:
        return ('TIMEOUT', '?', b'')
    out = r.stdout.decode('latin1')
    ret = '-'
    for line in out.splitlines():
        if line.startswith('parse_ret:'):
            ret = line.split()[1]
    can = 'ok' if 'canary: ok' in out else ('CLOBBER' if 'CLOBBER' in out else '?')
    return (r.returncode, ret, can.encode())

def analyse(path):
    """qemu gdbstub + gdb-multiarch: backtrace and fault PC."""
    port = 12345
    q = subprocess.Popen(['qemu-mips', '-g', str(port), '-L', ROOT, '-E',
                          'LD_LIBRARY_PATH=' + ROOT + '/lib', BIN, path],
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(1.0)
    gdb = ['gdb-multiarch', '-q', '-batch',
           '-ex', 'set architecture mips:isa32',
           '-ex', 'target remote :%d' % port,
           '-ex', 'continue', '-ex', 'bt', '-ex', 'info registers pc',
           '-ex', 'detach']
    try:
        g = subprocess.run(gdb, capture_output=True, timeout=40)
        return g.stdout.decode('latin1', 'replace') + g.stderr.decode('latin1', 'replace')
    except subprocess.TimeoutExpired:
        return '<gdb timeout>'
    finally:
        q.kill()

def main():
    good_path = sys.argv[1]
    budget = 4000
    if '--budget' in sys.argv:
        budget = int(sys.argv[sys.argv.index('--budget') + 1])
    good = open(good_path, 'rb').read()
    p = split(good)
    os.makedirs(WORK, exist_ok=True)
    os.makedirs(CRASH, exist_ok=True)

    n = len(good)
    lens = [o for o, _, _ in walk(good)]
    cases = []

    # 1. truncations: byte granularity at 1..4 from the end, every 4th byte,
    #    and immediately after every ASN.1 length byte
    for k in range(1, 5):
        cases.append(('trunc-n%d' % k, good[:n - k]))
    for cut in range(2, n, 4):
        cases.append(('trunc-4byte@%d' % cut, good[:cut]))
    for o in lens:
        cases.append(('trunc-at-len@%d' % o, good[:o + 1]))

    # 2. declared lengths +-1..16
    for o in lens:
        nb = good[o] & 0x7f if good[o] & 0x80 else 0
        ln = int.from_bytes(good[o + 1:o + 1 + nb], 'big') if nb else good[o]
        hdr = 2 + nb
        for d in list(range(-16, 0)) + list(range(1, 17)):
            v = ln + d
            if v < 0:
                continue
            b = bytearray(good)
            if v < 0x80:
                b[o:o + hdr] = bytes([v])
            else:
                nv = v.to_bytes((v.bit_length() + 7) // 8, 'big')
                b[o:o + hdr] = bytes([0x80 | len(nv)]) + nv
            cases.append(('len%+d@%d' % (d, o), bytes(b)))

    # 3. indefinite / over-long forms at every length byte
    for o in lens:
        for form, payload in ((0x80, b''), (0x81, b'\x7f'), (0x82, b'\xff\xff'),
                              (0x84, b'\x00\x00\xff\xff'),
                              (0x88, b'\x00\x00\x00\x00\xff\xff\xff\xff')):
            b = bytearray(good)
            nb = good[o] & 0x7f if good[o] & 0x80 else 0
            b[o:o + 2 + nb] = bytes([form]) + payload
            cases.append(('form%02x@%d' % (form, o), bytes(b)))

    # 4. nesting depth
    for depth in (64, 256):
        inner = b'\x05\x00'
        for _ in range(depth):
            inner = seq(inner)
        cases.append(('depth%d' % depth,
                      cert(p, exts=[ext(bytes.fromhex('2b060104018f3c'), inner)])))

    # 5. big CN / big OID arc
    for size in (1, 100, 250, 255, 256, 257, 300, 1024, 65536):
        cn = seq(tlv(0x31, seq(tlv(0x06, bytes.fromhex('550403')),
                             tlv(0x0c, b'A' * size))))
        cases.append(('cn%d' % size, cert(p, subject=cn)))
    for size in (0, 200, 1024, 4096, 65536):
        oid = bytes.fromhex('2b060104018f3c') + b'\x01' * size
        cases.append(('oid+%d' % size,
                      cert(p, exts=[ext(oid, b'\x05\x00')])))

    # 6. serial 0xffffffff
    cases.append(('serial-ffffffff', cert(p, serial=tlv(0x02, b'\xff\xff\xff\xff'))))
    cases.append(('serial-00ffffffff', cert(p, serial=tlv(0x02, b'\x00\xff\xff\xff\xff'))))

    # 7. duplicate / unknown-critical extensions
    real = ext(bytes.fromhex('551d0f'), b'\x03\x02\x05\xa0')       # keyUsage
    cases.append(('ext-duplicate', cert(p, exts=[real, real])))
    cases.append(('ext-unknown-critical',
                  cert(p, exts=[real, ext(bytes.fromhex('2b060104018f3c'),
                                          b'\x05\x00', critical=True)])))
    cases.append(('ext-unknown-critical-empty',
                  cert(p, exts=[ext(bytes.fromhex('2b060104018f3c'), b'', True)])))

    # 8. zero certs / trailing second cert / concatenated junk
    cases.append(('zero-certs', b''))
    cases.append(('trailing-second', good + good))
    rnd = random.Random(11)
    cases.append(('trailing-junk', good + bytes(rnd.randrange(256) for _ in range(64))))
    cases.append(('prefix-junk', b'\x00' * 8 + good))
    cases.append(('nested-in-seq', seq(good)))

    # 9. 1024 random single-byte flips
    for i in range(1024):
        b = bytearray(good)
        j = rnd.randrange(len(b))
        b[j] ^= 1 << rnd.randrange(8)
        cases.append(('flip#%d@%d' % (i, j), bytes(b)))

    print('cases:', len(cases), flush=True)
    stats, crashes = {}, []
    for idx, (tag, blob) in enumerate(cases[:budget]):
        rc, ret, can = run(blob)
        key = (rc, ret, can.decode())
        stats[key] = stats.get(key, 0) + 1
        if rc != 0 or can == b'CLOBBER':
            name = '%02d-%s.der' % (len(crashes) + 1, tag.replace('/', '_'))
            open(os.path.join(CRASH, name), 'wb').write(blob)
            print('CRASH', name, 'rc=%s ret=%s canary=%s' % (rc, ret, can.decode()),
                  flush=True)
            crashes.append(name)
            print(analyse(os.path.join(CRASH, name))[-2500:], flush=True)
            if len(crashes) >= 3:
                break
    print('--- summary ---')
    print('ran:', sum(stats.values()), 'crashes:', len(crashes), crashes)
    for k, v in sorted(stats.items(), key=lambda x: -x[1]):
        print('  rc=%s ret=%s canary=%s : %d' % (k[0], k[1], k[2], v))

if __name__ == '__main__':
    main()
