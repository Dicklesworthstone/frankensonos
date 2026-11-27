// Native speaker-queue pages. DJ selections are a separate view, never a substitute.
'use strict';
const DashboardQueue = (() => {
  const LIMIT = 40, SEARCH_LIMIT = 50, MAX_SELECTION = 100;
  const uint = (n) => Number.isInteger(n) && n >= 0 && n <= 0xffffffff;
  const text = (s) => typeof s === 'string' && s.length <= 65536;
  const group = (z) => JSON.stringify([z.household, z.coordinator_room, [...z.members].sort()]);
  const address = (z) => `${z.coordinator_room}@${z.household}`;
  function decode(value, zone, offset, limit = LIMIT) {
    if (!uint(offset) || !Number.isInteger(limit) || limit < 1 || limit > MAX_SELECTION
        || !value || value.zone !== zone.coordinator_room || !uint(value.total)
        || (value.revision != null && (!text(value.revision) || !value.revision.length))
        || (value.playing != null && (!uint(value.playing) || !value.playing || value.playing > value.total))
        || !Array.isArray(value.items)
        || value.items.length !== Math.min(limit, Math.max(0, value.total - offset))) {
      throw new Error('The speaker queue is incomplete. Refresh to read it again.');
    }
    const items = value.items.map((row, index) => {
      if (!row || row.position !== offset + index + 1 || !text(row.title)
          || ['creator', 'album', 'uri'].some((k) => row[k] != null && !text(row[k]))) {
        throw new Error('The speaker queue positions disagree. Refresh before navigating.');
      }
      return Object.freeze({ position: row.position, title: row.title, creator: row.creator ?? '',
        album: row.album ?? '', uri: row.uri ?? null });
    });
    return Object.freeze({ total: value.total, revision: value.revision ?? null,
      playing: value.playing ?? null, offset, items: Object.freeze(items) });
  }
  const choices = new WeakMap(), plans = new WeakSet();
  const nonempty = (s) => text(s) && s.trim() !== '';
  const revisionOf = (s) => nonempty(s) && new TextEncoder().encode(s).length <= 1024;
  const playable = (row) => nonempty(row.uri) && row.uri !== 'NOT_IMPLEMENTED';
  const sameQueue = (a, b) => !!a && !!b && revisionOf(a.revision) && a.revision === b.revision && a.total === b.total;
  const sameRow = (a, b) => !!a && !!b && ['position', 'title', 'creator', 'album', 'uri'].every((key) => a[key] === b[key]);
  const pagePath = (zone, offset, limit = LIMIT) => `/zones/${encodeURIComponent(address(zone))}/queue?offset=${offset}&limit=${Math.min(limit, 0xffffffff - offset)}`;
  const pageOffset = (position) => Math.floor((position - 1) / LIMIT) * LIMIT;
  const queryOf = (value) => {
    if (typeof value !== 'string' || new TextEncoder().encode(value).length > 1024
        || /[\u0000-\u001f\u007f-\u009f]/u.test(value)) return null;
    const query = value.trim();
    return query && query.split(/\s+/u).length <= 32 ? query : null;
  };
  // Search is a single bounded native read. Keep its exact coverage and every
  // positional occurrence; a matching URI is not a unique queue identity.
  function decodeSearch(value, zone, expected, query, offset) {
    const end = offset + (value?.examined ?? NaN);
    if (!value || value.zone !== zone.coordinator_room || value.query !== query
        || !sameQueue(value, expected) || value.offset !== offset || !uint(value.examined)
        || !uint(end) || end > value.total || offset > value.total
        || typeof value.complete !== 'boolean' || value.complete !== (end === value.total)
        || (value.complete ? value.next_offset != null : value.next_offset !== end || end <= offset)
        || !Array.isArray(value.items) || value.items.length > SEARCH_LIMIT || value.items.length > value.examined) {
      throw new Error('The queue search response is incomplete or belongs to a different queue. Refresh and search again.');
    }
    let previous = offset;
    const items = value.items.map((row) => {
      if (!row || !uint(row.position) || row.position <= previous || row.position > end
          || !text(row.title) || ['title', 'creator', 'album', 'uri'].some((key) => row[key] != null
            && (!text(row[key]) || new TextEncoder().encode(row[key]).length > 16384))) {
        throw new Error('The queue search positions or metadata are unreadable. Refresh and search again.');
      }
      previous = row.position;
      const item = Object.freeze({ position: row.position, title: row.title, creator: row.creator ?? '',
        album: row.album ?? '', uri: row.uri ?? null });
      const shown = expected.items?.find((entry) => entry.position === item.position);
      if (shown && !sameRow(shown, item)) throw new Error('A search result disagrees with the visible queue. Refresh and search again.');
      return item;
    });
    return Object.freeze({ query, total: value.total, revision: value.revision, offset, examined: value.examined,
      next_offset: value.next_offset ?? null, complete: value.complete, items: Object.freeze(items) });
  }
  const frozen = (v) => { Object.values(v).forEach((item) => {
    if (item && typeof item === 'object' && !Object.isFrozen(item)) frozen(item);
  }); return Object.freeze(v); };
  const unavailable = () => new Error('The queue selection is no longer current. Refresh the queue and select again.');

  // Selection authority belongs to an observed queue range and model lifetime,
  // not just to a URI (the same song can occur at several queue positions).
  function choiceOf(value) {
    if (!value || !choices.get(value)?.current()) throw unavailable();
    return value;
  }

  function plan(value, action, insertBefore) {
    const choice = choiceOf(value), { rows, page, target } = choice;
    if (!['play', 'remove', 'move'].includes(action) || (action === 'play' && rows.length !== 1)) {
      throw new Error('Choose one entry to play, or a consecutive range to move or remove.');
    }
    const position = rows[0].position, count = rows.length;
    const body = { zone: target, position, expected_revision: page.revision };
    let resultPosition = position;
    if (action === 'play') body.expected_uri = rows[0].uri;
    else body.expected_uris = rows.map((row) => row.uri);
    if (action === 'move') {
      if (!uint(insertBefore) || !insertBefore || insertBefore > page.total + 1) {
        throw new Error('Choose a destination in the original queue, or its length plus one to append.');
      }
      if (insertBefore >= position && insertBefore <= position + count) {
        throw new Error('That move would keep the selected entries in the same place.');
      }
      body.insert_before = insertBefore;
      resultPosition = insertBefore < position ? insertBefore : insertBefore - count;
    }
    const range = count === 1 ? `#${position}` : `#${position}–#${position + count - 1}`;
    const name = count === 1 ? `${range}: ${rows[0].title || 'Untitled track'}` : `${count} entries (${range})`;
    const title = `${{ play: 'Play', remove: 'Remove', move: 'Move' }[action]} ${name}`;
    const detail = action === 'play'
      ? 'Starts this existing queue entry, including when paused. Does not add tracks or rebuild the queue. This can interrupt the current work.'
      : action === 'remove'
        ? `Removes only the selected entries. Undo requires a complete backup saved by the daemon; check the result. ${page.playing >= position && page.playing < position + count ? 'Includes the currently playing entry and may interrupt playback.' : 'Other queue entries are retained.'}`
        : `Moves the range before original position #${insertBefore}${insertBefore === page.total + 1 ? ' (the end)' : ''}; its new first position is #${resultPosition}. Does not restart or resume playback. Undo requires a complete backup saved by the daemon; check the result. Shuffle can change the actual listening order.`;
    const result = frozen({ choice, action, body, resultPosition, title, detail, url: `/queue/${action}` });
    plans.add(result); return result;
  }

  async function verify(call, operation, check = () => {}) {
    if (!plans.has(operation)) throw unavailable();
    check();
    const choice = choiceOf(operation.choice), { zone, page, rows, readLimit } = choice;
    const read = decode(await call(pagePath(zone, page.offset, readLimit)), zone, page.offset, readLimit);
    check(); choiceOf(choice);
    const offset = rows[0].position - page.offset - 1;
    if (read.revision !== page.revision || read.total !== page.total
        || JSON.stringify(read.items.slice(offset, offset + rows.length)) !== JSON.stringify(rows)) {
      throw new Error('The queue or selected entries changed since review. Refresh and select again.');
    }
  }

  function submitted(operation) {
    if (plans.has(operation)) choices.get(operation.choice)?.retire();
  }

  // An acknowledgement is checked against the submitted operation, even when
  // an event or hidden page has already retired the originating selection.
  function receipt(value, operation) {
    if (!plans.has(operation) || !value || !nonempty(value.done)
        || (value.notes != null && (!Array.isArray(value.notes)
          || !value.notes.every((n) => n && typeof n.detail === 'string')))) {
      throw new Error('The queue command acknowledgement is incomplete.');
    }
    if (operation.action === 'play') {
      if (typeof value.changed !== 'boolean') throw new Error('The queue playback acknowledgement is incomplete.');
      return value;
    }
    const { rows, page, zone } = operation.choice;
    if (value.zone !== zone.coordinator_room || value.count !== rows.length
        || typeof value.recovered !== 'boolean' || typeof value.undoable !== 'boolean'
        || (operation.action === 'remove'
          ? value.position !== rows[0].position || value.remaining !== page.total - rows.length
          : value.position !== operation.resultPosition || value.total !== page.total
            || !revisionOf(value.revision) || typeof value.changed !== 'boolean')) {
      throw new Error('The queue edit acknowledgement did not match the reviewed change.');
    }
    return { done: value.done, changed: operation.action === 'remove' || value.changed, undoable: value.undoable,
      notes: [...(value.notes || []), ...(value.recovered
        ? [{ detail: 'The daemon confirmed the result by reading the queue; the edit was not replayed.' }] : [])] };
  }

  function model({ call, changed = () => {} }) {
    let selected = null, identity = '', generation = 0, disposed = false, active = true;
    let dirty = true, inFlight = null;
    const state = { open: false, target: '', loading: false, stale: true, page: null, error: '', selection: null,
      query: '', searching: false, search: null, searchError: '', found: null };
    const publish = () => { if (!disposed) { try { changed(state); } catch (_) { /* View only. */ } } };
    const current = (v) => !disposed && active && state.open && generation === v;
    const ready = () => current(generation) && selected?.fresh && !state.stale && !state.loading && !inFlight && state.page;
    const clearSearch = () => { state.search = null; state.searchError = ''; state.found = null; };
    const retireSearch = () => {
      clearSearch();
      if (inFlight?.kind === 'search') { ++generation; inFlight.abort.abort(); }
    };
    async function readPage(offset = state.page?.offset ?? 0, expected = null, keepSelection = false) {
      if (disposed || !active || !state.open || !selected?.fresh || !uint(offset)
          || offset === 0xffffffff || inFlight) return false;
      const retained = keepSelection ? state.selection : null;
      const z = selected.zone, version = ++generation;
      const flight = { kind: 'page' }; inFlight = flight; dirty = false;
      const previousSearch = state.search;
      state.found = null; state.searchError = '';
      state.loading = true; state.stale = true; state.selection = retained; state.error = ''; publish();
      try {
        let page = decode(await call(pagePath(z, offset)), z, offset);
        if (!current(version)) return false;
        if (expected && (!sameQueue(expected, page)
            || (expected.search && expected.search !== state.search)
            || (expected.item && !sameRow(page.items.find((row) => row.position === expected.item.position), expected.item)))) {
          throw new Error('The queue or located entry changed. Refresh and locate it again before using it.');
        }
        // Removing the tail can invalidate a page. One bounded read returns to the start.
        if (offset && offset >= page.total) {
          page = decode(await call(pagePath(z, 0)), z, 0);
          if (!current(version)) return false;
        }
        state.page = page; state.stale = dirty;
        // Browsing keeps the exact range only while its queue and any visible
        // selected rows still agree. A new choice retires an earlier review.
        const keep = retained && state.selection === retained && !dirty && sameQueue(retained.page, page)
            && page.items.every(row => {
              const known = retained.rows.find(item => item.position === row.position);
              return !known || sameRow(known, row);
            });
        state.selection = null;
        if (keep) keepChoice(Object.freeze({ ...retained.page, playing: page.playing }), retained.rows, retained.readLimit);
        if (dirty || !sameQueue(previousSearch, page)) clearSearch();
        if (!dirty && expected?.position) state.found = expected.position;
        return !dirty;
      } catch (error) {
        if (current(version)) {
          clearSearch(); state.selection = null;
          state.error = [error?.message || 'Queue unavailable.', error?.hint].filter(Boolean).join(' ');
        }
        return false;
      } finally {
        if (inFlight === flight) inFlight = null;
        if (current(version)) { state.loading = false; publish(); }
      }
    }
    async function search(value = state.query, continuation = null) {
      const page = ready(), query = queryOf(value);
      if (!page || !revisionOf(page.revision)) return false;
      if (continuation && (continuation !== state.search || continuation.complete
          || continuation.query !== query || !sameQueue(continuation, page))) return false;
      state.selection = null; clearSearch();
      if (!query) {
        state.searchError = 'Search for 1–32 words of a title, artist or album, at most 1024 bytes, without control characters.';
        publish(); return false;
      }
      const offset = continuation ? continuation.next_offset : 0;
      const z = selected.zone, version = ++generation, flight = { kind: 'search', abort: new AbortController() };
      inFlight = flight; state.query = query; state.loading = true; state.searching = true;
      state.error = ''; publish();
      const valid = () => current(version) && selected?.fresh && !state.stale && !dirty && state.page === page;
      try {
        const url = `/zones/${encodeURIComponent(address(z))}/queue/search?q=${encodeURIComponent(query)}&offset=${offset}&limit=${SEARCH_LIMIT}&expected_revision=${encodeURIComponent(page.revision)}`;
        const value = await call(url, undefined, { signal: flight.abort.signal });
        if (!valid()) return false;
        state.search = decodeSearch(value, z, page, query, offset);
        return true;
      } catch (error) {
        if (valid()) {
          state.search = null; state.stale = true; dirty = true;
          state.searchError = [error?.message || 'Queue search unavailable.', error?.hint].filter(Boolean).join(' ');
        }
        return false;
      } finally {
        if (inFlight === flight) {
          inFlight = null; state.loading = false; state.searching = false;
          if (flight.cancelled && state.searchError === 'Search cancelled. Waiting for the outstanding read to finish.') state.searchError = 'Search cancelled.';
          publish();
        }
      }
    }
    function cancelSearch() {
      if (disposed || inFlight?.kind !== 'search' || !state.searching) return false;
      inFlight.cancelled = true;
      retireSearch(); state.searchError = 'Search cancelled. Waiting for the outstanding read to finish.';
      publish(); return true;
    }
    function jump(position) {
      const page = ready();
      if (!page || !revisionOf(page.revision) || !uint(position) || !position || position > page.total) return Promise.resolve(false);
      return readPage(pageOffset(position), { revision: page.revision, total: page.total, position }, true);
    }
    function openMatch(row, result = state.search) {
      const page = ready();
      if (!page || !result || result !== state.search || !result.items.includes(row) || !playable(row)
          || result.query !== state.query || !sameQueue(result, page)) return Promise.resolve(false);
      return readPage(pageOffset(row.position), { revision: result.revision, total: result.total, item: row, search: result, position: row.position }, true);
    }
    function select(position, count = 1, expected = state.page) {
      if (!current(generation) || !selected?.fresh || state.loading || state.stale || !expected
          || expected !== state.page || !revisionOf(expected.revision) || !uint(position)
          || !Number.isInteger(count) || count < 1 || count > MAX_SELECTION) return null;
      const offset = position - expected.offset - 1;
      if (offset < 0 || offset + count > expected.items.length) return null;
      const rows = expected.items.slice(offset, offset + count);
      if (!rows.every(playable)) return null;
      return keepChoice(expected, rows);
    }
    function keepChoice(page, rows, readLimit = LIMIT) {
      const original = selected.zone;
      if (!nonempty(original.household) || !nonempty(original.coordinator_room)
          || !original.members.every(nonempty) || !original.members.includes(original.coordinator_room)
          || new Set(original.members).size !== original.members.length) return null;
      const zone = { household: original.household, coordinator_room: original.coordinator_room, members: [...original.members] };
      const choice = frozen({ zone, target: address(zone), signature: group(zone), page, rows, readLimit });
      const version = generation;
      choices.set(choice, {
        current: () => current(version) && selected?.fresh && !state.stale && !state.loading
          && !inFlight && sameQueue(state.page, page) && state.selection === choice && group(selected.zone) === choice.signature,
        retire() {
          if (state.selection !== choice) return;
          state.selection = null; state.stale = true; dirty = true; publish();
        },
      });
      state.selection = choice; publish(); return choice;
    }
    async function selectRange(first, last, expected = state.page) {
      const page = ready();
      if (!page || page !== expected || !revisionOf(page.revision)) return null;
      state.selection = null;
      const count = last - first + 1;
      if (!uint(first) || first < 1 || !uint(last) || last < first || last > page.total || count > MAX_SELECTION) {
        state.error = `Choose up to ${MAX_SELECTION} consecutive positions within this queue. No command was sent.`;
        publish(); return null;
      }
      const z = selected.zone, version = ++generation, flight = { kind: 'range' };
      inFlight = flight; state.loading = true; state.error = ''; publish();
      const valid = () => current(version) && selected?.fresh && !dirty && !state.stale && state.page === page;
      try {
        const range = decode(await call(pagePath(z, first - 1, count)), z, first - 1, count);
        if (!valid()) return null;
        if (!sameQueue(page, range) || !range.items.every(playable)
            || page.items.some(row => row.position >= first && row.position <= last
              && !sameRow(row, range.items[row.position - first]))) {
          throw new Error('The queue changed or the selected range is incomplete. Refresh and select it again.');
        }
        return keepChoice(range, range.items, count);
      } catch (error) {
        if (valid()) {
          clearSearch(); state.selection = null; state.stale = true; dirty = true;
          state.error = [error?.message || 'Queue range unavailable.', error?.hint, 'No queue command was sent.'].filter(Boolean).join(' ');
        }
        return null;
      } finally {
        if (inFlight === flight) { inFlight = null; state.loading = false; publish(); }
      }
    }
    return { state, read: (offset) => readPage(offset), select, selectRange, search, cancelSearch, jump, openMatch,
      setQuery(value) {
        if (disposed || typeof value !== 'string' || value === state.query) return false;
        retireSearch(); state.query = value; publish(); return true;
      },
      clearSelection() { if (inFlight?.kind === 'range') ++generation; state.selection = null; publish(); },
      observe(item) {
        if (disposed) return;
        const next = item ? group(item.zone) : '';
        selected = item;
        if (identity !== next) {
          retireSearch(); identity = next; ++generation; dirty = true;
          state.target = item ? address(item.zone) : '';
          state.page = null; state.selection = null; state.loading = false; state.searching = false; state.stale = true; state.error = ''; publish();
        }
        if (!item?.fresh) { retireSearch(); state.selection = null; state.stale = true; dirty = true; publish(); return; }
        if (state.open && active && dirty && !inFlight) void readPage();
      },
      open() { if (disposed) return; retireSearch(); state.open = true; state.selection = null; dirty = true; publish(); void readPage(); },
      close() { retireSearch(); ++generation; state.selection = null; state.open = false; state.loading = false; state.searching = false; state.stale = true; dirty = true; publish(); },
      invalidate() { if (!disposed) { retireSearch(); state.selection = null; dirty = true; state.stale = true; publish(); } },
      navigate(direction) {
        const p = state.page;
        if (state.loading || state.stale || !p || ![-1, 1].includes(direction)) return false;
        const offset = p.offset + direction * LIMIT;
        if (offset < 0 || offset >= p.total) return false;
        void readPage(offset, null, true); return true;
      },
      suspend() { retireSearch(); active = false; state.selection = null; ++generation; state.loading = false; state.searching = false; dirty = true; state.stale = true; publish(); },
      resume() { if (!disposed) { active = true; dirty = true; } },
      dispose() { retireSearch(); disposed = true; active = false; ++generation; },
    };
  }
  // Reuse the page's shared session review; this component never sends a write.
  function editor({ queue, controller, session, el }) {
    const node = el('details', 'queue-editor');
    node.append(el('summary', '', 'Arrange this queue'));
    const status = el('p', 'queue-edit-status'); status.setAttribute('role', 'status');
    const fields = el('div', 'queue-edit-fields'), actions = el('div', 'queue-edit-actions');
    const inputs = {};
    for (const [key, label] of [['first', 'First position'], ['last', 'Last position'], ['before', 'Move before original position']]) {
      const field = el('label', 'queue-edit-field', label), input = el('input');
      input.type = 'text'; input.inputMode = 'numeric'; input.maxLength = 10;
      input.setAttribute('aria-label', label); input.dataset.queueField = key;
      field.append(input); fields.append(field); inputs[key] = input;
    }
    const hint = el('p', 'small-note', `Select up to ${MAX_SELECTION} consecutive entries across queue pages. Read a range to keep a whole album or work together. Moving uses original queue positions; length + 1 means the end. All changes need review.`);
    node.append(hint, fields, actions, status);
    let shownPage = null, first = '', last = '', before = '', notice = '', rows = [], buttons = [];
    const integer = (value) => /^[1-9][0-9]{0,9}$/.test(value) && uint(Number(value)) ? Number(value) : null;
    const allowed = () => !document.hidden && queue.state.open && !queue.state.stale && !queue.state.loading
      && !!queue.state.page?.items.length && revisionOf(queue.state.page.revision) && controller.canControl();
    const choose = (position, count, page) => {
      if (!allowed() || session.state.pending || page !== queue.state.page) return null;
      first = String(position); last = String(position + count - 1); notice = '';
      return queue.select(position, count, page);
    };
    function review(action, destination) {
      if (!allowed() || session.state.pending) return;
      const choice = queue.state.selection;
      const result = session.reviewQueue(choice, action, destination);
      notice = result ? '' : session.state.error || 'Refresh the queue and select the entries again.';
      update();
    }
    const button = (label, key, run, enabled) => {
      const b = el('button', 'outline-button', label); b.type = 'button'; b.dataset.queueEdit = key;
      b.addEventListener('click', () => { if (allowed() && !session.state.pending && enabled()) run(); });
      buttons.push({ node: b, enabled }); actions.append(b); return b;
    };
    const selection = () => queue.state.selection;
    const range = () => {
      const from = integer(first), to = integer(last), total = queue.state.page?.total;
      return from !== null && to !== null && to >= from && to <= total && to - from + 1 <= MAX_SELECTION
        ? { from, to } : null;
    };
    button('Read and select range', 'range', async () => {
      const requested = range(), page = queue.state.page;
      if (!requested) return;
      notice = ''; const chosen = await queue.selectRange(requested.from, requested.to, page);
      if (queue.state.page === page) notice = chosen ? '' : queue.state.error || 'Read the queue and select the range again.';
      update();
    }, () => !!range());
    button('Play selected', 'play', () => review('play'), () => selection()?.rows.length === 1);
    button('Remove selected', 'remove', () => review('remove'), () => !!selection());
    button('Move up', 'up', () => review('move', selection().rows[0].position - 1), () => selection()?.rows[0].position > 1);
    button('Move down', 'down', () => review('move', selection().rows.at(-1).position + 2),
      () => !!selection() && selection().rows.at(-1).position < queue.state.page.total && selection().rows.at(-1).position < 0xfffffffe);
    button('Move to start', 'start', () => review('move', 1), () => selection()?.rows[0].position > 1);
    button('Move to end', 'end', () => review('move', queue.state.page.total + 1),
      () => !!selection() && selection().rows.at(-1).position < queue.state.page.total && queue.state.page.total < 0xffffffff);
    button('Review move', 'move', () => review('move', integer(before)), () => {
      const at = integer(before), choice = selection();
      return !!choice && at !== null && at <= choice.page.total + 1
        && (at < choice.rows[0].position || at > choice.rows.at(-1).position + 1);
    });
    button('Show selection start', 'show', () => { void queue.jump(selection().rows[0].position); }, () => !!selection());
    button('Clear selection', 'clear', () => { first = last = before = ''; notice = ''; queue.clearSelection(); }, () => !!selection() || !!first || !!last);
    for (const key of ['first', 'last']) inputs[key].addEventListener('input', () => {
      if (!allowed() || session.state.pending) return;
      first = inputs.first.value; last = inputs.last.value; notice = '';
      const from = integer(first), to = integer(last);
      // Invalid partial text must retire an older valid range, not fall back to it.
      if (!range() || !queue.select(from, to - from + 1, shownPage)) {
        queue.clearSelection();
        if (range()) notice = 'Read and select this range before reviewing a move or removal.';
      }
      update();
    });
    inputs.before.addEventListener('input', () => { before = inputs.before.value; notice = ''; update(); });
    inputs.before.addEventListener('keydown', (event) => {
      if (event.key === 'Enter' && !event.isComposing) {
        event.preventDefault(); if (allowed()) review('move', integer(before));
      }
    });
    function update() {
      const state = queue.state, p = state.page;
      const choice = state.selection, enabled = allowed();
      if (!p || !choice && (shownPage !== p || state.stale)) { first = last = before = ''; notice = ''; }
      else if (choice) { first = String(choice.rows[0].position); last = String(choice.rows.at(-1).position); }
      shownPage = p;
      for (const [key, value] of Object.entries({ first, last, before })) {
        if (inputs[key].value !== value) inputs[key].value = value;
        inputs[key].disabled = !enabled;
      }
      for (const key of ['first', 'last']) inputs[key].setAttribute('aria-invalid', String(!!(first || last) && !range()));
      const destination = integer(before);
      inputs.before.setAttribute('aria-invalid', String(!!before && (destination === null || !p || destination > p.total + 1)));
      for (const b of buttons) b.node.disabled = !enabled || !b.enabled();
      for (const entry of rows) {
        const usable = enabled && entry.page === p && playable(entry.row);
        entry.select.disabled = entry.play.disabled = !usable;
        const selected = !!choice?.rows.some((r) => r.position === entry.row.position);
        entry.select.setAttribute('aria-pressed', String(selected));
        entry.node.dataset.selected = String(selected);
      }
      const elsewhere = choice?.rows.filter(row => !p?.items.some(item => item.position === row.position)).length || 0;
      status.textContent = state.error || notice || (!p ? 'Read the queue to select entries.' : state.loading
        ? 'Reading queue entries. No change sent.' : !revisionOf(p.revision)
        ? 'The speaker did not provide a usable queue revision. This page is read-only.'
        : state.stale ? 'Queue observation is stale. Refresh before selecting.'
        : !p.items.length ? 'The queue is empty. Add music from your library.'
        : controller.state.busy ? 'Waiting for the current command to finish.'
        : !controller.canControl() ? 'Refresh the selected group before changing its queue.'
        : choice ? `Selected #${choice.rows[0].position}–#${choice.rows.at(-1).position}: ${choice.rows.length} entries${elsewhere ? ` · ${elsewhere} outside this page` : ''}. Page navigation keeps this selection; refresh clears it. No change sent.`
          : `Choose up to ${MAX_SELECTION} consecutive positions from 1–${p.total}. Read a range spanning pages before reviewing an edit. No change sent.`);
    }
    function rowControls(row, page) {
      const wrap = el('div', 'queue-entry-actions'), select = el('button', 'outline-button', 'Select'), play = el('button', 'outline-button', 'Play');
      select.type = play.type = 'button';
      select.setAttribute('aria-label', `Select queue entry ${row.position}`);
      play.setAttribute('aria-label', `Review playing queue entry ${row.position}`);
      select.dataset.queueFocus = `select-${row.position}`; play.dataset.queueFocus = `play-${row.position}`;
      select.addEventListener('click', () => {
        if (choose(row.position, 1, page)) { node.open = true; inputs.last.focus({ preventScroll: true }); }
      });
      play.addEventListener('click', () => { if (choose(row.position, 1, page)) review('play'); });
      wrap.append(select, play); rows.push({ row, page, node: wrap, select, play }); return wrap;
    }
    update();
    return { node, update, rowControls, resetRows() { rows = []; } };
  }

  // The finder opens a verified page; only the existing editor can review a
  // play, move or removal. Search and navigation never submit a command.
  function finder({ queue, controller, session, el }) {
    const node = el('details', 'queue-finder');
    node.append(el('summary', '', 'Find or jump in this queue'));
    const searchForm = el('form', 'queue-find-form'), queryLabel = el('label', 'queue-edit-field', 'Title, artist or album');
    const query = el('input'); query.type = 'search'; query.maxLength = 1024;
    query.setAttribute('aria-label', 'Find in speaker queue'); query.dataset.queueFind = 'query';
    queryLabel.append(query);
    const button = (label, key) => {
      const b = el('button', 'outline-button', label); b.type = 'button'; b.dataset.queueFind = key; return b;
    };
    const find = button('Find', 'submit'); find.type = 'submit';
    const cancel = button('Cancel search', 'cancel'), more = button('Search next section', 'more');
    searchForm.append(queryLabel, find, cancel);
    const status = el('p', 'queue-find-status'); status.setAttribute('role', 'status');
    const results = el('ol', 'queue-find-results'); results.setAttribute('aria-label', 'Matching queue occurrences');
    const jumpForm = el('form', 'queue-find-form'), jumpLabel = el('label', 'queue-edit-field', 'Queue position');
    const position = el('input'); position.type = 'text'; position.inputMode = 'numeric'; position.maxLength = 10;
    position.setAttribute('aria-label', 'Jump to queue position'); position.dataset.queueFind = 'position'; jumpLabel.append(position);
    const jump = button('Show position', 'jump'); jump.type = 'submit';
    const playing = button('Show playing entry', 'playing');
    const jumpStatus = el('p', 'queue-find-status'); jumpStatus.setAttribute('role', 'status');
    jumpForm.append(jumpLabel, jump, playing);
    node.append(searchForm, status, results, more, jumpForm, jumpStatus);
    let displayed = undefined, displayReady = false, notice = '', seenPage = null;
    const available = () => !document.hidden && queue.state.open && !controller.state.busy && !session.state.pending;
    const readable = () => available() && !queue.state.stale && !queue.state.loading
      && !!queue.state.page && revisionOf(queue.state.page.revision);
    const integer = () => /^[1-9][0-9]{0,9}$/.test(position.value) && uint(Number(position.value)) ? Number(position.value) : null;
    query.addEventListener('input', () => { if (available()) queue.setQuery(query.value); });
    searchForm.addEventListener('submit', (event) => {
      event.preventDefault(); if (readable()) void queue.search(query.value);
    });
    cancel.addEventListener('click', () => { if (available()) queue.cancelSearch(); });
    more.addEventListener('click', () => {
      if (readable() && displayed && displayed === queue.state.search) void queue.search(displayed.query, displayed);
    });
    jumpForm.addEventListener('submit', (event) => {
      event.preventDefault(); if (!readable()) return;
      const at = integer();
      if (at === null || at > queue.state.page.total) { notice = `Choose a position from 1 to ${queue.state.page.total}.`; update(); return; }
      notice = ''; void queue.jump(at);
    });
    position.addEventListener('input', () => { notice = ''; update(); });
    playing.addEventListener('click', () => {
      const at = queue.state.page?.playing;
      if (readable() && at) { position.value = String(at); notice = ''; void queue.jump(at); }
    });
    function update() {
      const state = queue.state, page = state.page, result = state.search, usable = readable();
      if (seenPage !== page) { seenPage = page; notice = ''; }
      if (query.value !== state.query) query.value = state.query;
      query.disabled = !available() || (state.loading && !state.searching);
      find.disabled = !usable || !queryOf(query.value);
      cancel.hidden = !state.searching; cancel.disabled = !available();
      more.hidden = !result || result.complete; more.disabled = !usable;
      position.disabled = !available() || state.loading;
      jump.disabled = !usable || !page.total;
      playing.disabled = !usable || !page?.playing;
      playing.textContent = page?.playing ? `Show playing #${page.playing}` : 'No playing entry reported';
      const at = integer();
      position.setAttribute('aria-invalid', String(!!position.value && (at === null || !page || at > page.total)));
      jumpStatus.textContent = notice || (state.found ? `Showing position #${state.found} below. Use its ordinary controls to review a change.`
        : 'Jumping opens a queue page without changing playback.');
      results.setAttribute('aria-busy', String(state.searching));
      if (displayed !== result || displayReady !== usable) {
        const focused = results.contains(document.activeElement) ? document.activeElement?.dataset?.queueMatch : null;
        displayed = result; displayReady = usable; results.replaceChildren();
        for (const row of result?.items || []) {
          const li = el('li'), copy = el('div'), show = button('Show in queue', 'show');
          copy.append(el('strong', '', `#${row.position} · ${row.title || 'Untitled track'}`),
            el('small', '', [row.creator, row.album].filter(Boolean).join(' · ')));
          show.dataset.queueMatch = String(row.position);
          show.setAttribute('aria-label', `Show queue position ${row.position}: ${row.title || 'Untitled track'}`);
          show.disabled = !usable || !playable(row);
          show.addEventListener('click', () => { if (readable()) void queue.openMatch(row, result); });
          li.append(copy, show);
          if (!playable(row)) li.append(el('small', '', 'No renderer identity was reported for this occurrence.'));
          results.append(li);
        }
        if (focused) [...results.querySelectorAll('button')].find((b) => b.dataset.queueMatch === focused && !b.disabled)?.focus({ preventScroll: true });
      }
      status.textContent = state.searchError || (state.searching ? 'Searching this queue. You can cancel the read.'
        : !page ? 'Open a fresh queue before searching.'
        : state.stale ? 'Refresh the queue before searching or opening a result.'
        : !revisionOf(page.revision) ? 'The speaker did not report a usable revision for searching or jumping.'
        : result ? `${result.items.length} matches for “${result.query}”. ${result.examined
          ? `Examined positions ${result.offset + 1}–${result.offset + result.examined} of ${result.total}.` : 'The queue is empty.'} ${result.complete
            ? result.offset === 0 ? 'The entire observed queue was searched.' : 'This section reached the end of the queue.'
            : 'More entries remain; search the next section to continue.'}`
        : 'Search titles, artists and albums. Each section returns up to 50 occurrences; repeated tracks keep their own positions.');
    }
    update(); return { node, update };
  }

  return { LIMIT, SEARCH_LIMIT, MAX_SELECTION, decode, decodeSearch, model, choiceOf, plan, verify, receipt, submitted, editor, finder };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = DashboardQueue;
