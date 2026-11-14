#!/usr/bin/env python3
"""Exercise both shipped DJ room-set interfaces with synthetic API responses.

Requires Python Playwright and Chromium. No daemon, accounts or speakers.
Assets are read in the exact order embedded by the Rust HTTP handlers.
"""
from __future__ import annotations
import argparse
import importlib.util
from pathlib import Path
import re
from playwright.sync_api import sync_playwright

REPO = Path(__file__).resolve().parents[1]
ASSETS = REPO / 'crates/fsonos-api/src/remote'


def load_fixture():
    spec = importlib.util.spec_from_file_location('dashboard_fixture', Path(__file__).with_name('test-dashboard-browser.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module.FIXTURE


def bundle(path, first):
    source = path.read_text()
    definitions = dict(re.findall(r'const (\w+): &str = include_str!\("([^"]+)"\);', source))
    template = re.search(r'"\{' + first + r'\}[^"\n]+"', source).group()
    return '\n'.join((path.parent / definitions[name]).read_text() for name in re.findall(r'\{(\w+)\}', template))


FIXTURE = r"""
fixture.revision = 'a'.repeat(64);
fixture.partialDj = false;
const originalFetch = window.fetch;
window.fetch = async (url, init) => {
  const f = fixture;
  const reply = value => new Response(JSON.stringify(value), {headers:{'Content-Type':'application/json'}});
  if (url === '/events') return new Response(new ReadableStream({start(controller) {
    f.feed = controller; controller.enqueue(new TextEncoder().encode(': heartbeat\n\n'));
  }}), {headers:{'Content-Type':'text/event-stream'}});
  const player = (household, room) => `fixture-${household}-${room}`;
  const groups = () => f.zones.map(zone => ({household:zone.household, coordinator:player(zone.household, zone.coordinator_room),
    coordinator_room:zone.coordinator_room, members:zone.members.map(room => ({household:zone.household, room,
      player:player(zone.household, room), coordinator:player(zone.household, zone.coordinator_room)}))}));
  if (url === '/rooms') return reply(groups().flatMap(group => group.members.map(room => ({name:room.room,
    household:room.household, zone:group.coordinator_room, aliases:['evening']}))));
  if (url.startsWith('/rooms/resolve?')) {
    const target = new URLSearchParams(url.split('?')[1]).get('target'), selected = groups();
    return reply({target, matched:target === 'everywhere' ? 'everywhere' : 'alias', revision:f.revision,
      rooms:selected.map(group => group.members[0]), groups:selected});
  }
  if (url.startsWith('/favorites')) return reply([]);
  if (url === '/dj/control') {
    const body = JSON.parse(init.body);
    if (body.expected_revision !== f.revision) throw new Error('The fixture target changed before submission');
    f.writes.push({url, body});
    if (body.action === 'start') f.running = f.running.map(() => true);
    if (body.action === 'stop') f.running = f.running.map(() => false);
    if (body.mood || body.constraints || body.clear) f.directions = f.zones.map(() => body.clear
      ? {source:'program', mood:'focus', session_mood:null, constraints:{energy_bias:-1}, added:{}}
      : {source:'session', mood:body.mood || 'focus', session_mood:body.mood || null,
        constraints:{energy_bias:-1,...body.constraints}, added:body.constraints || {},
        expires_at:body.for_secs ? 1700000000 : null, expires_in_secs:body.for_secs || null});
    if (f.lost) throw new Error('Fixture lost reply after mutation');
    return reply({done:f.partialDj ? 'Studio@S2: skipped current work' : 'Studio@S1: accepted; Studio@S2: accepted', changed:true,
      notes:f.partialDj ? [{code:'SKIPPED',detail:'Studio@S1: DJ_NOT_RUNNING: no active feed; do not replay the batch'}] : []});
  }
  return originalFetch(url, init);
};
"""


def setup(browser, kind, width):
    page = browser.new_page(viewport={'width': width, 'height': 940}, reduced_motion='reduce')
    page.set_default_timeout(8000)
    errors = []
    page.on('pageerror', lambda error: errors.append(str(error)))
    html = (ASSETS / ('dashboard.html' if kind == 'dashboard' else 'index.html')).read_text()
    page.set_content(re.sub(r'<link[^>]+>|<script[^>]+></script>', '', html))
    path = REPO / 'crates/fsonos-api/src/http/remote'
    source = path / 'dashboard.rs' if kind == 'dashboard' else path.with_suffix('.rs')
    page.add_style_tag(content=bundle(source, 'STYLE'))
    if kind == 'phone':
        page.add_style_tag(content=bundle(path / 'studio.rs', 'STYLE'))
    page.add_script_tag(content=load_fixture() + FIXTURE)
    page.add_script_tag(content=bundle(source, 'EVENTS' if kind == 'dashboard' else 'EVENT_FEED_SCRIPT'))
    if kind == 'dashboard':
        page.wait_for_function("!document.getElementById('open-dj-targets').disabled")
        page.locator('#open-dj-targets').click()
    else:
        page.add_script_tag(content=bundle(path / 'studio.rs', 'DIAGNOSTICS_SCRIPT'))
        page.wait_for_selector('#households .zone')
        # Navigate through the actual phone tool index; it must be discoverable.
        page.locator('[data-workspace="music"]').first.click()
        page.locator('#dj-targets-controls summary').click()
    panel = page.locator('.dj-targets-panel')
    page.wait_for_function("document.querySelector('.dj-targets-panel').getAttribute('aria-busy') === 'false'")
    panel.locator('input[id$="-target"]').fill('evening')
    return page, panel, errors


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--browser-executable', default='/usr/bin/chromium')
    parser.add_argument('--screenshots-dir', type=Path)
    args = parser.parse_args()
    if args.screenshots_dir:
        args.screenshots_dir.mkdir(parents=True, exist_ok=True)
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch(executable_path=args.browser_executable, headless=True)
        for kind, width in [('dashboard', 1280), ('phone', 390)]:
            page, panel, errors = setup(browser, kind, width)
            if kind == 'dashboard':
                panel.locator('select[id$="-direction-mode"]').select_option('replace')
                panel.locator('select[id$="-mood"]').select_option('focus')
                panel.locator('.dj-direction-editor summary').click()
                artists = panel.get_by_label('Only artists', exact=True)
                artists.press_sequentially('Earth, Wind & Fire')
                assert artists.evaluate('node => node === document.activeElement && node.selectionStart === node.value.length')
                panel.get_by_label('Direction duration (minutes)', exact=True).fill('90')
                panel.get_by_label('Keep selection filters strict', exact=True).check()
            else:
                panel.locator('select[id$="-action"]').select_option('skip')
                page.evaluate('fixture.partialDj = true')
            panel.get_by_role('button', name='Review affected groups', exact=True).click()
            review = panel.locator('.dj-targets-review')
            review.wait_for(state='visible')
            assert 'Studio@S1' in review.inner_text() and 'Studio@S2' in review.inner_text()
            assert 'Also affected through grouping: Kitchen@S2' in review.inner_text()
            assert 'Current source:' in review.inner_text() and 'Current whole work:' in review.inner_text()
            assert page.evaluate('fixture.writes.length') == 0
            if args.screenshots_dir:
                page.screenshot(path=str(args.screenshots_dir / f'dj-targets-{kind}-{width}.png'))
            review.get_by_role('button', name=re.compile('^Confirm:')).click()
            page.wait_for_function("fixture.writes.length === 1 && document.querySelector('.dj-targets-panel').getAttribute('aria-busy') === 'false'")
            sent = page.evaluate('fixture.writes[0]')
            assert sent['url'] == '/dj/control'
            assert sent['body']['expected_revision'] == 'a' * 64 and sent['body']['zone'] == 'evening'
            if kind == 'dashboard':
                assert sent['body'] == {'zone': 'evening', 'action': 'start', 'mood': 'focus',
                    'constraints': {'include_artists': ['Earth, Wind & Fire'], 'strict': True},
                    'for_secs': 5400, 'expected_revision': 'a' * 64}
                assert 'DJ command accepted' in panel.locator('.dj-targets-result').inner_text()
            else:
                assert sent['body']['action'] == 'skip'
                assert 'Incomplete DJ request' in panel.locator('.dj-targets-result').inner_text()
                assert 'Studio@S1: DJ_NOT_RUNNING' in panel.locator('.dj-targets-result').inner_text()
                assert panel.get_by_role('button', name='Review affected groups', exact=True).is_disabled()
                panel.get_by_role('button', name='Read affected rooms', exact=True).click()
                page.wait_for_function("!document.querySelector('.dj-targets-observation').hidden")
                assert 'Studio@S1' in panel.locator('.dj-targets-observation').inner_text()
                assert 'Studio@S2' in panel.locator('.dj-targets-observation').inner_text()
                assert page.evaluate('fixture.writes.length') == 1
            assert not errors, errors
            assert page.evaluate('document.documentElement.scrollWidth <= innerWidth'), 'horizontal page overflow'
            page.close()
            print(f'PASS {kind} {width}px: shipped assets, reviewed groups, one revision-bound command, truthful receipt')
        browser.close()


if __name__ == '__main__':
    main()
