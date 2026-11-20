#!/usr/bin/env python3
"""Speaker sound in the production dashboard, with explicit synthetic API replies.

Requires Python Playwright and Chromium; no Sonos devices or external services.
"""
from pathlib import Path
import argparse
import importlib.util
from playwright.sync_api import sync_playwright

spec = importlib.util.spec_from_file_location(
    'dashboard_browser', Path(__file__).with_name('test-dashboard-browser.py'))
base = importlib.util.module_from_spec(spec)
spec.loader.exec_module(base)
base.FIXTURE = base.FIXTURE.replace("else if(url.endsWith('/state'))", """else if(url.endsWith('/eq')) {
    f.sound ??= {};
    data = f.sound[room] ??= {room:room.slice(0,room.lastIndexOf('@')),bass:1,treble:-2,loudness:true};
    if (f.soundOffline) throw new Error('Fixture speaker unavailable');
  }
  else if(url.endsWith('/state'))""")
base.FIXTURE = base.FIXTURE.replace("data={done:'Fixture command accepted',changed:true};", """data={done:'Fixture command accepted',changed:true};
    if(url==='/eq') {
      const value=f.sound[body.zone];
      for(const field of ['bass','treble','loudness']) if(field in body)value[field]=body[field];
      data={done:value.room+' sound changed',eq:structuredClone(value)};
    }""")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--browser-executable', default='/usr/bin/chromium')
    parser.add_argument('--screenshots-dir', type=Path)
    args = parser.parse_args()
    with sync_playwright() as p:
        browser = p.chromium.launch(headless=True, executable_path=args.browser_executable)
        page = browser.new_page(viewport={'width': 1440, 'height': 1000}, reduced_motion='reduce')
        page.set_default_timeout(5000)
        errors = []
        page.on('pageerror', lambda e: errors.append(str(e)))
        base.load(page)
        page.wait_for_function("!document.getElementById('open-speaker-sound').disabled")
        assert page.evaluate("fixture.calls.filter(c=>c.url.endsWith('/eq')).length") == 0
        page.locator('#open-speaker-sound').click()
        bass = page.get_by_role('slider', name='Kitchen@S2 bass', exact=True)
        page.wait_for_function("document.querySelectorAll('.sound-room').length===2")
        assert not bass.is_disabled()
        assert page.get_by_role('slider', name='Studio@S2 bass', exact=True).input_value() == '1'
        assert page.evaluate('fixture.writes.length') == 0
        bass.press('Home')
        for _ in range(15):
            bass.press('ArrowRight')
        page.get_by_role('checkbox', name='Kitchen@S2 loudness', exact=True).uncheck()
        page.locator('.sound-room').nth(1).get_by_role('button', name='Review sound', exact=True).click()
        assert page.locator('#speaker-sound-preview').is_visible()
        assert 'bass +5' in page.locator('#speaker-sound-after').inner_text()
        assert page.evaluate('document.activeElement.id') == 'speaker-sound-preview'
        assert page.evaluate('fixture.writes.length') == 0
        page.locator('#speaker-sound-confirm').click()
        page.wait_for_function("document.getElementById('speaker-sound-result').textContent.includes('Kitchen@S2: bass +5')")
        assert page.evaluate('fixture.writes') == [
            {'url': '/eq', 'body': {'zone': 'Kitchen@S2', 'bass': 5, 'loudness': False}}]
        page.wait_for_function("!document.getElementById('speaker-sound-read').disabled")
        assert bass.input_value() == '5'
        assert page.evaluate('fixture.levels["Kitchen@S2"]') == 15
        assert page.evaluate('fixture.states[0]') == 'playing'
        print('PASS: actual member controls review and submit one partial native EQ patch, then read it back')

        page.set_viewport_size({'width': 390, 'height': 850})
        page.locator('.sound-room').nth(1).get_by_role('button', name='Flat sound', exact=True).click()
        assert bass.input_value() == '0'
        page.locator('.sound-room').nth(1).get_by_role('button', name='Discard edits', exact=True).click()
        assert bass.input_value() == '5'
        assert not page.evaluate('document.documentElement.scrollWidth>innerWidth')
        assert not page.locator('#speaker-sound-dialog').evaluate('(n)=>n.scrollWidth>n.clientWidth')
        if args.screenshots_dir:
            args.screenshots_dir.mkdir(parents=True, exist_ok=True)
            page.screenshot(path=str(args.screenshots_dir / 'speaker-sound-mobile.png'))
            page.set_viewport_size({'width': 1440, 'height': 1000})
            page.screenshot(path=str(args.screenshots_dir / 'speaker-sound-desktop.png'))
        page.keyboard.press('Escape')
        assert not page.locator('#speaker-sound-dialog').is_visible()
        assert page.evaluate('document.activeElement.id') == 'open-speaker-sound'
        assert page.evaluate('fixture.writes.length') == 1
        print('PASS: mobile layout, local flat/reset edits, Escape and trigger focus')

        page.locator('#open-speaker-sound').click()
        page.wait_for_function("!document.getElementById('speaker-sound-read').disabled")
        bass.press('Home')
        for _ in range(17):
            bass.press('ArrowRight')
        page.locator('.sound-room').nth(1).get_by_role('button', name='Review sound', exact=True).click()
        page.evaluate('fixture.sound["Kitchen@S2"].treble=4')
        page.locator('#speaker-sound-confirm').click()
        page.wait_for_function("document.getElementById('speaker-sound-message').textContent.includes('speaker sound changed')")
        assert page.evaluate('fixture.writes.length') == 1
        assert page.locator('#speaker-sound-confirm').is_disabled()
        assert not errors, errors
        print('PASS: a concurrent tone change invalidates confirmation without sending a write')
        browser.close()


if __name__ == '__main__':
    main()
