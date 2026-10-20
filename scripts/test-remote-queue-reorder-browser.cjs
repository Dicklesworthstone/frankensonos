// Optional Chromium component check of the shipped queue UI. Uses synthetic
// queue replies only; no daemon, speaker, Spotify or external API is contacted.
'use strict';
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { chromium } = require('playwright');
const assets = path.join(__dirname, '../crates/fsonos-api/src/remote');
const read = name => fs.readFileSync(path.join(assets, name), 'utf8');
const remote = read('remote.js');
const element = remote.slice(remote.indexOf('function el('), remote.indexOf('\nlet toastTimer'));
const screenshots = process.env.FSONOS_QUEUE_SCREENSHOTS;

async function load(page, theme) {
  await page.setContent(`<html><head><meta name="viewport" content="width=device-width, initial-scale=1"></head>
    <body class="studio" data-theme="${theme}"><main style="padding:16px;max-width:960px;margin:auto">
    <div id="households"><article class="zone"><h3>Listening room · S2</h3></article></div></main></body></html>`);
  for (const name of ['remote.css', 'studio.css']) await page.addStyleTag({ content: read(name) });
  await page.addScriptTag({ content: element + '\n' + read('queue.js') });
  await page.evaluate(() => {
    const entries = Array.from({ length: 40 }, (_, n) => ({
      title: n === 3 ? '<img src=x onerror=alert(1)> Repeated recording' : `Recording ${n + 1}`,
      creator: 'Fixture Ensemble', album: 'Example collection', uri: `test:entry:${n + 1}`,
    }));
    entries[1].uri = entries[3].uri = 'test:duplicate';
    let revision = 'q:1';
    window.fixture = { entries, calls: [], blocked: false };
    const call = async (route, args) => {
      fixture.calls.push([route, JSON.parse(JSON.stringify(args))]);
      if (route === 'queue') return { total: entries.length, revision, playing: 2,
        items: entries.slice(args.query.offset, args.query.offset + args.query.limit)
          .map((item, n) => ({ ...item, position: args.query.offset + n + 1 })) };
      if (route !== 'queueMove') throw new Error(`Unexpected write: ${route}`);
      const { position, insert_before: before, expected_uris: uris, expected_revision: expected } = args.body;
      if (expected !== revision || JSON.stringify(uris) !== JSON.stringify(entries.slice(position - 1, position - 1 + uris.length).map(i => i.uri))) {
        throw new Error('Queue selection changed');
      }
      const block = entries.splice(position - 1, uris.length);
      const at = before < position ? before - 1 : before - uris.length - 1;
      entries.splice(at, 0, ...block); revision += ':next';
      return { done: 'Moved queue entries', zone: 'Listening room', position: at + 1,
        count: block.length, total: entries.length, revision, changed: true, recovered: false, undoable: false };
    };
    fixture.panel = QueueRemote.panel({ zone: 'Listening room@S2', call, el, blocked: () => fixture.blocked });
    document.querySelector('.zone').append(fixture.panel.node);
    fixture.panel.node.open = true;
  });
  await page.waitForSelector('[data-focus="move-4"]');
}

const control = (page, key) => page.locator(`[data-focus="${key}"]`);
const posts = page => page.evaluate(() => fixture.calls.filter(([route]) => route !== 'queue'));

(async () => {
  if (screenshots) fs.mkdirSync(screenshots, { recursive: true });
  const browser = await chromium.launch({ executablePath: process.env.FSONOS_QUEUE_CHROMIUM,
    headless: true, args: ['--no-sandbox', '--disable-dev-shm-usage', '--disable-gpu', '--no-zygote'] });
  const results = [];
  try {
    for (const width of [320, 390, 768, 1440]) for (const theme of ['light', 'dark']) {
      const page = await browser.newPage({ viewport: { width, height: 960 }, reducedMotion: 'reduce' });
      const errors = []; page.on('pageerror', error => errors.push(error.message));
      try {
        await load(page, theme);
        // Open by keyboard, dismiss with Escape, and return focus to the row.
        await control(page, 'move-4').focus(); await page.keyboard.press('Enter');
        assert.equal(await control(page, 'move-4').getAttribute('aria-expanded'), 'true');
        await page.keyboard.press('Tab');
        assert.equal(await control(page, 'move-4-earlier').evaluate(el => el === document.activeElement), true);
        await page.keyboard.press('Escape');
        assert.equal(await control(page, 'move-4').getAttribute('aria-expanded'), 'false');
        assert.equal(await control(page, 'move-4').evaluate(el => el === document.activeElement), true);
        assert.deepEqual(await posts(page), []);

        await control(page, 'move-4').click();
        for (const key of ['move-4', 'move-4-earlier', 'move-4-later', 'move-4-next', 'move-4-start', 'move-4-end']) {
          const box = await control(page, key).boundingBox();
          assert.ok(box && box.height >= 44 && box.x >= 0 && box.x + box.width <= width, `${width} ${theme} ${key} touch bounds: ${JSON.stringify(box)}`);
        }
        const listBox = await page.locator('.queue-items').boundingBox();
        const endBox = await control(page, 'move-4-end').boundingBox();
        assert.ok(endBox.y >= listBox.y && endBox.y + endBox.height <= listBox.y + listBox.height + 1,
          'opening placement options reveals the last destination in the scrollable queue');
        assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
        assert.equal(await page.locator('.queue-items img').count(), 0, 'metadata stays literal');
        if (screenshots && [390, 1440].includes(width)) {
          await control(page, 'move-4').scrollIntoViewIfNeeded();
          await page.screenshot({ path: path.join(screenshots, `queue-move-${width}-${theme}.png`), fullPage: true });
        }
        await control(page, 'move-4-next').click();
        assert.match(await page.locator('.queue-review').innerText(), /before original position 3/);
        assert.equal(await control(page, 'Cancel move').evaluate(el => el === document.activeElement), true);
        assert.deepEqual(await posts(page), []);
        await control(page, 'Cancel move').click();
        assert.equal(await control(page, 'move-4').evaluate(el => el === document.activeElement), true);
        assert.deepEqual(await posts(page), []);

        // Touch/click path commits the existing occurrence only on confirmation.
        await control(page, 'move-4').click(); await control(page, 'move-4-end').click();
        await control(page, 'Move entries').click();
        await page.waitForSelector('[data-focus="move-40"]');
        assert.deepEqual(await posts(page), [['queueMove', { body: { zone: 'Listening room@S2', position: 4,
          insert_before: 41, expected_uris: ['test:duplicate'], expected_revision: 'q:1' } }]]);
        assert.equal(await page.evaluate(() => fixture.entries[39].title), '<img src=x onerror=alert(1)> Repeated recording');
        assert.equal(await control(page, 'move-40-end').isDisabled(), true);

        // A cross-page block uses the same shortcuts without losing its order.
        await control(page, 'range-first').fill('24'); await control(page, 'range-last').fill('27');
        await control(page, 'Read and select range').click();
        await control(page, 'move-selection-earlier').click();
        assert.match(await page.locator('.queue-review').innerText(), /Move 4 entry\(s\) from position 24/);
        await control(page, 'Cancel move').click();
        assert.equal(await control(page, 'move-selection-earlier').evaluate(el => el === document.activeElement), true);
        assert.equal((await posts(page)).length, 1);
        await page.evaluate(() => fixture.panel.stale());
        assert.equal(await control(page, 'move-40').isDisabled(), true);
        assert.equal((await posts(page)).length, 1);
        assert.deepEqual(errors, []);
        results.push({ width, theme, keyboard: true, review: true, exactOccurrence: true, crossPage: true, overflow: false });
      } finally { await page.close(); }
    }
    console.log(JSON.stringify({ component: 'queue movement', results }, null, 2));
  } finally { await browser.close(); }
})().catch(error => { console.error(error); process.exitCode = 1; });
