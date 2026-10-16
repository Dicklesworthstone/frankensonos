#!/usr/bin/env python3
"""Production interaction acceptance using explicit local API fixtures, not Sonos.

Requires Python Playwright and Chromium. No external services or speaker writes.
"""
from pathlib import Path
import argparse
import importlib.util
from playwright.sync_api import sync_playwright

spec = importlib.util.spec_from_file_location('dashboard_browser', Path(__file__).with_name('test-dashboard-browser.py'))
base = importlib.util.module_from_spec(spec)
spec.loader.exec_module(base)
base.FIXTURE = base.FIXTURE.replace("else if(url.endsWith('/dj'))", """else if(url.includes('/queue?')) {
    const offset=Number(new URLSearchParams(url.split('?')[1]).get('offset'));
    data={zone:'Studio',total:f.queueTotal??85,playing:(f.queueTotal??85)?2:null,revision:'fixture-queue-'+(f.queueRevision??1),
      items:Array.from({length:Math.min(40,Math.max(0,(f.queueTotal??85)-offset))},(_,i)=>({position:offset+i+1,title:`Queue track ${offset+i+1}`,creator:'Fixture artist',uri:'duplicate:track'}))};
    if(f.queueHold){f.queueHeld=data;await new Promise(resolve=>f.queueRelease=resolve);}
    if(f.queueMalformed)data={...data,items:[]};
  }
  else if(url.endsWith('/dj'))""")

def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--browser-executable', default='/usr/bin/chromium')
    parser.add_argument('--screenshots-dir', type=Path)
    args=parser.parse_args()
    with sync_playwright() as p:
        browser=p.chromium.launch(headless=True, executable_path=args.browser_executable)
        page=browser.new_page(viewport={'width':1440,'height':1000},reduced_motion='reduce')
        errors=[]; page.on('pageerror',lambda e:errors.append(str(e)))
        base.load(page)
        page.wait_for_function("!document.getElementById('play').disabled")
        assert page.evaluate("fixture.calls.filter(c=>c.url.includes('/queue?')).length")==0
        page.locator('#tab-queue').click()
        page.wait_for_function("document.querySelectorAll('#queue-items li').length===40")
        assert page.evaluate("fixture.calls.filter(c=>c.url.includes('/queue?')).at(-1).url")=='/zones/Studio%40S2/queue?offset=0&limit=40'
        assert page.locator('#queue-items [aria-current=true]').inner_text().startswith('▶')
        page.locator('#queue-next').click()
        page.wait_for_function("document.getElementById('queue-page').textContent==='41–80 of 85'")
        assert 'Queue track 41' in page.locator('#queue-items li').first.inner_text()
        page.locator('#queue-prev').click()
        page.wait_for_function("document.getElementById('queue-page').textContent==='1–40 of 85'")
        print('PASS: native queue is lazy, qualified, position-preserving and paged')
        page.locator('#tab-queue').focus();page.keyboard.press('ArrowLeft')
        assert page.locator('#tab-picks').get_attribute('aria-selected')=='true'
        assert page.locator('#tab-queue').get_attribute('tabindex')=='-1'
        count=page.evaluate("fixture.calls.filter(c=>c.url.includes('/queue?')).length")
        page.evaluate("fixture.feeds.at(-1).emit('zone.state')");page.wait_for_timeout(600)
        assert page.evaluate("fixture.calls.filter(c=>c.url.includes('/queue?')).length")==count
        page.keyboard.press('ArrowRight');page.wait_for_function("!document.getElementById('queue-next').disabled")
        print('PASS: accessible queue tabs and no hidden-view queue requests')
        page.evaluate("fixture.queueMalformed=true;fixture.feeds.at(-1).emit('zone.state')")
        page.wait_for_function("document.getElementById('queue-status').textContent.includes('incomplete')")
        assert page.locator('#queue-next').is_disabled()
        assert page.locator('#queue-items li').count()==40
        page.evaluate('fixture.queueMalformed=false;fixture.queueTotal=0')
        page.locator('#queue-refresh').click()
        page.wait_for_function("document.getElementById('queue-items').textContent.includes('empty')")
        assert page.locator('#queue-next').is_disabled()
        print('PASS: malformed reads retain stale state; real empty queues stay empty')
        page.locator('#main').focus();page.keyboard.press('r')
        assert page.locator('#rooms-dialog').is_visible()
        assert page.evaluate('document.activeElement.id')=='room-filter'
        page.locator('#room-filter').fill('S1');assert page.locator('.room-choice').count()==1
        page.keyboard.press('ArrowDown');page.keyboard.press('Enter')
        assert not page.locator('#rooms-dialog').is_visible()
        page.wait_for_function("document.getElementById('dock-target').textContent.includes('S1')")
        assert page.evaluate('fixture.writes.length')==0
        print('PASS: searchable household-aware room picker and keyboard selection do not move music')
        page.locator('#scope').click();page.locator('#room-filter').fill('nothing matching')
        assert 'No matching rooms' in page.locator('#room-choices').inner_text()
        page.keyboard.press('Escape');assert page.evaluate('document.activeElement.id')=='scope'
        page.locator('#main').focus();page.keyboard.press('Control+k')
        page.locator('#search-input').fill('fixture')
        page.wait_for_selector('.search-result')
        page.keyboard.press('ArrowDown');assert 'search-result' in page.evaluate('document.activeElement.className')
        page.keyboard.press('Enter');assert page.locator('#review-dialog').is_visible()
        assert page.evaluate('fixture.writes.length')==0
        page.keyboard.press('Space');assert page.evaluate('fixture.writes.length')==0
        page.keyboard.press('Escape')
        print('PASS: modal focus restoration, search roving focus and safe review boundaries')
        page.locator('#main').focus();page.keyboard.press('Space')
        page.wait_for_function('fixture.writes.length===1')
        assert page.evaluate('fixture.writes[0]')=={'url':'/resume','body':{'zone':'Studio@S1'}}
        page.wait_for_function("!document.getElementById('play').disabled")
        page.locator('#main').focus();page.keyboard.press('?');assert page.locator('#shortcuts-dialog').is_visible()
        page.keyboard.press('Space');assert page.evaluate('fixture.writes.length')==1
        page.keyboard.press('Escape')
        print('PASS: Space uses native guarded transport; shortcuts are inactive in dialogs')
        page.evaluate('fixture.queueTotal=85;fixture.queueHold=true')
        page.locator('#queue-refresh').click();page.wait_for_function('!!fixture.queueRelease')
        page.evaluate("window.dispatchEvent(new Event('pagehide'));fixture.queueRelease()")
        page.wait_for_timeout(100)
        assert not page.locator('#queue-status').inner_text().startswith('85 tracks')
        page.evaluate("fixture.queueHold=false;window.dispatchEvent(new Event('pageshow'))")
        page.wait_for_function("document.getElementById('queue-status').textContent.startsWith('85 tracks')")
        print('PASS: suspended queue reads cannot publish into a new page lifetime')
        page.set_viewport_size({'width':390,'height':850})
        page.locator('#toggle-volume').click();assert page.locator('#volume-up').is_visible()
        page.locator('#volume-up').click(timeout=2500);page.wait_for_function('fixture.writes.length===2')
        assert page.evaluate('fixture.writes[1]')=={'url':'/volume','body':{'zone':'Studio@S1','delta':5,'group':True}}
        page.keyboard.press('Escape');assert page.locator('#toggle-volume').get_attribute('aria-expanded')=='false'
        assert page.evaluate('document.activeElement.id')=='toggle-volume'
        assert not page.evaluate('document.documentElement.scrollWidth>innerWidth')
        print('PASS: mobile group volume is reachable, native, dismissible and focus-safe')
        page.close()
        preview=browser.new_page(viewport={'width':1440,'height':1080},reduced_motion='reduce')
        preview.on('pageerror',lambda e:errors.append(str(e)));base.load(preview,True)
        preview.locator('#tab-queue').click();preview.wait_for_function("document.querySelectorAll('#queue-items li').length===5")
        assert preview.evaluate('fixture.calls.length')==0
        assert preview.evaluate('fixture.writes.length')==0
        if args.screenshots_dir:
            args.screenshots_dir.mkdir(parents=True,exist_ok=True)
            preview.locator('#scope').click();preview.screenshot(path=str(args.screenshots_dir/'room-picker.png'))
            preview.keyboard.press('Escape');preview.locator('#tab-picks').click()
            preview.screenshot(path=str(args.screenshots_dir/'desktop.png'),full_page=True)
            preview.screenshot(path=str(args.screenshots_dir/'desktop-viewport.png'))
            preview.locator('#theme').click();preview.screenshot(path=str(args.screenshots_dir/'light.png'));preview.locator('#theme').click()
            preview.locator('#immerse').click();preview.screenshot(path=str(args.screenshots_dir/'immersive.png'));preview.keyboard.press('Escape')
            preview.set_viewport_size({'width':390,'height':850});preview.screenshot(path=str(args.screenshots_dir/'mobile.png'))
        assert not errors,errors
        print('PASS: read-only preview queue, no unexpected requests or page exceptions')
        browser.close()

if __name__=='__main__': main()
