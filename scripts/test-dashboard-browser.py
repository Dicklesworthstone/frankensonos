#!/usr/bin/env python3
"""Exercise actual dashboard assets with explicit synthetic API fixtures.

Requires Python Playwright and Chromium. Component execution intentionally uses
set_content rather than a Rust daemon or physical speakers. This also works in
browser environments whose policy disallows navigation to a local HTTP listener.
"""
from __future__ import annotations
import argparse
from pathlib import Path
import re
from playwright.sync_api import sync_playwright

ROOT = Path(__file__).resolve().parents[1] / 'crates/fsonos-api/src/remote'
SCRIPTS = ['dashboard-core.js', 'dashboard-visualizer.js', 'dashboard-audio.js', 'scene-queue.js', 'dashboard-session.js', 'dashboard-queue.js', 'dashboard-workspace.js', 'dashboard.js']
FIXTURE = r"""
window.fixture = {
  writes: [], calls: [], feeds: [], offline:false, lost:false, partial:false, rawTitle:'II. Fixture movement',
  zones:[{household:'S2',coordinator_room:'Studio',members:['Studio','Kitchen'],transport_state:'playing'},
    {household:'S1',coordinator_room:'Studio',members:['Studio'],transport_state:'paused'}],
  states:['playing','paused'], running:[true,false], mood:'focus', track:'fixture:movement',
  work:{composer:'Fixture composer',title:'Three rooms',album:'Fixture recording',performers:['Fixture quartet'],movements:3,movement:2,
    movement_title:'II. Fixture movement',minutes:21,reason:{summary:'Variety and the owner’s preferences.',factors:[{factor:'artist_spacing',weight:1400},{factor:'feedback',weight:800}],relaxed:[]}},
  scene:{name:'Dinner',saved:1700000000,groups:[{coordinator:'Studio@S2',members:['Kitchen@S2'],source:{kind:'dj',mood:'dinner'},playing:true}],volumes:{'Studio@S2':20,'Kitchen@S2':15},mutes:{}},
  hit:{title:'A fixture song',subtitle:'Fixture artist',kind:'track',source_uri:'spotify:track:fixture'},
};
window.fetch=async(url,init)=>{
  const f=fixture; f.calls.push({url,at:performance.now()});
  if(f.offline) throw new Error('Fixture offline');
  let data;
  const room=decodeURIComponent(String(url).split('/')[2]||'');
  const index=f.zones.findIndex(z=>`${z.coordinator_room}@${z.household}`===room);
  if(url==='/zones') data=f.zones;
  else if(url.endsWith('/state')) data={zone:f.zones[index],transport_state:f.states[index],volume:20,group_volume:22,
    track:{uri:f.track,title:f.rawTitle,creator:'Fixture artist',album:'Fixture album',position_secs:20,duration_secs:300,queue_position:2}};
  else if(url.endsWith('/dj')) data={zone:'Studio',running:f.running[index],now:f.running[index]?f.work:null,
    next:f.running[index]?[{...f.work,movement:undefined,title:'The next work'}]:[],steering:{source:'session',mood:f.mood,constraints:{energy_bias:-1},added:{}}};
  else if(url==='/dj/moods') data={moods:[{name:'focus',constraints:{energy_bias:-1}},{name:'dinner',constraints:{energy_bias:0}}]};
  else if(url==='/scenes') data=[f.scene];
  else if(url==='/scenes/Dinner') data=f.scene;
  else if(url.startsWith('/library/search')) data=[f.hit];
  else if(url.startsWith('/history')) data=[];
  else if(init.method==='POST'){
    const body=JSON.parse(init.body); f.writes.push({url,body});
    const target=f.zones.findIndex(z=>`${z.coordinator_room}@${z.household}`===body.zone);
    data={done:'Fixture command accepted',changed:true};
    if(url==='/pause')f.states[target]='paused';
    if(url==='/resume')f.states[target]='playing';
    if(url==='/dj/steer')f.mood=body.mood||'focus';
    if(url==='/dj/stop')f.running[target]=false;
    if(url==='/dj/start')f.running[target]=true;
    if(url==='/dj/feedback')data={zone:'Studio',signal:body.signal,work:`${f.work.composer}: ${f.work.title}`,composer:f.work.composer,done:'Feedback recorded'};
    if(url==='/scenes/Dinner/apply')data={scene:'Dinner',done:'Scene attempted',changed:true,complete:!f.partial,steps:['Volume set'],failed:f.partial?[{step:'Join',detail:'Speaker offline'}]:[],skipped:[]};
    if(f.lost)throw new Error('Fixture lost reply');
  }else throw new Error(`Unexpected request ${url}`);
  return new Response(JSON.stringify(data),{headers:{'Content-Type':'application/json'}});
};
window.EventSource=class{
  constructor(){this.listeners={};this.closed=false;fixture.feeds.push(this);setTimeout(()=>this.onopen?.(),0);}
  addEventListener(name,fn){this.listeners[name]=fn;}
  emit(name){this.listeners[name]?.({data:'{}'});}
  close(){this.closed=true;}
};
"""

def load(page, preview=False):
    html=(ROOT/'dashboard.html').read_text()
    if preview: html=html.replace('<body>','<body data-preview="true">')
    html=re.sub(r'<link[^>]+>|<script[^>]+></script>','',html)
    page.set_content(html)
    page.add_style_tag(content=(ROOT/'dashboard.css').read_text())
    page.add_script_tag(content=FIXTURE)
    for name in SCRIPTS: page.add_script_tag(content=(ROOT/name).read_text())
    page.wait_for_selector('.room-card')
    page.wait_for_function("document.getElementById('dj-insight').textContent.includes('Fixture') || document.body.dataset.preview==='true'")

def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--browser-executable')
    parser.add_argument('--screenshots-dir',type=Path)
    args=parser.parse_args()
    checks=[]
    with sync_playwright() as p:
        browser=p.chromium.launch(headless=True,executable_path=args.browser_executable)
        page=browser.new_page(viewport={'width':1536,'height':1120},reduced_motion='reduce')
        errors=[];page.on('pageerror',lambda e:errors.append(str(e)))
        load(page)
        page.wait_for_function("!document.getElementById('play').disabled")
        assert page.locator('.room-card').count()==2
        assert page.locator('#track-title').inner_text()=='Three rooms'
        assert page.locator('#work-count').inner_text()=='2 / 3'
        assert page.locator('.movement.current').count()==1
        assert '×1.4' in page.locator('#factor-list').inner_text()
        assert '×0.8' in page.locator('#factor-list').inner_text()
        checks.append('whole-work progress and actual selection multipliers')
        page.locator('#play').click()
        page.wait_for_function("document.getElementById('stage-status').textContent==='PAUSED'")
        assert page.evaluate('fixture.writes[0]')=={'url':'/pause','body':{'zone':'Studio@S2'}}
        page.locator('.room-card').nth(1).click()
        page.wait_for_function("!document.getElementById('play').disabled")
        page.locator('#play').click()
        page.wait_for_function('fixture.writes.length===2')
        assert page.evaluate('fixture.writes[1].body.zone')=='Studio@S1'
        checks.append('native transport and cross-household same-name targeting')
        page.locator('.room-card').nth(0).click()
        page.wait_for_function("!document.getElementById('like').disabled")
        page.locator('#mood-duration').select_option('7200')
        page.locator('[data-mood="dinner"]').click()
        assert page.locator('#review-dialog').is_visible()
        assert '120 minutes' in page.locator('#review-details').inner_text()
        assert page.evaluate('fixture.writes.length')==2
        page.locator('#confirm-action').click()
        page.wait_for_function('fixture.writes.length===3')
        assert page.evaluate('fixture.writes[2]')=={'url':'/dj/steer','body':{'zone':'Studio@S2','mood':'dinner','for_secs':7200}}
        page.wait_for_function("!document.getElementById('like').disabled")
        page.locator('#like').click();page.locator('#confirm-action').click()
        page.wait_for_function('fixture.writes.length===4')
        assert page.evaluate('fixture.writes[3]')=={'url':'/dj/feedback','body':{'zone':'Studio@S2','signal':'like'}}
        checks.append('reviewed mood duration and feedback without hidden writes')
        page.wait_for_function("!document.getElementById('skip-work').disabled")
        page.locator('#skip-work').click();page.locator('#confirm-action').click()
        page.wait_for_function('fixture.writes.length===5')
        assert page.evaluate('fixture.writes[4].url')=='/dj/skip'
        checks.append('whole-work skip uses DJ, not track-next')
        page.wait_for_function("!document.querySelector('.scene-card').disabled")
        page.locator('[data-scene="Dinner"]').click()
        page.wait_for_selector('#review-dialog[open]')
        assert 'Kitchen@S2: 15' in page.locator('#review-details').inner_text()
        page.evaluate('fixture.scene.volumes["Kitchen@S2"]=50')
        page.locator('#confirm-action').click()
        page.wait_for_function("document.getElementById('outcome').textContent.includes('scene or household changed')")
        assert page.evaluate('fixture.writes.length')==5
        page.locator('#refresh').click()
        page.wait_for_function("!document.querySelector('.scene-card').disabled")
        page.locator('[data-scene="Dinner"]').click();page.wait_for_selector('#review-dialog[open]')
        page.evaluate('fixture.partial=true');page.locator('#confirm-action').click()
        page.wait_for_function('fixture.writes.length===6')
        page.wait_for_function("document.getElementById('scene-result').textContent.includes('Speaker offline')")
        assert page.evaluate('fixture.writes[5]')=={'url':'/scenes/Dinner/apply','body':{}}
        checks.append('scene definition recheck and visible partial outcomes')
        page.locator('#open-library').click()
        page.locator('#search-input').fill('fixture');page.locator('#search-form').evaluate('(form)=>form.requestSubmit()')
        page.wait_for_selector('.search-result');page.locator('.search-result').click()
        page.wait_for_selector('#review-dialog[open]');page.locator('#confirm-action').click()
        page.wait_for_function('fixture.writes.length===7')
        assert page.evaluate('fixture.writes[6]')=={'url':'/play','body':{'zone':'Studio@S2','source_uri':'spotify:track:fixture','title':'A fixture song'}}
        checks.append('search-to-reviewed-exact-playback')
        page.wait_for_function("!document.getElementById('next').disabled")
        page.evaluate('fixture.lost=true');page.locator('#next').click()
        page.wait_for_function("document.getElementById('outcome').textContent.includes('Outcome uncertain')")
        assert page.locator('#next').is_disabled()
        page.evaluate("fixture.feeds.at(-1).emit('zone.state')");page.wait_for_timeout(650)
        assert page.locator('#next').is_disabled()
        assert page.evaluate('fixture.writes.length')==8
        page.evaluate('fixture.lost=false');page.locator('#refresh').click()
        page.wait_for_function("!document.getElementById('next').disabled")
        checks.append('uncertain write stays blocked through passive refresh')
        page.evaluate("fixture.rawTitle='<img src=x onerror=alert(1)>';fixture.running[0]=false;fixture.feeds.at(-1).emit('zone.state')")
        page.wait_for_function("document.getElementById('track-title').textContent.startsWith('<img')")
        assert page.locator('#track-title img').count()==0
        checks.append('untrusted metadata rendered as text')
        before=page.evaluate("fixture.calls.filter(c=>c.url==='/zones').length")
        page.evaluate("window.storm=setInterval(()=>fixture.feeds.at(-1).emit('zone.state'),40)")
        page.wait_for_timeout(850)
        after=page.evaluate("fixture.calls.filter(c=>c.url==='/zones').length")
        page.evaluate('clearInterval(window.storm)')
        assert 1<=after-before<=4,(before,after)
        checks.append('continuous events coalesce without starving fresh state')
        page.evaluate("fixture.offline=true;fixture.feeds.at(-1).emit('zone.state')")
        page.wait_for_function("!document.getElementById('error').hidden")
        assert page.locator('#play').is_disabled()
        assert page.locator('.room-state.stale').count()==2
        page.evaluate('fixture.offline=false');page.locator('#refresh').click()
        page.wait_for_function("document.getElementById('error').hidden")
        checks.append('offline state and explicit recovery')
        page.evaluate("window.dispatchEvent(new Event('pagehide'))")
        assert page.evaluate('fixture.feeds.at(-1).closed')
        before=page.evaluate('fixture.calls.length');page.wait_for_timeout(500)
        assert page.evaluate('fixture.calls.length')==before
        page.evaluate("window.dispatchEvent(new Event('pageshow'))")
        page.wait_for_function('fixture.feeds.length===2')
        checks.append('hidden-page read shutdown and reconnect')
        page.locator('[data-house-view="map"]').click()
        assert page.locator('#topology').is_visible()
        assert page.locator('.map-node.leader').count()==2
        page.locator('.map-select').nth(1).click()
        assert 'S1' in page.locator('#dock-target').inner_text()
        checks.append('truthful household map and group selection')
        page.close()
        preview=browser.new_page(viewport={'width':1536,'height':1120},reduced_motion='reduce')
        preview.on('pageerror',lambda e:errors.append(str(e)));load(preview,True)
        preview.wait_for_function("document.getElementById('track-title').textContent==='Nocturne in Blue'")
        assert preview.locator('#demo-banner').is_visible()
        assert preview.locator('#play').is_disabled()
        assert preview.evaluate('fixture.calls.length')==0
        preview.locator('[data-mood="dinner"]').click();assert preview.locator('#confirm-action').is_disabled()
        preview.locator('#close-review').click()
        assert preview.evaluate('fixture.writes.length')==0
        checks.append('preview can explore reviews without contacting any speaker')
        assert preview.locator('#motion').inner_text()=='Resume motion'
        frames=[]
        for mode in ['aurora','orbit','contour']:
            preview.locator(f'[data-mode="{mode}"]').click()
            assert preview.locator(f'[data-mode="{mode}"]').get_attribute('aria-pressed')=='true'
            frames.append(preview.locator('#visualizer').evaluate('(c)=>c.toDataURL()'))
        assert len(set(frames))==3
        preview.locator('[data-mode="aurora"]').click()
        checks.append('three genuinely different rendered modes and reduced-motion default')
        preview.locator('#theme').click()
        assert preview.evaluate('document.documentElement.dataset.theme')=='light'
        preview.locator('#theme').click()
        preview.locator('#immerse').click()
        assert 'immersive' in preview.locator('#stage').get_attribute('class')
        assert preview.evaluate("document.getElementById('room-section').inert")
        preview.keyboard.press('Shift+Tab');assert preview.evaluate("document.getElementById('stage').contains(document.activeElement)")
        if args.screenshots_dir:
            args.screenshots_dir.mkdir(parents=True,exist_ok=True)
            preview.screenshot(path=str(args.screenshots_dir/'listening-room-immersive.png'))
        preview.keyboard.press('Escape')
        assert 'immersive' not in preview.locator('#stage').get_attribute('class')
        assert not preview.evaluate("document.getElementById('room-section').inert")
        assert preview.evaluate("document.activeElement.id")=='immerse'
        checks.append('dark/light appearance and immersive focus/Escape restoration')
        preview.locator('.audio-options summary').click();preview.locator('#microphone').click()
        preview.wait_for_function("document.getElementById('audio-status').textContent.includes('localhost or HTTPS')")
        preview.locator('.audio-options summary').click()
        checks.append('microphone fallback without imaginary audio analysis')
        if args.screenshots_dir:
            preview.screenshot(path=str(args.screenshots_dir/'listening-room-desktop.png'))
            preview.locator('#theme').click();preview.screenshot(path=str(args.screenshots_dir/'listening-room-light.png'));preview.locator('#theme').click()
            preview.locator('#scene-section').scroll_into_view_if_needed();preview.screenshot(path=str(args.screenshots_dir/'listening-room-scenes.png'));preview.evaluate('window.scrollTo(0,0)')
        for width in [1536,1280,1050,820,768,600,390,320]:
            preview.set_viewport_size({'width':width,'height':900});preview.wait_for_timeout(100)
            assert not preview.evaluate('document.documentElement.scrollWidth>innerWidth'),width
            if width<=820:
                preview.locator('#toggle-volume').click()
                assert preview.locator('#volume-up').is_visible(),width
                preview.locator('#toggle-volume').click()
            if args.screenshots_dir and width==390:preview.screenshot(path=str(args.screenshots_dir/'listening-room-mobile.png'),full_page=True)
        checks.append('eight responsive widths; group volume remains available on mobile')
        assert not errors,errors
        for check in checks:print('PASS:',check)
        print(f'PASS: {len(checks)} browser acceptance groups, no page exceptions.')
        browser.close()

if __name__=='__main__':main()
