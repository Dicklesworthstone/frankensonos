'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const R = require('../crates/fsonos-api/src/remote/favorite-routines.js');
const P = require('../crates/fsonos-api/src/remote/favorite-routines-panel.js');
// Supply the browser API when the Node runner does not expose it globally.
if (!globalThis.crypto) globalThis.crypto = require('node:crypto').webcrypto;
function storage() {
  const map = new Map();
  return { getItem: (k) => map.get(k) ?? null, setItem: (k, v) => map.set(k, v), removeItem: (k) => map.delete(k) };
}

test('session journal preserves the submission boundaries through a new adapter instance', () => {
  const s = storage(), j = P.sessionJournal(() => s);
  assert.equal(j.load(), null);
  const attempt = { version: 1, sceneSent: true, scheduleSent: true, plan: { name: 'test' } };
  j.save(attempt); assert.deepEqual(P.sessionJournal(() => s).load(), attempt);
  j.clear(); assert.equal(j.load(), null);
});

test('session journal does not delete unrelated tab data', () => {
  const s = storage(), j = P.sessionJournal(() => s);
  s.setItem('other', 'unchanged'); j.save({ version: 1 }); j.clear();
  assert.equal(s.getItem('other'), 'unchanged');
});

test('corrupt and oversized session records are refused without clearing them', () => {
  const s = storage(), j = P.sessionJournal(() => s);
  for (const raw of ['{', 'a'.repeat(P.MAX_JOURNAL + 1)]) {
    s.setItem(P.JOURNAL_KEY, raw); assert.throws(() => j.load()); assert.equal(s.getItem(P.JOURNAL_KEY), raw);
  }
  assert.throws(() => j.save({ value: 'a'.repeat(P.MAX_JOURNAL) }));
});

test('storage that drops a journal write cannot acknowledge persistence', () => {
  const s = storage(); s.setItem = () => {};
  assert.throws(() => P.sessionJournal(() => s).save({ version: 1 }), /could not keep/);
});

test('unavailable browser storage blocks creation visibly rather than falling back to memory', async () => {
  let calls = 0;
  const journal = P.sessionJournal(() => { throw new Error('storage disabled'); });
  const c = R.model({ journal, call: async () => { ++calls; return []; } });
  await c.refresh(); c.review({}); await c.confirm({});
  assert.equal(c.state.phase, 'blocked'); assert.match(c.state.message, /storage disabled/); assert.equal(calls, 0);
});

test('identity generation does not depend on secure-context randomUUID', async () => {
  const c = R.model({ call: async (name) => name === 'routineZones'
    ? [{ household: 'S2', coordinator_room: 'Kitchen', members: ['Kitchen'] }]
    : [{ id: 'FV:2/1', title: 'Station', kind: 'stream' }] });
  await c.refresh(); await c.choose('Kitchen@S2');
  c.review({ label: 'Morning', cadence: 'daily', time: '07:00', favorite: 'FV:2/1' });
  assert.match(c.state.pending.name, / [a-f0-9]{32}$/);
});

test('remote serves both routine scripts and styles and checks them for embedded secrets', () => {
  const source = fs.readFileSync(path.join(__dirname, '../crates/fsonos-api/src/http/remote.rs'), 'utf8');
  for (const [symbol, file] of [['ROUTINES_SCRIPT', 'favorite-routines.js'],
    ['ROUTINES_PANEL_SCRIPT', 'favorite-routines-panel.js'], ['ROUTINES_STYLE', 'favorite-routines.css']]) {
    assert.ok(source.includes(`const ${symbol}: &str = include_str!("../remote/${file}");`));
    assert.ok(source.includes(`{${symbol}}`));
    assert.ok(source.includes(`("${file}", ${symbol})`));
    const text = fs.readFileSync(path.join(__dirname, '../crates/fsonos-api/src/remote', file), 'utf8');
    for (const secret of ['token', 'authorization', 'x-fsonos', 'tailscale-user', 'password', 'cookie', 'ts.net', 'http://', 'https://']) {
      assert.ok(!text.toLowerCase().includes(secret), `${file}: ${secret}`);
    }
    assert.ok(!/innerHTML|outerHTML|insertAdjacentHTML|eval\(/.test(text));
  }
  assert.match(source, /\{GROUP_VOLUME_SCRIPT\}\\n\{ROUTINES_SCRIPT\}\\n\{ROUTINES_PANEL_SCRIPT\}/);
});

test('the composer permits only scene storage and schedule storage writes', () => {
  assert.deepEqual(Object.values(R.routes).filter(([m]) => m !== 'GET'), [
    ['PUT', '/scenes/{name}'], ['POST', '/schedules'],
  ]);
});
