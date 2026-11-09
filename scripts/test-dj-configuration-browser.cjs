// Optional rendered checks of the production shared editor on both surfaces.
// Only synthetic configuration requests are allowed; no speakers are involved.
'use strict';
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { test } = require('node:test');
const assets = path.join(__dirname, '../crates/fsonos-api/src/remote');
const read = name => fs.readFileSync(path.join(assets, name), 'utf8');
const optional = { skip: process.env.FSONOS_CONFIG_CHROMIUM ? false : 'Set FSONOS_CONFIG_CHROMIUM to run the rendered Playwright checks.' };
const markup = name => read(name).replace(/<script\b[^>]*>[\s\S]*?<\/script>/g, '').replace(/<link\b[^>]*>/g, '');
const remote = read('remote.js');
const element = remote.slice(remote.indexOf('function el('), remote.indexOf('\nlet toastTimer'));
const fixture = `
window.fixture = { calls: [], serial: 1, loseReply: false };
fixture.builtins = { calm: DjConfiguration.constraintsOf({energy_bias:-1}), focus: DjConfiguration.constraintsOf({include_keywords:['instrumental']}) };
fixture.defaults = [{days:[...DjConfiguration.days],from:'00:00',to:'24:00',mood:'calm'}];
fixture.config = {moods:{existing:DjConfiguration.constraintsOf({include_keywords:[' piano '],song_keywords:['gospel, soul'],expires_at:2000000001})},programs:null};
fixture.snapshot = () => ({revision:String(fixture.serial).padStart(64,'0'),config:structuredClone(fixture.config),
  builtin_moods:fixture.builtins,builtin_programs:fixture.defaults,
  moods:{...fixture.builtins,...fixture.config.moods},programs:fixture.config.programs??fixture.defaults});
window.configRequest = async (url, body) => {
  if(url!=='/dj/configuration')throw new Error('Unexpected request: '+url);
  fixture.calls.push({url,body:body===undefined?null:structuredClone(body)});
  if(body===undefined)return structuredClone(fixture.snapshot());
  if(body.expected_revision!==String(fixture.serial).padStart(64,'0'))throw Object.assign(new Error('Changed'),{code:'CONFIG_CHANGED'});
  fixture.config=structuredClone(body.config);fixture.serial++;
  if(fixture.loseReply)throw new Error('Lost acknowledgement');
  return {done:'Fixture configuration saved.',changed:true,configuration:structuredClone(fixture.snapshot())};
};`;

async function setup(page, surface) {
  const errors = []; page.on('pageerror', error => errors.push(error.message));
  await page.setContent(markup(surface === 'phone' ? 'index.html' : 'dashboard.html'));
  for (const file of [surface === 'phone' ? 'remote.css' : 'dashboard.css', 'dj-steering.css', 'dj-configuration.css']) {
    await page.addStyleTag({ content: read(file) });
  }
  await page.addScriptTag({ content: read('dj-direction.js') + '\n' + read('dj-configuration.js') + '\n' + fixture });
  if (surface === 'phone') {
    await page.addScriptTag({ content: `const ROUTES={}; const catalogs={refresh(){}};\n${element}\nfunction call(name,{body}={}){return configRequest(ROUTES[name][1],body);}` });
    await page.addScriptTag({ content: read('dj-configuration-panel.js') });
  } else {
    await page.addScriptTag({ content: read('dj-configuration-panel.js') });
    await page.evaluate(() => {
      fixture.panel = DjConfigurationPanel.panel({ call: configRequest });
      document.getElementById('open-preferences').parentElement.append(fixture.panel.node);
    });
  }
  assert.equal(await page.evaluate(() => fixture.calls.length), 0, 'configuration must be lazy');
  await page.getByRole('button', { name: 'Moods & daily programs', exact: true }).click();
  await page.locator('.dj-config-dialog[open]').waitFor();
  await page.waitForFunction(() => !document.querySelector('[id$="-new-name"]').disabled);
  assert.equal(await page.evaluate(() => fixture.calls.length), 1);
  return errors;
}

test('phone and dashboard dialogs preserve focus, full mood fields, ordered windows and CAS drafts', optional, async () => {
  const { chromium } = require('playwright');
  const browser = await chromium.launch({ executablePath: process.env.FSONOS_CONFIG_CHROMIUM, headless: true,
    args: ['--no-sandbox', '--disable-dev-shm-usage', '--disable-gpu', '--no-zygote'] });
  try {
    for (const surface of ['phone', 'dashboard']) for (const width of [390, 1280]) {
      const page = await browser.newPage({ viewport: { width, height: 960 }, reducedMotion: 'reduce' });
      const errors = await setup(page, surface);
      const dialog = page.locator('.dj-config-dialog[open]');
      await dialog.getByLabel('New mood name', { exact: true }).fill('Night Jazz');
      await dialog.getByRole('button', { name: 'Create mood', exact: true }).click();
      const artists = dialog.getByRole('textbox', { name: 'Only artists', exact: true });
      await artists.fill('Earth, Wind & Fire');
      assert.equal(await page.evaluate(() => document.activeElement.getAttribute('aria-label')), 'Only artists');
      await artists.press('End'); await artists.press('Enter'); await artists.pressSequentially('Miles Davis');
      assert.equal(await artists.inputValue(), 'Earth, Wind & Fire\nMiles Davis');
      await dialog.getByLabel('Song alternatives to classical periods (one keyword per line)', { exact: true }).fill('soul\njazz');
      await dialog.getByLabel('Absolute rule expiry (Unix seconds, optional)', { exact: true }).fill('2100000000');
      await dialog.getByLabel('Program behavior', { exact: true }).selectOption('custom');
      const row = dialog.locator('.dj-config-program').first();
      for (const day of ['mon', 'tue', 'wed', 'thu', 'sat', 'sun']) await row.getByLabel(`Program 1 starts on ${day}`, { exact: true }).uncheck();
      await row.getByLabel('From', { exact: true }).fill('21:00');
      await row.getByLabel('Until', { exact: true }).fill('06:00');
      await row.getByLabel('Mood', { exact: true }).selectOption('night jazz');
      await dialog.getByLabel('Day at the daemon', { exact: true }).selectOption('sat');
      await dialog.getByLabel('Time at the daemon', { exact: true }).fill('03:00');
      assert.match(await dialog.locator('.dj-config-program-result').innerText(), /SAT 03:00 → night jazz/);
      await dialog.getByRole('button', { name: 'Add time window', exact: true }).click();
      await dialog.getByRole('button', { name: 'Move program 2 up', exact: true }).click();
      assert.equal(await dialog.locator('.dj-config-program').nth(1).getByLabel('Mood', { exact: true }).inputValue(), 'night jazz');
      assert.equal(await dialog.evaluate(node => node.scrollWidth <= node.clientWidth + 1), true, 'dialog must fit its viewport');
      await dialog.getByRole('button', { name: 'Review changes', exact: true }).click();
      assert.equal(await page.evaluate(() => document.activeElement.className), 'dj-config-review');
      await dialog.getByRole('button', { name: 'Save for every DJ', exact: true }).click();
      await page.waitForFunction(() => document.querySelector('.dj-config-status').textContent === 'Fixture configuration saved.');
      const body = await page.evaluate(() => fixture.calls.find(call => call.body).body);
      assert.equal(body.expected_revision, '1'.padStart(64, '0'));
      assert.deepEqual(body.config.moods['night jazz'].include_artists, ['Earth, Wind & Fire', 'Miles Davis']);
      assert.deepEqual(body.config.moods['night jazz'].song_keywords, ['soul', 'jazz']);
      assert.equal(body.config.moods['night jazz'].expires_at, 2100000000);
      assert.deepEqual(body.config.moods.existing.include_keywords, [' piano ']);
      assert.deepEqual(body.config.moods.existing.song_keywords, ['gospel, soul']);
      assert.equal(body.config.moods.existing.expires_at, 2000000001);
      assert.deepEqual(body.config.programs[1], { days: ['fri'], from: '21:00', to: '06:00', mood: 'night jazz' });

      await dialog.getByLabel('New mood name', { exact: true }).fill('retained draft');
      await dialog.getByRole('button', { name: 'Create mood', exact: true }).click();
      await page.evaluate(() => { fixture.serial++; fixture.config.moods.external = DjConfiguration.constraintsOf({ strict: true }); });
      await dialog.getByRole('button', { name: 'Review changes', exact: true }).click();
      await dialog.getByRole('button', { name: 'Save for every DJ', exact: true }).click();
      await page.waitForFunction(() => document.querySelector('.dj-config-status').textContent.includes('nothing was overwritten'));
      assert.equal(await dialog.getByLabel('Mood definition', { exact: true }).inputValue(), 'retained draft');
      await dialog.getByRole('button', { name: 'Check saved settings', exact: true }).click();
      await page.waitForFunction(() => document.querySelector('.dj-config-status').textContent.includes('Compare both definitions'));
      assert.equal(await dialog.getByRole('button', { name: 'Review changes', exact: true }).isDisabled(), true);
      await dialog.getByRole('button', { name: 'Discard draft & reload', exact: true }).click();
      await page.waitForFunction(() => document.querySelector('.dj-config-status').textContent === 'Saved configuration loaded.');
      assert.equal(await dialog.getByLabel('Mood definition', { exact: true }).locator('option[value="external"]').count(), 1);
      assert.equal(await dialog.getByLabel('Mood definition', { exact: true }).locator('option[value="retained draft"]').count(), 0);

      await dialog.getByLabel('New mood name', { exact: true }).fill('saved without reply');
      await dialog.getByRole('button', { name: 'Create mood', exact: true }).click();
      await page.evaluate(() => { fixture.loseReply = true; });
      await dialog.getByRole('button', { name: 'Review changes', exact: true }).click();
      await dialog.getByRole('button', { name: 'Save for every DJ', exact: true }).click();
      await page.waitForFunction(() => document.querySelector('.dj-config-status').textContent.includes('outcome is uncertain'));
      await dialog.getByRole('button', { name: 'Check saved settings', exact: true }).click();
      await page.waitForFunction(() => document.querySelector('.dj-config-status').textContent.includes('matches your submitted draft'));
      assert.equal(await page.evaluate(() => fixture.calls.filter(call => call.body).length), 3, 'readback must never resend a write');
      if (process.env.FSONOS_CONFIG_SCREENSHOTS) {
        fs.mkdirSync(process.env.FSONOS_CONFIG_SCREENSHOTS, { recursive: true });
        await page.screenshot({ path: path.join(process.env.FSONOS_CONFIG_SCREENSHOTS, `configuration-${surface}-${width}.png`) });
      }
      assert.deepEqual(errors, []); await page.close();
    }
  } finally { await browser.close(); }
});

test('the actual dashboard bundle includes a read-only configuration dialog in design preview', optional, async () => {
  const { chromium } = require('playwright');
  const preview = fs.readFileSync(path.join(__dirname, 'dashboard-preview.py'), 'utf8');
  const scripts = JSON.parse(preview.match(/SCRIPTS = (\[[\s\S]*?\])/)[1]);
  const styles = JSON.parse(preview.match(/STYLES = (\[[\s\S]*?\])/)[1]);
  const browser = await chromium.launch({ executablePath: process.env.FSONOS_CONFIG_CHROMIUM, headless: true,
    args: ['--no-sandbox', '--disable-dev-shm-usage', '--disable-gpu', '--no-zygote'] });
  try {
    const page = await browser.newPage({ viewport: { width: 1280, height: 960 }, reducedMotion: 'reduce' });
    const errors = []; page.on('pageerror', error => errors.push(error.message));
    await page.setContent(markup('dashboard.html').replace('<body>', '<body data-preview="true">'));
    for (const style of styles) await page.addStyleTag({ content: read(style) });
    await page.addScriptTag({ content: scripts.map(read).join('\n') });
    await page.getByRole('button', { name: 'Moods & daily programs', exact: true }).click();
    await page.locator('.dj-config-dialog[open]').waitFor();
    await page.waitForFunction(() => document.querySelector('.dj-config-status').textContent === 'Saved configuration loaded.');
    assert.equal(await page.getByRole('button', { name: 'Create mood', exact: true }).isDisabled(), true);
    assert.match(await page.locator('.dj-config-effective pre').innerText(), /sunday-morning/);
    assert.deepEqual(errors, []);
  } finally { await browser.close(); }
});
