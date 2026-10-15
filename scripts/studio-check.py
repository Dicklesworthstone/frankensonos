"""Optional offline Chromium checks. Synthetic native-shaped controls, no daemon.

Install Python Playwright and Chromium. FSONOS_STUDIO_CHROMIUM overrides the
browser executable; FSONOS_STUDIO_SCREENSHOTS sets the output directory.
"""
import json
import os
import re
from pathlib import Path
from playwright.sync_api import sync_playwright

ROOT = Path(__file__).resolve().parents[1]
ASSETS = ROOT / 'crates/fsonos-api/src/remote'
OUTPUT = Path(os.environ.get('FSONOS_STUDIO_SCREENSHOTS', '/tmp/fsonos-studio'))
CHROMIUM = os.environ.get('FSONOS_STUDIO_CHROMIUM', '/usr/bin/chromium')
OUTPUT.mkdir(parents=True, exist_ok=True)


def run():
    observations = []
    with sync_playwright() as pw:
        browser = pw.chromium.launch(executable_path=CHROMIUM, headless=True, args=['--no-sandbox'])
        for width in [1440, 1280, 960, 768, 390, 320]:
            page = browser.new_page(viewport={'width': width, 'height': 1080}, reduced_motion='reduce')
            errors = []
            page.on('pageerror', lambda err: errors.append(str(err)))
            # Offline DOM fixtures: deliberately not an HTTP/CSP delivery test.
            html = re.sub(r'<script[^>]*>.*?</script>|<link[^>]*>', '', (ASSETS / 'index.html').read_text(), flags=re.S)
            page.set_content(html)
            page.add_style_tag(content='.top{display:flex;flex-wrap:wrap;justify-content:space-between}.zone{display:flex;flex-direction:column}.row{display:flex;flex-wrap:wrap;gap:8px}.track{min-width:0}[hidden]{display:none!important}input[type=range]{width:100%}.toast{position:fixed;left:50%;bottom:16px;transform:translateX(-50%)}')
            for name in ['studio.css', 'studio-insights.css']:
                page.add_style_tag(content=(ASSETS / name).read_text())
            page.add_script_tag(content=(ROOT / 'scripts/studio-fixture.js').read_text())
            for name in ['studio.js', 'studio-insights.js']:
                page.add_script_tag(content=(ASSETS / name).read_text())
            page.wait_for_timeout(150)
            assert page.locator('#studio-room-rail button').count() == 3
            assert page.locator('article.zone:visible').count() == 1
            assert page.evaluate('fixture.commands.length') == 0
            assert page.evaluate('document.documentElement.scrollWidth <= innerWidth'), f'overflow at {width}'
            original = page.evaluate('fixture.house.cards.values().next().value.play === document.querySelector(".studio-transport .primary")')
            assert original
            assert page.locator('.studio-work-badge').inner_text() == 'WHOLE WORK · MOVEMENT 2 OF 3'
            assert 'Ravel' in page.locator('.studio-story-reason').inner_text()
            assert page.locator('.studio-story-next li').count() == 3
            reads = page.evaluate('fixture.storyReads.length')
            page.wait_for_timeout(100)
            assert page.evaluate('fixture.storyReads.length') == reads, 'layout reconciliation cannot refetch continuously'
            page.locator('.studio-transport .primary:visible').click()
            assert page.evaluate('fixture.commands[0].zone') == 'Living room@S2'
            page.locator('#studio-room-rail button').nth(2).click()
            page.wait_for_timeout(70)
            assert 'not running' in page.locator('.studio-story-content').inner_text()
            assert page.locator('.studio-work-badge').count() == 0, 'another household cannot retain the prior story'
            page.locator('.studio-transport .primary:visible').click()
            assert page.evaluate('fixture.commands.at(-1).zone') == 'Living room@S1'
            page.locator('#studio-room-rail button').nth(0).click()
            page.wait_for_timeout(60)
            before = page.evaluate('fixture.commands.length')
            page.locator('#studio-command').click()
            page.locator('#studio-search').fill('whole work')
            assert page.locator('#studio-results button').count() >= 1
            page.locator('#studio-results button').first.click()
            page.wait_for_timeout(100)
            assert not page.locator('#studio-music').is_hidden()
            assert page.locator('#studio-music details[open]').count() == 1
            assert page.evaluate('fixture.editorTarget.target') == 'Living room@S2'
            assert page.evaluate('JSON.parse(fixture.editorTarget.signature)[2].length') == 2
            assert page.evaluate('fixture.commands.slice(%d).every(c => c.kind === "tool-open")' % before)
            # Returning to a native panel preserves its node, listeners and draft.
            draft = page.locator('#studio-music details[open] input').first
            draft.fill('warm, acoustic, no repeats')
            page.locator('[data-workspace="routines"]').click()
            quiet_reads = page.evaluate('fixture.storyReads.length')
            page.wait_for_timeout(80)
            assert page.evaluate('fixture.storyReads.length') == quiet_reads
            page.locator('[data-workspace="music"]').click()
            assert draft.input_value() == 'warm, acoustic, no repeats'
            page.locator('[data-workspace="listen"]').click()
            page.wait_for_timeout(80)
            # State invalidation and the shared native observation request one DJ reread.
            count = page.evaluate('fixture.storyReads.length')
            page.evaluate("fixture.notify('stale'); fixture.notify('observe')")
            page.wait_for_timeout(70)
            assert page.evaluate('fixture.storyReads.length') == count + 1
            assert page.locator('.studio-story-notice').inner_text() == ''
            # Failure retains observed context and a truthful old timestamp.
            page.evaluate("() => { fixture.storyRead = () => { throw new Error('Fixture coordinator unavailable'); }; }")
            page.get_by_role('button', name='Refresh the selected group’s DJ story').click()
            page.wait_for_timeout(50)
            assert 'Fixture coordinator unavailable' in page.locator('.studio-story-notice').inner_text()
            assert 'Previous observation' in page.locator('.studio-story-time').inner_text()
            page.evaluate('fixture.storyRead = null')
            page.get_by_role('button', name='Refresh the selected group’s DJ story').click()
            page.wait_for_timeout(70)
            assert page.locator('.studio-story-notice').inner_text() == ''
            # All five workspaces have working navigation and keep page bounds.
            for workspace in ['music', 'house', 'routines', 'activity', 'listen']:
                page.locator(f'[data-workspace="{workspace}"]').click()
                assert page.evaluate('document.documentElement.scrollWidth <= innerWidth'), f'{workspace} overflow at {width}'
            page.wait_for_timeout(80)
            page.evaluate('window.scrollTo(0,0)')
            if width in [1440, 390]:
                page.screenshot(path=str(OUTPUT / f'studio-{width}-dark.png'), full_page=True)
            page.locator('#studio-theme').click()
            assert page.locator('body').get_attribute('data-theme') == 'light'
            assert page.evaluate('document.documentElement.scrollWidth <= innerWidth')
            if width == 1440:
                page.screenshot(path=str(OUTPUT / 'studio-1440-light.png'), full_page=True)
            page.locator('#studio-theme').click()
            # Untrusted metadata is text, never executable DOM.
            page.evaluate("() => { fixture.storyRead = target => { const value = fixture.storyFor(target); value.now.reason.summary = '<img src=x onerror=window.bad=true>'; return value; }; }")
            page.get_by_role('button', name='Refresh the selected group’s DJ story').click()
            page.wait_for_timeout(50)
            assert '<img' in page.locator('.studio-story-reason').inner_text()
            assert page.locator('.studio-story-reason img').count() == 0
            assert not page.evaluate('Boolean(window.bad)')
            page.evaluate('fixture.storyRead = null')
            # Native controls mounted later remain discoverable without reconstruction.
            page.evaluate("document.getElementById('households').before(fixture.el('section', {'class':'scenes','aria-label':'Future native capability'}, fixture.el('details', {}, fixture.el('summary', {text:'Future native control'}))))")
            page.wait_for_timeout(70)
            assert page.locator('#studio-house [aria-label="Future native capability"]').count() == 1
            # Retained selection follows household-qualified identity through recreation.
            page.evaluate('fixture.makeHouse(fixture.house.zones.slice().reverse())')
            page.wait_for_timeout(100)
            assert 'S2' in page.locator('#studio-room-rail button[aria-pressed=true]').get_attribute('aria-label')
            assert not errors, errors
            observations.append({'width': width, 'themes': ['dark', 'light'], 'overflow': False,
                                 'native_node_identity_preserved': original, 'native_listener_targets': True,
                                 'draft_preserved': True, 'scoped_dj_story': True, 'single_flight_recovery': True,
                                 'untrusted_text_safe': True, 'late_module_accessible': True, 'page_errors': errors})
            page.close()
        browser.close()
    print(json.dumps({'scope': 'Offline DOM fixture; synthetic native-shaped controls and API observations, not the Rust daemon.', 'results': observations}, indent=2))


if __name__ == '__main__':
    run()
