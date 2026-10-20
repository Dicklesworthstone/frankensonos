// Native speaker-queue pages. DJ selections are a separate view, never a substitute.
'use strict';
const DashboardQueue = (() => {
  const LIMIT = 40;
  const uint = (n) => Number.isInteger(n) && n >= 0 && n <= 0xffffffff;
  const text = (s) => typeof s === 'string' && s.length <= 65536;
  const group = (z) => JSON.stringify([z.household, z.coordinator_room, [...z.members].sort()]);
  const address = (z) => `${z.coordinator_room}@${z.household}`;
  function decode(value, zone, offset) {
    if (!value || value.zone !== zone.coordinator_room || !uint(value.total)
        || (value.revision != null && (!text(value.revision) || !value.revision.length))
        || (value.playing != null && (!uint(value.playing) || !value.playing || value.playing > value.total))
        || !Array.isArray(value.items)
        || value.items.length !== Math.min(LIMIT, Math.max(0, value.total - offset))) {
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
  const frozen = (v) => { Object.values(v).forEach((item) => {
    if (item && typeof item === 'object' && !Object.isFrozen(item)) frozen(item);
  }); return Object.freeze(v); };
  const unavailable = () => new Error('The queue selection is no longer current. Refresh the queue and select again.');

  // Selection authority belongs to a specific visible page and model lifetime,
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
        ? `Permanently removes only the selected entries. Not undoable. ${page.playing >= position && page.playing < position + count ? 'Includes the currently playing entry and may interrupt playback.' : 'Other queue entries are retained.'}`
        : `Moves the range before original position #${insertBefore}${insertBefore === page.total + 1 ? ' (the end)' : ''}; its new first position is #${resultPosition}. Does not restart or resume playback. Not undoable. Shuffle can change the actual listening order.`;
    const result = frozen({ choice, action, body, resultPosition, title, detail, url: `/queue/${action}` });
    plans.add(result); return result;
  }

  async function verify(call, operation, check = () => {}) {
    if (!plans.has(operation)) throw unavailable();
    check();
    const choice = choiceOf(operation.choice), { zone, page, rows, target } = choice;
    const read = decode(await call(`/zones/${encodeURIComponent(target)}/queue?offset=${page.offset}&limit=${LIMIT}`), zone, page.offset);
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
        || typeof value.recovered !== 'boolean' || value.undoable !== false
        || (operation.action === 'remove'
          ? value.position !== rows[0].position || value.remaining !== page.total - rows.length
          : value.position !== operation.resultPosition || value.total !== page.total
            || !revisionOf(value.revision) || typeof value.changed !== 'boolean')) {
      throw new Error('The queue edit acknowledgement did not match the reviewed change.');
    }
    return { done: value.done, changed: operation.action === 'remove' || value.changed,
      notes: [...(value.notes || []), ...(value.recovered
        ? [{ detail: 'The daemon confirmed the result by reading the queue; the edit was not replayed.' }] : [])] };
  }

  function model({ call, changed = () => {} }) {
    let selected = null, identity = '', generation = 0, disposed = false, active = true;
    let dirty = true, inFlight = null;
    const state = { open: false, target: '', loading: false, stale: true, page: null, error: '', selection: null };
    const publish = () => { if (!disposed) { try { changed(state); } catch (_) { /* View only. */ } } };
    const current = (v) => !disposed && active && state.open && generation === v;
    async function read(offset = state.page?.offset ?? 0) {
      if (disposed || !active || !state.open || !selected?.fresh || !uint(offset)
          || offset > 0xffffffff - LIMIT || inFlight) return false;
      const z = selected.zone, version = ++generation;
      const flight = {}; inFlight = flight; dirty = false;
      state.loading = true; state.stale = true; state.selection = null; state.error = ''; publish();
      try {
        let page = decode(await call(`/zones/${encodeURIComponent(address(z))}/queue?offset=${offset}&limit=${LIMIT}`), z, offset);
        if (!current(version)) return false;
        // Removing the tail can invalidate a page. One bounded read returns to the start.
        if (offset && offset >= page.total) {
          page = decode(await call(`/zones/${encodeURIComponent(address(z))}/queue?offset=0&limit=${LIMIT}`), z, 0);
          if (!current(version)) return false;
        }
        state.page = page; state.stale = dirty; return true;
      } catch (error) {
        if (current(version)) state.error = [error?.message || 'Queue unavailable.', error?.hint].filter(Boolean).join(' ');
        return false;
      } finally {
        if (inFlight === flight) inFlight = null;
        if (current(version)) { state.loading = false; publish(); }
      }
    }
    function select(position, count = 1, expected = state.page) {
      if (!current(generation) || !selected?.fresh || state.loading || state.stale || !expected
          || expected !== state.page || !revisionOf(expected.revision) || !uint(position)
          || !Number.isInteger(count) || count < 1 || count > LIMIT) return null;
      const offset = position - expected.offset - 1;
      if (offset < 0 || offset + count > expected.items.length) return null;
      const rows = expected.items.slice(offset, offset + count);
      if (!rows.every(playable)) return null;
      const original = selected.zone;
      if (!nonempty(original.household) || !nonempty(original.coordinator_room)
          || !original.members.every(nonempty) || !original.members.includes(original.coordinator_room)
          || new Set(original.members).size !== original.members.length) return null;
      const zone = { household: original.household, coordinator_room: original.coordinator_room, members: [...original.members] };
      const choice = frozen({ zone, target: address(zone), signature: group(zone), page: expected, rows });
      const version = generation;
      choices.set(choice, {
        current: () => current(version) && selected?.fresh && !state.stale && !state.loading
          && state.page === expected && state.selection === choice && group(selected.zone) === choice.signature,
        retire() {
          if (state.selection !== choice) return;
          state.selection = null; state.stale = true; dirty = true; publish();
        },
      });
      state.selection = choice; publish(); return choice;
    }
    return { state, read, select,
      clearSelection() { state.selection = null; publish(); },
      observe(item) {
        if (disposed) return;
        const next = item ? group(item.zone) : '';
        selected = item;
        if (identity !== next) {
          identity = next; ++generation; dirty = true;
          state.target = item ? address(item.zone) : '';
          state.page = null; state.selection = null; state.loading = false; state.stale = true; state.error = ''; publish();
        }
        if (!item?.fresh) { state.selection = null; state.stale = true; dirty = true; publish(); return; }
        if (state.open && active && dirty && !inFlight) void read();
      },
      open() { if (disposed) return; state.open = true; state.selection = null; dirty = true; publish(); void read(); },
      close() { ++generation; state.selection = null; state.open = false; state.loading = false; state.stale = true; dirty = true; publish(); },
      invalidate() { if (!disposed) { state.selection = null; dirty = true; state.stale = true; publish(); } },
      navigate(direction) {
        const p = state.page;
        if (state.loading || state.stale || !p || ![-1, 1].includes(direction)) return false;
        const offset = p.offset + direction * LIMIT;
        if (offset < 0 || offset >= p.total) return false;
        void read(offset); return true;
      },
      suspend() { active = false; state.selection = null; ++generation; state.loading = false; dirty = true; state.stale = true; publish(); },
      resume() { if (!disposed) { active = true; dirty = true; } },
      dispose() { disposed = true; active = false; ++generation; },
    };
  }
  // Reuse the page's shared session review; this component never sends a write.
  function editor({ queue, controller, session, el }) {
    const node = el('details', 'queue-editor');
    node.append(el('summary', '', 'Arrange this queue'));
    const status = el('p', 'queue-edit-status'); status.setAttribute('role', 'status');
    const fields = el('div', 'queue-edit-fields'), actions = el('div', 'queue-edit-actions');
    const inputs = {};
    for (const [key, label] of [['first', 'First position'], ['last', 'Last position on this page'], ['before', 'Move before original position']]) {
      const field = el('label', 'queue-edit-field', label), input = el('input');
      input.type = 'text'; input.inputMode = 'numeric'; input.maxLength = 10;
      input.setAttribute('aria-label', label); input.dataset.queueField = key;
      field.append(input); fields.append(field); inputs[key] = input;
    }
    const hint = el('p', 'small-note', 'Select one entry or a consecutive range on this page. Moving uses original queue positions; length + 1 means the end. Edits can disrupt a DJ work. All changes need review.');
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
    button('Clear selection', 'clear', () => { first = last = before = ''; notice = ''; queue.clearSelection(); }, () => !!selection() || !!first || !!last);
    for (const key of ['first', 'last']) inputs[key].addEventListener('input', () => {
      if (!allowed() || session.state.pending) return;
      first = inputs.first.value; last = inputs.last.value; notice = '';
      const from = integer(first), to = integer(last);
      // Invalid partial text must retire an older valid range, not fall back to it.
      if (from === null || to === null || to < from || !queue.select(from, to - from + 1, shownPage)) {
        queue.clearSelection();
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
      if (shownPage !== p || state.stale) { shownPage = p; first = last = before = ''; notice = ''; }
      const choice = state.selection, enabled = allowed();
      for (const [key, value] of Object.entries({ first, last, before })) {
        if (inputs[key].value !== value) inputs[key].value = value;
        inputs[key].disabled = !enabled;
      }
      for (const key of ['first', 'last']) inputs[key].setAttribute('aria-invalid', String(!!(first || last) && !choice));
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
      status.textContent = notice || (!p ? 'Read the queue to select entries.' : !revisionOf(p.revision)
        ? 'The speaker did not provide a usable queue revision. This page is read-only.'
        : state.stale ? 'Queue observation is stale. Refresh before selecting.'
        : !p.items.length ? 'The queue is empty. Add music from your library.'
        : controller.state.busy ? 'Waiting for the current command to finish.'
        : !controller.canControl() ? 'Refresh the selected group before changing its queue.'
        : choice ? `Selected #${choice.rows[0].position}–#${choice.rows.at(-1).position}: ${choice.rows.length} entry/entries. No change sent.`
          : `Choose consecutive positions ${p.offset + 1}–${p.offset + p.items.length} on this page. No change sent.`);
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

  return { LIMIT, decode, model, choiceOf, plan, verify, receipt, submitted, editor };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = DashboardQueue;
