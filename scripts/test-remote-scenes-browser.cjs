// Optional real-browser checks of scene music selection. All replies are
// synthetic; the component never contacts a daemon or a music service here.
'use strict';
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { test } = require('node:test');
const assets = path.join(__dirname, '../crates/fsonos-api/src/remote');
const read = name => fs.readFileSync(path.join(assets, name), 'utf8');
const remote = read('remote.js');
const element = remote.slice(remote.indexOf('function el('), remote.indexOf('\nlet toastTimer'));
const screenshots = process.env.FSONOS_SCENES_SCREENSHOTS;
const control = (page, key) => page.locator(`[data-scene-focus="${key}"]`);
const picker = (page, number) => page.getByRole('region', { name: `Group ${number} music choices`, exact: true });
const writes = page => page.evaluate(() => fixture.calls.filter(([route]) => route === 'sceneSave'));
const checkWidth = async page => assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);

async function load(page, theme) {
  await page.setContent(`<html><head><meta name="viewport" content="width=device-width, initial-scale=1"></head>
    <body class="studio" data-theme="${theme}"><main style="padding:16px;max-width:960px;margin:auto"></main></body></html>`);
  for (const name of ['remote.css', 'scenes.css', 'studio.css']) await page.addStyleTag({ content: read(name) });
  await page.addScriptTag({ content: `${element}\nconst module = { exports: {} };\n${read('scenes.js')}\nwindow.SceneComponent = module.exports;` });
  await page.evaluate(() => {
    window.fixture = { calls: [], failFavorites: false, emptyFavorites: false, emptyMoods: false,
      deferFavorites: false, waiting: [], rows: [] };
    const favorites = zone => Array.from({ length: 16 }, (_, n) => ({ id: `FV:2/${n + 1}`,
      title: n === 12 ? 'Cello evening <img src=x onerror=alert(1)>' : `Collection ${n + 1}`,
      kind: n % 3 === 0 ? 'container' : n % 3 === 1 ? 'track' : 'stream',
      description: `${zone} · ${n === 12 ? 'Evening chamber music' : 'Fixture collection'}` }));
    const call = async (route, args = {}) => {
      fixture.calls.push([route, JSON.parse(JSON.stringify(args))]);
      if (route === 'scenes') return structuredClone(fixture.rows);
      if (route === 'zones') return [
        { household: 'S1', coordinator_room: 'Den', members: ['Den', 'Hall'] },
        { household: 'S2', coordinator_room: 'Den', members: ['Den', 'Studio'] },
      ];
      if (route === 'favorites') {
        if (fixture.failFavorites) throw new Error('Fixture favorite connection interrupted');
        if (fixture.deferFavorites) await new Promise(resolve => fixture.waiting.push(resolve));
        return fixture.emptyFavorites ? [] : favorites(args.query.zone);
      }
      if (route === 'moods') return { moods: fixture.emptyMoods ? [] : [{ name: 'focus' }, { name: 'Late evening' }] };
      if (route !== 'sceneSave') throw new Error(`Unexpected speaker route: ${route}`);
      const saved = { name: args.params.name, saved: 1_900_000_000, ...structuredClone(args.body) };
      for (const group of saved.groups) if (group.source.kind === 'favorite') {
        group.source.name = favorites(group.coordinator).find(item => item.id === group.source.name).title;
        group.source.uri = 'x-file:fixture-server-metadata';
      }
      fixture.rows.push(saved);
      return structuredClone(saved);
    };
    fixture.panel = SceneComponent.panel({ call, el });
    document.querySelector('main').append(fixture.panel.node);
    fixture.panel.node.open = true;
  });
  await control(page, 'new-definition').waitFor();
  await control(page, 'new-definition').click();
  await control(page, 'name').fill('Evening music');
  await control(page, 'add-group').click();
  await control(page, 'leader-1').selectOption('Den@S1');
}

async function capture(page, width, theme, state) {
  if (screenshots && [390, 1440].includes(width)) {
    await page.screenshot({ path: path.join(screenshots, `scene-${state}-${width}-${theme}.png`), fullPage: true });
  }
}

test('scene music browser matrix: favorites, search, paging, retry, DJ, review and save', {
  skip: process.env.FSONOS_SCENES_CHROMIUM ? false : 'Set FSONOS_SCENES_CHROMIUM to opt into the Playwright browser check.',
}, async () => {
  const { chromium } = require('playwright');
  if (screenshots) fs.mkdirSync(screenshots, { recursive: true });
  const browser = await chromium.launch({ executablePath: process.env.FSONOS_SCENES_CHROMIUM,
    headless: true, args: ['--no-sandbox', '--disable-dev-shm-usage', '--disable-gpu', '--no-zygote'] });
  try {
    for (const width of [320, 390, 768, 1440]) for (const theme of ['light', 'dark']) {
      const page = await browser.newPage({ viewport: { width, height: 960 }, reducedMotion: 'reduce' });
      const errors = []; page.on('pageerror', error => errors.push(error.message));
      try {
        await load(page, theme);
        await control(page, 'source-1').selectOption('favorite');
        await control(page, 'source-1-FV:2/1').waitFor();
        assert.equal(await picker(page, 1).locator('.scene-source-option').count(), 6);
        assert.match(await picker(page, 1).innerText(), /For Den@S1/);
        const target = await control(page, 'source-1-FV:2/1').boundingBox();
        assert.ok(target.height >= 44 && target.width >= 44);
        await checkWidth(page); await capture(page, width, theme, 'browse');

        await control(page, 'sources-search-1').pressSequentially('cello');
        assert.equal(await control(page, 'sources-search-1').inputValue(), 'cello');
        assert.equal(await control(page, 'sources-search-1').evaluate(el => el === document.activeElement && el.selectionStart === 5), true);
        assert.equal(await picker(page, 1).locator('.scene-source-option').count(), 1);
        await control(page, 'source-1-FV:2/13').focus(); await page.keyboard.press('Enter');
        assert.match(await page.getByLabel('Group 1 selected source', { exact: true }).innerText(), /Cello evening <img src=x/);
        assert.equal(await picker(page, 1).locator('img').count(), 0, 'titles render as text');
        await control(page, 'sources-search-1').fill('No matching favorite');
        assert.match(await control(page, 'sources-count-1').innerText(), /No favorites match/);
        assert.equal(await page.getByLabel('Group 1 selected source', { exact: true }).isVisible(), true);
        await control(page, 'sources-search-1').fill('');
        await control(page, 'sources-next-1').click();
        assert.equal(await control(page, 'sources-count-1').evaluate(el => el === document.activeElement), true);
        assert.match(await control(page, 'sources-count-1').innerText(), /7–12 of 16/);
        await control(page, 'sources-next-1').click();
        assert.equal(await control(page, 'sources-next-1').isDisabled(), true);
        assert.equal(await control(page, 'source-1-FV:2/13').getAttribute('aria-pressed'), 'true');
        await checkWidth(page); await capture(page, width, theme, 'selected');

        await page.evaluate(() => { fixture.failFavorites = true; });
        await control(page, 'sources-reload-1').click();
        await picker(page, 1).getByRole('alert').waitFor();
        assert.equal(await control(page, 'sources-reload-1').innerText(), 'Retry favorites');
        assert.equal(await page.getByLabel('Group 1 selected source', { exact: true }).isVisible(), true);
        await page.evaluate(() => { fixture.failFavorites = false; });
        await control(page, 'sources-reload-1').click();
        await control(page, 'source-1-FV:2/1').waitFor();

        await control(page, 'add-group').click();
        await control(page, 'leader-2').selectOption('Den@S2');
        await control(page, 'source-2').selectOption('dj');
        await control(page, 'source-2-focus').waitFor();
        assert.equal(await control(page, 'automatic-2').getAttribute('aria-pressed'), 'true');
        await control(page, 'source-2-focus').focus(); await page.keyboard.press('Enter');
        assert.equal(await control(page, 'automatic-2').getAttribute('aria-pressed'), 'false');
        assert.match(await page.getByLabel('Group 2 selected source', { exact: true }).innerText(), /focus/);
        assert.deepEqual(await writes(page), []);
        await control(page, 'review-definition').click();
        const review = page.getByRole('region', { name: 'Review scene change' });
        await review.waitFor();
        const reviewBounds = await review.boundingBox();
        assert.ok(reviewBounds.y >= 0 && reviewBounds.y + Math.min(reviewBounds.height, 300) <= 960,
          'review is brought into view after composing music');
        assert.match(await review.innerText(), /Den@S1: favorite “Cello evening/);
        assert.match(await review.innerText(), /Den@S2: DJ \(focus\)/);
        assert.deepEqual(await writes(page), []);
        await checkWidth(page); await capture(page, width, theme, 'review');
        await control(page, 'confirm').click();
        await page.getByText('Saved custom scene “Evening music”. No playback was changed.', { exact: false }).waitFor();
        const sent = await writes(page);
        assert.equal(sent.length, 1);
        assert.deepEqual(sent[0][1].body.groups.map(g => [g.coordinator, g.source]), [
          ['Den@S1', { kind: 'favorite', name: 'FV:2/13' }], ['Den@S2', { kind: 'dj', mood: 'focus' }],
        ]);
        assert.deepEqual(errors, []);
      } finally { await page.close(); }
    }

    const page = await browser.newPage({ viewport: { width: 390, height: 960 } });
    try {
      await load(page, 'dark');
      await page.evaluate(() => { fixture.emptyFavorites = true; fixture.emptyMoods = true; });
      await control(page, 'source-1').selectOption('favorite');
      await picker(page, 1).getByText('No uniquely named playable favorites', { exact: false }).waitFor();
      await control(page, 'source-1').selectOption('dj');
      await picker(page, 1).getByText('No named moods are available.', { exact: false }).waitFor();
      assert.equal(await control(page, 'automatic-1').isEnabled(), true);
      await control(page, 'source-1').selectOption('uri');
      await control(page, 'value-1').fill('https://open.spotify.com/album/0123456789ABCDEFabcdef');
      await control(page, 'review-definition').click();
      await page.getByRole('region', { name: 'Review scene change' }).waitFor();
      assert.match(await page.getByRole('region', { name: 'Review scene change' }).innerText(), /spotify:album:0123456789ABCDEFabcdef/);
      assert.deepEqual(await writes(page), []);
      await control(page, 'cancel').click();
      await page.evaluate(() => { fixture.emptyFavorites = false; fixture.deferFavorites = true; });
      await control(page, 'source-1').selectOption('favorite');
      await picker(page, 1).getByText('Reading favorites for this group…', { exact: true }).waitFor();
      await control(page, 'leader-1').selectOption('Den@S2');
      assert.equal(await control(page, 'source-1').inputValue(), 'keep');
      await page.evaluate(() => { fixture.deferFavorites = false; fixture.waiting.forEach(resolve => resolve()); });
      await control(page, 'source-1').selectOption('favorite');
      await control(page, 'source-1-FV:2/1').waitFor();
      assert.match(await picker(page, 1).innerText(), /For Den@S2/);
      assert.doesNotMatch(await picker(page, 1).innerText(), /Den@S1/);
      assert.deepEqual(await writes(page), []);
    } finally { await page.close(); }
  } finally { await browser.close(); }
});
