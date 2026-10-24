#!/usr/bin/env python3
"""Pre-auth length fuzzers for two S2 surfaces.

  settings <ip>   GET /status/ZPLocalSettingsFile/<name>, 256 B to 64 KiB,
                  plain and with a "../" prefix; records code/size/time and
                  /info liveness after each case.
  mtm <ip>        POST /mtmhhsetup with one oversized field at a time
                  (hhid/name/icon), then all three; records code/size. The
                  route is devmode-gated, so 403 is the expected code on a
                  locked unit and the handler only runs once unlocked.

Prints one JSON line per case plus a final summary.
"""
import http.client, json, sys, time, urllib.parse, urllib.request

APIKEY = "123e4567-e89b-12d3-a456-426655440000"


def req(ip, path, body=None, headers=None, timeout=25):
    c = http.client.HTTPConnection(ip, 1400, timeout=timeout)
    t0 = time.time()
    try:
        c.request("POST" if body is not None else "GET", path, body=body, headers=headers or {})
        r = c.getresponse()
        data = r.read()
        return r.status, len(data), round(time.time() - t0, 3), None
    except Exception as e:
        return None, None, round(time.time() - t0, 3), type(e).__name__
    finally:
        try: c.close()
        except Exception: pass


def alive(ip):
    try:
        urllib.request.urlopen("http://%s:1400/info" % ip, timeout=3).read()
        return True
    except Exception:
        return False


def settings(ip):
    cases = []
    for n in (256, 1024, 4096, 65536):
        cases.append(("plain-%d" % n, "a" * n))
        cases.append(("../-%d" % n, "../" + "a" * n))
    for n in (2048, 8192):
        cases.append(("plain-%d" % n, "a" * n))
        cases.append(("../-%d" % n, "../" + "a" * n))
    out = []
    for name, val in cases:
        path = "/status/ZPLocalSettingsFile/" + urllib.parse.quote(val, safe="/")
        code, size, dt, err = req(ip, path)
        up = alive(ip)
        rec = {"case": name, "urllen": len(path), "code": code, "size": size,
               "time": dt, "err": err, "info": up}
        print(json.dumps(rec), flush=True); out.append(rec)
        time.sleep(1.5)
    return out


BASE = {"hhid": "Sonos_MTM_" + "a" * 22,
        "name": "Room", "icon": "x-rincon-roomicon:generic",
        "key": "AAAAAAAAAAAAAAAAAAAAAA==", "reboot": "0"}


def mtm(ip):
    def field(name, n):
        if name == "hhid":
            return "Sonos_MTM_" + "a" * max(0, n - 10)
        if name == "icon":
            pre = "x-rincon-roomicon:"
            return pre + "a" * max(0, n - len(pre))
        return "a" * n

    cases = []
    for name, n in (("hhid", 33), ("hhid", 64), ("hhid", 1024), ("hhid", 65536),
                    ("name", 32), ("name", 128), ("name", 1024),
                    ("icon", 129), ("icon", 1024), ("icon", 65536)):
        cases.append(("%s-%d" % (name, n), name, n))
    cases.append(("all-64k", "all", 65536))
    cases.append(("all-1k", "all", 1024))
    out = []
    for label, name, n in cases:
        f = dict(BASE)
        if name == "all":
            for k in ("hhid", "name", "icon"):
                f[k] = field(k, n)
        else:
            f[name] = field(name, n)
        body = urllib.parse.urlencode(f).encode()
        code, size, dt, err = req(ip, "/mtmhhsetup", body,
                                  {"Content-Type": "application/x-www-form-urlencoded",
                                   "X-Sonos-Api-Key": APIKEY})
        up = alive(ip)
        rec = {"case": label, "bodylen": len(body), "code": code, "size": size,
               "time": dt, "err": err, "info": up}
        print(json.dumps(rec), flush=True); out.append(rec)
        time.sleep(1.5)
    return out


if __name__ == "__main__":
    which, ip = sys.argv[1], sys.argv[2]
    if which == "settings": settings(ip)
    else: mtm(ip)
