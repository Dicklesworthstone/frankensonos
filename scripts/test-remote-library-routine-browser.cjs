// Optional Chromium integration of the actual library, routine composer and
// Studio navigation. Uses synthetic API replies and tab storage only.
'use strict';
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { test } = require('node:test');
const assets = path.join(__dirname, '../crates/fsonos-api/src/remote');
const read = name => fs.readFileSync(path.join(assets, name), 'utf8');
const remote = read('remote.js');
const element = remote.slice(remote.indexOf('function el('), remote.indexOf('\nlet toastTimer'));
const ID = '0123456789ABCDEFabcdef';
const screenshots = process.env.FSONOS_ROUTINE_SCREENSHOTS;
const routine = (page, key) => page.locator(`[data-routine-focus="${key}"]`);
const library = (page, key) => page.locator(`[data-library-focus="${key}"]`);
const nav = (page, workspace) => page.locator(`#studio-nav [data-workspace="${workspace}"]`);
const writes = page => page.evaluate(() => fixture.calls.filter(([name]) => ['routineSaveScene', 'routineAdd'].includes(name)));
const checkWidth = async page => assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);

async function load(page, theme) {
  await page.setContent(read('index.html').replace(/<script\b[^>]*>[\s\S]*?<\/script>/g, '').replace(/<link\b[^>]*>/g, ''));
  for (const file of ['remote.css', 'library.css', 'favorite-routines.css', 'studio.css']) await page.addStyleTag({ content: read(file) });
  await page.addScriptTag({ content: `const module = { exports: {} };\n${element}\n${read('library.js')}\nwindow.LibraryComponent = module.exports;\n${read('favorite-routines.js')}\nwindow.RoutineCore = module.exports;\n${read('favorite-routines-panel.js')}\nwindow.RoutinePanel = module.exports;\n${read('studio.js')}\nwindow.StudioComponent = module.exports;` });
  await page.evaluate(theme => {
    const ids = ['0123456789ABCDEFabcdef', '1123456789ABCDEFabcdef'];
    window.fixture = { calls: [], scenes: [], schedules: [], journal: null, mode: 'normal', holdZones: false, release: null };
    const groups = [
      { household: 'S1', coordinator_room: 'Den', members: ['Den', 'Hall'] },
      { household: 'S2', coordinator_room: 'Den', members: ['Den'] },
    ];
    const favorites = target => [{ id: 'FV:2/1', title: target.endsWith('@S1') ? 'Morning station' : 'Other household music',
      kind: 'stream', description: 'Fixture radio' }];
    const item = (kind, n = 0) => ({ kind, source_uri: `spotify:${kind}:${ids[n]}`,
      title: `Morning collection ${n + 1}`, subtitle: 'Fixture artist', explicit: false, cached_tracks: 2 });
    const call = async (name, args = {}) => {
      fixture.calls.push([name, JSON.parse(JSON.stringify(args))]);
      if (name === 'zones' || name === 'routineZones') {
        if (name === 'routineZones' && fixture.holdZones) await new Promise(resolve => { fixture.release = resolve; });
        return structuredClone(groups);
      }
      if (name === 'libraryBrowse') return { kind: args.query.kind, offset: 0, limit: 50, total: 2,
        album_uri: null, coverage: 'cached', next_offset: null, revision: 'fixture-cache',
        items: [item(args.query.kind.slice(0, -1)), item(args.query.kind.slice(0, -1), 1)] };
      if (name === 'librarySearch') return [{ kind: 'favorite', favorite: 'FV:2/1',
        title: favorites(args.query.zone)[0].title, subtitle: 'Fixture radio' }, item('track')];
      if (name === 'routineFavorites') return favorites(args.query.zone);
      if (name === 'routineScenes') return structuredClone(fixture.scenes);
      if (name === 'routineSchedules') return structuredClone(fixture.schedules);
      if (name === 'routineSaveScene') {
        const saved = { name: args.params.name, saved: 1900000000, ...structuredClone(args.body) };
        if (saved.groups[0].source.kind === 'favorite') saved.groups[0].source = { kind: 'favorite',
          name: favorites(saved.groups[0].coordinator)[0].title, uri: 'x-file:fixture-radio' };
        fixture.scenes.push(saved);
        if (fixture.mode === 'lost_put') throw new Error('Fixture scene reply lost');
        return structuredClone(saved);
      }
      if (name === 'routineAdd') {
        const saved = { id: fixture.schedules.length + 1, scene: args.body.scene, action: 'apply_scene',
          when: args.body.when, enabled: true, creator: 'fixture', summary: 'Apply fixture scene' };
        fixture.schedules.push(saved); return structuredClone(saved);
      }
      throw new Error(`Unexpected speaker route ${name}`);
    };
    fixture.library = LibraryComponent.panel({ call, el });
    document.getElementById('music-library').append(fixture.library.node);
    fixture.routine = RoutinePanel.panel({ core: RoutineCore, call, el,
      journal: { load: () => fixture.journal && structuredClone(fixture.journal),
        save: value => { fixture.journal = structuredClone(value); }, clear: () => { fixture.journal = null; } } });
    document.getElementById('scenes-section').before(fixture.routine.node);
    fixture.studio = StudioComponent.mount({ document, window, el, house: { zones: [], cards: new Map() }, register() {} });
    document.body.dataset.theme = theme;
  }, theme);
  await nav(page, 'music').click();
  await page.locator('.library > summary').click();
  await page.waitForFunction(() => document.querySelector('.library select').options.length === 3);
}

async function browse(page, chooseGroup = true) {
  if (chooseGroup) await page.getByLabel('Destination group', { exact: true }).selectOption('Den@S1');
  await library(page, 'Browse albums').click();
  await library(page, 'routine-0').waitFor();
}

async function capture(page, width, theme, state) {
  if (screenshots && [390, 1440].includes(width)) {
    await page.screenshot({ path: path.join(screenshots, `routine-handoff-${state}-${width}-${theme}.png`), fullPage: true });
  }
}

test('library routine handoff with real Studio navigation, responsive composer and preserved drafts', {
  skip: process.env.FSONOS_ROUTINE_CHROMIUM ? false : 'Set FSONOS_ROUTINE_CHROMIUM to opt into the Playwright browser check.',
}, async () => {
  const { chromium } = require('playwright');
  if (screenshots) fs.mkdirSync(screenshots, { recursive: true });
  const browser = await chromium.launch({ executablePath: process.env.FSONOS_ROUTINE_CHROMIUM,
    headless: true, args: ['--no-sandbox', '--disable-dev-shm-usage', '--disable-gpu', '--no-zygote'] });
  try {
    for (const width of [320, 390, 768, 1440]) for (const theme of ['light', 'dark']) {
      const page = await browser.newPage({ viewport: { width, height: 960 }, reducedMotion: 'reduce' });
      const errors = []; page.on('pageerror', e => errors.push(e.message));
      try {
        await load(page, theme); await browse(page);
        await checkWidth(page); await capture(page, width, theme, 'library');
        await library(page, 'routine-0').focus(); await page.keyboard.press('Enter');
        await page.waitForFunction(() => document.querySelector('[data-routine-focus="source_uri"]')?.value.startsWith('spotify:album:'));
        assert.equal(await nav(page, 'routines').getAttribute('aria-current'), 'page');
        assert.equal(await page.locator('#studio-music').isHidden(), true);
        assert.equal(await routine(page, 'label').inputValue(), 'Morning collection 1');
        assert.equal(await routine(page, 'group').inputValue(), 'Den@S1');
        assert.equal(await routine(page, 'source_kind').inputValue(), 'spotify');
        assert.equal(await routine(page, 'source_uri').inputValue(), `spotify:album:${ID}`);
        assert.equal(await page.getByRole('region', { name: 'Selected library music' }).isVisible(), true);
        assert.deepEqual(await writes(page), []);
        const target = await routine(page, 'review').boundingBox();
        assert.ok(target.height >= 44 && target.width >= 44);
        await checkWidth(page); await capture(page, width, theme, 'composer');
        await routine(page, 'review').click();
        const review = page.getByRole('region', { name: 'Review music routine' }); await review.waitFor();
        assert.match(await review.innerText(), /Morning collection 1/);
        assert.match(await review.innerText(), /Exact saved group: Den@S1 \+ Hall@S1/);
        assert.deepEqual(await writes(page), []);
        await checkWidth(page); await capture(page, width, theme, 'review');
        await routine(page, 'confirm').click();
        await page.waitForFunction(() => document.querySelector('.favorite-routine-content [role="status"]').textContent.includes('was acknowledged'));
        const sent = await writes(page);
        assert.deepEqual(sent.map(([name]) => name), ['routineSaveScene', 'routineAdd']);
        assert.deepEqual(sent[0][1].body.groups[0].source, { kind: 'uri', uri: `spotify:album:${ID}` });
        assert.deepEqual(errors, []);
      } finally { await page.close(); }
    }

    // Existing unsent values survive leaving the workspace and a new incoming
    // source. Only an explicit Use action replaces music; timing remains.
    const draftPage = await browser.newPage({ viewport: { width: 390, height: 960 } });
    try {
      await load(draftPage, 'dark');
      await nav(draftPage, 'routines').click();
      await draftPage.locator('#favorite-routines-section > summary').click();
      await routine(draftPage, 'label').fill('Existing bedtime');
      await routine(draftPage, 'source_kind').selectOption('spotify');
      await routine(draftPage, 'group').selectOption('Den@S2');
      await routine(draftPage, 'source_uri').fill(`spotify:track:1123456789ABCDEFabcdef`);
      await routine(draftPage, 'cadence').selectOption('weekends');
      await routine(draftPage, 'time').fill('22:15');
      await routine(draftPage, 'volume').fill('17');
      await routine(draftPage, 'unmute').check();
      await nav(draftPage, 'music').click(); await browse(draftPage);
      await library(draftPage, 'routine-0').click();
      await draftPage.getByRole('region', { name: 'Music from library', exact: true }).waitFor();
      assert.equal(await routine(draftPage, 'source_uri').inputValue(), 'spotify:track:1123456789ABCDEFabcdef');
      assert.equal(await routine(draftPage, 'label').inputValue(), 'Existing bedtime');
      assert.equal(await routine(draftPage, 'group').inputValue(), 'Den@S2');
      assert.deepEqual(await writes(draftPage), []);
      await routine(draftPage, 'library-use').click();
      await draftPage.waitForFunction(() => document.querySelector('[data-routine-focus="source_uri"]')?.value.startsWith('spotify:album:'));
      assert.equal(await routine(draftPage, 'label').inputValue(), 'Existing bedtime');
      assert.equal(await routine(draftPage, 'group').inputValue(), 'Den@S2');
      assert.equal(await routine(draftPage, 'cadence').inputValue(), 'weekends');
      assert.equal(await routine(draftPage, 'time').inputValue(), '22:15');
      assert.equal(await routine(draftPage, 'volume').inputValue(), '17');
      assert.equal(await routine(draftPage, 'unmute').isChecked(), true);
      await routine(draftPage, 'review').click();
      const review = draftPage.getByRole('region', { name: 'Review music routine' }); await review.waitFor();
      assert.match(await review.innerText(), /When: weekends 22:15/);
      assert.match(await review.innerText(), /Exact saved group: Den@S2/);
      await routine(draftPage, 'cancel').click();
      await nav(draftPage, 'music').click(); await nav(draftPage, 'routines').click();
      await draftPage.locator('#favorite-routines-section > summary').click();
      assert.equal(await routine(draftPage, 'group').inputValue(), 'Den@S2');
      assert.equal(await routine(draftPage, 'time').inputValue(), '22:15');
      assert.deepEqual(await writes(draftPage), []);
    } finally { await draftPage.close(); }

    const favoritePage = await browser.newPage({ viewport: { width: 390, height: 960 } });
    try {
      await load(favoritePage, 'light');
      await favoritePage.getByLabel('Destination group', { exact: true }).selectOption('Den@S1');
      await favoritePage.getByLabel('Search your music library', { exact: true }).fill('Morning');
      await library(favoritePage, 'Search library').click(); await library(favoritePage, 'routine-0').click();
      await favoritePage.waitForFunction(() => document.querySelector('[data-routine-focus="favorite"]')?.value === 'FV:2/1');
      assert.equal(await routine(favoritePage, 'group').inputValue(), 'Den@S1');
      assert.equal(await routine(favoritePage, 'label').inputValue(), 'Morning station');
      await routine(favoritePage, 'group').selectOption('Den@S2');
      await favoritePage.waitForFunction(() => !document.querySelector('[data-routine-focus="favorite"]').disabled);
      assert.equal(await routine(favoritePage, 'favorite').inputValue(), '', 'a repeated ID in another household is not silently selected');
      assert.match(await favoritePage.getByRole('region', { name: 'Selected library music' }).innerText(), /matching favorite in S1/);
      assert.deepEqual(await writes(favoritePage), []);
    } finally { await favoritePage.close(); }

    const recoveryPage = await browser.newPage({ viewport: { width: 390, height: 960 } });
    try {
      await load(recoveryPage, 'dark'); await browse(recoveryPage);
      await library(recoveryPage, 'routine-0').click();
      await recoveryPage.waitForFunction(() => document.querySelector('[data-routine-focus="source_uri"]')?.value.startsWith('spotify:album:'));
      await recoveryPage.evaluate(() => { fixture.mode = 'lost_put'; });
      await routine(recoveryPage, 'review').click(); await routine(recoveryPage, 'confirm').click();
      await recoveryPage.waitForFunction(() => document.querySelector('.favorite-routine-content [role="status"]').textContent.includes('needs inspection'));
      const saved = await recoveryPage.evaluate(() => JSON.stringify(fixture.journal));
      await nav(recoveryPage, 'music').click(); await library(recoveryPage, 'routine-1').click();
      await recoveryPage.getByRole('region', { name: 'Music from library', exact: true }).waitFor();
      assert.equal(await routine(recoveryPage, 'library-use').isDisabled(), true);
      assert.equal(await routine(recoveryPage, 'inspect').isEnabled(), true);
      assert.equal(await recoveryPage.evaluate(() => JSON.stringify(fixture.journal)), saved);
      assert.equal((await writes(recoveryPage)).length, 1);
      await routine(recoveryPage, 'inspect').click();
      await recoveryPage.waitForFunction(() => document.querySelector('.favorite-routine-content [role="status"]').textContent.includes('routine not submitted'));
      assert.equal(await routine(recoveryPage, 'library-use').isDisabled(), true);
      assert.equal((await writes(recoveryPage)).length, 1);
    } finally { await recoveryPage.close(); }

    const latePage = await browser.newPage({ viewport: { width: 390, height: 960 } });
    try {
      await load(latePage, 'dark'); await browse(latePage, false);
      await latePage.evaluate(() => { fixture.holdZones = true; });
      await library(latePage, 'routine-0').click();
      await latePage.waitForFunction(() => typeof fixture.release === 'function');
      await nav(latePage, 'music').click(); await library(latePage, 'routine-1').click();
      await latePage.evaluate(() => { fixture.holdZones = false; fixture.release(); });
      await latePage.waitForFunction(() => !document.querySelector('[data-routine-focus="library-use"]').disabled);
      assert.equal(await routine(latePage, 'source_kind').inputValue(), 'favorite');
      assert.equal(await routine(latePage, 'label').inputValue(), '');
      await routine(latePage, 'library-use').click();
      await latePage.waitForFunction(() => document.querySelector('[data-routine-focus="source_uri"]')?.value === 'spotify:album:1123456789ABCDEFabcdef');
      assert.equal(await routine(latePage, 'label').inputValue(), 'Morning collection 2');
      assert.equal(await routine(latePage, 'group').inputValue(), '');
      assert.deepEqual(await writes(latePage), []);
    } finally { await latePage.close(); }
  } finally { await browser.close(); }
});
