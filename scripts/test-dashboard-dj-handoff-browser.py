#!/usr/bin/env python3
"""Exercise rehearsal-to-direction with the production dashboard and fixture API.

Requires Python Playwright and Chromium. No daemon, Spotify or speakers.
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
FIXTURE = r"""
fixture.plans = [];
const baseFetch = window.fetch;
window.fetch = async (url, init) => {
  if(url==='/events') {
    const body=new ReadableStream({start(controller){fixture.feed=controller;
      controller.enqueue(new TextEncoder().encode(': heartbeat\n\n'));}});
    return new Response(body,{headers:{'Content-Type':'text/event-stream'}});
  }
  if(url!=='/dj/rehearse')return baseFetch(url,init);
  if(init.method!=='POST')throw new Error('Unexpected rehearsal method');
  const body=JSON.parse(init.body);fixture.plans.push({url,body});
  const from='2026-11-08T23:40:00-04:00';
  return new Response(JSON.stringify({zone:'Studio',from,seed:body.seed,
    works:[{at:from,work:{title:'A rehearsed whole work',composer:'Fixture artist',
      movements:3,minutes:24,reason:{summary:'The requested direction and the owner’s preferences.',factors:[],relaxed:[]}},
      energy:35,energy_target:40,explicit:false}],
    summary:{works:1,minutes:24,distinct_artists:1,explicit:0,hours:[
      {from,works:1,energy:35,energy_target:40,artist_entropy:0,genre_entropy:0}]}}));
};
"""


def setup(browser, width):
    page = browser.new_page(viewport={'width': width, 'height': 940}, reduced_motion='reduce')
    page.set_default_timeout(5000)
    errors = []
    page.on('pageerror', lambda error: errors.append(str(error)))
    html = (BUNDLE.ASSETS / 'dashboard.html').read_text()
    page.set_content(re.sub(r'<link[^>]+>|<script[^>]+></script>', '', html))
    for name in BUNDLE.STYLES:
        page.add_style_tag(content=(BUNDLE.ASSETS / name).read_text())
    page.add_script_tag(content=BASE.FIXTURE + FIXTURE)
    for name in BUNDLE.SCRIPTS:
        page.add_script_tag(content=(BUNDLE.ASSETS / name).read_text())
    page.wait_for_selector('.room-card')
    page.wait_for_function("!document.getElementById('open-rehearsal').disabled")
    return page, errors


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--browser-executable', default='/usr/bin/chromium')
    parser.add_argument('--screenshots-dir', type=Path)
    args = parser.parse_args()
    if args.screenshots_dir:
        args.screenshots_dir.mkdir(parents=True, exist_ok=True)
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch(executable_path=args.browser_executable, headless=True)
        for width, household, action in [(1280, 'S2', 'save'), (390, 'S1', 'start')]:
            page, errors = setup(browser, width)
            page.locator(f'.room-card[data-room="Studio@{household}"]').click()
            page.locator('#open-rehearsal').click()
            page.wait_for_function("!document.getElementById('rehearsal-moods').disabled")
            page.locator('#rehearsal-mode').select_option('hours')
            page.locator('#rehearsal-at').fill('23:40')
            page.locator('#rehearsal-seed').fill('7')
            page.locator('#rehearsal-mood').select_option('focus')
            page.locator('.rehearsal-filters summary').click()
            page.locator('#rehearsal-include_artists').fill('Earth, Wind & Fire')
            page.locator('#rehearsal-strict').check()
            page.locator('#rehearsal-generate').click()
            page.wait_for_function("!document.getElementById('rehearsal-transfer').disabled")
            assert page.evaluate('fixture.writes.length') == 0
            # Editing never passes off the old preview as the new settings.
            page.locator('#rehearsal-seed').fill('8')
            assert page.locator('#rehearsal-transfer').is_disabled()
            assert 'Generate the edited settings again' in page.locator('#rehearsal-transfer-hint').inner_text()
            page.locator('#rehearsal-generate').click()
            page.wait_for_function("!document.getElementById('rehearsal-transfer').disabled")
            page.locator('#rehearsal-transfer').click()
            page.wait_for_function("document.getElementById('direction-dialog').open && document.getElementById('direction-expiry').value==='choose'")
            assert page.locator('dialog[open]').count() == 1
            assert not page.locator('#rehearsal-dialog').is_visible()
            assert page.locator('#direction-rehearsal-note').is_visible()
            assert page.locator('#direction-expiry').evaluate('node => node === document.activeElement')
            assert page.locator('#live-direction-include_artists').input_value() == 'Earth, Wind & Fire'
            assert page.locator('#live-direction-strict').is_checked()
            assert page.locator('#direction-review').is_disabled()
            assert page.locator('#direction-start').is_disabled()
            assert page.evaluate('fixture.writes.length') == 0
            page.locator('#direction-expiry').select_option('duration')
            page.locator('#direction-minutes').fill('45')
            page.locator('#direction-start' if action == 'start' else '#direction-review').click()
            assert '45 minutes from saving' in page.locator('#direction-preview-expiry').inner_text()
            assert page.evaluate('fixture.writes.length') == 0
            if args.screenshots_dir:
                page.screenshot(path=str(args.screenshots_dir / f'dj-handoff-{width}-{action}.png'))
            page.locator('#direction-confirm').click()
            page.wait_for_function("fixture.writes.length===1 && document.getElementById('direction-dialog').getAttribute('aria-busy')==='false'")
            assert page.evaluate('fixture.writes') == [{'url': '/dj/start' if action == 'start' else '/dj/steer', 'body': {
                'zone': f'Studio@{household}', 'mood': 'focus', 'constraints': {
                    'include_artists': ['Earth, Wind & Fire'], 'strict': True}, 'for_secs': 2700}}]
            assert page.evaluate('fixture.plans.length') == 2
            assert page.evaluate('fixture.plans.at(-1).body') == {
                'zone': f'Studio@{household}', 'hours': 2, 'seed': 8, 'at': '23:40', 'mood': 'focus',
                'constraints': {'include_artists': ['Earth, Wind & Fire'], 'strict': True}}
            page.locator('#close-direction').click()
            assert page.locator('#open-rehearsal').evaluate('node => node === document.activeElement')
            assert not errors, errors
            page.close()
            print(f'PASS {width}px {household}: captured rehearsal → unsaved direction → explicit {action}, one native command')
        browser.close()


if __name__ == '__main__':
    main()
