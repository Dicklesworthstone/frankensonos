'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const { once } = require('node:events');
const remote = require('../crates/fsonos-api/src/remote/library-sync.js');

const completed = (overrides = {}) => ({ at: 1790000000, tracks: 100, candidates: 80,
  classical: 12, retired: 3, ...overrides });
const status = (overrides = {}) => ({ done: 'Library read.', running: false, last: completed(), ...overrides });
const genres = (overrides = {}) => ({ pending: 12, completed: 7, running: false,
  retry_at: 1790000900, not_before: 1790003600, requires_refresh: false, ...overrides });
const intake = (overrides = {}) => ({ pending: true, phase: 'reading', pages: 512, tracks: 24500,
  verified_pages: 0, running: false, retry_at: 1790000900, not_before: 1790003600, ...overrides });
const followed = (overrides = {}) => ({ pending: true, phase: 'reading', pages: 20, artists: 1000,
  verified_pages: 0, limited: false, running: false, retry_at: 1790000900, not_before: 1790003600,
  requires_refresh: false, ...overrides });
const recent = (overrides = {}) => ({ running: false, checked_at: 1790000800,
  received: 50, accepted: 45, skipped: 2, more_available: true, missing_scope: false,
  requires_refresh: false, retry_at: 1790001700, not_before: null, error: null, ...overrides });
const deferred = () => { let resolve, reject; const promise = new Promise((yes, no) => { resolve = yes; reject = no; }); return { promise, resolve, reject }; };
const settle = () => new Promise((resolve) => setImmediate(resolve));

function harness(handler = () => status()) {
  const calls = [], timers = new Map(); let serial = 0;
  const model = remote.model({ call: async (name, options) => { calls.push([name, options]); return handler(name, options); },
    schedule: (fn, delay) => { const id = ++serial; timers.set(id, { fn, delay }); return id; },
    cancel: (id) => timers.delete(id) });
  return { model, calls, timers, fire() { assert.equal(timers.size, 1); const [id, t] = [...timers][0]; timers.delete(id); t.fn(); return t.delay; } };
}

test('opening only observes; neither construction nor observation starts a refresh', async () => {
  const h = harness(); assert.equal(h.calls.length, 0); assert.equal(await h.model.start(), false);
  await h.model.open(); assert.deepEqual(h.calls, [['librarySyncStatus', undefined]]);
  assert.equal(h.model.state.fresh, true); assert.equal(h.timers.size, 0);
});

test('old completed DTOs keep omitted notes and pending genres at their defaults', () => {
  const s = remote.statusOf(status()); assert.deepEqual(s.last.notes, []); assert.equal(s.last.genre_artists_pending, 0);
  assert.equal(s.genre_enrichment, null);
  assert.equal(s.library_intake, null);
  assert.equal(s.followed_artists, null);
  assert.equal(s.recent_listening, null);
  assert.equal(remote.statusOf({ done: 'Not read yet', running: false }).last, null);
  assert.ok(Object.isFrozen(s) && Object.isFrozen(s.last) && Object.isFrozen(s.last.notes));
});

test('recent listening is immutable bounded evidence independent of full-library freshness', () => {
  const raw = status({ recent_listening: recent() });
  const parsed = remote.statusOf(raw);
  raw.recent_listening.accepted = 1;
  assert.deepEqual(parsed.recent_listening, recent());
  assert.ok(Object.isFrozen(parsed.recent_listening));
  assert.equal(parsed.last.at, completed().at);
  assert.notEqual(parsed.last.at, parsed.recent_listening.checked_at);
  const pending = remote.statusOf(status({ last: null, recent_listening: recent({
    checked_at: null, received: 0, accepted: 0, skipped: 0, more_available: false, requires_refresh: true,
  }) }));
  assert.equal(pending.last, null);
  assert.equal(pending.recent_listening.checked_at, null);
});

test('malformed recent listening never enables a cache mutation', async () => {
  for (const value of [null, [], {}, recent({ received: 51 }), recent({ accepted: 49 }),
    recent({ skipped: -1 }), recent({ checked_at: null }), recent({ checked_at: 'now' }),
    recent({ checked_at: -1 }), recent({ retry_at: 'later' }), recent({ error: {} }),
    recent({ more_available: 1 }), recent({ missing_scope: true, requires_refresh: true }),
    recent({ running: true })].filter((v) => v !== null)) {
    const h = harness(() => status({ recent_listening: value }));
    assert.equal(await h.model.open(), false);
    assert.equal(await h.model.start(), false);
    assert.equal(h.calls.length, 1);
    h.model.dispose();
  }
  for (const other of [{ genre_enrichment: genres({ running: true }) },
    { library_intake: intake({ running: true }) }, { followed_artists: followed({ running: true }) }]) {
    assert.throws(() => remote.statusOf(status({ running: true, ...other, recent_listening: recent({ running: true }) })));
  }
});

test('an active recent-listening read is observed without issuing a full refresh or endless idle polling', async () => {
  let current = status({ running: true, recent_listening: recent({ running: true }) });
  const h = harness(() => current);
  await h.model.open();
  assert.equal(await h.model.start(), false);
  current = status({ recent_listening: recent() });
  assert.equal(h.fire(), remote.POLL_MS);
  await settle();
  assert.equal(h.model.state.status.last.at, completed().at);
  assert.equal(h.timers.size, 0);
  assert.ok(h.calls.every(([operation]) => operation === 'librarySyncStatus'));
  h.model.dispose();
});

test('saved intake retains its exact progress without inventing a full refresh', () => {
  const raw = status({ last: null, library_intake: intake({ phase: 'verifying', verified_pages: 100 }) });
  const parsed = remote.statusOf(raw); raw.library_intake.tracks = 1;
  assert.deepEqual(parsed.library_intake, intake({ phase: 'verifying', verified_pages: 100 }));
  assert.ok(Object.isFrozen(parsed.library_intake)); assert.equal(parsed.last, null); assert.equal(parsed.error, null);
  const ready = remote.statusOf(status({ library_intake: intake({ phase: 'ready', verified_pages: 512 }) }));
  assert.equal(ready.library_intake.pending, true); assert.equal(ready.last.at, completed().at);
});

test('malformed or contradictory saved intake never authorizes a refresh', async () => {
  for (const value of [[], {}, intake({ pending: 1 }), intake({ phase: 'complete' }), intake({ phase: null }),
    intake({ pages: -1 }), intake({ tracks: 2 ** 53 }), intake({ verified_pages: 513 }),
    intake({ verified_pages: 1.2 }), intake({ running: 'yes' }), intake({ running: true }),
    intake({ retry_at: 'later' }), intake({ not_before: 8640000000001 })]) {
    const raw = status({ library_intake: value });
    assert.throws(() => remote.statusOf(raw)); const h = harness(() => raw);
    assert.equal(await h.model.open(), false); assert.equal(await h.model.start(), false);
    assert.equal(h.calls.length, 1); h.model.dispose();
  }
  assert.throws(() => remote.statusOf(status({ running: true,
    library_intake: intake({ running: true }), genre_enrichment: genres({ running: true }) })));
});

test('a first bounded sync keeps observing through waits and reconstructed progress until publication', async () => {
  let answer = status({ running: true, last: null,
    library_intake: intake({ running: true, pages: 0, tracks: 0, retry_at: null, not_before: null }) });
  const h = harness(() => answer); await h.model.open();
  assert.equal(h.fire(), remote.POLL_MS); await settle();
  answer = status({ last: null, library_intake: intake() });
  h.fire(); await settle();
  assert.equal(h.model.state.status.error, null);
  assert.equal([...h.timers.values()][0].delay, remote.PENDING_POLL_MS);
  assert.ok(remote.PENDING_POLL_MS >= 30000);
  h.model.close(); assert.equal(h.timers.size, 0);
  answer = JSON.parse(JSON.stringify(answer));
  await h.model.open(); assert.equal(h.model.state.status.library_intake.pages, 512);
  assert.equal(h.fire(), remote.PENDING_POLL_MS); await settle();
  answer = status({ last: null, library_intake: intake({ phase: 'ready', verified_pages: 512 }) });
  h.fire(); await settle(); assert.equal(h.timers.size, 1);
  assert.equal(h.model.state.status.last, null);
  answer = status(); h.fire(); await settle();
  assert.equal(h.model.state.status.library_intake, null); assert.equal(h.timers.size, 0);
  assert.ok(h.calls.every(([name]) => name === 'librarySyncStatus')); h.model.dispose();
});

test('saved intake and genres share one observer and wait quietly while hidden', async () => {
  let answer = status({ library_intake: intake(), genre_enrichment: genres() });
  const h = harness(() => answer); await h.model.open(); assert.equal(h.timers.size, 1);
  h.model.setVisible(false); assert.equal(h.timers.size, 0);
  h.model.setVisible(true); await settle(); assert.equal(h.timers.size, 1);
  answer = status({ genre_enrichment: genres() }); h.fire(); await settle();
  assert.equal([...h.timers.values()][0].delay, remote.PENDING_POLL_MS);
  answer = status({ genre_enrichment: genres({ pending: 0, completed: 12, retry_at: null, not_before: null }) });
  h.fire(); await settle(); assert.equal(h.timers.size, 0);
  assert.ok(h.calls.every(([name]) => name === 'librarySyncStatus')); h.model.dispose();
});

test('real saved-library errors stay visible while retained work continues to be observed', async () => {
  const h = harness(() => status({ error: 'Spotify is unavailable', library_intake: intake() }));
  await h.model.open(); assert.equal(h.model.state.status.error, 'Spotify is unavailable');
  assert.equal(h.model.state.status.library_intake.pending, true);
  assert.equal(h.fire(), remote.PENDING_POLL_MS); await settle();
  assert.equal(h.calls.length, 2); assert.equal(h.timers.size, 1);
  assert.ok(h.calls.every(([name]) => name === 'librarySyncStatus')); h.model.dispose();
});

test('followed-artist progress survives parsing separately from full-refresh history', () => {
  const raw = status({ followed_artists: followed({ phase: 'verifying', verified_pages: 8 }) });
  const parsed = remote.statusOf(raw); raw.followed_artists.artists = 1;
  assert.deepEqual(parsed.followed_artists, followed({ phase: 'verifying', verified_pages: 8 }));
  assert.ok(Object.isFrozen(parsed.followed_artists)); assert.equal(parsed.last.at, completed().at);
  const ready = remote.statusOf(status({ followed_artists: followed({ phase: 'ready', verified_pages: 20 }) }));
  assert.equal(ready.followed_artists.pending, true); assert.equal(ready.last.at, completed().at);
});

test('malformed or contradictory followed-artist progress never authorizes a refresh', async () => {
  for (const value of [[], {}, followed({ pending: 1 }), followed({ phase: 'complete' }),
    followed({ pages: -1 }), followed({ artists: 2 ** 53 }), followed({ verified_pages: 21 }),
    followed({ verified_pages: 1.5 }), followed({ limited: 'no' }), followed({ limited: true }),
    followed({ requires_refresh: 'yes' }), followed({ running: 'yes' }), followed({ running: true }),
    followed({ retry_at: 'later' }), followed({ not_before: 8640000000001 })]) {
    const raw = status({ followed_artists: value });
    assert.throws(() => remote.statusOf(raw)); const h = harness(() => raw);
    assert.equal(await h.model.open(), false); assert.equal(await h.model.start(), false);
    assert.equal(h.calls.length, 1); h.model.dispose();
  }
  for (const active of [{ genre_enrichment: genres({ running: true }) },
    { library_intake: intake({ running: true }) }]) {
    assert.throws(() => remote.statusOf(status({ running: true,
      followed_artists: followed({ running: true }), ...active })));
  }
});

test('bounded artist intake keeps one read observer through verification and publication', async () => {
  let answer = status({ followed_artists: followed() }); const h = harness(() => answer);
  await h.model.open(); assert.equal(h.fire(), remote.PENDING_POLL_MS); await settle();
  answer = status({ running: true, followed_artists: followed({ phase: 'verifying', verified_pages: 8, running: true }) });
  h.fire(); await settle(); assert.equal(h.fire(), remote.POLL_MS); await settle();
  answer = status({ followed_artists: followed({ phase: 'ready', verified_pages: 20 }) });
  h.fire(); await settle(); assert.equal([...h.timers.values()][0].delay, remote.PENDING_POLL_MS);
  assert.equal(h.model.state.status.last.at, completed().at);
  h.model.close(); assert.equal(h.timers.size, 0);
  answer = JSON.parse(JSON.stringify(answer)); await h.model.open();
  assert.equal(h.model.state.status.followed_artists.phase, 'ready'); assert.equal(h.timers.size, 1);
  answer = status({ last: completed({ at: 1790007200 }),
    followed_artists: followed({ pending: false, phase: 'ready', verified_pages: 20, retry_at: null, not_before: null }) });
  h.fire(); await settle(); assert.equal(h.timers.size, 0);
  assert.equal(h.model.state.status.last.at, 1790007200);
  assert.ok(h.calls.every(([name]) => name === 'librarySyncStatus')); h.model.dispose();
});

test('artist intake shares one poller with saved music and genres, and pauses while hidden', async () => {
  let answer = status({ library_intake: intake(), genre_enrichment: genres(), followed_artists: followed() });
  const h = harness(() => answer); await h.model.open(); assert.equal(h.timers.size, 1);
  h.model.setVisible(false); assert.equal(h.timers.size, 0);
  h.model.setVisible(true); await settle(); assert.equal(h.timers.size, 1);
  answer = status({ followed_artists: followed() }); h.fire(); await settle(); assert.equal(h.timers.size, 1);
  answer = status(); h.fire(); await settle(); assert.equal(h.timers.size, 0);
  assert.ok(h.calls.every(([name]) => name === 'librarySyncStatus')); h.model.dispose();
});

test('artist cache limits stop automatic observation while required refreshes remain observable', async () => {
  let answer = status({ followed_artists: followed({ pending: false, limited: true }) });
  const h = harness(() => answer); await h.model.open(); assert.equal(h.timers.size, 0);
  answer = status({ followed_artists: followed({ pending: false, requires_refresh: true }) });
  await h.model.refresh(); assert.equal([...h.timers.values()][0].delay, remote.PENDING_POLL_MS);
  assert.ok(h.calls.every(([name]) => name === 'librarySyncStatus')); h.model.dispose();
});

test('a real artist-read failure preserves observed progress without retrying the write', async () => {
  const h = harness(() => status({ error: 'Spotify is unavailable', followed_artists: followed() }));
  await h.model.open(); assert.equal(h.model.state.status.error, 'Spotify is unavailable');
  assert.equal(h.model.state.status.followed_artists.artists, 1000);
  assert.equal(h.fire(), remote.PENDING_POLL_MS); await settle(); assert.equal(h.timers.size, 1);
  assert.ok(h.calls.every(([name]) => name === 'librarySyncStatus')); h.model.dispose();
});

test('current genre progress and cooldowns survive parsing independently of the full refresh', () => {
  const raw = status({ last: completed({ genre_artists_pending: 99 }), genre_enrichment: genres() });
  const parsed = remote.statusOf(raw); raw.genre_enrichment.pending = 100;
  assert.deepEqual(parsed.genre_enrichment, genres()); assert.ok(Object.isFrozen(parsed.genre_enrichment));
  assert.equal(parsed.last.genre_artists_pending, 99); assert.equal(parsed.last.at, completed().at);
  const finished = remote.statusOf(status({ genre_enrichment: genres({ pending: 0, completed: 12, retry_at: null, not_before: null }) }));
  assert.equal(finished.genre_enrichment.pending, 0); assert.equal(finished.genre_enrichment.completed, 12);
});

test('malformed or contradictory genre progress cannot be mistaken for a complete update', async () => {
  for (const value of [[], {}, genres({ pending: -1 }), genres({ pending: 2 ** 53 }),
    genres({ completed: 1.5 }), genres({ running: 'yes' }), genres({ requires_refresh: 1 }),
    genres({ retry_at: 'later' }), genres({ not_before: 8640000000001 }), genres({ running: true })]) {
    const raw = status({ genre_enrichment: value });
    assert.throws(() => remote.statusOf(raw)); const h = harness(() => raw);
    assert.equal(await h.model.open(), false); assert.equal(await h.model.start(), false);
    assert.equal(h.calls.length, 1); h.model.dispose();
  }
});

test('pending genre passes use modest read polling until actual completion', async () => {
  let answer = status({ genre_enrichment: genres() }); const h = harness(() => answer);
  await h.model.open(); assert.equal(h.model.state.status.running, false);
  assert.equal(h.fire(), remote.GENRE_POLL_MS); await settle(); assert.ok(remote.GENRE_POLL_MS >= 30000);
  answer = status({ running: true, genre_enrichment: genres({ running: true }) });
  h.fire(); await settle(); assert.equal(h.fire(), remote.POLL_MS); await settle();
  answer = status({ last: completed({ genre_artists_pending: 99 }),
    genre_enrichment: genres({ pending: 0, completed: 12, retry_at: null, not_before: null }) });
  h.fire(); await settle(); assert.equal(h.timers.size, 0);
  assert.equal(h.model.state.status.last.at, completed().at);
  assert.ok(h.calls.every(([name]) => name === 'librarySyncStatus')); h.model.dispose();
});

test('required full refreshes remain observable without starting work and hidden tabs stop waiting polls', async () => {
  const h = harness(() => status({ genre_enrichment: genres({ pending: 0, requires_refresh: true }) }));
  await h.model.open(); assert.equal([...h.timers.values()][0].delay, remote.GENRE_POLL_MS);
  h.model.setVisible(false); assert.equal(h.timers.size, 0);
  h.model.setVisible(true); await settle(); assert.equal(h.calls.length, 2);
  assert.equal([...h.timers.values()][0].delay, remote.GENRE_POLL_MS);
  h.model.close(); assert.equal(h.timers.size, 0);
  assert.ok(h.calls.every(([name]) => name === 'librarySyncStatus')); h.model.dispose();
});

test('partial results, previous completion and latest failure stay distinct', () => {
  const raw = status({ running: true, error: 'The last attempt failed',
    last: completed({ genre_artists_pending: 4, notes: ['Missing taste scope', '<script>unread album</script>'] }) });
  const s = remote.statusOf(raw); raw.last.notes.push('later');
  assert.equal(s.running, true); assert.equal(s.last.genre_artists_pending, 4);
  assert.equal(s.last.notes.length, 2); assert.equal(s.error, 'The last attempt failed');
  assert.equal(s.last.notes[1], '<script>unread album</script>');
});

test('unreadable DTOs never authorize a refresh', async () => {
  for (const value of [null, [], {}, { running: false }, status({ running: 'false' }), status({ error: {} }),
    status({ last: [] }), status({ last: completed({ tracks: -1 }) }),
    status({ last: completed({ candidates: 101 }) }), status({ last: completed({ classical: 81 }) }),
    status({ last: completed({ tracks: 2 ** 53 }) }), status({ last: completed({ at: 8640000000001 }) }),
    status({ last: completed({ notes: 'not a list' }) }), status({ last: completed({ notes: [5] }) }),
    status({ last: completed({ genre_artists_pending: 1.2 }) })]) {
    assert.throws(() => remote.statusOf(value)); const h = harness(() => value);
    assert.equal(await h.model.open(), false); assert.equal(await h.model.start(), false);
    assert.equal(h.calls.length, 1); h.model.dispose();
  }
});

test('running jobs are observed until completion without an invented progress percentage', async () => {
  let running = true; const h = harness(() => status({ running })); await h.model.open();
  assert.equal(await h.model.start(), false); assert.equal(h.fire(), remote.POLL_MS); await settle();
  assert.equal(h.timers.size, 1); running = false; h.fire(); await settle();
  assert.equal(h.model.state.status.running, false); assert.equal(h.timers.size, 0);
  assert.ok(h.calls.every(([name]) => name === 'librarySyncStatus'));
});

test('the explicit start sends exactly one empty JSON body', async () => {
  const h = harness((name) => status({ running: name === 'librarySyncStart' })); await h.model.open();
  assert.equal(await h.model.start(), true);
  assert.deepEqual(h.calls[1], ['librarySyncStart', { body: {} }]);
  assert.equal(await h.model.start(), false); assert.equal(h.calls.length, 2);
});

test('double clicks and status requests cannot duplicate an in-flight write', async () => {
  const wait = deferred(); const h = harness((name) => name === 'librarySyncStart' ? wait.promise : status());
  await h.model.open(); const first = h.model.start();
  assert.equal(await h.model.start(), false); assert.equal(await h.model.refresh(), false);
  assert.equal(h.calls.length, 2); wait.resolve(status({ running: true }));
  assert.equal(await first, true); await settle();
  assert.equal(h.calls.filter(([name]) => name === 'librarySyncStart').length, 1);
});

test('a lost write reply is followed by status reads, not an automatic retry', async () => {
  const h = harness((name) => { if (name === 'librarySyncStart') throw new Error('Lost reply'); return status(); });
  await h.model.open(); assert.equal(await h.model.start(), false); await settle();
  assert.deepEqual(h.calls.map(([name]) => name), ['librarySyncStatus', 'librarySyncStart', 'librarySyncStatus']);
  assert.equal(h.model.state.uncertain, true); assert.match(h.model.state.message, /nothing was retried/);
  assert.equal(h.model.state.fresh, true); assert.equal(h.timers.size, 0);
});

test('a malformed write reply is uncertain, never mistaken for completion', async () => {
  const h = harness((name) => name === 'librarySyncStart' ? {} : status({ last: null }));
  await h.model.open(); assert.equal(await h.model.start(), false); await settle();
  assert.equal(h.model.state.uncertain, true); assert.equal(h.model.state.status.last, null);
  assert.equal(h.calls.filter(([name]) => name === 'librarySyncStart').length, 1);
});

test('failed observations retain only explicitly stale results and use capped backoff', async () => {
  let fail = false; const h = harness(() => { if (fail) throw new Error('Offline'); return status({ running: true }); });
  await h.model.open(); const previous = h.model.state.status; fail = true;
  await h.model.refresh(); assert.equal(h.model.state.status, previous); assert.equal(h.model.state.fresh, false);
  assert.equal(await h.model.start(), false); const delays = [];
  for (let i = 0; i < 7; i++) { delays.push(h.fire()); await settle(); }
  assert.deepEqual(delays, [6000, 12000, 24000, 48000, 60000, 60000, 60000]);
  fail = false; await h.model.refresh(); assert.equal([...h.timers.values()][0].delay, remote.POLL_MS);
  h.model.dispose();
});

test('policy and unavailable-engine hints remain actionable', async () => {
  const h = harness(() => { throw Object.assign(new Error('No Spotify app configured'), { code: 'NOT_IMPLEMENTED', hint: 'Configure the daemon client ID.' }); });
  await h.model.open(); assert.match(h.model.state.message, /Configure the daemon client ID/);
  assert.equal(await h.model.start(), false); h.model.dispose();
});

test('closing stops observation without pretending to stop the daemon job', async () => {
  const h = harness(() => status({ running: true })); await h.model.open(); h.model.close();
  assert.equal(h.timers.size, 0); assert.equal(h.model.state.status.running, true);
  assert.equal(await h.model.refresh(), false); assert.equal(await h.model.start(), false);
  await h.model.open(); assert.equal(h.calls.length, 2); h.model.dispose();
});

test('a late read cannot reopen a closed panel or publish fresh stale state', async () => {
  const wait = deferred(); const h = harness(() => wait.promise); const opening = h.model.open();
  h.model.close(); wait.resolve(status()); assert.equal(await opening, false);
  assert.equal(h.model.state.open, false); assert.equal(h.model.state.status, null); assert.equal(h.timers.size, 0);
});

test('close/open bursts coalesce to one pending read, with one fresh read afterwards', async () => {
  const wait = deferred(); let first = true;
  const h = harness(() => { if (first) { first = false; return wait.promise; } return status({ done: 'Fresh' }); });
  const opening = h.model.open();
  for (let i = 0; i < 8; i++) { h.model.close(); await h.model.open(); }
  assert.equal(h.calls.length, 1); wait.resolve(status({ done: 'Obsolete' }));
  assert.equal(await opening, false); await settle();
  assert.equal(h.calls.length, 2); assert.equal(h.model.state.status.done, 'Fresh');
});

test('background tabs stop polling and returning performs only a status read', async () => {
  const h = harness(() => status({ running: true })); await h.model.open();
  h.model.setVisible(false); assert.equal(h.timers.size, 0); assert.equal(await h.model.start(), false);
  h.model.setVisible(true); await settle(); assert.equal(h.calls.length, 2);
  assert.ok(h.calls.every(([name]) => name === 'librarySyncStatus')); h.model.dispose();
});

test('initially hidden tabs do not start network work', async () => {
  const h = harness(); h.model.setVisible(false); await h.model.open(); assert.equal(h.calls.length, 0);
  h.model.setVisible(true); await settle(); assert.equal(h.calls.length, 1);
});

test('late writes after closing do not schedule reads or another write', async () => {
  const wait = deferred(); const h = harness((name) => name === 'librarySyncStart' ? wait.promise : status());
  await h.model.open(); const starting = h.model.start(); h.model.close();
  wait.resolve(status({ running: true })); await starting; await settle();
  assert.equal(h.calls.length, 2); assert.equal(h.timers.size, 0); assert.equal(h.model.state.fresh, false);
});

test('reopening during a write waits, then re-observes without dispatching again', async () => {
  const wait = deferred(); const h = harness((name) => name === 'librarySyncStart' ? wait.promise : status());
  await h.model.open(); const starting = h.model.start(); h.model.close(); await h.model.open();
  assert.equal(h.calls.length, 2); wait.resolve(status({ running: true })); await starting; await settle();
  assert.equal(h.calls.length, 3); assert.equal(h.calls[2][0], 'librarySyncStatus');
});

test('disposal invalidates all late reads and timers', async () => {
  const wait = deferred(); const h = harness(() => wait.promise); const reading = h.model.open(); h.model.dispose();
  wait.resolve(status({ running: true })); await reading; await settle();
  assert.equal(await h.model.open(), false); assert.equal(h.calls.length, 1); assert.equal(h.timers.size, 0);
});

test('a failed view callback cannot turn an acknowledged write into a retry', async () => {
  const calls = []; const m = remote.model({ call: async (name) => { calls.push(name); return status(); }, changed: () => { throw new Error('View failure'); } });
  await m.open(); assert.equal(await m.start(), true); assert.deepEqual(calls, ['librarySyncStatus', 'librarySyncStart']); m.dispose();
});

test('the registered routes dispatch through actual loopback HTTP with JSON', async (t) => {
  const requests = []; const server = http.createServer(async (req, res) => {
    let body = ''; for await (const bytes of req) body += bytes;
    requests.push([req.method, req.url, req.headers['content-type'], body]);
    res.writeHead(200, { 'Content-Type': 'application/json' }); res.end(JSON.stringify(status()));
  });
  server.listen(0, '127.0.0.1'); await once(server, 'listening');
  t.after(() => { server.closeAllConnections(); server.close(); });
  const m = remote.model({ call: async (name, opts) => {
    const [method, path] = remote.routes[name];
    const reply = await fetch(`http://127.0.0.1:${server.address().port}${path}`, { method,
      ...(method === 'POST' ? { headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(opts.body) } : {}) });
    return reply.json();
  } });
  await m.open(); assert.equal(await m.start(), true); m.dispose();
  assert.deepEqual(requests, [['GET', '/dj/sync', undefined, ''], ['POST', '/dj/sync', 'application/json', '{}']]);
});

function panelHarness(t, initial) {
  class Element {
    constructor(tag) { this.tag = tag; this.children = []; this.attrs = {}; this.textContent = ''; }
    append(...children) { this.children.push(...children); }
    replaceChildren(...children) { this.children = children; }
    setAttribute(name, value) { this.attrs[name] = value; }
    focus() {}
  }
  const el = (tag, attrs = {}, ...children) => {
    const node = new Element(tag);
    for (const [key, value] of Object.entries(attrs)) {
      if (key === 'text') node.textContent = value; else node[key] = value;
    }
    node.append(...children); return node;
  };
  const all = (node) => [node, ...node.children.flatMap(all)];
  const previous = global.document; let panel;
  global.document = { hidden: false, addEventListener() {}, removeEventListener() {} };
  t.after(() => { panel?.dispose(); if (previous === undefined) delete global.document; else global.document = previous; });
  let answer = initial;
  panel = remote.panel({ call: async () => answer, el });
  return { panel, nodes: () => all(panel.node), answer: (value) => { answer = value; return panel.model.refresh(); } };
}

test('the panel distinguishes a bounded listening window, a failed retry and the last full refresh', async (t) => {
  const h = panelHarness(t, status({ recent_listening: recent() }));
  await h.panel.model.open();
  let nodes = h.nodes();
  assert.ok(nodes.some((node) => node.textContent === 'Last completed full refresh'));
  assert.ok(nodes.some((node) => node.textContent === 'Recent listening on Spotify'));
  assert.ok(nodes.some((node) => node.textContent.includes('latest 50 plays') && node.textContent.includes('can be missed')));
  const counts = nodes.filter((node) => node.tag === 'dl').map((node) => node.children.map((child) => child.textContent));
  assert.ok(counts.some((values) => values.join('|') === 'History rows observed|50|Distinct track identities accepted|45|Unavailable or non-track rows skipped|2'));
  let checked = nodes.find((node) => node.tag === 'p' && node.children[0]?.textContent === 'Last accepted listening check ');
  assert.equal(checked.children[1].datetime, new Date(recent().checked_at * 1000).toISOString());
  await h.answer(status({ recent_listening: recent({ error: 'Spotify unavailable', not_before: 1790005000 }) }));
  nodes = h.nodes();
  assert.ok(nodes.some((node) => node.textContent.includes('last accepted window retains its original timestamp')));
  checked = nodes.find((node) => node.tag === 'p' && node.children[0]?.textContent === 'Last accepted listening check ');
  assert.equal(checked.children[1].datetime, new Date(recent().checked_at * 1000).toISOString());
  await h.answer(status({ last: null, recent_listening: recent({ checked_at: null,
    received: 0, accepted: 0, skipped: 0, more_available: false, missing_scope: true }) }));
  nodes = h.nodes();
  assert.ok(nodes.some((node) => node.textContent.includes('recent-listening grant')));
  assert.ok(!nodes.some((node) => node.textContent === 'Last completed full refresh'));
  assert.ok(!nodes.some((node) => node.children[0]?.textContent === 'Last accepted listening check '));
});

test('the panel shows current genre counts and cooldown instead of a stale full-refresh count', async (t) => {
  const h = panelHarness(t, status({ last: completed({ genre_artists_pending: 99 }), genre_enrichment: genres() }));
  await h.panel.model.open();
  let nodes = h.nodes();
  const counts = nodes.filter((node) => node.tag === 'dl').map((node) => node.children.map((child) => child.textContent));
  assert.ok(counts.some((items) => items.join('|') === 'Artist lookups pending|12|Completed in the latest pass|7'));
  assert.ok(!nodes.some((node) => node.textContent === '99' || node.textContent === 'Artist genres pending'));
  const retry = nodes.find((node) => node.tag === 'p' && node.children[0]?.textContent === 'Next genre pass no earlier than ');
  assert.equal(retry.children[1].datetime, new Date(genres().not_before * 1000).toISOString());
  assert.ok(nodes.some((node) => node.textContent === 'Last completed full refresh'));

  await h.answer(status({ genre_enrichment: genres({ pending: 0, completed: 12, not_before: null, retry_at: null }) }));
  nodes = h.nodes();
  assert.ok(nodes.some((node) => node.textContent === 'Artist genres are complete for the cached library.'));
  assert.ok(!nodes.some((node) => node.textContent === 'Spotify cooldown ends at '));
});

test('the panel explains saved progress before first completion and keeps verification distinct', async (t) => {
  const h = panelHarness(t, status({ last: null, library_intake: intake() }));
  await h.panel.model.open(); let nodes = h.nodes();
  assert.ok(nodes.some((node) => node.textContent === 'Saved music refresh'));
  assert.ok(nodes.some((node) => node.textContent.includes('progress') && node.textContent.includes('unchanged')));
  assert.ok(!nodes.some((node) => node.textContent.includes('Last reported failure')));
  assert.ok(!nodes.some((node) => node.textContent === 'Last completed full refresh'));
  const counts = nodes.find((node) => node.tag === 'dl').children.map((child) => child.textContent);
  assert.deepEqual(counts, ['Validated pages retained', '512', 'Tracks retained', (24500).toLocaleString(), 'Retained pages rechecked', '0']);
  const retry = nodes.find((node) => node.tag === 'p' && node.children[0]?.textContent === 'Next library pass no earlier than ');
  assert.equal(retry.children[1].datetime, new Date(intake().not_before * 1000).toISOString());

  await h.answer(status({ running: true, last: null,
    library_intake: intake({ phase: 'verifying', verified_pages: 100, running: true }) }));
  nodes = h.nodes();
  assert.ok(nodes.some((node) => node.textContent === 'Checking retained pages before they can replace the published library.'));
  assert.ok(!nodes.some((node) => node.textContent === 'Next library pass no earlier than '));
  await h.answer(status({ library_intake: null })); nodes = h.nodes();
  assert.ok(nodes.some((node) => node.textContent === 'Last completed full refresh'));
  assert.ok(!nodes.some((node) => node.textContent === 'Saved music refresh'));
});

test('the panel distinguishes followed-artist verification, reconciliation and cache limits', async (t) => {
  const h = panelHarness(t, status({ followed_artists: followed({ phase: 'verifying', verified_pages: 8 }) }));
  await h.panel.model.open(); let nodes = h.nodes();
  assert.ok(nodes.some((node) => node.textContent === 'Followed artists'));
  assert.ok(nodes.some((node) => node.textContent === 'Checking retained followed-artist pages before reconciliation.'));
  const counts = nodes.filter((node) => node.tag === 'dl').map((node) => node.children.map((child) => child.textContent));
  assert.ok(counts.some((values) => values.join('|') === `Validated pages retained|20|Artists retained|${(1000).toLocaleString()}|Pages validated for reconciliation|8`));
  let retry = nodes.find((node) => node.tag === 'p' && node.children[0]?.textContent === 'Next followed-artist pass no earlier than ');
  assert.equal(retry.children[1].datetime, new Date(followed().not_before * 1000).toISOString());
  await h.answer(status({ genre_enrichment: genres(), library_intake: intake(),
    followed_artists: followed({ phase: 'ready', verified_pages: 20 }) }));
  nodes = h.nodes();
  assert.ok(nodes.some((node) => node.textContent.includes('full library refresh still needs to reconcile')));
  assert.ok(!nodes.some((node) => node.textContent === 'Followed-artist directory intake is complete.'));
  retry = nodes.find((node) => node.tag === 'p' && node.children[0]?.textContent === 'Next library reconciliation no earlier than ');
  assert.equal(retry.children[1].datetime, new Date(followed().not_before * 1000).toISOString());
  assert.equal(nodes.filter((node) => node.textContent === 'Spotify cooldown ends at ').length, 1);

  await h.answer(status({ followed_artists: followed({ limited: true, pending: false }) }));
  nodes = h.nodes();
  assert.ok(nodes.some((node) => node.textContent.includes('cache limit') && node.textContent.includes('incomplete')));
  assert.ok(!nodes.some((node) => node.textContent.includes('no earlier than')));
  assert.ok(!nodes.some((node) => node.textContent === 'Followed-artist directory intake is complete.'));
  await h.answer(status({ followed_artists: followed({ pending: false, phase: 'ready', verified_pages: 20, retry_at: null, not_before: null }) }));
  nodes = h.nodes();
  assert.ok(nodes.some((node) => node.textContent === 'Followed-artist directory intake is complete.'));
  assert.ok(nodes.some((node) => node.textContent === 'Last completed full refresh'));
});

test('the write error hint survives a subsequent successful observation', async () => {
  const h = harness((name) => { if (name === 'librarySyncStart') throw Object.assign(new Error('Sign in first'), { hint: 'Use Spotify setup on the daemon.' }); return status(); });
  await h.model.open(); await h.model.start(); await settle();
  assert.match(h.model.state.message, /Sign in first/); assert.match(h.model.state.message, /Spotify setup/);
});

test('closing from a view callback before dispatch prevents the write', async () => {
  const calls = []; let m;
  m = remote.model({ call: async (name) => { calls.push(name); return status(); }, changed: (s) => { if (s.open && s.busy) m.close(); } });
  await m.open(); assert.equal(await m.start(), false); assert.deepEqual(calls, ['librarySyncStatus']); m.dispose();
});

test('the shipped script and style are embedded and retain the no-external-code policy', () => {
  const fs = require('node:fs'), path = require('node:path');
  const root = path.resolve(__dirname, '..');
  const embed = fs.readFileSync(path.join(root, 'crates/fsonos-api/src/http/remote.rs'), 'utf8');
  for (const [file, name] of [['library-sync.js','SYNC_SCRIPT'],['library-sync.css','SYNC_STYLE']]) {
    assert.ok(embed.includes(`include_str!("../remote/${file}")`));
    assert.ok(embed.includes(`{${name}}`)); assert.ok(embed.includes(`("${file}", ${name})`));
    const source = fs.readFileSync(path.join(root, 'crates/fsonos-api/src/remote', file), 'utf8');
    for (const banned of ['innerHTML','localStorage','sessionStorage','eval(', 'http://','https://']) assert.ok(!source.includes(banned), `${file}: ${banned}`);
  }
  assert.ok(embed.includes("connect-src 'self'; base-uri 'none'; form-action 'none';"));
});
