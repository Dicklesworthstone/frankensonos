#!/usr/bin/env python3
"""Exercise the standing-taste dialog with production assets and synthetic HTTP.

Requires Python Playwright and Chromium. No real daemon, Spotify or speakers.
"""
from __future__ import annotations
import argparse
import importlib.util
from pathlib import Path
import re
from playwright.sync_api import sync_playwright


def load_module(name, filename):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(filename))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


BASE = load_module('dashboard_fixture', 'test-dashboard-browser.py')
BUNDLE = load_module('dashboard_bundle', 'dashboard-preview.py')
ASSETS = BUNDLE.ASSETS
FIXTURE = r"""
fixture.preferences = {energy:null,explicit:false,
  favor:{genres:[],artists:[],eras:[],moods:[]},avoid:{genres:[],artists:[],eras:[],moods:[]},
  pin:{artists:[],albums:[],tracks:[]},ban:{artists:[],albums:[],tracks:[]}};
fixture.preferenceReads = 0;
fixture.hide = hidden => {
  Object.defineProperty(document,'hidden',{value:hidden,configurable:true});
  document.dispatchEvent(new Event('visibilitychange'));
};
const baseFetch = window.fetch;
window.fetch = async (url, init) => {
  const f=fixture;
  if(url==='/events') {
    const body=new ReadableStream({start(controller){f.feed=controller;controller.enqueue(new TextEncoder().encode(': heartbeat\n\n'));}});
    return new Response(body,{headers:{'Content-Type':'text/event-stream'}});
  }
  if(url!=='/dj/preferences')return baseFetch(url,init);
  f.calls.push({url,method:init.method,at:performance.now()});
  if(init.method==='GET') {
    f.preferenceReads++;
    if(f.badPreferences)return new Response('{}');
    return new Response(JSON.stringify(f.preferences));
  }
  if(init.method!=='POST')throw new Error('Unexpected preference method');
  const body=JSON.parse(init.body);f.writes.push({url,body});
  const {key,value,unset}=body;
  if(key==='energy')f.preferences.energy=unset?null:Number(value);
  else if(key==='explicit')f.preferences.explicit=!unset&&value==='true';
  else {
    const [group,field]=key.split('.');
    if(unset)f.preferences[group][field]=f.preferences[group][field].filter(item=>item!==value);
    else if(!f.preferences[group][field].includes(value))f.preferences[group][field].push(value);
  }
  if(f.holdPreferenceSave)await new Promise(resolve=>{f.releasePreferenceSave=resolve;});
  if(f.losePreferenceReply)throw new Error('Lost preference acknowledgement');
  return new Response(JSON.stringify({done:'Fixture preference saved.',changed:true,preferences:f.preferences}));
};
"""


def setup(browser, width=1280, speakers=True, preview=False):
    page = browser.new_page(viewport={'width': width, 'height': 920}, reduced_motion='reduce')
    page.set_default_timeout(5000)
    errors = []
    page.on('pageerror', lambda error: errors.append(str(error)))
    html = (ASSETS / 'dashboard.html').read_text()
    if preview:
        html = html.replace('<body>', '<body data-preview="true">')
    page.set_content(re.sub(r'<link[^>]+>|<script[^>]+></script>', '', html))
    for filename in BUNDLE.STYLES:
        page.add_style_tag(content=(ASSETS / filename).read_text())
    page.add_script_tag(content=BASE.FIXTURE + FIXTURE)
    if not speakers:
        page.evaluate('fixture.zones=[]')
    for filename in BUNDLE.SCRIPTS:
        page.add_script_tag(content=(ASSETS / filename).read_text())
    page.wait_for_function("document.getElementById('connection').textContent !== 'Connecting to your daemon'")
    assert not errors, errors
    return page, errors


def open_taste(page):
    page.locator('#open-preferences').click()
    page.wait_for_selector('#preferences-dialog[open]')
    page.wait_for_function("!document.getElementById('preferences-add').disabled")


def review_artist(page, name='Earth, Wind & Fire'):
    page.locator('#preferences-key').select_option('favor.artists')
    page.locator('#preferences-value').fill(name)
    page.locator('#preferences-add').click()
    page.wait_for_selector('#preferences-preview:not([hidden])')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--browser-executable', default='/usr/bin/chromium')
    parser.add_argument('--screenshots-dir', type=Path)
    args = parser.parse_args()
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch(executable_path=args.browser_executable, headless=True)
        page, errors = setup(browser, speakers=False)
        assert page.evaluate('fixture.preferenceReads') == 0
        open_taste(page)
        assert page.locator('#preferences-key option').count() == 14
        assert 'every group' in page.locator('.preferences-scope').inner_text()
        review_artist(page)
        assert page.evaluate('fixture.writes.length') == 0
        page.locator('#preferences-confirm').click()
        page.wait_for_selector('#preferences-result:not([hidden])')
        assert page.evaluate('fixture.writes') == [{'url': '/dj/preferences', 'body': {
            'key': 'favor.artists', 'value': 'Earth, Wind & Fire', 'unset': False}}]
        assert page.evaluate('fixture.preferenceReads') == 2
        page.locator('[data-preference-item="favor.artists:0"]').click()
        page.locator('#preferences-confirm').click()
        page.wait_for_function('fixture.writes.length===2')
        assert page.evaluate('fixture.preferences.favor.artists') == []
        page.locator('#close-preferences').click()
        assert page.evaluate('document.activeElement.id') == 'open-preferences'
        assert not errors, errors
        page.close()
        print('PASS: no-speaker read, exact literal addition/removal and focus restoration')

        page, errors = setup(browser, width=390)
        open_taste(page)
        page.locator('#preferences-energy').fill('0')
        page.locator('#preferences-energy-save').click()
        page.locator('#preferences-confirm').click()
        page.wait_for_function('fixture.preferences.energy===0')
        page.locator('#preferences-energy-reset').click()
        page.locator('#preferences-confirm').click()
        page.wait_for_function('fixture.writes.length===2')
        assert page.evaluate('fixture.preferences.energy') is None
        page.locator('#preferences-explicit').select_option('true')
        page.locator('#preferences-explicit-save').click()
        page.locator('#preferences-confirm').click()
        page.wait_for_function('fixture.writes.length===3')
        assert page.evaluate('fixture.preferences.explicit') is True
        page.evaluate("fixture.preferences.ban.artists=['<img src=x onerror=alert(1)>','Long name '.repeat(24)]")
        page.locator('#preferences-read').click()
        page.wait_for_function("document.getElementById('preferences-lists').textContent.includes('<img')")
        assert page.locator('#preferences-lists img').count() == 0
        assert not page.evaluate('document.documentElement.scrollWidth>innerWidth')
        assert not page.locator('#preferences-dialog').evaluate('(dialog)=>dialog.scrollWidth>dialog.clientWidth')
        if args.screenshots_dir:
            args.screenshots_dir.mkdir(parents=True, exist_ok=True)
            page.screenshot(path=str(args.screenshots_dir / 'standing-taste-mobile.png'))
        assert not errors, errors
        page.close()
        print('PASS: energy, explicit setting, literal metadata and mobile overflow')

        page, errors = setup(browser)
        open_taste(page); review_artist(page)
        page.evaluate("fixture.preferences.favor.artists=['Changed elsewhere']")
        page.locator('#preferences-confirm').click()
        page.wait_for_function("document.getElementById('preferences-message').textContent.includes('changed since you reviewed')")
        assert page.evaluate('fixture.writes.length') == 0
        page.locator('#preferences-read').click(); review_artist(page)
        page.evaluate('fixture.losePreferenceReply=true')
        page.locator('#preferences-confirm').click()
        page.wait_for_function("document.getElementById('preferences-message').textContent.includes('Outcome uncertain')")
        page.locator('#close-preferences').click(); page.locator('#refresh').click()
        page.wait_for_function("document.getElementById('connection').textContent.includes('Live')")
        assert page.locator('#play').is_disabled()
        page.locator('#open-preferences').click()
        page.wait_for_function("document.getElementById('preferences-message').textContent.includes('earlier command')")
        assert page.locator('#preferences-add').is_disabled()
        page.evaluate('fixture.losePreferenceReply=false'); page.locator('#preferences-read').click()
        page.wait_for_function("!document.getElementById('preferences-add').disabled")
        assert page.evaluate('fixture.writes.length') == 1
        assert not errors, errors
        page.close()
        print('PASS: conflicting setting blocks dispatch; uncertain save needs its own explicit reread')

        page, errors = setup(browser)
        open_taste(page); review_artist(page)
        page.evaluate('fixture.holdPreferenceSave=true')
        page.locator('#preferences-confirm').click()
        page.wait_for_function('typeof fixture.releasePreferenceSave === "function"')
        page.evaluate('fixture.hide(true); fixture.releasePreferenceSave()')
        page.wait_for_function("document.getElementById('preferences-result').textContent.includes('saved')")
        assert page.evaluate('fixture.writes.length') == 1
        assert not page.locator('#preferences-dialog').is_visible()
        page.evaluate('fixture.hide(false)')
        page.locator('#open-preferences').click()
        page.wait_for_function("!document.getElementById('preferences-add').disabled")
        assert page.evaluate('fixture.writes.length') == 1
        assert not errors, errors
        page.close()
        print('PASS: hiding preserves the submitted save and receipt without replay')

        page, errors = setup(browser, preview=True)
        page.locator('#open-preferences').click()
        page.wait_for_function("document.getElementById('preferences-lists').textContent.includes('Adrian Vale')")
        assert page.locator('#preferences-add').is_disabled()
        assert page.evaluate('fixture.writes.length') == 0
        assert not errors, errors
        page.close(); browser.close()
        print('PASS: read-only preview exposes fictional preferences without writes')


if __name__ == '__main__':
    main()
