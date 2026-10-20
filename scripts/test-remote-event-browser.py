#!/usr/bin/env python3
"""Chromium component checks: actual Streams/DOM, fixture HTTP and virtual time.

This executes the shipped event client and full remote bootstrap. It does not
start the Rust daemon, browse a tailnet, or operate physical speakers.
"""
from pathlib import Path
import argparse
import shutil
from playwright.sync_api import sync_playwright

REMOTE = Path(__file__).resolve().parents[1] / 'crates/fsonos-api/src/remote'
FIXTURE = r"""
const fixture = { at:0, serial:0, timers:new Map(), requests:[], streams:[], holdWrite:null };
Math.random = () => 0;
Date.now = () => fixture.at;
performance.now = () => fixture.at;
window.setTimeout = (fn, ms) => { fixture.timers.set(++fixture.serial, {fn,at:fixture.at+ms}); return fixture.serial; };
window.clearTimeout = id => fixture.timers.delete(id);
const settle = async () => { for(let i=0;i<100;i++) await Promise.resolve(); };
fixture.tick = async ms => {
  const end=fixture.at+ms; await settle();
  for(;;) {
    const next=[...fixture.timers].filter(([,t])=>t.at<=end).sort((a,b)=>a[1].at-b[1].at||a[0]-b[0])[0];
    if(!next) break;
    fixture.timers.delete(next[0]); fixture.at=next[1].at; next[1].fn(); await settle();
  }
  fixture.at=end; await new Promise(requestAnimationFrame); await settle();
};
window.fetch = async (url,init) => {
  fixture.requests.push({url,init});
  if(url==='/events') {
    const entry={controller:null,cancelled:0};
    const body=new ReadableStream({start(c){entry.controller=c;},cancel(){entry.cancelled++;}});
    entry.send=text=>entry.controller.enqueue(new TextEncoder().encode(text));
    fixture.streams.push(entry);
    return new Response(body,{headers:{'content-type':'text/event-stream'}});
  }
  if(init.method==='POST') return await new Promise(resolve=>{fixture.holdWrite=()=>resolve(new Response('{"done":"Advanced","changed":true}'));});
  return new Response(JSON.stringify(url==='/doctor'?{checks:[]}:[]), {headers:{'content-type':'application/json'}});
};
fixture.label=()=>document.querySelector('#live').textContent;
fixture.reads=()=>fixture.requests.filter(r=>r.url==='/zones').length;
fixture.event=text=>fixture.streams.at(-1).send(text);
"""


def run_case(browser, name, code):
    page = browser.new_page()
    errors = []
    page.on('pageerror', lambda error: errors.append(str(error)))
    try:
        page.set_content('<!doctype html><button id="pause-all">Pause everywhere</button><p id="live"></p>'
                         '<main id="households"></main><section id="scenes-section"><div id="scenes"></div></section>'
                         '<div id="doctor"></div><div id="toast"></div>')
        page.add_script_tag(content=FIXTURE)
        page.add_script_tag(content=(REMOTE / 'event-feed.js').read_text())
        page.add_script_tag(content=(REMOTE / 'read-requests.js').read_text())
        page.add_script_tag(content=(REMOTE / 'remote.js').read_text())
        page.evaluate('fixture.tick(300)')
        assert page.evaluate('fixture.label()') == 'Live'
        result = page.evaluate('async () => {' + code + '}')
        assert result is True, page.evaluate('({label:fixture.label(),at:fixture.at,reads:fixture.reads(),streams:fixture.streams.length,cancelled:fixture.streams.map(s=>s.cancelled),timers:[...fixture.timers.values()].map(t=>t.at)})')
        assert not errors, errors
        print(f'PASS {name}', flush=True)
    finally:
        page.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--chromium', default=shutil.which('chromium') or shutil.which('chromium-browser'))
    args = parser.parse_args()
    cases = [
        ('quiet heartbeats keep one subscription without polling', """
          for(let i=0;i<12;i++){fixture.event(': heartbeat\\n\\n');await fixture.tick(15000);}
          return fixture.streams.length===1 && fixture.reads()===1 && fixture.label()==='Live';
        """),
        ('silent body expires, cancels its reader and reconciles after reconnect', """
          await fixture.tick(44700);
          if(fixture.label()!=='Refreshing without live events'||fixture.streams[0].cancelled!==1)return false;
          await fixture.tick(1000);
          if(fixture.label()!=='Synchronizing…')return false;
          await fixture.tick(300);
          return fixture.streams.length===2 && fixture.reads()===2 && fixture.label()==='Live';
        """),
        ('reconnect carries a complete u64 cursor without rounding', """
          fixture.event('id: 9007199254740993\\nevent: zone.state\\ndata: {"room":"Den"}\\n\\n');
          await fixture.tick(300);
          fixture.streams[0].controller.close();await settle();await fixture.tick(1300);
          const requests=fixture.requests.filter(r=>r.url==='/events');
          return requests.length===2 && requests[1].init.headers['Last-Event-ID']==='9007199254740993';
        """),
        ('journal reset clears the previous cursor and refreshes without another event', """
          fixture.event('id: 99\\ndata: old\\n\\nevent: events.reset\\ndata: {"oldest":1}\\n\\n');
          await fixture.tick(300);
          if(fixture.reads()!==2)return false;
          fixture.streams[0].controller.close();await settle();await fixture.tick(1300);
          return !fixture.requests.filter(r=>r.url==='/events')[1].init.headers['Last-Event-ID'];
        """),
        ('pagehide releases native stream and pageshow opens only one replacement', """
          dispatchEvent(new Event('pagehide'));await settle();const before=fixture.requests.length;
          await fixture.tick(600000);
          if(fixture.requests.length!==before||fixture.streams[0].cancelled!==1)return false;
          dispatchEvent(new Event('pageshow'));dispatchEvent(new Event('pageshow'));await fixture.tick(300);
          return fixture.streams.length===2 && fixture.reads()===2 && fixture.label()==='Live';
        """),
        ('stream recovery leaves a pending command alone', """
          const operation=call('next',{body:{zone:'Den@S2'}});await settle();
          fixture.streams[0].controller.close();await settle();await fixture.tick(1300);
          const writes=fixture.requests.filter(r=>r.init.method==='POST');
          if(writes.length!==1||writes[0].init.signal.aborted)return false;
          fixture.holdWrite();return (await operation).done==='Advanced';
        """),
    ]
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch(executable_path=args.chromium, headless=True, args=['--no-sandbox'])
        try:
            for name, code in cases:
                run_case(browser, name, code)
        finally:
            browser.close()
    print(f'{len(cases)} Chromium cases passed; HTTP and time are fixtures, Streams and DOM are native.')


if __name__ == '__main__':
    main()
