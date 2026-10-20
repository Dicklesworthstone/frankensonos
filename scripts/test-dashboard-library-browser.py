#!/usr/bin/env python3
"""Chromium component checks with real dashboard models/workspace and fixture HTTP.

The host and review presentation are minimal fixtures, not a running Rust daemon.
Requires Playwright and an installed Chromium; no external network is used.
"""
from pathlib import Path
import argparse
import shutil
from playwright.sync_api import sync_playwright, expect

ROOT = Path(__file__).resolve().parents[1]
REMOTE = ROOT / 'crates/fsonos-api/src/remote'
HTML = '''<!doctype html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width">
<style>body{margin:0;font:16px system-ui}dialog{box-sizing:border-box;width:min(95vw,54rem);max-height:95dvh;overflow:auto}
button,input,select{font:inherit}dialog p{overflow-wrap:anywhere} .fixture-extras{display:none}</style></head><body>
<button id="open-search">Choose music</button><p id="outcome"></p>
<dialog id="search-dialog"><h2>Find music</h2><button id="close-search">Close search</button>
<input id="search-input" aria-label="Search music"><div id="search-results"></div></dialog>
<dialog id="review-dialog"><h2 id="review-title"></h2><p id="review-target"></p><p id="review-detail"></p>
<button id="cancel-review">Cancel review</button><button id="confirm-review">Confirm action</button></dialog>
<div class="fixture-extras"><div id="queue-items"></div><p id="queue-status"></p><p id="queue-page"></p>
<button id="queue-prev"></button><button id="queue-next"></button><button id="queue-refresh"></button>
<button id="tab-picks"></button><button id="tab-queue"></button><div id="dj-picks-panel"></div><div id="speaker-queue-panel"></div>
<button id="dock-queue"></button><h3 id="queue-title"></h3><dialog id="rooms-dialog"><input id="room-filter"><div id="room-choices"></div></dialog>
<button id="close-rooms"></button><button id="shortcut-help"></button><dialog id="shortcuts-dialog"></dialog><button id="close-shortcuts"></button>
<button id="toggle-volume"></button><div class="player-dock"></div></div></body></html>'''
FIXTURE = r'''
const fixture={revision:'cache-a',calls:[],writes:[],hold:false,gate:null,lost:false,hostile:false,
  zones:[{household:'S2',coordinator_room:'Studio',members:['Studio','Kitchen'],transport_state:'paused'},
    {household:'S1',coordinator_room:'Studio',members:['Studio','Kitchen'],transport_state:'paused'}]};
const clone=v=>JSON.parse(JSON.stringify(v));
const id=n=>String(n).padStart(22,'0');
function listing(url){
  const p=new URL(url,'http://localhost').searchParams,kind=p.get('kind'),offset=Number(p.get('offset')),
    album=p.get('album_uri'),q=(p.get('q')||'').toLowerCase(),singular=DashboardLibrary.KINDS[kind];
  let all=Array.from({length:album?7:53},(_,i)=>({kind:singular,source_uri:`spotify:${singular}:${id(i+1)}`,
    title:fixture.hostile?'<img src=x onerror=alert(1)>'+ 'W'.repeat(3000):`${singular} ${i+1}`,
    subtitle:'Synthetic performer',explicit:i===1,
    ...(kind==='albums'?{cached_tracks:7}:{}),...(album?{album_uri:album,disc_number:i<3?1:2,track_number:i+1}:{})}));
  if(q)all=all.filter(i=>`${i.title} ${i.subtitle}`.toLowerCase().includes(q));
  const items=all.slice(offset,offset+50);
  return {kind,offset,limit:50,album_uri:album,coverage:'cached',total:all.length,revision:fixture.revision,
    items,next_offset:offset+items.length<all.length?offset+items.length:null};
}
async function call(url,body){
  fixture.calls.push({url,body});
  if(body!==undefined){fixture.writes.push({url,body:clone(body)});if(fixture.lost)throw new Error('Acknowledgement lost');
    return url==='/queue'?{done:'Queued without starting playback',first:1,count:7,playing_queue:false}:{done:'Playing',changed:true};}
  if(url==='/zones')return clone(fixture.zones);
  if(url.endsWith('/state')){
    const z=fixture.zones.find(z=>url.includes(encodeURIComponent(DashboardCore.key(z))));
    return {zone:z,transport_state:'paused',volume:20,group_volume:22,track:{uri:'fixture:playing',queue_position:1}};
  }
  if(url.endsWith('/dj'))return {zone:'Studio',running:false,next:[],steering:{source:'none'}};
  if(url.startsWith('/library/browse?')){
    const result=listing(url);
    if(fixture.hold){fixture.hold=false;return new Promise(resolve=>{fixture.gate=()=>resolve(result);});}
    return result;
  }
  throw new Error('Unexpected fixture route '+url);
}
let workspace;
const controller=DashboardCore.model({call,changed(state){workspace?.observe(state);document.getElementById('outcome').textContent=state.message;}});
const session=DashboardSession.model({call,controller,changed(state){
  const dialog=document.getElementById('review-dialog'),r=state.pending;
  if(!r){if(dialog.open)dialog.close();return;}
  document.getElementById('review-title').textContent=r.title;
  document.getElementById('review-target').textContent=r.target;
  document.getElementById('review-detail').textContent=r.detail;
  document.getElementById('confirm-review').onclick=()=>session.confirm(r);
  document.getElementById('cancel-review').onclick=()=>session.cancel();
  if(!dialog.open)dialog.showModal();document.getElementById('cancel-review').focus();
}});
const DashboardQueue={model:()=>({observe(){},open(){},close(){},read(){},navigate(){},invalidate(){},suspend(){},resume(){}})};
const make=(tag,cls,text)=>{const n=document.createElement(tag);if(cls)n.className=cls;if(text!=null)n.textContent=text;return n;};
const openSearch=()=>document.getElementById('search-dialog').showModal();
document.getElementById('open-search').onclick=openSearch;
document.getElementById('close-search').onclick=()=>document.getElementById('search-dialog').close();
const ready=(async()=>{await controller.refresh();await session.load();
  workspace=DashboardWorkspace.create({call,controller,session,el:make,icon:()=>make('span'),openSearch,native:()=>{},immerse:()=>{}});
  workspace.observe(controller.state);
})();
'''

def setup(browser):
    page = browser.new_page(viewport={'width': 390, 'height': 850})
    page.set_default_timeout(5000)
    errors = []
    page.on('pageerror', lambda error: errors.append(str(error)))
    page.set_content(HTML)
    page.add_style_tag(content=(REMOTE/'dashboard-library.css').read_text())
    for name in ['dashboard-core.js','dashboard-library.js','dashboard-session.js','dashboard-library-panel.js','dashboard-workspace.js']:
        page.add_script_tag(content=(REMOTE/name).read_text())
    page.add_script_tag(content=FIXTURE)
    page.evaluate('ready')
    return page, errors

def open_browser(page):
    page.get_by_role('button', name='Choose music', exact=True).click()
    page.get_by_text('Browse cached library', exact=True).click()
    expect(page.locator('.dashboard-library [role=status]')).to_contain_text('53 entries')

def rows(page):
    return page.locator('.dashboard-library-item')

def writes(page):
    return page.evaluate('fixture.writes')

def pages(page):
    open_browser(page)
    expect(rows(page)).to_have_count(50)
    page.get_by_role('button', name='Next page', exact=True).click()
    expect(rows(page)).to_have_count(3)
    expect(page.get_by_role('navigation', name='Cached library pages')).to_contain_text('51–53 of 53')
    page.get_by_role('button', name='Previous page', exact=True).click()
    expect(rows(page)).to_have_count(50)
    assert writes(page)==[]

def filtering(page):
    open_browser(page)
    entry=page.get_by_role('searchbox', name='Filter cached library')
    entry.fill('album 51')
    expect(page.get_by_role('button', name='Play now: album 1', exact=True)).to_be_disabled()
    expect(rows(page)).to_have_count(1)
    expect(rows(page).first).to_contain_text('album 51')
    expect(entry).to_be_focused()
    entry.fill('album 52')
    entry.press('Enter')
    expect(rows(page).first).to_contain_text('album 52')
    assert writes(page)==[]

def drilldown(page):
    open_browser(page)
    page.get_by_role('button', name='View cached tracks', exact=True).nth(2).click()
    expect(rows(page)).to_have_count(7)
    expect(rows(page).nth(3)).to_contain_text('Disc 2, track 4')
    expect(page.locator('.dashboard-library [role=status]')).to_be_focused()
    query=page.evaluate('fixture.calls.filter(c=>c.url.startsWith("/library/browse")).at(-1).url')
    assert 'album_uri=spotify%3Aalbum%3A0000000000000000000003' in query
    page.get_by_role('button', name='Back to albums', exact=True).click()
    expect(rows(page)).to_have_count(50)


def play_album(page):
    open_browser(page)
    page.get_by_role('button', name='Play now: album 1', exact=True).click()
    expect(page.locator('#review-dialog')).to_be_visible()
    expect(page.locator('#search-dialog')).not_to_be_visible()
    expect(page.locator('#review-detail')).to_contain_text('full album source')
    expect(page.locator('#review-target')).to_have_text('Studio@S2')
    expect(page.get_by_role('button',name='Cancel review')).to_be_focused()
    assert writes(page)==[]
    page.get_by_role('button',name='Confirm action').click()
    expect(page.locator('#outcome')).to_have_text('Playing')
    assert writes(page)==[{'url':'/play','body':{'zone':'Studio@S2','source_uri':'spotify:album:0000000000000000000001','title':'album 1'}}]


def queue_playlist(page):
    open_browser(page)
    page.get_by_role('combobox',name='Library category').select_option('playlists')
    page.get_by_role('button',name='Queue next: playlist 2',exact=True).click()
    expect(page.locator('#review-detail')).to_contain_text('without starting playback')
    page.get_by_role('button',name='Confirm action').click()
    expect(page.locator('#outcome')).to_contain_text('without starting playback')
    assert len(writes(page))==1 and writes(page)[0]['url']=='/queue' and writes(page)[0]['body']['next'] is True


def revision(page):
    open_browser(page)
    page.evaluate("fixture.revision='cache-b'")
    page.get_by_role('button',name='Next page',exact=True).click()
    expect(page.locator('.dashboard-library [role=status]')).to_contain_text('changed between pages')
    expect(page.get_by_role('button',name='Play now: album 1',exact=True)).to_be_disabled()
    page.get_by_role('button',name='Refresh listing',exact=True).click()
    expect(page.get_by_role('button',name='Play now: album 1',exact=True)).to_be_enabled()
    expect(page.get_by_role('navigation',name='Cached library pages')).to_contain_text('1–50 of 53')


def target_fence(page):
    open_browser(page)
    page.evaluate("globalThis.retained=[...document.querySelectorAll('.dashboard-library button')].find(b=>b.textContent==='Play now');controller.select('Studio@S1');retained.click()")
    assert page.evaluate('session.state.pending') is None
    page.get_by_role('button',name='Play now: album 1',exact=True).click()
    expect(page.locator('#review-target')).to_have_text('Studio@S1')
    assert writes(page)==[]


def hidden_preflight(page):
    open_browser(page)
    page.get_by_role('button',name='Play now: album 1',exact=True).click()
    page.evaluate('fixture.hold=true')
    page.get_by_role('button',name='Confirm action').click()
    page.wait_for_function('fixture.gate !== null')
    page.evaluate('workspace.suspend();session.suspend();controller.suspend();fixture.gate()')
    page.wait_for_function('controller.state.busy === false')
    assert writes(page)==[]


def close_read(page):
    open_browser(page)
    page.evaluate('fixture.hold=true')
    page.get_by_role('button',name='Refresh listing',exact=True).click()
    page.wait_for_function('fixture.gate !== null')
    page.get_by_role('button',name='Close search',exact=True).click()
    page.evaluate('fixture.gate()')
    page.wait_for_timeout(30)
    assert not page.evaluate("document.querySelector('.dashboard-library').open")
    assert writes(page)==[]


def no_speakers(page):
    page.evaluate('fixture.zones=[]; controller.refresh()')
    open_browser(page)
    expect(rows(page)).to_have_count(50)
    expect(page.get_by_role('button',name='Play now: album 1',exact=True)).to_be_disabled()
    expect(page.get_by_text('You can browse without speakers.',exact=False)).to_be_visible()


def hostile(page):
    page.evaluate('fixture.hostile=true')
    open_browser(page)
    assert page.locator('.dashboard-library img').count()==0
    for width in [320,390,768,1440]:
        page.set_viewport_size({'width':width,'height':900})
        assert page.evaluate("(()=>{const d=document.getElementById('search-dialog');return d.scrollWidth<=d.clientWidth+1 && document.documentElement.scrollWidth<=innerWidth})()"),width
    assert writes(page)==[]


def lost_reply(page):
    open_browser(page)
    page.get_by_role('button',name='Add to end: album 1',exact=True).click()
    page.evaluate('fixture.lost=true')
    page.get_by_role('button',name='Confirm action').click()
    expect(page.locator('#outcome')).to_contain_text('Outcome uncertain')
    page.evaluate('controller.refresh()')
    open_browser(page)
    expect(page.get_by_role('button',name='Add to end: album 1',exact=True)).to_be_disabled()
    assert len(writes(page))==1


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--chromium', default=shutil.which('chromium') or shutil.which('chromium-browser'))
    args=parser.parse_args()
    cases=[('pagination',pages),('filter and Enter',filtering),('album drill-down',drilldown),('reviewed album playback',play_album),
        ('playlist queue next',queue_playlist),('revision recovery',revision),('retained target fence',target_fence),
        ('hidden preflight',hidden_preflight),('close during read',close_read),('browse without speakers',no_speakers),
        ('safe metadata and four widths',hostile),('lost reply without replay',lost_reply)]
    with sync_playwright() as p:
        browser=p.chromium.launch(executable_path=args.chromium,headless=True,args=['--no-sandbox'])
        try:
            for name,case in cases:
                page,errors=setup(browser)
                try:
                    case(page);assert not errors,errors;print('PASS '+name,flush=True)
                finally:page.close()
        finally:browser.close()
    print(f'{len(cases)} Chromium component cases passed; host, HTTP and review presentation are fixtures.')
if __name__=='__main__':main()
