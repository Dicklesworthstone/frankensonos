// Optional Chromium component check of the shipped queue UI. Uses synthetic
// queue replies only; no daemon, speaker, Spotify or external API is contacted.
'use strict';
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { test } = require('node:test');
const assets = path.join(__dirname, '../crates/fsonos-api/src/remote');
const read = name => fs.readFileSync(path.join(assets, name), 'utf8');
const remote = read('remote.js');
const element = remote.slice(remote.indexOf('function el('), remote.indexOf('\nlet toastTimer'));
const screenshots = process.env.FSONOS_QUEUE_SCREENSHOTS;

async function load(page, theme, total = 40) {
  await page.setContent(`<html><head><meta name="viewport" content="width=device-width, initial-scale=1"></head>
    <body class="studio" data-theme="${theme}"><main style="padding:16px;max-width:960px;margin:auto">
    <div id="households"><article class="zone"><h3>Listening room · S2</h3></article></div></main></body></html>`);
  for (const name of ['remote.css', 'studio.css']) await page.addStyleTag({ content: read(name) });
  await page.addScriptTag({ content: element + '\n' + read('queue.js') });
  await page.evaluate(total => {
    const entries = Array.from({ length: total }, (_, n) => ({
      title: n === 3 ? '<img src=x onerror=alert(1)> Repeated recording' : `Recording ${n + 1}`,
      creator: 'Fixture Ensemble', album: 'Example collection', uri: `test:entry:${n + 1}`,
    }));
    if (total >= 4) entries[1].uri = entries[3].uri = 'test:duplicate';
    let revision = 'q:1';
    window.fixture = { entries, calls: [], blocked: false, failRead: false };
    const call = async (route, args) => {
      fixture.calls.push([route, JSON.parse(JSON.stringify(args))]);
      if (route === 'queue') {
        if (fixture.failRead) throw new Error('Fixture connection lost');
        return { total: entries.length, revision, playing: 2,
          items: entries.slice(args.query.offset, args.query.offset + args.query.limit)
            .map((item, n) => ({ ...item, position: args.query.offset + n + 1 })) };
      }
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
  }, total);
  if (total) await page.waitForSelector('[data-focus="move-4"]');
  else await page.waitForFunction(() => document.querySelector('[data-focus="queue-location"]')?.textContent === '0 entries');
}

const control = (page, key) => page.locator(`[data-focus="${key}"]`);
const posts = page => page.evaluate(() => fixture.calls.filter(([route]) => route !== 'queue'));
const tool = (page, key) => page.locator(`[data-queue-tool="${key}"]`);
const checkWidth = async page => assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
async function capture(page, width, theme, state) {
  if (screenshots && [390, 1440].includes(width)) {
    await page.screenshot({ path: path.join(screenshots, `queue-${state}-${width}-${theme}.png`), fullPage: true });
  }
}

test('queue interaction and disclosure browser matrix', {
  skip: process.env.FSONOS_QUEUE_CHROMIUM ? false : 'Set FSONOS_QUEUE_CHROMIUM to opt into the Playwright browser check.',
}, async () => {
  const { chromium } = require('playwright');
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
        const firstEntry = await page.locator('.queue-items li').first().boundingBox();
        assert.ok(firstEntry.y < 440, `${width} ${theme}: queue entries appear before the fold (${firstEntry.y}px)`);
        for (const key of ['source', 'search', 'advanced']) assert.equal(await tool(page, key).getAttribute('open'), null);
        assert.equal(await control(page, 'source').isVisible(), false);
        assert.equal(await control(page, 'queue-search').isVisible(), false);
        assert.equal(await control(page, 'range-first').isVisible(), false);
        assert.equal(await page.locator('.queue-overview').isVisible(), true);
        await checkWidth(page); await capture(page, width, theme, 'initial');

        // Disclosure navigation and typing are read-only. Input redraws retain
        // the actual open tool, its draft and keyboard caret.
        const initialCalls = await page.evaluate(() => fixture.calls.length);
        await control(page, 'tool-source').click();
        await control(page, 'source').fill('spotify:track:unsent-fixture-draft');
        await control(page, 'tool-source').click();
        await control(page, 'tool-search').focus(); await page.keyboard.press('Enter');
        await control(page, 'queue-search').pressSequentially('Recording 30');
        assert.equal(await control(page, 'queue-search').inputValue(), 'Recording 30');
        assert.equal(await page.evaluate(() => fixture.calls.length), initialCalls);
        await control(page, 'Search queue').click();
        await page.waitForSelector('[data-focus="Show position 30"]');
        assert.equal(await tool(page, 'search').getAttribute('open'), '');
        assert.match(await control(page, 'tool-search').textContent(), /1 match\b/);
        await checkWidth(page); await capture(page, width, theme, 'search');
        await control(page, 'Show position 30').click();
        await page.waitForSelector('[data-focus="found-30"].queue-found');
        assert.equal(await tool(page, 'search').getAttribute('open'), null, 'opening a result returns attention to its queue occurrence');
        assert.equal(await control(page, 'found-30').evaluate(el => el === document.activeElement), true);
        assert.match(await control(page, 'tool-search').textContent(), /1 match\b/, 'collapsed search retains its result summary');
        await control(page, 'select-30').check();
        await control(page, 'Previous page').click();
        await page.waitForSelector('[data-focus="move-1"]');
        assert.equal(await page.locator('.queue-items').evaluate(el => el.scrollTop), 0,
          'a newly browsed page starts with its first entry, not the previous page scroll offset');
        assert.equal(await control(page, 'queue-location').evaluate(el => el === document.activeElement), true,
          'when paging disables the previous button, focus moves to the visible page range');
        assert.match(await page.locator('.queue-selection-heading').innerText(), /1 selected · 1 outside this page/);
        assert.equal(await page.locator('.queue-selection-moves').evaluate(el => Boolean(el.closest('[data-queue-tool]'))), false);
        await checkWidth(page); await capture(page, width, theme, 'selected');

        await control(page, 'tool-advanced').click();
        await control(page, 'jump').fill('35');
        await control(page, 'range-first').pressSequentially('24');
        assert.equal(await control(page, 'range-first').inputValue(), '24');
        assert.equal(await tool(page, 'advanced').getAttribute('open'), '');
        await control(page, 'Refresh queue').click();
        await page.waitForSelector('[data-focus="move-4"]');
        assert.equal(await tool(page, 'advanced').getAttribute('open'), '', 'refresh keeps deliberate disclosure state');
        assert.equal(await control(page, 'jump').inputValue(), '35');
        assert.equal(await control(page, 'range-first').inputValue(), '24');
        assert.equal(await control(page, 'source').inputValue(), 'spotify:track:unsent-fixture-draft');
        assert.equal(await tool(page, 'source').getAttribute('open'), null);
        await control(page, 'tool-advanced').click();
        assert.deepEqual(await posts(page), []);

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
        await capture(page, width, theme, 'move');
        await control(page, 'move-4-next').click();
        assert.match(await page.locator('.queue-review').innerText(), /before original position 3/);
        assert.equal(await control(page, 'Cancel move').evaluate(el => el === document.activeElement), true);
        assert.equal(await page.locator('.queue-review').evaluate(el => Boolean(el.closest('[data-queue-tool]'))), false);
        const cancelBox = await control(page, 'Cancel move').boundingBox();
        assert.ok(cancelBox.y >= 0 && cancelBox.y + cancelBox.height <= 960, 'review actions are brought into view');
        await checkWidth(page); await capture(page, width, theme, 'review');
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
        await control(page, 'tool-advanced').click();
        await control(page, 'range-first').fill('24'); await control(page, 'range-last').fill('27');
        await control(page, 'Read and select range').click();
        await control(page, 'move-selection-earlier').click();
        assert.match(await page.locator('.queue-review').innerText(), /Move 4 entry\(s\) from position 24/);
        await control(page, 'Cancel move').click();
        assert.equal(await control(page, 'move-selection-earlier').evaluate(el => el === document.activeElement), true);
        assert.equal((await posts(page)).length, 1);
        await page.evaluate(() => fixture.panel.stale());
        assert.equal(await control(page, 'move-40').isDisabled(), true);
        assert.match(await page.locator('.queue-notice').innerText(), /Queue changed/);
        assert.equal((await posts(page)).length, 1);
        // A failed read stays prominent even when every form is collapsed.
        await control(page, 'tool-advanced').click();
        await page.evaluate(() => { fixture.failRead = true; });
        await control(page, 'Refresh queue').click();
        await page.waitForFunction(() => document.querySelector('.queue-notice')?.textContent.includes('Fixture connection lost'));
        assert.equal(await page.locator('.queue-notice').isVisible(), true);
        assert.equal(await page.locator('.queue-notice').evaluate(el => Boolean(el.closest('[data-queue-tool]'))), false);
        assert.equal(await tool(page, 'source').getAttribute('open'), null);
        await checkWidth(page);
        assert.deepEqual(errors, []);
        results.push({ width, theme, firstEntryY: Math.round(firstEntry.y), keyboard: true, review: true,
          exactOccurrence: true, crossPage: true, searchAndDrafts: true, noticesVisible: true, overflow: false });
      } finally { await page.close(); }
    }
    const empty = await browser.newPage({ viewport: { width: 320, height: 960 } });
    try {
      await load(empty, 'dark', 0);
      assert.equal(await tool(empty, 'source').getAttribute('open'), '', 'an empty queue offers its add-music form immediately');
      assert.equal(await control(empty, 'source').isVisible(), true);
      await control(empty, 'source').fill('spotify:track:empty-queue-draft');
      await control(empty, 'tool-source').click();
      await control(empty, 'Refresh queue').click();
      await empty.waitForFunction(() => fixture.calls.length === 2);
      assert.equal(await tool(empty, 'source').getAttribute('open'), null, 'an explicit collapse remains respected');
      assert.equal(await control(empty, 'source').inputValue(), 'spotify:track:empty-queue-draft');
      assert.deepEqual(await posts(empty), []);
      await checkWidth(empty);
    } finally { await empty.close(); }
    console.log(JSON.stringify({ component: 'queue movement', emptyQueue: true, results }, null, 2));
  } finally { await browser.close(); }
});
