#!/usr/bin/env python3
"""Chromium lifecycle checks for the shipped dashboard page and live components.

The DOM is a minimal native host; HTTP, time and the unrelated visual/audio/
workspace collaborators are fixtures. No speaker, daemon or browser navigation
acceptance is claimed. Requires Python Playwright and installed Chromium.
"""
from pathlib import Path
import argparse
import re
import shutil
from playwright.sync_api import expect, sync_playwright

ROOT = Path(__file__).resolve().parents[1]
REMOTE = ROOT / "crates/fsonos-api/src/remote"
FIXTURE = r"""
window.fixture = {
  requests: [], streams: [], now: 0, timers: new Map(), serial: 0,
  title: 'First song', playing: 'playing', mode: '', release: null, stops: 0,
};
const f = fixture;
Date.now = () => f.now;
Object.defineProperty(performance, 'now', {value: () => f.now});
window.setTimeout = (fn, delay=0) => {
  f.timers.set(++f.serial, {fn, at:f.now+Math.max(0, Number(delay)||0)}); return f.serial;
};
window.clearTimeout = id => f.timers.delete(id);
f.settle = async () => {for(let n=0;n<100;n++)await Promise.resolve();};
f.tick = async ms => {
  const end=f.now+ms; await f.settle();
  for(let n=0;;n++) {
    if(n>10000)throw new Error('timer storm');
    const next=[...f.timers].filter(([,t])=>t.at<=end).sort((a,b)=>a[1].at-b[1].at || a[0]-b[0])[0];
    if(!next)break;
    f.timers.delete(next[0]); f.now=next[1].at; next[1].fn(); await f.settle();
  }
  f.now=end; await f.settle();
};
f.zones = () => [
  {household:'S2',coordinator_room:'Den',members:['Den'],transport_state:f.playing},
  {household:'S1',coordinator_room:'Den',members:['Den'],transport_state:'paused'},
];
f.state = url => ({zone:f.zones().find(z=>url.includes(encodeURIComponent(`${z.coordinator_room}@${z.household}`))),
  transport_state:f.playing,volume:20,group_volume:20,mute:false,
  track:{uri:'fixture:one',title:f.title,creator:'Fixture artist',album:'Fixture album',position_secs:20,duration_secs:600,queue_position:1}});
f.frame = text => f.streams.at(-1).source.enqueue(new TextEncoder().encode(text));
f.lose = () => f.streams.at(-1).source.error(new Error('connection lost'));
f.writes = () => f.requests.filter(r=>r.init.method==='POST');
f.snapshots = () => f.requests.filter(r=>r.url==='/zones').length;
f.hide = value => {
  Object.defineProperty(document,'hidden',{value,configurable:true});
  document.dispatchEvent(new Event('visibilitychange'));
};
window.fetch = async (url,init) => {
  f.requests.push({url,init,at:f.now});
  if(url==='/events') {
    const record={source:null,cancelled:false};
    const body=new ReadableStream({start(c){record.source=c;},cancel(){record.cancelled=true;}});
    f.streams.push(record);record.source.enqueue(new TextEncoder().encode(': heartbeat\n\n'));
    return new Response(body,{headers:{'content-type':'text/event-stream'}});
  }
  if(f.mode==='hold-state' && url.endsWith('/state')) await new Promise(resolve=>{f.release=resolve;});
  if(f.mode==='hold-dj' && url.endsWith('/dj')) await new Promise(resolve=>{f.release=resolve;});
  if(f.mode==='hold-post' && init.method==='POST') await new Promise(resolve=>{f.release=resolve;});
  if(f.mode==='lost-post' && init.method==='POST') throw new Error('lost command acknowledgement');
  const value=init.method==='POST'?{done:'Accepted exactly once',changed:true}
    :url==='/zones'?f.zones():url.endsWith('/state')?f.state(url)
    :url.endsWith('/dj')?{zone:'Den',running:false,now:null,next:[],steering:{source:'none'}}
    :url==='/dj/moods'?{moods:[{name:'focus',constraints:{}}]}:[];
  // JSON bodies are deterministic fixtures; the event body above is a native stream.
  return {ok:true,status:200,headers:new Headers({'content-type':'application/json'}),text:async()=>JSON.stringify(value)};
};
window.DashboardVisualizer = {MODES:['aurora'],create:()=>({supported:true,
  resize(){},mode(){},track(){},audio(){},pause(){},visible(){},immersive(){},dispose(){}})};
window.DashboardAudio = {create:()=>({state:{status:'idle'},start(){},stop(){f.stops++;}})};
window.DashboardWorkspace = {create:()=>({observe(){},invalidate(){},suspend(){},resume(){},immerse(){}})};
"""


def host(script, demo=False):
    ids = set(re.findall(r"\$\('([^']+)'\)", script))
    ids.update(['art', 'dock-art', 'like', 'dislike', 'skip-work', 'open-library', 'open-library-nav'])
    review = {'review-title','review-target','review-description','review-details','review-buttons','close-review'}
    search = {'search-scope','search-results','search-input','search-form','close-search'}
    buttons = {'play','previous','next','refresh','theme','microphone','motion','dj-start-stop','dj-clear','volume-down','volume-up',
               'like','dislike','skip-work','open-library','open-library-nav','close-review','close-search','history-refresh','immerse','nav-immerse','exit-immerse'}
    def node(key):
        if key == 'theme': return '<button id="theme"><svg><use></use></svg></button>'
        if key == 'play-icon': return '<svg id="play-icon"><use></use></svg>'
        if key == 'search-input': return '<input id="search-input">'
        if key == 'search-form': return '<form id="search-form"></form>'
        if key == 'mood-duration': return '<select id="mood-duration"><option value="">Until cleared</option></select>'
        tag = 'button' if key in buttons else 'progress' if key == 'progress' else 'div'
        return f'<{tag} id="{key}"></{tag}>'
    ordinary = ids - review - search - {'review-dialog','search-dialog'}
    return '<!doctype html><html><head><meta charset="utf-8"></head><body' + (' data-preview="true"' if demo else '') + '>' + \
        ''.join(node(k) for k in sorted(ordinary)) + \
        '<dialog id="review-dialog">'+''.join(node(k) for k in sorted(review))+'</dialog>'+\
        '<dialog id="search-dialog">'+''.join(node(k) for k in sorted(search))+'</dialog></body></html>'


def setup(browser, demo=False):
    page = browser.new_page()
    page.set_default_timeout(3000)
    errors = []
    page.on('pageerror', lambda e: errors.append(str(e)))
    script = (REMOTE/'dashboard.js').read_text()
    page.set_content(host(script, demo))
    page.add_script_tag(content=FIXTURE)
    for name in ['event-feed.js','dashboard-core.js','dashboard-live.js','dashboard-session.js','dashboard.js']:
        page.add_script_tag(content=(REMOTE/name).read_text())
    page.evaluate('fixture.tick(350)')
    assert not errors, errors
    return page, errors


def click(page, id):
    page.locator('#'+id).evaluate('(n)=>n.click()')
    page.evaluate('fixture.settle()')


def live(page):
    assert page.locator('#connection').inner_text() == 'Live from your daemon'
    assert page.locator('#track-title').inner_text() == 'First song'
    before = page.evaluate('fixture.requests.length')
    for _ in range(8):
        page.evaluate("async()=>{await fixture.tick(15000);fixture.frame(': heartbeat\\n\\n');await fixture.settle();}")
    assert page.evaluate('fixture.requests.length') == before
    assert page.locator('#play').is_enabled()
    assert page.evaluate('fixture.writes().length') == 0


def reset(page):
    click(page, 'dj-start-stop')
    expect(page.locator('#review-dialog')).to_be_visible()
    page.evaluate("fixture.title='After reset'; fixture.frame('event: events.reset\\ndata: {\"oldest\":1}\\n\\n'); fixture.settle()")
    expect(page.locator('#review-dialog')).not_to_be_visible()
    assert page.locator('#play').is_disabled()
    page.evaluate('fixture.tick(350)')
    assert page.locator('#track-title').inner_text() == 'After reset'
    assert page.locator('#connection').inner_text() == 'Live from your daemon'
    assert page.evaluate('fixture.writes().length') == 0


def silent(page):
    page.evaluate('fixture.tick(44650)')
    assert page.locator('#connection').inner_text() != 'Live from your daemon'
    assert page.locator('#play').is_disabled()
    assert page.evaluate('fixture.streams[0].cancelled')
    page.evaluate("fixture.title='After recovery';fixture.tick(2000)")
    assert page.locator('#track-title').inner_text() == 'After recovery'
    assert page.locator('#connection').inner_text() == 'Live from your daemon'
    assert page.evaluate('fixture.streams.length') == 2


def visibility(page):
    page.evaluate('fixture.hide(true);fixture.settle()')
    count = page.evaluate('fixture.requests.length')
    page.evaluate('fixture.tick(180000)')
    assert page.evaluate('fixture.requests.length') == count
    assert page.locator('#connection').inner_text() == 'Suspended while hidden'
    assert page.evaluate('fixture.stops') > 0
    page.evaluate("fixture.title='After foreground';fixture.hide(false);fixture.tick(350)")
    assert page.locator('#track-title').inner_text() == 'After foreground'
    assert page.locator('#connection').inner_text() == 'Live from your daemon'


def command(page):
    page.evaluate("fixture.mode='hold-post'")
    click(page,'play')
    assert page.evaluate('fixture.writes().length') == 1
    page.evaluate('fixture.lose();fixture.tick(2000)')
    assert not page.evaluate('fixture.writes()[0].init.signal.aborted')
    page.evaluate("fixture.mode='';fixture.release();fixture.settle()")
    page.evaluate('fixture.tick(2000)')
    assert page.evaluate('fixture.writes().length') == 1
    assert page.locator('#outcome').inner_text() == 'Accepted exactly once'


def preflight(page):
    page.evaluate("fixture.mode='hold-state'")
    click(page,'play')
    page.evaluate("fixture.frame('event: events.reset\\ndata: {}\\n\\n');fixture.settle()")
    assert page.evaluate('fixture.writes().length') == 0
    page.evaluate("fixture.mode='';fixture.release();fixture.tick(1350)")
    assert page.evaluate('fixture.writes().length') == 0
    assert 'No command was sent' in page.locator('#outcome').inner_text()


def quiet_dj(page):
    for _ in range(4):
        page.evaluate("async()=>{await fixture.tick(15000);fixture.frame(': heartbeat\\n\\n');await fixture.settle();}")
    before = page.evaluate("fixture.requests.filter(r=>r.url.endsWith('/dj')).length")
    click(page,'dj-start-stop')
    expect(page.locator('#review-dialog')).to_be_visible()
    assert page.locator('#review-title').inner_text() == 'Start your personal DJ'
    assert page.evaluate("fixture.requests.filter(r=>r.url.endsWith('/dj')).length") == before+1
    assert page.evaluate('fixture.writes().length') == 0


def dj_target(page):
    page.evaluate("fixture.mode='hold-dj'")
    click(page,'dj-start-stop')
    page.locator('[data-room="Den@S1"]').evaluate('(n)=>n.click()')
    page.evaluate("fixture.mode='';fixture.release();fixture.settle()")
    expect(page.locator('#review-dialog')).not_to_be_visible()
    assert page.evaluate('fixture.writes().length') == 0


def uncertain(page):
    page.evaluate("fixture.mode='lost-post'")
    click(page,'play')
    assert 'Outcome uncertain' in page.locator('#outcome').inner_text()
    page.evaluate("fixture.mode='';fixture.frame('event: events.reset\\ndata: {}\\n\\n');fixture.tick(350)")
    assert page.locator('#play').is_disabled()
    click(page,'refresh');page.evaluate('fixture.tick(0)')
    assert page.locator('#play').is_enabled()
    assert page.evaluate('fixture.writes().length') == 1


def preview(page):
    page.evaluate('fixture.tick(180000)')
    assert page.locator('#connection').inner_text() == 'Read-only design preview'
    assert page.evaluate('fixture.requests.length') == 0
    assert page.locator('#play').is_disabled()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--chromium', default=shutil.which('chromium') or shutil.which('chromium-browser'))
    args = parser.parse_args()
    cases = [('quiet live startup',live),('isolated reset',reset),('silent stream recovery',silent),
             ('visibility lifecycle',visibility),('submitted command survives disconnect',command),
             ('cancelled preflight sends nothing',preflight),('on-demand quiet DJ review',quiet_dj),
             ('DJ review cannot follow a changed target',dj_target),('explicit uncertain recovery',uncertain),
             ('design preview stays offline',preview)]
    with sync_playwright() as p:
        browser = p.chromium.launch(executable_path=args.chromium, headless=True, args=['--no-sandbox'])
        try:
            for name, run in cases:
                page, errors = setup(browser, run is preview)
                try:
                    run(page)
                    assert not errors, errors
                    print('PASS '+name, flush=True)
                except Exception:
                    print(page.evaluate('JSON.stringify({at:fixture.now,mode:fixture.mode,connection:document.querySelector("#connection").textContent,error:document.querySelector("#error").textContent,review:document.querySelector("#review-dialog").open,disabled:document.querySelector("#dj-start-stop").disabled,outcome:document.querySelector("#outcome").textContent,requests:fixture.requests.map(r=>[r.url,r.at,r.init.signal.aborted])})'),flush=True)
                    raise
                finally:
                    page.close()
        finally:
            browser.close()
    print(f'{len(cases)} Chromium lifecycle cases passed; native DOM/Streams, fixture HTTP/time and visual collaborators.')

if __name__ == '__main__':
    main()
