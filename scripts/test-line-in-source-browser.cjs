// Optional real-browser check of the shipped phone and dashboard source pickers.
'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const assets = path.join(__dirname, '../crates/fsonos-api/src/remote');
const read = name => fs.readFileSync(path.join(assets, name), 'utf8');
const remote = read('remote.js');
const element = remote.slice(remote.indexOf('function el('), remote.indexOf('\nlet toastTimer'));
const dashboard = read('dashboard.html');
const sourceDialog = dashboard.slice(dashboard.indexOf('<dialog id="source-dialog"'), dashboard.indexOf('</dialog>', dashboard.indexOf('<dialog id="source-dialog"')) + '</dialog>'.length);

async function load(page, kind, set) {
  await page.setContent('<html><head><meta name="viewport" content="width=device-width, initial-scale=1"></head>'
    + '<body data-theme="light"><main id="fixture" style="max-width:900px;margin:auto;padding:12px">'
    + (kind === 'dashboard' ? '<button id="open-source" type="button">Music source</button>' + sourceDialog : '') + '</main></body></html>');
  for (const file of kind === 'phone' ? ['remote.css', 'source-player.css'] : ['dashboard.css', 'dashboard-source.css']) await page.addStyleTag({ content: read(file) });
  await page.addScriptTag({ content: element });
  await page.addScriptTag({ content: read('dashboard-core.js') });
  await page.addScriptTag({ content: read('room-sets.js') });
  await page.addScriptTag({ content: read('source-player.js') });
  await page.addScriptTag({ content: read('source-targets.js') });
  await page.addScriptTag({ content: read('dashboard-source.js') });
  await page.evaluate(async ({ kind, set }) => {
    const zone = { household: 'S2', coordinator_room: 'Den', members: ['Den'], transport_state: 'playing' };
    const kitchen = { ...zone, coordinator_room: 'Kitchen', members: ['Kitchen'] };
    const zones = set ? [zone, kitchen] : [zone];
    const groups = zones.map(z => ({ household: z.household, coordinator_room: z.coordinator_room,
      coordinator: 'RINCON_FIXTURE_' + z.coordinator_room, members: z.members.map(room => ({ household: z.household,
        room, player: 'RINCON_FIXTURE_' + room, coordinator: 'RINCON_FIXTURE_' + z.coordinator_room })) }));
    const snapshot = { zone, transport_state: 'playing', volume: 20, mute: false,
      track: { uri: 'fixture:current', title: 'Current song', queue_position: 2, position_secs: 10, duration_secs: 120 } };
    const source = { player: 'RINCON_FIXTURE_INPUT', room: 'Record player', name: '<img src=x onerror=alert(1)> Turntable', icon: '1' };
    const catalog = { sources: [source], unavailable: [{ player: 'RINCON_FIXTURE_OFFLINE', room: 'Other input', detail: 'Player unavailable.' }] };
    const calls = []; let api, controller;
    const call = async (url, body) => {
      calls.push({ url, body });
      if (body !== undefined) {
        if (url !== (set ? '/sources/control' : '/line-in/play')) throw new Error('Unexpected playback route: ' + url);
        return { done: 'Playing line-in', changed: true, ...(set ? { batch: { complete: true,
          results: groups.map(g => ({ household: g.household, target: g.coordinator, zone: g.coordinator_room,
            result: { status: 'succeeded', done: 'Playing line-in', changed: true } })) } } : {}) };
      }
      if (url === '/zones') return structuredClone(zones);
      if (url.startsWith('/rooms/resolve?')) return structuredClone({ target: new URLSearchParams(url.split('?')[1]).get('target'),
        revision: 'a'.repeat(64), matched: 'alias', groups, rooms: groups.flatMap(g => g.members) });
      if (url === '/schedules') return [];
      if (url === '/zones/Den%40S2/state') return structuredClone(snapshot);
      if (url === '/zones/Kitchen%40S2/state') return structuredClone({ ...snapshot, zone: kitchen });
      if (url === '/line-in?zone=Den%40S2') return structuredClone(catalog);
      throw new Error('Unexpected read: ' + url);
    };
    if (kind === 'phone') {
      api = SourcePlayerRemote.panel({ el, call(name, args = {}) {
        const route = name === 'zones' ? '/zones' : name === 'state' ? '/zones/' + encodeURIComponent(args.params.room) + '/state'
          : { ...SourcePlayerRemote.routes, ...SourceTargetsRemote.routes }[name]?.[1];
        return call(route + (args.query ? '?' + new URLSearchParams(args.query) : ''), args.body);
      } });
      document.getElementById('fixture').append(api.node);
      await api.refresh(); api.node.querySelector('details').open = true;
    } else {
      controller = DashboardCore.model({ call, now: () => 1000, changed: () => api?.observe() });
      api = DashboardSource.panel({ call, controller, now: () => 1000 });
      await controller.refresh(); controller.connection(true); controller.select('Den@S2');
      document.getElementById('open-source').click();
    }
    window.fixture = { api, controller, calls, catalog };
  }, { kind, set });
}

test('phone and dashboard line-in groups and room sets work at mobile and desktop widths', {
  skip: process.env.FSONOS_LINE_IN_CHROMIUM ? false : 'Set FSONOS_LINE_IN_CHROMIUM to run the Chromium component check.',
}, async () => {
  const { chromium } = require('playwright');
  const browser = await chromium.launch({ executablePath: process.env.FSONOS_LINE_IN_CHROMIUM, headless: true,
    args: ['--no-sandbox', '--disable-dev-shm-usage', '--disable-gpu', '--no-zygote'] });
  try {
    for (const kind of ['phone', 'dashboard']) for (const width of [320, 1440]) for (const set of [false, true]) {
      const page = await browser.newPage({ viewport: { width, height: 1000 }, reducedMotion: 'reduce' });
      const errors = []; page.on('pageerror', error => errors.push(error.message));
      try {
        await load(page, kind, set);
        if (kind === 'phone') await page.getByLabel('Destination group for music', { exact: true }).selectOption('Den@S2');
        await (kind === 'phone' ? page.getByLabel('Music source', { exact: true }) : page.locator('#source-type')).selectOption('line_in');
        if (set) {
          await (kind === 'phone' ? page.getByLabel('Destination selection', { exact: true }) : page.locator('#source-destination-mode')).selectOption('set');
          await (kind === 'phone' ? page.getByLabel('Rooms or room set', { exact: true }) : page.locator('#source-destination-input')).fill('Record rooms');
        }
        assert.equal(await page.evaluate(() => fixture.calls.filter(call => call.url.startsWith('/line-in')).length), 0);
        assert.equal(await page.getByRole('button', { name: 'Review playback', exact: true }).isDisabled(), true);
        assert.equal(await (kind === 'phone' ? page.getByLabel('Play now or add to queue', { exact: true }) : page.locator('#source-mode')).isVisible(), false);
        await page.getByRole('button', { name: 'Read line-in sources', exact: true }).click();
        const input = page.getByLabel('Line-in source', { exact: true });
        await input.selectOption('RINCON_FIXTURE_INPUT');
        assert.match(await input.locator('option:checked').textContent(), /<img.*Turntable · Record player/);
        assert.match(await page.locator(kind === 'phone' ? '.source-player' : '#source-dialog').innerText(), /Other input: Player unavailable/);
        await page.getByRole('button', { name: 'Review playback', exact: true }).click();
        const review = page.locator(kind === 'phone' ? '.source-review' : '#source-preview');
        await review.waitFor({ state: 'visible' });
        assert.match(await review.innerText(), /signal state are not reported/);
        if (set) { assert.match(await review.innerText(), /Kitchen/); assert.match(await review.innerText(), /queue.*remain|queue stays available/i); }
        assert.equal(await page.locator('img').count(), 0, 'source metadata is rendered as text');
        assert.equal(await page.evaluate(() => fixture.calls.filter(call => call.body !== undefined).length), 0);
        assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true, kind + ' fits ' + width);
        await review.getByRole('button', { name: 'Play now', exact: true }).click();
        await page.waitForFunction(() => fixture.api.state.result?.status === 'accepted');
        const writes = await page.evaluate(() => fixture.calls.filter(call => call.body !== undefined));
        assert.deepEqual(writes, set ? [{ url: '/sources/control', body: { zone: 'Record rooms', expected_revision: 'a'.repeat(64),
          mode: 'play', source: { kind: 'line_in', source: 'RINCON_FIXTURE_INPUT' } } }]
          : [{ url: '/line-in/play', body: { zone: 'Den@S2', source: 'RINCON_FIXTURE_INPUT' } }]);
        assert.equal(await page.evaluate(() => fixture.calls.filter(call => call.url.startsWith('/line-in?')).length), 3);
        assert.deepEqual(errors, []);
      } finally { await page.close(); }
    }
  } finally { await browser.close(); }
});
