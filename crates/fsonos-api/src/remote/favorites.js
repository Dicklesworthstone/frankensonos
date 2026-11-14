// Shared native favorite authoring. Observations and receipts stay tied to the
// original household; a lost reply is never permission to repeat a catalog write.
'use strict';
const FavoritesRemote = (() => {
  const P = typeof SourcePlayerRemote !== 'undefined' ? SourcePlayerRemote : require('./source-player.js');
  const routes = Object.freeze({
    favoriteGroups: ['GET', '/zones'], favoriteCatalog: ['GET', '/favorites'],
    favoriteCurrent: ['GET', '/favorites/current'], favoriteInspect: ['GET', '/favorites/inspect'],
    favoriteSave: ['POST', '/favorites/save'], favoriteRename: ['POST', '/favorites/rename'],
    favoriteRemove: ['POST', '/favorites/remove'],
  });
  const object = v => v !== null && typeof v === 'object' && !Array.isArray(v);
  const text = (v, max = 8192) => typeof v === 'string' && v.length > 0 && v.length <= max && !/[\u0000-\u001f\u007f-\u009f]/u.test(v);
  const problem = error => text(error?.message, 4096) ? error.message : 'The favorite request was not confirmed.';
  const same = (a, b) => JSON.stringify(a) === JSON.stringify(b);
  const id = value => typeof value === 'string' && /^FV:2\/[0-9]+$/.test(value) && value.length <= 128;
  const query = (path, values) => path + '?' + new URLSearchParams(values);

  function favorite(value, current = false) {
    if (!object(value) || (current ? value.id !== '' : !id(value.id)) || !text(value.title)
        || !(current ? ['track', 'stream'] : ['track', 'stream', 'container', 'unplayable']).includes(value.kind)
        || ['description', 'art_uri'].some(key => value[key] != null && (typeof value[key] !== 'string' || value[key].length > 16384))) {
      throw new Error('The favorite observation is incomplete. Read the household again.');
    }
    return Object.freeze({ id: value.id, title: value.title, kind: value.kind,
      description: value.description ?? null, art_uri: value.art_uri ?? null });
  }
  function catalog(value) {
    if (!Array.isArray(value) || value.length > 10000) throw new Error('The favorites catalog is unreadable.');
    const rows = value.map(item => favorite(item));
    if (new Set(rows.map(item => item.id)).size !== rows.length) throw new Error('The favorites catalog repeats a native identity.');
    return Object.freeze(rows);
  }
  function titleOf(raw) {
    if (typeof raw !== 'string') throw new Error('Enter a favorite name.');
    const title = raw.replace(/^\p{White_Space}+|\p{White_Space}+$/gu, '');
    if (!title || /[\u0000-\u001f\u007f-\u009f\uD800-\uDFFF]/u.test(title)
        || new TextEncoder().encode(title).length > 1023) {
      throw new Error('Use a nonblank favorite name of at most 1023 UTF-8 bytes without control characters.');
    }
    return title;
  }
  function observation(value, group, current) {
    if (!object(value) || value.zone !== group.coordinator || !text(value.revision, 1024)) {
      throw new Error('The favorite observation does not identify this listening group. Read it again.');
    }
    return Object.freeze({ favorite: favorite(value.favorite, current), revision: value.revision });
  }
  function receipt(value, reviewed) {
    const item = favorite(value?.favorite);
    const expectedTitle = reviewed.operation === 'remove' ? reviewed.item.title : reviewed.title;
    if (!object(value) || value.zone !== reviewed.group.coordinator || value.operation !== reviewed.operation
        || !text(value.done) || typeof value.logged !== 'boolean' || typeof value.changed !== 'boolean'
        || item.title !== expectedTitle || item.kind !== reviewed.item.kind
        || (reviewed.operation !== 'save' && item.id !== reviewed.item.id)
        || value.changed !== (reviewed.operation !== 'rename' || reviewed.title !== reviewed.item.title)) {
      throw new Error('The favorite command returned an incomplete or mismatched receipt.');
    }
    return Object.freeze({ favorite: item, changed: value.changed, logged: value.logged,
      detail: value.done + (value.logged ? '' : ' The favorite change was confirmed, but its action log could not be written. Do not repeat the change to repair the log.') });
  }

  // Dashboard calls use URLs. The phone adapter maps only these same endpoints
  // onto its authenticated route client; it does not create another HTTP client.
  function phoneClient(call) {
    return (url, body, options = {}) => {
      const parsed = new URL(url, 'http://local.invalid');
      if (!url.startsWith('/') || parsed.origin !== 'http://local.invalid' || parsed.hash) throw new Error('Unsupported favorites request.');
      const route = Object.entries(routes).find(([, value]) => value[1] === parsed.pathname
        && value[0] === (body === undefined ? 'GET' : 'POST'));
      if (!route || body !== undefined && parsed.search) throw new Error('Unsupported favorites request.');
      return call(route[0], body === undefined ? { query: Object.fromEntries(parsed.searchParams), signal: options.signal } : { body });
    };
  }

  function model({ call, changed = () => {}, completed = () => {}, demo = false,
    selection = null, available = () => true, readable = () => true,
    commit = (_, work) => work({ check() {}, send: call }),
    readCatalog = (target, decode) => call(query('/favorites', { zone: target })).then(decode) }) {
    let epoch = 0, disposed = false, owned = false;
    const state = { open: false, active: true, groups: [], group: null, catalog: [], catalogFresh: false,
      item: null, current: null, title: '', pending: null, reading: false, writing: false,
      result: null, recovery: null, recoveryCatalog: null, message: '' };
    const publish = () => { if (!disposed) { try { changed(state); } catch (_) { /* Presentation cannot replay a command. */ } } };
    const live = () => !disposed && state.active && state.open && !demo;
    const busy = () => state.reading || state.writing;
    const editable = () => live() && !busy() && !state.recovery && available();
    const matches = group => !!group && state.group?.signature === group.signature
      && (!selection || selection()?.signature === group.signature);
    function invalidate(message = '') {
      ++epoch; state.reading = false; state.pending = null; state.current = null;
      if (message) state.message = message;
    }
    function clearSelection() { state.item = null; state.current = null; state.title = ''; state.pending = null; }
    function setGroup(group) {
      if (state.group?.signature !== group?.signature) {
        invalidate(); clearSelection(); state.catalog = []; state.catalogFresh = false;
      }
      state.group = group;
    }
    function guard(version, group = null) {
      if (!live() || version !== epoch || group && !matches(group)) throw new Error('The favorites view or listening group changed. Read it again before reviewing.');
    }
    async function checkGroup(group, valid) {
      valid(); const groups = P.groups(await call('/zones')); valid();
      if (!groups.some(row => row.signature === group.signature && row.target === group.target)) {
        throw new Error('The original listening group changed. Read its favorites and review again.');
      }
    }
    async function inspect(group, current, item, valid) {
      await checkGroup(group, valid);
      const result = observation(await call(query(current ? '/favorites/current' : '/favorites/inspect', {
        zone: group.target, ...(current ? {} : { favorite: item.id }),
      })), group, current); valid();
      if (!current && !same(result.favorite, item)) throw new Error('This favorite changed. Read its current catalog and select the exact entry again.');
      return result;
    }
    async function refresh(target = state.group?.target) {
      if (!live() || busy() || !readable()) return false;
      invalidate(); const version = epoch;
      state.reading = true; state.catalogFresh = false; clearSelection(); state.message = 'Reading household favorites…'; publish();
      try {
        const groups = P.groups(await call('/zones')); guard(version);
        const external = selection?.();
        const group = groups.find(row => row.target === (selection ? external?.target : target))
          || (!selection && !target ? groups[0] : null);
        if (!group || external && external.signature !== group.signature) throw new Error('Choose an available listening group and read its favorites again.');
        state.groups = groups; state.group = group;
        const rows = catalog(await call(query('/favorites', { zone: group.target }))); guard(version, group);
        state.catalog = rows; state.catalogFresh = true; state.message = ''; return true;
      } catch (error) { if (version === epoch) state.message = problem(error); return false; }
      finally { if (version === epoch) { state.reading = false; publish(); } }
    }
    async function readCurrent() {
      if (!editable() || !state.group) return false;
      invalidate(); clearSelection(); const group = state.group, version = epoch;
      state.reading = true; state.message = 'Reading the actual selected music source…'; publish();
      try {
        const current = await inspect(group, true, null, () => guard(version, group));
        state.current = current; state.title = current.favorite.title; state.message = ''; return true;
      } catch (error) { if (version === epoch) state.message = problem(error); return false; }
      finally { if (version === epoch) { state.reading = false; publish(); } }
    }
    function select(item) {
      if (!editable() || !state.catalogFresh || !state.catalog.includes(item)) return false;
      invalidate(); state.item = item; state.title = item.title; state.message = ''; publish(); return true;
    }
    function editTitle(value) {
      if (!editable() || typeof value !== 'string' || value.length > 8192) return false;
      ++epoch; state.pending = null; state.title = value; state.message = ''; publish(); return true;
    }
    async function review(operation) {
      if (!editable() || !state.group || !['save', 'rename', 'remove'].includes(operation)) return null;
      const current = operation === 'save', source = current ? state.current : null;
      const item = current ? source?.favorite : state.item;
      if (!item || !current && !state.catalogFresh) return null;
      let title;
      try { title = operation === 'remove' ? item.title : titleOf(state.title); }
      catch (error) { state.message = problem(error); publish(); return null; }
      const group = state.group, version = ++epoch;
      state.pending = null; state.reading = true; state.message = 'Checking the exact favorite for review…'; publish();
      try {
        const observed = await inspect(group, current, item, () => guard(version, group));
        if (current && (source.revision !== observed.revision || !same(item, observed.favorite))) {
          state.current = null; throw new Error('The selected music changed. Read current music again before saving it.');
        }
        state.pending = Object.freeze({ group, item, title, operation, revision: observed.revision,
          body: Object.freeze({ zone: group.target, expected_revision: observed.revision,
            ...(current ? {} : { favorite: item.id }), ...(operation === 'remove' ? {} : { title }) }) });
        state.message = ''; return state.pending;
      } catch (error) { if (version === epoch) state.message = problem(error); return null; }
      finally { if (version === epoch) { state.reading = false; publish(); } }
    }
    async function confirm(reviewed = state.pending) {
      if (!editable() || !reviewed || reviewed !== state.pending || !matches(reviewed.group)) return false;
      const version = ++epoch; state.pending = null; state.writing = true; state.message = 'Checking the reviewed favorite again…'; publish();
      let sent = false, accepted = null;
      try {
        owned = true;
        await commit(reviewed, async ({ check, send }) => {
          const valid = () => { check(); guard(version, reviewed.group); };
          const observed = await inspect(reviewed.group, reviewed.operation === 'save', reviewed.item, valid);
          if (observed.revision !== reviewed.revision || !same(observed.favorite, reviewed.item)) throw new Error('The reviewed favorite or current music changed. Nothing was submitted.');
          valid();
          if (sent) throw new Error('This favorite command was already submitted.');
          sent = true;
          accepted = receipt(await send('/favorites/' + reviewed.operation, reviewed.body), reviewed);
          return { done: accepted.detail, changed: accepted.changed };
        });
        if (!accepted) throw new Error('The favorite command was not submitted. Review it again.');
        state.result = Object.freeze({ ...reviewed, status: 'accepted', favorite: accepted.favorite,
          logged: accepted.logged, detail: accepted.detail });
        state.message = ''; state.catalogFresh = false; clearSelection();
        try { completed(state.result); } catch (_) { /* Preserve a confirmed native result. */ }
        return true;
      } catch (error) {
        state.result = Object.freeze({ ...reviewed, status: sent ? 'uncertain' : 'not_sent', detail: problem(error)
          + (sent ? ' Read the original household favorites before another change. This request will not be repeated automatically.' : ' No favorite change was sent.') });
        if (sent) { state.recovery = state.result; state.recoveryCatalog = null; state.catalogFresh = false; }
        state.message = ''; return false;
      } finally { owned = false; state.writing = false; publish(); }
    }
    async function recover() {
      const result = state.recovery;
      if (!live() || busy() || !result || !readable()) return null;
      const version = ++epoch; state.reading = true; state.pending = null;
      state.message = 'Reading the original household favorites in ' + result.group.target + '…'; publish();
      const valid = () => { guard(version); if (state.recovery !== result) throw new Error('Read the original favorites catalog again.'); };
      try {
        const zones = await call('/zones'); P.groups(zones); valid();
        // A household catalog belongs to every member, not just its current
        // coordinator. Keep the original qualified room/store identity even
        // when that room has since joined another group in the same household.
        if (!zones.some(zone => zone.household === result.group.household
            && zone.members.includes(result.group.coordinator))) {
          throw new Error('The original household group is unavailable. The unconfirmed result is retained.');
        }
        const rows = await readCatalog(result.group.target, value => { valid(); return catalog(value); }); valid();
        if (!rows) throw new Error('The original household catalog was not refreshed.');
        const found = result.operation === 'save' ? rows.filter(item => item.title === result.title && item.kind === result.item.kind)
          : rows.filter(item => item.id === result.item.id);
        const detail = result.operation === 'save' ? `${found.length} matching favorite(s) are currently present.`
          : found.length ? `${found[0].title} (${found[0].id}) is currently present.` : `${result.item.id} is currently absent.`;
        state.recoveryCatalog = Object.freeze({ group: result.group, items: rows });
        state.recovery = null;
        state.result = Object.freeze({ ...result, status: 'observed', detail: detail
          + ' This catalog observation does not prove how the earlier request finished. Nothing was repeated; review any new change separately.' });
        if (state.group?.household === result.group.household) { state.catalog = rows; state.catalogFresh = true; clearSelection(); }
        state.message = ''; return rows;
      } catch (error) { if (version === epoch) state.message = problem(error); return null; }
      finally { if (version === epoch) { state.reading = false; publish(); } }
    }
    return { state, busy, editable, refresh, readCurrent, select, editTitle, review, confirm, recover,
      canReview: () => editable() && !!state.group && !!(state.current || state.item && state.catalogFresh),
      canRead: () => live() && !busy() && readable(),
      async open(target) {
        if (disposed || !state.active) return false;
        state.open = true; publish();
        if (demo || state.writing || state.recovery) return true;
        return refresh(target);
      },
      close() { if (disposed) return; state.open = false; invalidate(); publish(); },
      cancel() { if (state.writing) return false; ++epoch; state.pending = null; state.message = ''; publish(); return true; },
      observe() {
        if (disposed) return;
        const external = selection?.();
        if (selection && state.group?.signature !== external?.signature) {
          setGroup(external || null); state.message = 'The selected group changed. Read its favorites before reviewing.';
        } else if (!owned && !available() && state.pending) invalidate('Another command is using this group. Read and review again.');
        publish();
      },
      invalidate(message) { invalidate(message); state.catalogFresh = false; publish(); },
      catalogChanged(change = null) {
        if (change?.favorites === false) return;
        const household = change?.household;
        const expected = state.recovery?.group.household || state.pending?.group.household || state.group?.household;
        if (household && household !== expected) {
          if (household === state.group?.household) { state.catalogFresh = false; publish(); }
          return;
        }
        invalidate('Household favorites changed. Read the current catalog before another review.');
        if (!household || household === state.group?.household) state.catalogFresh = false;
        publish();
      },
      suspend() { if (disposed) return; state.active = false; state.open = false; invalidate(); publish(); },
      resume() { if (!disposed) { state.active = true; publish(); } },
      dispose() { if (!disposed) { state.open = false; invalidate(); disposed = true; } },
    };
  }
  return { routes, favorite, catalog, titleOf, observation, receipt, phoneClient, model };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = FavoritesRemote;
