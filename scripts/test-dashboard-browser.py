#!/usr/bin/env python3
"""Chromium component acceptance of the production dashboard assets.

Requires Python Playwright and Chromium. Uses explicit synthetic API fixtures,
not a Rust daemon or physical speakers. No remote navigation is required.
"""
from __future__ import annotations
import argparse
from pathlib import Path
import re
from playwright.sync_api import sync_playwright

ROOT = Path(__file__).resolve().parents[1] / "crates/fsonos-api/src/remote"
SCRIPTS = ["dashboard-core.js", "dashboard-visualizer.js", "dashboard-audio.js", "dashboard.js"]
FIXTURE = r"""
window.fixture = { writes: [], calls: [], lost: false, offline: false, feeds: [],
  zones: [{household:'S2',coordinator_room:'Fixture Studio',members:['Fixture Studio','Fixture Kitchen'],transport_state:'playing'},
          {household:'S1',coordinator_room:'Fixture Studio',members:['Fixture Studio'],transport_state:'paused'}],
  states: ['playing','paused'], title: 'Fixture song' };
window.fetch = async (url, init) => {
  const f=window.fixture; f.calls.push(String(url));
  if(f.offline) throw new Error('Fixture offline');
  let data;
  if(url === '/zones') data=f.zones;
  else if(url.startsWith('/history')) data=[];
  else if(url.endsWith('/dj')) data={running:false};
  else if(init.method === 'POST') {
    const body=JSON.parse(init.body); f.writes.push({url,body});
    const i=f.zones.findIndex(z=>`${z.coordinator_room}@${z.household}`===body.zone);
    if(url==='/pause') f.states[i]='paused';
    if(url==='/resume') f.states[i]='playing';
    if(f.lost) throw new Error('Fixture lost reply');
    data={done:'Fixture command accepted',changed:true};
  } else {
    const i=f.zones.findIndex(z=>url.includes(encodeURIComponent(`${z.coordinator_room}@${z.household}`)));
    data={zone:f.zones[i],transport_state:f.states[i],volume:20,group_volume:22,
      track:{title:f.title,creator:'Synthetic artist',uri:'fixture:track',position_secs:23,duration_secs:123,queue_position:1}};
  }
  return new Response(JSON.stringify(data),{headers:{'Content-Type':'application/json'}});
};
window.EventSource=class {
  constructor(){this.listeners={};this.closed=false;fixture.feeds.push(this);setTimeout(()=>this.onopen?.(),0);}
  addEventListener(name,fn){this.listeners[name]=fn;}
  close(){this.closed=true;}
  emit(name){this.listeners[name]?.({data:'{}'});}
};
"""

def load(page, preview=False):
    html = (ROOT / "dashboard.html").read_text()
    if preview:
        html = html.replace("<body>", '<body data-preview="true">')
    html = re.sub(r'<link[^>]+>|<script[^>]+></script>', '', html)
    page.set_content(html)
    page.add_style_tag(content=(ROOT / "dashboard.css").read_text())
    page.add_script_tag(content=FIXTURE)
    for name in SCRIPTS:
        page.add_script_tag(content=(ROOT / name).read_text())
    page.wait_for_selector(".room-card")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--browser-executable")
    parser.add_argument("--screenshots-dir", type=Path)
    args = parser.parse_args()
    with sync_playwright() as p:
        browser = p.chromium.launch(headless=True, executable_path=args.browser_executable)
        page = browser.new_page(viewport={"width":1440,"height":1130})
        errors = []
        page.on("pageerror", lambda e: errors.append(str(e)))
        load(page)
        page.wait_for_function("!document.getElementById('play').disabled")
        assert page.locator('.room-card').count() == 2
        page.locator('#play').click()
        page.wait_for_function("document.getElementById('stage-status').textContent==='PAUSED'")
        assert page.evaluate("fixture.writes") == [{"url":"/pause","body":{"zone":"Fixture Studio@S2"}}]
        page.locator('.room-card').nth(1).click()
        page.locator('#play').click()
        page.wait_for_function("fixture.writes.length===2")
        assert page.evaluate("fixture.writes[1].body.zone") == 'Fixture Studio@S1'
        page.wait_for_function("!document.getElementById('play').disabled")
        page.evaluate("fixture.lost=true")
        page.locator('#next').click()
        page.wait_for_function("document.getElementById('outcome').textContent.includes('Outcome uncertain')")
        assert page.locator('#next').is_disabled()
        page.evaluate("fixture.feeds.at(-1).emit('zone.state')")
        page.wait_for_timeout(700)
        assert page.locator('#next').is_disabled()
        assert page.evaluate("fixture.writes.length") == 3
        page.evaluate("fixture.lost=false")
        page.locator('#refresh').click()
        page.wait_for_function("!document.getElementById('next').disabled")
        page.evaluate("fixture.title='<img src=x onerror=alert(1)>';fixture.feeds.at(-1).emit('zone.state')")
        page.wait_for_function("document.getElementById('track-title').textContent.startsWith('<img')")
        assert page.locator('#track-title img').count() == 0
        page.evaluate("fixture.offline=true;fixture.feeds.at(-1).emit('zone.state')")
        page.wait_for_function("!document.getElementById('error').hidden")
        assert page.locator('#play').is_disabled()
        assert page.locator('.room-state.stale').count() == 2
        page.evaluate("fixture.offline=false")
        page.locator('#refresh').click()
        page.wait_for_function("document.getElementById('error').hidden")
        page.evaluate("window.dispatchEvent(new Event('pagehide'))")
        assert page.evaluate("fixture.feeds.at(-1).closed")
        count=page.evaluate('fixture.calls.length')
        page.wait_for_timeout(400)
        assert page.evaluate('fixture.calls.length') == count
        page.evaluate("window.dispatchEvent(new Event('pageshow'))")
        page.wait_for_function("fixture.feeds.length===2")
        page.close()

        preview = browser.new_page(viewport={"width":1440,"height":1130}, reduced_motion="reduce")
        preview.on("pageerror", lambda e: errors.append(str(e)))
        load(preview, True)
        assert preview.locator('#demo-banner').is_visible()
        assert preview.locator('#play').is_disabled()
        assert preview.evaluate('fixture.calls.length') == 0
        assert preview.locator('#motion').inner_text() == 'Resume motion'
        for mode in ['orbit','contour','aurora']:
            preview.locator(f'[data-mode="{mode}"]').click()
            assert preview.locator(f'[data-mode="{mode}"]').get_attribute('aria-pressed') == 'true'
        preview.locator('#immerse').click()
        assert 'immersive' in preview.locator('#stage').get_attribute('class')
        assert preview.evaluate("document.getElementById('room-section').inert")
        preview.keyboard.press('Shift+Tab')
        assert preview.evaluate("document.getElementById('stage').contains(document.activeElement)")
        preview.keyboard.press('Escape')
        assert 'immersive' not in preview.locator('#stage').get_attribute('class')
        assert not preview.evaluate("document.getElementById('room-section').inert")
        preview.locator('#microphone').click()
        preview.wait_for_function("document.getElementById('audio-status').textContent.includes('localhost or HTTPS')")
        if args.screenshots_dir:
            args.screenshots_dir.mkdir(parents=True, exist_ok=True)
        for width in [1440, 768, 390, 320]:
            preview.set_viewport_size({"width":width,"height":900})
            preview.wait_for_timeout(100)
            assert not preview.evaluate('document.documentElement.scrollWidth > innerWidth'), width
            if args.screenshots_dir and width in (1440,390):
                preview.screenshot(path=str(args.screenshots_dir / f"dashboard-{width}.png"), full_page=True)
        assert not errors, errors
        print('PASS: production controls, household targeting, uncertainty, live invalidation, safe metadata, offline recovery, page lifecycle, preview isolation, three modes, reduced motion, immersion focus/Escape, microphone fallback, and four viewport widths.')
        browser.close()

if __name__ == '__main__':
    main()
