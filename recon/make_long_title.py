#!/usr/bin/env python3
"""Create a favourite with an arbitrarily long title/res/art, without playing audio.

usage: make_long_title.py [device] [--title N] [--res N] [--art N] [--browse]

Creates the object in FV:2 via ContentDirectory CreateObject (the URI must be
unique or the device answers 803), then optionally browses FV:2 and prints the
lengths the device actually stored. Nothing is played; the queue is untouched.
"""
import sys, random, re, html, subprocess

DEV = sys.argv[1] if len(sys.argv) > 1 and not sys.argv[1].startswith('-') else '192.168.4.84'
BASE = f'http://{DEV}:1400'


def arg(name, default=0):
    if name in sys.argv:
        return int(sys.argv[sys.argv.index(name) + 1])
    return default


TITLE = arg('--title'); RES = arg('--res'); ART = arg('--art')
BROWSE = '--browse' in sys.argv
CD = f'{BASE}/MediaServer/ContentDirectory/Control'


def soap(action, inner):
    body = ('<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">'
            f'<s:Body><u:{action} xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1">'
            f'{inner}</u:{action}></s:Body></s:Envelope>')
    return subprocess.run(['curl', '-s', '-m', '10', '-o', '-', '-w', '\n%{http_code}',
                           '-X', 'POST', '-H',
                           f'SOAPACTION: "urn:schemas-upnp-org:service:ContentDirectory:1#{action}"',
                           '-H', 'Content-Type: text/xml', '--data-binary', body, CD],
                          capture_output=True, text=True).stdout


def main():
    tag = random.randint(100000, 999999)
    title = 'T' * TITLE if TITLE else f'fav{tag}'
    uri = f'http://192.168.4.160:3012/S/u{tag}.mp3'
    if RES:
        uri = f'http://192.168.4.160:3012/S/{"R" * RES}{tag}.mp3'
    el = ('<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" '
          'xmlns:dc="http://purl.org/dc/elements/1.1/" '
          'xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">'
          f'<item id="-1" parentID="FV:2" restricted="1"><dc:title>{title}</dc:title>'
          '<upnp:class>object.item.audioItem.musicTrack</upnp:class>'
          f'<res protocolInfo="http-get:*:audio/mpeg:*">{uri}</res>')
    if ART:
        el += f'<upnp:albumArtURI>http://192.168.4.160:3012/S/{"A" * ART}.jpg</upnp:albumArtURI>'
    el += '</item></DIDL-Lite>'

    out = soap('CreateObject', f'<ContainerID>FV:2</ContainerID><Elements>{html.escape(el)}</Elements>')
    code = out.rsplit('\n', 1)[-1].strip()
    oid = re.search(r'<ObjectID>([^<]*)</ObjectID>', out)
    print(f'create title={len(title)} res={len(uri)} art={ART} -> {code} {oid.group(1) if oid else ""}')
    if code != '200':
        print(out[:300])
        return
    if BROWSE:
        b = soap('Browse', '<ObjectID>FV:2</ObjectID><BrowseFlag>BrowseDirectChildren</BrowseFlag>'
                          '<Filter>*</Filter><StartingIndex>0</StartingIndex>'
                          '<RequestedCount>50</RequestedCount><SortCriteria></SortCriteria>')
        r = re.search(r'<Result>(.*?)</Result>', b, re.S)
        if r:
            d = html.unescape(r.group(1))
            print('stored titles', [len(x) for x in re.findall(r'<dc:title>(.*?)</dc:title>', d, re.S)][-3:])
            print('stored res    ', [len(x) for x in re.findall(r'<res[^>]*>(.*?)</res>', d, re.S)][-3:])
            print('stored art    ', [len(x) for x in re.findall(r'albumArtURI>(.*?)</upnp:albumArtURI>', d, re.S)][-3:])


if __name__ == '__main__':
    main()
