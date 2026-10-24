# RE_HTTPCLIENT_FUZZ — the daemon's HTTP *client* response parser

Lane owner: this pane. The **updater's** fetch response is CreamCompass's (root context, `.202`); this file
is the daemon's own client, exercised on `.86` only (S2 86.10, Play:1).

## Mechanism

A pre-auth `SUBSCRIBE` with `CALLBACK: <http://192.168.4.160:8901/<shape>/avt>` makes the daemon send its
initial GENA `NOTIFY` to our server. We answer with the shape under test, then probe liveness with `GET
/info` and a fresh `SUBSCRIBE`. The server (`captures/research/httpclient-fuzz/srv.py`) logs the shape,
bytes written and whether the write failed. One subscription per shape, log truncated per case, so the
count of `NOTIFY`s for a shape is itself a signal: **>1 means the client treated the answer as a failure
and retried**.

## Results — 18 shapes, all negative for crash

| shape | answer | verdict |
|---|---|---|
| `control` | 200 + valid LastChange | ok, 1 notify, no retry |
| `status8k` | status line with an 8 KB reason phrase | survived, no retry |
| `hdr1000` | 1000 response headers | survived |
| `hline4k` | one 4 KB header line | survived |
| `hdr1m` | 1 MB of headers (1024 × 1 KB) | survived |
| `nul` | NUL bytes inside a header value | survived |
| `fold` | obs-fold with SP and TAB continuation | survived |
| `negcl` | `Content-Length: -5` | survived |
| `hugecl` | `Content-Length: 4294967295` then a short body | survived |
| `badchunk` | `ZZZ` where a chunk size belongs | survived |
| `chunkhuge` | `FFFFFFFF` chunk size | survived |
| `chunkext` | chunk size with `;foo=bar` extension | survived |
| `chunk0thenbody` | terminating `0` then a body anyway | survived |
| `trailers` | chunked with a `Trailer:` + trailer block | survived |
| `dupecl` | two conflicting `Content-Length` headers | survived |
| `clpluschunk` | `Content-Length` **and** `Transfer-Encoding: chunked` | survived |
| `lfonly` | bare-LF line endings throughout | survived, retried |
| `nocolon` | header line with no colon | survived |
| `truncstatus` | status line cut off after `HTTP/1.1 200` (no CRLF, close) | survived, retried |
| `garbagestatus` | status line that does not start `HTTP/` | survived, retried |
| `noheaders` | 200 with no headers at all, then close | survived |
| `collapse` | close immediately after the blank line | survived |
| `midclose` | `Content-Length: 5000`, 352 bytes, close | survived |
| `slow` | body dribbled 16 B / 150 ms | survived (client waits indefinitely) |
| `e412` | 412 | survived, retried |
| `e500` | 500 | survived, retried |
| `relredirect` | 302, `Location: /control/avt` | survived, **not followed**, retried |
| `refused` | 302, `Location: http://192.168.4.160:9/...` | survived, retried |

Retry set (`truncstatus`, `garbagestatus`, `lfonly`, `e412`, `e500`, `relredirect`, `refused`) is the only
behavioural difference any shape produced: everything is accepted or retried, nothing crashes or wedges.

## Repetition and a storm

The eight shapes most likely to be size/state dependent (`status8k`, `hdr1m`, `chunkhuge`, `dupecl`,
`clpluschunk`, `midclose`, `garbagestatus`, `truncstatus`) were each repeated 6× back to back: **48 runs,
0 liveness failures**. Then 25 rapid `SUBSCRIBE`s in one burst, all to a `garbagestatus` callback: 25× 200,
daemon alive after, 23 NOTIFYs served and retried. No descriptor or queue exhaustion shows at this scale.

## Coverage limits

- The NOTIFY response body is not required to be anything, so body shapes only exercise framing
  (`Content-Length`/chunked/close), not XML: the daemon parses the *event* body elsewhere and errors there
  are swallowed.
- 302s are not followed on this path (a 302 to our own control URL still retries), so redirect loops are
  exercised only as single redirects here; the updater is where redirect-following matters.
