"""urllib.request -> curl shim: this host's Python connect() to 192.168.4.0/24 returns
EHOSTUNREACH while curl works (see RE_HLS_EXPLOIT harness note). Import this module BEFORE
hls_det.py, or run with PYTHONSTARTUP... simplest: `python3 -i` no — use `python3 shim.py target`.
Actually cleanest: sitecustomize-style import via -c wrapper.
"""
import subprocess


def _curl_urlopen(req, timeout=8, **kw):
    url = req.full_url if hasattr(req, "full_url") else req
    method = getattr(req, "get_method", lambda: "GET")()
    headers = getattr(req, "headers", {}) or {}
    cmd = ["curl", "-s", "-m", str(timeout), "-X", method, "-w", "\n%{http_code}"]
    for k, v in headers.items():
        if k.lower() not in ("host", "accept-encoding", "content-length", "connection"):
            cmd += ["-H", "%s: %s" % (k.replace("_", "-").title(), v)]
    data = getattr(req, "data", None)
    if data:
        cmd += ["--data-binary", data if isinstance(data, str) else data.decode("latin-1")]
    cmd.append(url)
    p = subprocess.run(cmd, capture_output=True, timeout=timeout + 5)
    out = p.stdout.decode("latin-1")
    body, _, code = out.rpartition("\n%{http_code}"[:0] + "\n")
    status = int(code) if code.strip().isdigit() else 0
    if status == 0:
        raise OSError("curl failed: %s" % p.stderr.decode()[:120])

    class R:
        def read(self):
            return body.encode("latin-1")

        def __enter__(self):
            return self

        def __exit__(self, *a):
            return False

    return R()


def install():
    import urllib.request
    urllib.request.urlopen = _curl_urlopen
