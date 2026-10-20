// Optional Chromium component checks: actual studio assets, synthetic native
// controls and observations. No daemon, speaker, or external service is used.
'use strict';
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { chromium } = require('playwright');
const root = path.join(__dirname, '..');
const assets = path.join(root, 'crates/fsonos-api/src/remote');
const read = name => fs.readFileSync(path.join(assets, name), 'utf8');
const screenshots = process.env.FSONOS_STUDIO_SCREENSHOTS;

async function load(page) {
  await page.setContent(read('index.html').replace(/<script[^>]*>[\s\S]*?<\/script>|<link[^>]*>/g, ''));
  for (const name of ['remote.css', 'studio.css', 'studio-insights.css']) await page.addStyleTag({ content: read(name) });
  await page.addScriptTag({ content: fs.readFileSync(path.join(__dirname, 'studio-fixture.js'), 'utf8') });
  for (const name of ['studio.js', 'studio-insights.js']) await page.addScriptTag({ content: read(name) });
  await page.waitForFunction(() => !document.getElementById('studio-player-play').disabled);
}
async function checkBounds(page, label) {
  assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true, label);
  const box = await page.locator('#studio-player').boundingBox();
  const viewport = page.viewportSize();
  assert.ok(box.x >= 0 && box.x + box.width <= viewport.width + 1, label + ' player width');
  assert.ok(box.y >= 0 && box.y + box.height <= viewport.height, label + ' player height');
  if (viewport.width <= 700) {
    const nav = await page.locator('.studio-sidebar').boundingBox();
    assert.ok(box.y + box.height <= nav.y, label + ' player clears bottom navigation');
  }
}

(async () => {
  if (screenshots) fs.mkdirSync(screenshots, { recursive: true });
  const browser = await chromium.launch({ executablePath: process.env.FSONOS_STUDIO_CHROMIUM,
    headless: true, args: ['--no-sandbox', '--disable-dev-shm-usage', '--disable-gpu', '--no-zygote'] });
  const results = [];
  try {
    for (const width of [320, 390, 700, 768, 960, 1440]) {
      const page = await browser.newPage({ viewport: { width, height: width <= 700 ? 844 : 1080 }, reducedMotion: 'reduce' });
      const errors = [];
      page.on('pageerror', error => errors.push(error.message));
      try {
        await load(page);
        assert.equal(await page.evaluate(() => fixture.commands.length), 0);
        await checkBounds(page, `${width} initial`);
        if (width <= 700) {
          // Larger navigation labels must not push the fixed player over tabs.
          const enlarged = await page.addStyleTag({ content: '#studio-nav button { font-size: 14px; line-height: 1.5; }' });
          await page.waitForFunction(() => parseInt(document.body.style.getPropertyValue('--studio-navigation-height'), 10)
            === Math.ceil(document.querySelector('.studio-sidebar').getBoundingClientRect().height));
          await checkBounds(page, `${width} enlarged navigation`);
          await enlarged.evaluate(node => node.remove());
          await page.waitForFunction(() => parseInt(document.body.style.getPropertyValue('--studio-navigation-height'), 10)
            === Math.ceil(document.querySelector('.studio-sidebar').getBoundingClientRect().height));
        }
        // Switching workspaces retains a native form's actual DOM and draft.
        await page.locator('[data-workspace="music"]').click();
        await page.locator('#studio-music details').first().evaluate(node => { node.open = true; });
        const draft = page.locator('#studio-music input').first();
        await draft.fill('warm acoustic');
        for (const workspace of ['house', 'routines', 'activity', 'listen', 'music']) {
          await page.locator(`[data-workspace="${workspace}"]`).click();
          assert.equal(await page.locator('#studio-player').isVisible(), true);
          await checkBounds(page, `${width} ${workspace}`);
        }
        assert.equal(await draft.inputValue(), 'warm acoustic');
        await page.locator('#studio-player-play').click();
        assert.deepEqual(await page.evaluate(() => fixture.commands.at(-1)), { kind: 'play', zone: 'Living room@S2' });
        const before = await page.evaluate(() => fixture.commands.length);
        await page.locator('#studio-player-room').click();
        assert.equal(await page.locator('#studio-room-search').evaluate(node => node === document.activeElement), true);
        await page.locator('#studio-room-search').fill('S1');
        assert.equal(await page.locator('#studio-room-choices button:visible').count(), 1);
        await page.keyboard.press('ArrowDown');
        await page.keyboard.press('Enter');
        await page.waitForFunction(() => document.getElementById('studio-player-target').textContent.endsWith('S1'));
        assert.equal(await page.locator('#studio-music').isVisible(), true, 'room selection preserves workspace');
        assert.equal(await page.evaluate(() => fixture.commands.length), before, 'selection cannot move or start music');
        assert.equal(await page.locator('#studio-player-room').evaluate(node => node === document.activeElement), true);
        await page.locator('#studio-player-play').click();
        assert.deepEqual(await page.evaluate(() => fixture.commands.at(-1)), { kind: 'play', zone: 'Living room@S1' });
        // Empty search, literal metadata, and keyboard navigation use real DOM.
        await page.locator('#studio-player-room').click();
        await page.locator('#studio-room-search').fill('no such room');
        assert.equal(await page.locator('#studio-room-choices button:visible').count(), 0);
        assert.match(await page.locator('#studio-room-result').innerText(), /No rooms match/);
        await page.keyboard.press('Escape');
        await page.waitForFunction(() => !document.getElementById('studio-rooms').open);
        // Block a stale native observation immediately, then restore it once.
        await page.evaluate(() => {
          const parts = fixture.house.cards.get('["S1","Living room"]');
          window.savedState = parts.lastState;
          parts.lastState = null;
          for (const panel of parts.panels) panel.stale?.();
        });
        await page.waitForFunction(() => document.getElementById('studio-player-play').disabled);
        assert.match(await page.locator('#studio-player-target').textContent(), /Updating/);
        await page.evaluate(() => {
          const parts = fixture.house.cards.get('["S1","Living room"]');
          parts.lastState = window.savedState;
          for (const panel of parts.panels) panel.observe?.(parts.lastState);
        });
        await page.waitForFunction(() => !document.getElementById('studio-player-play').disabled);
        if (width > 700) {
          const count = await page.evaluate(() => fixture.commands.length);
          await page.locator('#studio-player-queue').click();
          await page.waitForFunction(() => document.querySelector('article.zone:not([hidden]) details.queue').open);
          assert.equal(await page.locator('#studio-listen').isVisible(), true);
          assert.equal(await page.evaluate(() => fixture.commands.length), count, 'opening queue is navigation');
          await page.locator('#studio-player-volume').click();
          await page.waitForFunction(() => document.activeElement?.type === 'range');
        }
        for (const theme of ['light', 'dark']) {
          await page.locator('#studio-theme').click();
          assert.equal(await page.locator('body').getAttribute('data-theme'), theme);
          await checkBounds(page, `${width} ${theme}`);
          if (screenshots && [390, 1440].includes(width)) {
            await page.locator('[data-workspace="listen"]').click();
            await page.locator('#studio-room-rail button').first().click();
            await page.evaluate(() => window.scrollTo(0, 0));
            await page.screenshot({ path: path.join(screenshots, `studio-player-${width}-${theme}.png`), fullPage: true });
          }
        }
        // Native replacement and an empty house cannot leave the old dock active.
        const count = await page.evaluate(() => fixture.commands.length);
        await page.evaluate(() => fixture.makeHouse([]));
        await page.waitForFunction(() => document.getElementById('studio-player').hidden);
        assert.equal(await page.evaluate(() => fixture.commands.length), count);
        assert.deepEqual(errors, []);
        results.push({ width, themes: 2, workspaces: 5, nativeDispatch: true, staleGuard: true, overflow: false });
      } finally { await page.close(); }
    }
    console.log(JSON.stringify({ component: 'studio player', results }, null, 2));
  } finally { await browser.close(); }
})().catch(error => { console.error(error); process.exitCode = 1; });
