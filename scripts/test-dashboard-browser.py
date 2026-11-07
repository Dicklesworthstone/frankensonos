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
SCRIPTS = ['event-feed.js', 'dashboard-core.js', 'dashboard-seek.js', 'dashboard-live.js', 'dashboard-visualizer.js', 'dashboard-audio.js', 'scene-queue.js', 'dj-direction.js', 'dashboard-session.js', 'dashboard-queue.js', 'dashboard-workspace.js', 'dashboard-play-mode.js', 'dashboard-groups.js', 'timers.js', 'dashboard-sleep.js', 'dashboard-volume.js', 'dashboard-scenes.js', 'schedules.js', 'dashboard-routines.js', 'announcements.js', 'dashboard-announcements.js', 'dashboard.js']
FIXTURE = r"""
window.fixture = {
  writes: [], calls: [], feeds: [], savedScenes: [], routines: [], nextRoutine:100, offline:false, lost:false, partial:false, rawTitle:'II. Fixture movement',
  zones:[{household:'S2',coordinator_room:'Studio',members:['Studio','Kitchen'],transport_state:'playing'},
    {household:'S1',coordinator_room:'Studio',members:['Studio'],transport_state:'paused'}],
  queueRevision:'fixture-queue',
  queue:Array.from({length:120},(_,i)=>({position:i+1,title:[41,97].includes(i)?'A fugue':`Queue track ${i+1}`,
    creator:'Fixture artist',album:'Fixture album',uri:[41,97].includes(i)?'same:duplicate':`fixture:queue:${i+1}`})),
  states:['playing','paused'], running:[true,false], modes:['NORMAL','NORMAL'], timers:[null,null], levels:{'Studio@S2':20,'Kitchen@S2':15,'Studio@S1':20}, muted:{}, mood:'focus', track:'fixture:movement',
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
  const index=f.zones.findIndex(z=>z.members.some(member=>`${member}@${z.household}`===room));
  if(url==='/zones') data=f.zones;
  else if(url.startsWith('/sleep?zone=')) {
    const target=new URLSearchParams(url.split('?')[1]).get('zone');
    const index=f.zones.findIndex(z=>`${z.coordinator_room}@${z.household}`===target);
    data=f.timers[index]?[f.timers[index]]:[];
  }
  else if(url.endsWith('/state')) data={zone:f.zones[index],transport_state:f.states[index],volume:f.levels[room],mute:!!f.muted[room],group_volume:22,
    track:{uri:f.track,title:f.rawTitle,creator:'Fixture artist',album:'Fixture album',position_secs:20,duration_secs:300,queue_position:2}};
  else if(url.includes('/queue?')||url.includes('/queue/search?')){
    const params=new URLSearchParams(url.split('?')[1]),offset=Number(params.get('offset')||0),limit=Number(params.get('limit')||40);
    if(url.includes('/queue/search?')){
      if(params.get('expected_revision')!==f.queueRevision)throw new Error('Fixture queue changed');
      const query=(params.get('q')||'').trim(),terms=query.toLowerCase().split(/\s+/);
      const items=f.queue.filter(row=>row.position>offset&&terms.every(term=>[row.title,row.creator,row.album].join(' ').toLowerCase().includes(term))).slice(0,limit);
      const end=items.length===limit?items.at(-1).position:f.queue.length;
      data={zone:f.zones[index].coordinator_room,query,total:f.queue.length,revision:f.queueRevision,
        offset,examined:end-offset,next_offset:end<f.queue.length?end:null,complete:end===f.queue.length,items};
    }else data={zone:f.zones[index].coordinator_room,total:f.queue.length,playing:2,revision:f.queueRevision,items:f.queue.slice(offset,offset+limit)};
  }
  else if(url.startsWith('/zones/')&&url.endsWith('/play-mode')) data={zone:f.zones[index].coordinator_room,mode:f.modes[index],queue_active:true,dj_active:f.running[index]};
  else if(url.endsWith('/dj')) data={zone:'Studio',running:f.running[index],now:f.running[index]?f.work:null,
    next:f.running[index]?[{...f.work,movement:undefined,title:'The next work'}]:[],steering:{source:'session',mood:f.mood,constraints:{energy_bias:-1},added:{}}};
  else if(url==='/dj/moods') data={moods:[{name:'focus',constraints:{energy_bias:-1}},{name:'dinner',constraints:{energy_bias:0}}]};
  else if(init.method==='GET'&&url==='/schedules') data=f.routines;
  else if(url==='/scenes') data=[f.scene,...f.savedScenes];
  else if(init.method==='GET'&&url.startsWith('/scenes/')) data=[f.scene,...f.savedScenes].find(scene=>scene.name===decodeURIComponent(url.slice(8)));
  else if(init.method==='PUT'&&/^\/scenes\/[^/]+$/.test(url)){
    const body=JSON.parse(init.body),name=decodeURIComponent(url.slice(8));
    f.writes.push({url,body,method:'PUT'});
    data={name,saved:1700000001,groups:f.zones.map((zone,index)=>({
      coordinator:zone.coordinator_room+'@'+zone.household,
      members:zone.members.filter(room=>room!==zone.coordinator_room).map(room=>room+'@'+zone.household),
      playing:f.states[index]==='playing',
      source:{kind:body.backup_queues?'queue_backup':'queue',queue:{coordinator:'fixture-player-'+zone.household,household:zone.household,
        update_id:9,track:2,position_secs:20,play_mode:f.modes[index],...(body.backup_queues?{total:3}:{})}}
    })),volumes:{...f.levels},mutes:{...f.muted}};
    if(name===f.scene.name)f.scene=data;
    else{const index=f.savedScenes.findIndex(scene=>scene.name===name);if(index<0)f.savedScenes.push(data);else f.savedScenes[index]=data;}
    if(f.lost)throw new Error('Fixture lost reply');
  }
  else if(url.startsWith('/library/search')) data=[f.hit];
  else if(url.startsWith('/history')) data=[];
  else if(init.method==='POST'){
    const body=JSON.parse(init.body); f.writes.push({url,body});
    const target=f.zones.findIndex(z=>z.members.some(member=>`${member}@${z.household}`===body.zone));
    data={done:'Fixture command accepted',changed:true};
    if(url==='/schedules'){
      const when=body.when.startsWith('in ')?'once 2030-06-20T19:00:00+09:00':/^\d{4}-/.test(body.when)?'once '+body.when:body.when;
      data={id:f.nextRoutine++,when,action:body.action,zone:body.zone??null,scene:body.scene??null,mood:body.mood??null,
        ...(body.direction!==undefined?{direction:body.direction}:{}),volume:null,fade_secs:null,creator:'Fixture owner',enabled:true,
        summary:'Fixture stored routine',next:when.startsWith('once ')?when.slice(5):'2030-06-18T07:30:00+09:00',last_fired:null};
      f.routines.push(data);
    }
    if(['/schedules/pause','/schedules/resume','/schedules/remove'].includes(url)){
      const index=f.routines.findIndex(row=>row.id===body.id);data={...f.routines[index]};
      if(url.endsWith('/remove'))f.routines.splice(index,1);
      else{data.enabled=url.endsWith('/resume');data.next=data.enabled?'2030-06-18T07:30:00+09:00':null;f.routines[index]=data;}
    }
    if(url==='/announce'){
      const households=f.zones.map(zone=>({zone,rooms:zone.members.filter(room=>body.rooms.includes(room+'@'+zone.household))}))
        .filter(group=>group.rooms.length).map(({rooms})=>({lead:rooms[0],rooms,
          levels:rooms.map(room=>({room,volume:Math.min(body.volume,30)})),outcome:'finished',not_restored:[]}));
      data={done:'Fixture announcement played; playback restored',clean:!f.announcementPartial,households};
      if(f.announcementPartial){households[0].outcome='deadline';households[0].not_restored=['Fixture room: grouping unavailable'];}
    }
    if(url==='/pause')f.states[target]='paused';
    if(url==='/resume')f.states[target]='playing';
    if(url==='/volume'&&body.volume!==undefined){f.levels[body.zone]=body.volume;data={done:'Room volume changed',changed:true,volume:body.volume};}
    if(url==='/mute'){f.muted[body.zone]=body.mute;data={done:body.mute?'Room muted':'Room unmuted',changed:true};}
    if(url==='/sleep'){
      const remaining=body.cancel?null:Number(body.duration.slice(0,-1))*60+(body.extend?f.timers[target].remaining_secs:0);
      data={zone:f.zones[target].coordinator_room,ends:remaining===null?null:new Date(Date.now()+remaining*1000).toISOString(),
        remaining_secs:remaining,fades:true,fading:false,done:body.cancel?'Sleep timer cancelled':body.extend?'Sleep timer extended':'Sleep timer set',changed:true};
      f.timers[target]=body.cancel?null:data;
    }
    if(url==='/play-mode'){
      if(body.expected_mode!==f.modes[target])throw new Error('Fixture playback order changed');
      f.modes[target]=body.mode;
      data={done:'Playback order changed',changed:true,recovered:false,state:{zone:f.zones[target].coordinator_room,mode:body.mode,queue_active:true,dj_active:f.running[target]}};
    }
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
    for name in ['dashboard.css','dashboard-queue.css','dashboard-announcements.css']:
        page.add_style_tag(content=(ROOT/name).read_text())
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
        page.locator('#open-sleep').click()
        page.wait_for_function("!document.getElementById('sleep-set').disabled")
        assert 'S1' in page.locator('#sleep-target').inner_text()
        page.locator('#sleep-preset-30').click();page.locator('#sleep-set').click()
        before=page.evaluate('fixture.writes.length')
        assert '30 minutes' in page.locator('#sleep-preview-title').inner_text()
        page.locator('#sleep-confirm').click()
        page.wait_for_function("document.getElementById('sleep-message').textContent==='Sleep timer set'")
        assert page.evaluate('fixture.writes.length')==before+1
        assert page.evaluate('fixture.writes.at(-1)')=={'url':'/sleep','body':{'zone':'Studio@S1','duration':'30m'}}
        assert '30:00' in page.locator('#sleep-countdown').inner_text()
        page.locator('#close-sleep').click()
        page.set_viewport_size({'width':390,'height':900})
        page.locator('#open-sleep').click();page.wait_for_function("!document.getElementById('sleep-extend').disabled")
        page.locator('#sleep-preset-15').click();page.locator('#sleep-extend').click();page.locator('#sleep-confirm').click()
        page.wait_for_function("document.getElementById('sleep-message').textContent==='Sleep timer extended'")
        assert page.evaluate('fixture.writes.at(-1)')=={'url':'/sleep','body':{'zone':'Studio@S1','duration':'15m','extend':True}}
        page.locator('#sleep-cancel').click();page.locator('#sleep-confirm').click()
        page.wait_for_function("document.getElementById('sleep-current').textContent==='No active sleep timer.'")
        assert page.evaluate('fixture.writes.at(-1)')=={'url':'/sleep','body':{'zone':'Studio@S1','cancel':True}}
        assert not page.evaluate('document.documentElement.scrollWidth>innerWidth')
        page.locator('#close-sleep').click()
        checks.append('reviewed sleep timer set, extend and cancel on desktop and mobile')
        page.locator('.map-select').nth(0).click()
        page.wait_for_function("document.getElementById('dock-target').textContent.includes('S2')")
        page.locator('#open-room-volume').click()
        page.wait_for_function("document.querySelectorAll('.volume-room').length===2")
        kitchen=page.locator('.volume-room').nth(1)
        assert 'Kitchen' in kitchen.inner_text()
        slider=kitchen.locator('input[type=range]')
        slider.focus();slider.press('ArrowRight')
        assert '16' in kitchen.locator('output').inner_text()
        before=page.evaluate('fixture.writes.length')
        kitchen.get_by_role('button',name='Review level',exact=True).click()
        assert 'Kitchen: 15 → 16' in page.locator('#room-volume-preview-title').inner_text()
        assert page.evaluate('fixture.writes.length')==before
        page.locator('#room-volume-confirm').click()
        page.wait_for_function("document.getElementById('room-volume-message').textContent==='Room volume changed'")
        assert page.evaluate('fixture.writes.at(-1)')=={'url':'/volume','body':{'zone':'Kitchen@S2','volume':16}}
        assert page.evaluate("fixture.levels['Studio@S2']")==20
        kitchen.get_by_role('button',name='Mute Kitchen@S2',exact=True).click();page.locator('#room-volume-confirm').click()
        page.wait_for_function("document.getElementById('room-volume-message').textContent==='Room muted'")
        assert page.evaluate('fixture.writes.at(-1)')=={'url':'/mute','body':{'zone':'Kitchen@S2','mute':True}}
        assert kitchen.get_by_role('button',name='Unmute Kitchen@S2',exact=True).get_attribute('aria-pressed')=='true'
        assert not page.evaluate('document.documentElement.scrollWidth>innerWidth')
        page.locator('#close-room-volume').click()
        checks.append('physical member volume and mute through reviewed keyboard controls on mobile')
        page.set_viewport_size({'width':1536,'height':1120})
        page.locator('#open-capture-scene').click()
        page.wait_for_function("!document.getElementById('capture-scene-read').disabled")
        page.locator('#capture-scene-name').fill('A later listen')
        page.locator('#capture-scene-backup').check()
        page.locator('#capture-scene-review').click()
        assert 'Create scene “A later listen”' in page.locator('#capture-scene-preview-title').inner_text()
        assert '1,000 entries and 4 MiB' in page.locator('#capture-scene-preview-detail').inner_text()
        before=page.evaluate('fixture.writes.length')
        page.locator('#capture-scene-confirm').click()
        page.wait_for_function("!document.getElementById('capture-scene-recall').disabled")
        assert page.evaluate('fixture.writes.length')==before+1
        assert page.evaluate('fixture.writes.at(-1)')=={'url':'/scenes/A%20later%20listen','body':{'backup_queues':True},'method':'PUT'}
        page.locator('#capture-scene-recall').click()
        page.wait_for_function("document.getElementById('review-dialog').open")
        assert page.locator('#review-dialog').is_visible()
        assert 'A later listen' in page.locator('#review-title').inner_text()
        assert '3 entries' in page.locator('#review-details').inner_text()
        assert page.evaluate('fixture.writes.length')==before+1
        page.locator('#close-review').click()
        page.set_viewport_size({'width':390,'height':900})
        page.locator('#open-capture-scene').click()
        page.wait_for_function("!document.getElementById('capture-scene-read').disabled")
        page.locator('#capture-scene-name').fill('a LATER LISTEN')
        page.locator('#capture-scene-backup').uncheck()
        page.locator('#capture-scene-review').click()
        assert 'Replace scene “A later listen”' in page.locator('#capture-scene-preview-title').inner_text()
        page.locator('#capture-scene-existing summary').click()
        assert 'Saved queue backup, 3 entries' in page.locator('#capture-scene-existing-detail').inner_text()
        page.locator('#capture-scene-confirm').click()
        page.wait_for_function("!document.getElementById('capture-scene-recall').disabled")
        assert page.evaluate('fixture.writes.at(-1)')=={'url':'/scenes/A%20later%20listen','body':{},'method':'PUT'}
        assert page.evaluate('fixture.savedScenes.length')==1
        assert not page.evaluate('document.documentElement.scrollWidth>innerWidth')
        page.locator('#close-capture-scene').click()
        checks.append('whole-house scene capture, queue backup, reviewed recall and explicit replacement on desktop and mobile')
        before=page.evaluate('fixture.writes.length')
        page.locator('#open-routines').click()
        page.wait_for_function("!document.getElementById('routines-create-scene').disabled")
        page.locator('#routines-create-scene').click()
        page.locator('#routine-scene').select_option('Dinner')
        page.locator('#routine-time').fill('08:15')
        page.locator('[data-routine-focus="review-create"]').click()
        assert 'daemon host' in page.locator('.routine-review').inner_text()
        assert page.evaluate('fixture.writes.length')==before
        page.locator('[data-routine-focus="confirm"]').click()
        page.wait_for_selector('.routine-result')
        assert page.evaluate('fixture.writes.at(-1).body')=={'when':'weekdays 08:15','action':'apply_scene','scene':'Dinner'}
        assert '2030-06-18T07:30:00+09:00' in page.locator('.routine-result').inner_text()
        page.locator('#routines-read').click()
        page.wait_for_function("!document.getElementById('routines-create-scene').disabled")
        page.locator('[data-routine-focus="toggle-100"]').click()
        page.locator('[data-routine-focus="confirm"]').click()
        page.wait_for_function("document.getElementById('routines-message').textContent.includes('paused')")
        assert page.evaluate('fixture.writes.at(-1)')=={'url':'/schedules/pause','body':{'id':100}}
        assert not page.evaluate('document.documentElement.scrollWidth>innerWidth')
        page.locator('#close-routines').click()
        checks.append('persistent scene routines, daemon next-run times and exact-ID pause on mobile')
        before=page.evaluate('fixture.writes.length')
        page.locator('#open-announcements').click()
        page.wait_for_selector('#announcement-rooms input')
        assert page.locator('#announcement-rooms input:checked').count()==0
        page.locator('#announcement-rooms input').nth(1).check()
        page.locator('#announcement-rooms input').nth(2).check()
        page.locator('#announcement-words').fill('A fixture announcement')
        page.locator('#announcement-volume').fill('25')
        page.locator('#announcement-review').click()
        assert 'Kitchen (S2)' in page.locator('#announcement-preview-rooms').inner_text()
        assert 'Studio (S1)' in page.locator('#announcement-preview-rooms').inner_text()
        assert page.evaluate('fixture.writes.length')==before
        page.locator('#announcement-confirm').click()
        page.wait_for_selector('#announcement-result:not([hidden])')
        assert page.evaluate('fixture.writes.at(-1)')=={'url':'/announce','body':{'rooms':['Kitchen@S2','Studio@S1'],'volume':25,'text':'A fixture announcement'}}
        assert 'Announcement restored' in page.locator('#announcement-result-title').inner_text()
        page.wait_for_function("!document.getElementById('announcement-read').disabled")
        page.locator('#announcement-read').click()
        page.wait_for_function("!document.getElementById('announcement-review').disabled")
        page.locator('#announcement-source').select_option('chime')
        page.locator('#announcement-chime').select_option('rise')
        page.evaluate('fixture.announcementPartial=true')
        page.locator('#announcement-review').click()
        page.locator('#announcement-confirm').click()
        page.wait_for_function("document.getElementById('announcement-result-title').textContent==='Check announcement results'")
        assert 'Not restored: Fixture room: grouping unavailable' in page.locator('#announcement-result-detail').inner_text()
        assert page.evaluate('fixture.writes.at(-1).body.chime')=='rise'
        assert not page.evaluate('document.documentElement.scrollWidth>innerWidth')
        page.locator('#close-announcements').click()
        page.evaluate('fixture.announcementPartial=false')
        checks.append('reviewed physical-room announcements, native speech/chime playback and partial restoration on mobile')
        page.locator('#tab-queue').click()
        page.wait_for_function("document.getElementById('queue-status').textContent.includes('120 tracks')")
        page.locator('.queue-finder summary').click()
        before=page.evaluate('fixture.writes.length')
        page.locator('[data-queue-find="query"]').fill('fugue')
        page.locator('[data-queue-find="submit"]').click()
        page.wait_for_function("document.querySelector('.queue-find-status').textContent.includes('2 matches')")
        assert page.locator('[data-queue-match]').count()==2
        page.locator('[data-queue-match="98"]').click()
        page.wait_for_function("document.querySelector('#queue-items li[data-found]')?.textContent.includes('98')")
        assert '81–120 of 120' in page.locator('#queue-page').inner_text()
        assert page.evaluate('fixture.writes.length')==before
        page.locator('[data-queue-focus="play-98"]').click()
        page.wait_for_selector('#review-dialog[open]')
        assert '#98' in page.locator('#review-title').inner_text()
        assert page.evaluate('fixture.writes.length')==before
        page.locator('#close-review').click()
        page.locator('[data-queue-find="position"]').fill('3')
        page.locator('[data-queue-find="jump"]').click()
        page.wait_for_function("document.querySelector('#queue-items li[data-found]')?.textContent.includes('3')")
        assert '1–40 of 120' in page.locator('#queue-page').inner_text()
        assert not page.evaluate('document.documentElement.scrollWidth>innerWidth')
        assert page.evaluate('fixture.writes.length')==before
        checks.append('native queue search, duplicate occurrences, reviewed playback and direct position navigation on mobile')
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
        preview.locator('#open-capture-scene').click()
        preview.locator('#capture-scene-name').fill('Preview only')
        assert preview.locator('#capture-scene-review').is_disabled()
        preview.locator('#close-capture-scene').click()
        assert preview.evaluate('fixture.calls.length')==0
        preview.locator('#tab-queue').click()
        preview.wait_for_function("document.getElementById('queue-status').textContent.includes('5 tracks')")
        preview.locator('.queue-finder summary').click()
        preview.locator('[data-queue-find="query"]').fill('Canopy')
        preview.locator('[data-queue-find="submit"]').click()
        preview.wait_for_function("document.querySelector('.queue-find-status').textContent.includes('1 matches')")
        assert preview.locator('[data-queue-match="5"]').count()==1
        assert preview.evaluate('fixture.calls.length')==0
        preview.locator('#tab-picks').click()
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
