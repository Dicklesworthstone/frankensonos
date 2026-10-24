#!/usr/bin/env python3
"""SMB2 state-machine fuzzer for the Sonos *client* (libsmb2).

The device is made to be an SMB client: GET /getaa?u=x-file-cifs://<us>:<port>/S/folder.jpg.
impacket's SimpleSMBServer provides the valid state machine (NEGOTIATE -> SESSION_SETUP ->
TREE_CONNECT -> CREATE -> QUERY_INFO -> READ -> CLOSE); this harness answers one case at a
time and mutates exactly one field or header word of the reply to the chosen command.

Per case it records the SMB op sequence the client actually performed, the /getaa result,
daemon liveness (HTTP + ICMP), and on an anomaly saves the raw mutated PDU.

usage: smb_state_fuzz.py <device-ip> <listen-port> [case ...]
       (no case list = run the whole matrix)
"""
import sys, os, json, time, socket, struct, threading, subprocess, urllib.request, urllib.error
from impacket.smbserver import SimpleSMBServer, SMB2Commands

TGT = sys.argv[1]; PORT = int(sys.argv[2], 0)
FILE = os.environ.get('SMBFILE', 'folder.jpg')
SELF = None
SHARE_DIR = '/Users/jemanuel/sonos-root-local/smbshare'
OUT = os.path.dirname(os.path.abspath(__file__)) + '/saves'
os.makedirs(OUT, exist_ok=True)

# SMB2 header body offset
BODY = 64
# field offsets in the response body (spec) for the ops we mutate
FIELDS = {
    'negotiate':  {'u16': [(66, 'SecurityMode'), (68, 'DialectRevision')]},
    'sesssetup':  {'u16': [(66, 'SessionFlags')], 'u32': [(68, 'SecurityBufferOffset'), (72, 'SecurityBufferLength')]},
    'treeconnect':{'u32': [(76, 'MaximalAccess')]},
    'create':     {'u8': [(66, 'OplockLevel')], 'u32': [(80, 'CreateContextsOffset'), (84, 'CreateContextsLength')]},
    'read':       {'u8': [(66, 'DataOffset')], 'u32': [(68, 'DataLength'), (72, 'DataRemaining')]},
    'write':      {'u32': [(68, 'Count'), (72, 'Remaining')]},
    'queryinfo':  {'u16': [(66, 'OutputBufferOffset')], 'u32': [(68, 'OutputBufferLength')]},
    'ioctl':      {'u32': [(88, 'OutputOffset'), (92, 'OutputCount')]},
}
CMDNAME = {0: 'negotiate', 1: 'sesssetup', 3: 'treeconnect', 5: 'create', 8: 'read', 9: 'write',
           16: 'queryinfo', 11: 'ioctl'}
ORD = {v: k for k, v in CMDNAME.items()}
METHOD = {'negotiate': 'smb2Negotiate', 'sesssetup': 'smb2SessionSetup', 'treeconnect': 'smb2TreeConnect',
          'create': 'smb2Create', 'read': 'smb2Read', 'write': 'smb2Write', 'queryinfo': 'smb2QueryInfo', 'ioctl': 'smb2Ioctl'}

STATE = {'case': None, 'seq': [], 'applied': False, 'saved': None}

def enc(blob):
    return blob if isinstance(blob, bytes) else bytes(blob)

def mutate(pkt, case):
    """pkt: bytearray of one SMB2 response PDU (starts with the 64-byte header)."""
    op, field, value = case.split(':')
    cmd = struct.unpack('>H', pkt[12:14])[0]
    if CMDNAME.get(cmd) != op:
        return False
    v = int(value, 0) if value.startswith('0x') else None
    if field == 'nextcommand':
        struct.pack_into('>I', pkt, 20, v if v is not None else len(pkt) + 8); return True
    if field == 'creditcharge':
        struct.pack_into('>H', pkt, 6, v if v is not None else 0); return True
    if field == 'chainlen':
        struct.pack_into('>I', pkt, 20, v if v is not None else 4); return True
    kind = ('u16' if any(o == int(field.split('@')[1]) for o, _ in FIELDS[op].get('u16', [])) else
            'u32' if any(o == int(field.split('@')[1]) for o, _ in FIELDS[op].get('u32', [])) else 'u8')
    off = int(field.split('@')[1])
    val = v if v is not None else (0xFFFFFFFF if kind == 'u32' else 0xFFFF if kind == 'u16' else 0xFF)
    struct.pack_into('>' + {'u8': 'B', 'u16': 'H', 'u32': 'I'}[kind], pkt, off, val & (0xFF if kind == 'u8' else 0xFFFF if kind == 'u16' else 0xFFFFFFFF))
    return True

def wrap(cmd_byte):
    orig = getattr(SMB2Commands, METHOD[CMDNAME[cmd_byte]])
    def patched(connId, smbServer, recvPacket):
        res = orig(connId, smbServer, recvPacket)
        STATE['seq'].append(CMDNAME[cmd_byte])
        if STATE['case'] and not STATE['applied'] and STATE['case'].split(':')[0] == CMDNAME[cmd_byte]:
            try:
                pkts = res[1]
                if pkts:
                    p = pkts[0]
                    blob = bytearray(p.getData() if hasattr(p, 'getData') else bytes(p))
                    if mutate(blob, STATE['case']):
                        STATE['applied'] = True
                        STATE['saved'] = bytes(blob)
                        pkts[0] = p.__class__(bytes(blob))
            except Exception as e:
                STATE.setdefault('err', repr(e))
        return res
    setattr(SMB2Commands, METHOD[CMDNAME[cmd_byte]], staticmethod(patched))

for c in CMDNAME:
    wrap(c)

def alive(timeout=2):
    try:
        with urllib.request.urlopen(f'http://{TGT}:1400/status/zp', timeout=timeout) as r:
            return str(r.status)
    except urllib.error.HTTPError as e:
        return f'http{e.code}'
    except Exception as e:
        return type(e).__name__

def ping():
    try:
        return subprocess.run(['ping', '-c1', '-W1', TGT], capture_output=True, timeout=3).returncode == 0
    except Exception:
        return False

def trigger():
    u = (f'http://{TGT}:1400/getaa?u=x-file-cifs%3A%2F%2F{SELF}%3A{PORT}%2FS%2F{FILE}&w=1&h=1')
    try:
        with urllib.request.urlopen(u, timeout=12) as r:
            return f'http{r.status}:{len(r.read())}'
    except urllib.error.HTTPError as e:
        return f'http{e.code}'
    except Exception as e:
        return type(e).__name__

CASES = [
    'control',
    'negotiate:creditcharge:0x0', 'negotiate:creditcharge:0xFFFF', 'negotiate:nextcommand:0xFFFFFFFF',
    'sesssetup:66@SecurityBufferOffset', 'sesssetup:72@SecurityBufferLength', 'sesssetup:creditcharge:0x0',
    'treeconnect:76@MaximalAccess', 'treeconnect:nextcommand:0xFFFFFFFF',
    'create:80@CreateContextsOffset', 'create:84@CreateContextsLength', 'create:66@OplockLevel',
    'queryinfo:66@OutputBufferOffset', 'queryinfo:68@OutputBufferLength',
    'read:66@DataOffset', 'read:68@DataLength', 'read:72@DataRemaining', 'read:creditcharge:0x0',
    'ioctl:88@OutputOffset', 'ioctl:92@OutputCount',
    'read:nextcommand:0xFFFFFFFF', 'read:chainlen:4', 'write:68@Count',
]

def run_case(case, srv):
    STATE.update(case=None if case == 'control' else case, seq=[], applied=False, saved=None)
    pre_http, pre_ping = alive(), ping()
    t0 = time.time()
    res = trigger()
    dt = round((time.time() - t0) * 1000)
    time.sleep(0.6)
    post_http, post_ping = alive(), ping()
    time.sleep(1.4)
    late_http = alive()
    rec = {'case': case, 'getaa': res, 'ms': dt, 'ops': STATE['seq'], 'applied': STATE['applied'],
           'pre': pre_http, 'post': post_http, 'late': late_http,
           'icmp': [pre_ping, post_ping]}
    if STATE.get('err'):
        rec['err'] = STATE['err']
    bad = (post_http not in ('200',) or late_http not in ('200',) or not post_ping
           or res.startswith(('ConnectionReset', 'Timeout', 'timeout'))
           or (case != 'control' and not STATE['applied']))
    if STATE['saved'] is not None and (bad or case != 'control'):
        fn = f'{OUT}/{case.replace(":", "_")}.pdu'
        open(fn, 'wb').write(STATE['saved'])
        rec['pdu'] = fn
    if bad:
        rec['verdict'] = 'ANOMALY'
    else:
        rec['verdict'] = 'ok'
    print(json.dumps(rec), flush=True)
    return rec

def main():
    global SELF
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); s.connect((TGT, 1400)); SELF = s.getsockname()[0]; s.close()
    srv = SimpleSMBServer(listenAddress='0.0.0.0', listenPort=PORT)
    srv.addShare('S', SHARE_DIR)
    srv.setSMB2Support(True)
    threading.Thread(target=srv.start, daemon=True).start()
    time.sleep(1.0)
    cases = sys.argv[3:] or CASES
    nreq = 0
    for c in cases:
        if nreq >= 200:
            print(json.dumps({'note': 'cap reached'}), flush=True); break
        r = run_case(c, srv)
        nreq += len(r['ops'])
    print(json.dumps({'requests': nreq}), flush=True)
    sys.stdout.flush()
    os._exit(0)

if __name__ == '__main__':
    main()
