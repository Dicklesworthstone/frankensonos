#!/usr/bin/env python3
"""Component acceptance using shipped queue/session/workspace code and native DOM.

Run with Python Playwright and Chromium installed. The HTML host, HTTP answers
and review presentation are fixtures; this is not full-daemon/hardware acceptance.
"""
import json
import os
from pathlib import Path
import shutil
import unittest

try:
    from playwright.sync_api import sync_playwright
except ImportError:
    sync_playwright = None

ROOT = Path(__file__).resolve().parents[1]
ASSETS = ROOT / "crates/fsonos-api/src/remote"
HTML = """<!doctype html><html><head><meta name="viewport" content="width=device-width, initial-scale=1">
<style>body{margin:12px;font:15px system-ui}*{box-sizing:border-box}section{max-width:850px}button{font:inherit;cursor:pointer}button:disabled{cursor:default}#queue-items{list-style:none;padding:0;max-height:330px;overflow:auto}#queue-items li{padding:8px;gap:6px;border-bottom:1px solid #aaa}#queue-items strong,#queue-items small{display:block}dialog{max-width:calc(100% - 30px)}.queue-edit-fields input{border:1px solid #777}</style></head><body>
<section><button id="tab-picks">DJ picks</button><button id="tab-queue">Speaker queue</button>
<div id="dj-picks-panel"></div><div id="speaker-queue-panel" hidden><h2 id="queue-title">Speaker queue</h2>
<p id="queue-status"></p><button id="queue-refresh">Refresh queue</button><ul id="queue-items"></ul>
<span id="queue-page"></span><button id="queue-prev">Previous</button><button id="queue-next">Next</button></div></section>
<button id="dock-queue">Queue</button><div class="player-dock"><button id="toggle-volume">Volume</button><div id="volume-panel"></div></div>
<dialog id="rooms-dialog"><input id="room-filter"><div id="room-choices"></div><button id="close-rooms">Close rooms</button></dialog>
<dialog id="search-dialog"><input id="search-input"><div id="search-results"></div></dialog>
<button id="shortcut-help">Shortcuts</button><dialog id="shortcuts-dialog"><button id="close-shortcuts">Close shortcuts</button></dialog>
<dialog id="review-dialog"><h2 id="review-title"></h2><p id="review-description"></p><button id="cancel-review">Cancel</button><button id="confirm-action">Confirm</button></dialog>
<p id="outcome"></p></body></html>"""
FIXTURE = r"""async () => {
  const z = (household) => ({ household, coordinator_room: 'Synthetic room', members: ['Synthetic room', 'Other'], transport_state: 'paused' });
  const data = { zones: [z('S2'), z('S1')], transport: 'paused', playing: 2, revision: 'r1',
    tracks: Array.from({ length: 55 }, (_, i) => ({ title: `Track ${i + 1}`, uri: i < 5 ? 'same:duplicate' : `song:${i}` })) };
  const calls = []; let hold = '', release, postBehavior = '', hidden = false, controller, workspace, session;
  Object.defineProperty(document, 'hidden', { configurable: true, get: () => hidden });
  const $ = (id) => document.getElementById(id);
  const el = (tag, cls, text) => { const node = document.createElement(tag); if (cls) node.className = cls; if (text != null) node.textContent = text; return node; };
  const raw = async (url, body) => {
    calls.push({ url, body });
    if ((hold === 'state' && url.endsWith('/state')) || (hold === 'write' && body !== undefined)) {
      hold = ''; await new Promise((resolve) => { release = resolve; });
    }
    const zone = data.zones.find((z) => DashboardCore.key(z) === decodeURIComponent(url.split('/')[2] || '')) || data.zones[0];
    if (url === '/zones') return structuredClone(data.zones);
    if (url.endsWith('/state')) return { zone, transport_state: data.transport, volume: 20,
      track: data.playing ? { uri: data.tracks[data.playing - 1]?.uri || 'song:old', queue_position: data.playing } : null };
    if (url.endsWith('/dj')) return { zone: zone.coordinator_room, running: false, next: [], steering: { source: 'none' } };
    if (url.includes('/queue?')) { const offset = Number(new URLSearchParams(url.split('?')[1]).get('offset'));
      return { zone: zone.coordinator_room, total: data.tracks.length, playing: data.playing, revision: data.revision,
        items: data.tracks.slice(offset, offset + 40).map((row, n) => ({ ...row, position: offset + n + 1 })) }; }
    if (body !== undefined) {
      let result;
      if (url === '/queue/play') { data.playing = body.position; data.transport = 'playing'; result = { done: 'Played selected entry', changed: true }; }
      else if (url === '/queue/remove') { const count = body.expected_uris.length;
        data.tracks.splice(body.position - 1, count); data.playing = Math.min(data.playing, data.tracks.length) || null; data.revision += 'x';
        result = { done: 'Removed selected entries', zone: zone.coordinator_room, position: body.position, count,
          remaining: data.tracks.length, undoable: false, recovered: false }; }
      else if (url === '/queue/move') { const count = body.expected_uris.length, playing = data.tracks[data.playing - 1];
        const moved = data.tracks.splice(body.position - 1, count), position = body.insert_before < body.position ? body.insert_before : body.insert_before - count;
        data.tracks.splice(position - 1, 0, ...moved); data.playing = data.tracks.indexOf(playing) + 1; data.revision += 'x';
        result = { done: 'Moved selected entries', zone: zone.coordinator_room, position, count, total: data.tracks.length,
          revision: data.revision, changed: true, recovered: false, undoable: false }; }
      else result = { done: 'Transport accepted', changed: true };
      if (postBehavior === 'lost') throw new TypeError('Reply lost after applying');
      if (postBehavior === 'malformed') return { done: 'Incomplete' };
      return result;
    }
    throw new Error(`Unexpected fixture read ${url}`);
  };
  const call = DashboardCore.request({ fetcher: async (url, init) => {
    const result = await raw(url, init.body === undefined ? undefined : JSON.parse(init.body));
    return new Response(JSON.stringify(result), { headers: { 'content-type': 'application/json' } });
  } });
  controller = DashboardCore.model({ call, changed: () => { workspace?.observe(controller.state); $('outcome').textContent = controller.state.message; } });
  session = DashboardSession.model({ call, controller, changed: (state) => {
    const dialog = $('review-dialog'), r = state.pending;
    if (r) { $('review-title').textContent = r.title; $('review-description').textContent = r.detail; if (!dialog.open) dialog.showModal(); }
    else if (dialog.open) dialog.close();
  } });
  $('cancel-review').addEventListener('click', () => session.cancel());
  $('review-dialog').addEventListener('cancel', (e) => { e.preventDefault(); session.cancel(); });
  $('confirm-action').addEventListener('click', () => { void session.confirm(session.state.pending); });
  workspace = DashboardWorkspace.create({ call, controller, session, el, icon: () => el('span'), openSearch() {},
    native: (...args) => controller.command(...args), immerse() {} });
  window.fixture = { data, calls, controller, session, workspace, call,
    hold(value) { hold = value; }, release() { release?.(); release = null; }, behavior(value) { postBehavior = value; },
    writes: () => calls.filter((c) => c.body !== undefined),
    suspend() { hidden = true; workspace.suspend(); session.suspend(); controller.suspend(); call.cancelReads(); },
    async resume(explicit = false) { hidden = false; workspace.resume(); session.resume(); controller.resume(); await controller.refresh(explicit); },
  };
  await controller.refresh(); $('tab-queue').click();
}"""

@unittest.skipIf(sync_playwright is None, "Python Playwright is not installed")
class QueueBrowserTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.pw = sync_playwright().start()
        executable = os.environ.get("FSONOS_BROWSER_PATH") or shutil.which("chromium")
        cls.browser = cls.pw.chromium.launch(**({"executable_path": executable} if executable else {}))

    @classmethod
    def tearDownClass(cls):
        cls.browser.close()
        cls.pw.stop()

    def setUp(self):
        self.page = self.browser.new_page(viewport={"width": 390, "height": 844})
        self.page.set_default_timeout(4000)
        self.errors = []
        self.page.on("pageerror", lambda error: self.errors.append(str(error)))
        self.page.set_content(HTML)
        self.page.add_style_tag(content=(ASSETS / "dashboard-queue.css").read_text())
        for name in ["dashboard-core.js", "dashboard-session.js", "dashboard-queue.js", "dashboard-workspace.js"]:
            self.page.add_script_tag(content=(ASSETS / name).read_text())
        self.page.evaluate(FIXTURE)
        self.page.wait_for_function("document.getElementById('queue-status').textContent.includes('55 tracks')")

    def tearDown(self):
        try:
            self.assertEqual(self.errors, [])
        finally:
            self.page.close()

    def writes(self):
        return self.page.evaluate("fixture.writes()")

    def select(self, first=2, last=None):
        self.page.locator(f'[data-queue-focus="select-{first}"]').click()
        if last is not None:
            self.page.locator('[data-queue-field="last"]').fill(str(last))

    def confirm(self):
        self.page.locator('#confirm-action').click()
        self.page.wait_for_function("!fixture.controller.state.busy")

    def test_play_uses_review_and_existing_slot(self):
        self.page.locator('[data-queue-focus="play-3"]').click()
        self.assertIn('#3: Track 3', self.page.locator('#review-title').inner_text())
        self.assertEqual(self.writes(), [])
        self.confirm()
        self.assertEqual(self.writes(), [{"url": "/queue/play", "body": {"zone": "Synthetic room@S2", "position": 3, "expected_revision": "r1", "expected_uri": "same:duplicate"}}])

    def test_range_move_preserves_original_coordinates(self):
        self.select(2, 4)
        self.page.locator('[data-queue-field="before"]').fill('10')
        self.page.locator('[data-queue-edit="move"]').click()
        self.assertIn('new first position is #7', self.page.locator('#review-description').inner_text())
        self.confirm()
        body = self.writes()[0]['body']
        self.assertEqual((body['position'], body['insert_before'], len(body['expected_uris'])), (2, 10, 3))
        self.assertEqual(self.page.evaluate('fixture.data.transport'), 'paused')

    def test_range_removal_warns_and_refreshes_the_queue(self):
        self.select(2, 4)
        self.page.locator('[data-queue-edit="remove"]').click()
        self.assertIn('Not undoable', self.page.locator('#review-description').inner_text())
        self.assertIn('currently playing', self.page.locator('#review-description').inner_text())
        self.confirm()
        self.page.wait_for_function("document.getElementById('queue-status').textContent.includes('52 tracks')")
        self.assertEqual(len(self.writes()), 1)
        self.assertEqual(self.page.evaluate('fixture.data.transport'), 'paused')

    def test_invalid_text_retires_the_previous_range(self):
        self.select(2, 4)
        field = self.page.locator('[data-queue-field="last"]')
        field.fill('3x')
        self.assertEqual(field.input_value(), '3x')
        self.assertTrue(self.page.locator('[data-queue-edit="remove"]').is_disabled())
        self.page.locator('[data-queue-edit="remove"]').dispatch_event('click')
        self.assertFalse(self.page.locator('#review-dialog').evaluate('(n) => n.open'))
        self.assertEqual(self.writes(), [])

    def test_cancel_preserves_selection_and_allows_a_new_review(self):
        self.select()
        self.page.locator('[data-queue-edit="remove"]').click()
        self.page.locator('#cancel-review').click()
        self.page.locator('[data-queue-edit="play"]').click()
        self.assertIn('Play #2', self.page.locator('#review-title').inner_text())
        self.assertEqual(self.writes(), [])

    def test_retained_row_does_not_target_a_different_household(self):
        self.page.evaluate("window.oldPlay = document.querySelector('[data-queue-focus=play-3]'); fixture.controller.select('Synthetic room@S1')")
        self.page.wait_for_function("document.querySelector('[data-queue-focus=play-3]') !== window.oldPlay")
        self.page.evaluate("window.oldPlay.dispatchEvent(new Event('click'))")
        self.assertEqual(self.page.evaluate('fixture.session.state.pending'), None)
        self.page.locator('[data-queue-focus="play-3"]').click()
        self.confirm()
        self.assertEqual(self.writes()[0]['body']['zone'], 'Synthetic room@S1')

    def test_changed_revision_refuses_confirmation(self):
        self.select()
        self.page.locator('[data-queue-edit="remove"]').click()
        self.page.evaluate("fixture.data.revision = 'r2'")
        self.confirm()
        self.assertEqual(self.writes(), [])
        self.assertIn('queue or selected entries changed', self.page.locator('#outcome').inner_text())

    def test_hide_during_preflight_prevents_a_write(self):
        self.page.locator('[data-queue-focus="play-3"]').click()
        self.page.evaluate("fixture.hold('state')")
        self.page.locator('#confirm-action').click()
        self.page.wait_for_function('fixture.controller.state.busy')
        self.page.evaluate('fixture.suspend(); fixture.release()')
        self.page.wait_for_function('!fixture.controller.state.busy')
        self.assertEqual(self.writes(), [])

    def test_submitted_write_finishes_while_hidden(self):
        self.page.locator('[data-queue-focus="play-3"]').click()
        self.page.evaluate("fixture.hold('write')")
        self.page.locator('#confirm-action').click()
        self.page.wait_for_function('fixture.writes().length === 1')
        self.page.evaluate('fixture.suspend(); fixture.release()')
        self.page.wait_for_function('!fixture.controller.state.busy')
        self.assertEqual(len(self.writes()), 1)
        self.assertFalse(self.page.evaluate('fixture.controller.state.uncertain'))
        self.assertIn('Played selected entry', self.page.locator('#outcome').inner_text())

    def test_lost_acknowledgement_requires_explicit_recovery(self):
        self.select()
        self.page.locator('[data-queue-edit="remove"]').click()
        self.page.evaluate("fixture.behavior('lost')")
        self.confirm()
        self.assertTrue(self.page.evaluate('fixture.controller.state.uncertain'))
        self.page.evaluate('fixture.controller.refresh()')
        self.assertTrue(self.page.locator('[data-queue-focus="play-3"]').is_disabled())
        self.page.evaluate('fixture.controller.refresh(true)')
        self.page.wait_for_function("!document.querySelector('[data-queue-focus=play-3]').disabled")
        self.assertEqual(len(self.writes()), 1)

    def test_keyboard_move_and_input_composition(self):
        self.select()
        before = self.page.locator('[data-queue-field="before"]')
        before.fill('5')
        before.evaluate("(n) => n.dispatchEvent(new KeyboardEvent('keydown', {key:'Enter', isComposing:true, bubbles:true}))")
        self.assertEqual(self.page.evaluate('fixture.session.state.pending'), None)
        before.press('Enter')
        self.assertIn('Move #2', self.page.locator('#review-title').inner_text())
        self.assertEqual(self.writes(), [])

    def test_busy_transport_releases_queue_navigation(self):
        self.page.evaluate("fixture.hold('write'); void fixture.controller.command('next')")
        self.page.wait_for_function('fixture.writes().length === 1')
        self.assertTrue(self.page.locator('#queue-next').is_disabled())
        self.page.evaluate('fixture.release()')
        self.page.wait_for_function("!document.getElementById('queue-next').disabled")
        self.page.locator('#queue-next').click()
        self.page.wait_for_function("document.getElementById('queue-page').textContent.startsWith('41')")
        self.assertEqual(len(self.writes()), 1)

    def test_revisionless_queue_is_visible_but_read_only(self):
        self.page.evaluate('fixture.data.revision = null')
        self.page.locator('#queue-refresh').click()
        self.page.wait_for_function("document.querySelector('[data-queue-focus=play-3]').disabled")
        self.page.locator('.queue-editor summary').click()
        self.assertIn('read-only', self.page.locator('.queue-edit-status').inner_text())
        self.assertEqual(self.writes(), [])

    def test_long_metadata_and_range_fields_fit_phone_and_desktop_widths(self):
        self.page.evaluate("fixture.data.tracks[0].title = 'Synthetic-' + 'x'.repeat(2000)")
        self.page.locator('#queue-refresh').click()
        self.page.wait_for_function("document.querySelector('#queue-items strong').textContent.length > 1000")
        self.select(2, 4)
        for width in [320, 390, 768, 1440]:
            with self.subTest(width=width):
                self.page.set_viewport_size({"width": width, "height": 900})
                self.assertLessEqual(self.page.evaluate('document.documentElement.scrollWidth'), width)
                self.assertLessEqual(self.page.locator('.queue-editor').evaluate('(n) => n.scrollWidth'), self.page.locator('.queue-editor').evaluate('(n) => n.clientWidth') + 1)
        if os.environ.get('FSONOS_QUEUE_SCREENSHOT'):
            self.page.screenshot(path=os.environ['FSONOS_QUEUE_SCREENSHOT'], full_page=True)

if __name__ == '__main__':
    unittest.main(verbosity=2)
