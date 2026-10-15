// Revision-bound queue browsing. Served before remote.js in the same asset.
'use strict';

const QueueRemote = (() => {
  const PAGE_SIZE = 25;
  const MAX_INDEX = 4294967295;
  const nonblank = (value) => typeof value === 'string' && value.trim() !== '';
  const identity = (value) => nonblank(value) && value !== 'NOT_IMPLEMENTED';
  const index = (value) => Number.isInteger(value) && value >= 0 && value <= MAX_INDEX;

  const positionOf = (value) => typeof value === 'string' && /^[0-9]{1,10}$/.test(value)
    && index(Number(value)) && Number(value) > 0 ? Number(value) : null;

  function pageOf(value, offset) {
    if (!value || !index(value.total) || !Array.isArray(value.items)
        || value.items.length > PAGE_SIZE
        || value.items.length > Math.max(0, value.total - offset)
        || (!value.items.length && value.total > offset)) {
      throw new Error('The queue response is incomplete. Refresh to read it again.');
    }
    const items = value.items.map((item, i) => {
      if (!item || item.position !== offset + i + 1 || !index(item.position)
          || typeof item.title !== 'string') {
        throw new Error('The queue positions are inconsistent. Refresh to read them again.');
      }
      return { position: item.position, title: item.title, uri: item.uri,
        creator: typeof item.creator === 'string' ? item.creator : '',
        album: typeof item.album === 'string' ? item.album : '' };
    });
    const playing = index(value.playing) && value.playing > 0 && value.playing <= value.total
      ? value.playing : null;
    // Older daemons can still be browsed, but never silently lose a precondition.
    return { total: value.total, items, playing,
      revision: nonblank(value.revision) ? value.revision : null };
  }

  /** One card owns one snapshot. Reads can supersede reads, never a write. */
  function model({ zone, call, changed = () => {}, played = () => {},
    execute = (work) => work(), blocked = () => false, current = () => true }) {
    let epoch = 0;
    let disposed = false;
    let visible = false, active = true, reconciliation = null;
    const reads = new Set();
    const state = { offset: 0, page: null, loading: false, writing: false, message: '',
      stale: true, updating: false, uncertain: false,
      selected: new Set(), pending: null, source: '', destination: '' };
    const clearSelection = () => { state.selected.clear(); state.pending = null; state.destination = ''; };
    const publish = () => {
      if (!disposed && current()) { try { changed(state); } catch (_) { /* View only. */ } }
    };
    const ready = () => !disposed && current() && visible && active && !blocked()
      && !state.stale && !state.uncertain && !state.loading && !state.writing && state.page;

    // Live reads share the household's coalesced refresh, not a second stream
    // or per-card polling loop. A snapshot is never edited while it is stale.
    function stale(message = 'Queue changed; waiting for a fresh snapshot.') {
      const notify = !state.stale || state.pending || state.loading || state.updating;
      ++epoch;
      state.stale = true;
      state.loading = state.updating = false;
      state.pending = null;
      if (!state.writing && !state.uncertain) state.message = message;
      if (notify) publish();
    }

    const available = () => !disposed && current() && active && visible
      && !blocked() && !state.writing;
    const sameEntries = (a, b) => a && a.revision && a.revision === b.revision
      && a.total === b.total && a.items.length === b.items.length
      && a.items.every((item, n) => item.position === b.items[n].position && item.uri === b.items[n].uri);

    async function readPage(offset, background, keepMessage) {
      const previous = background ? state.page : null;
      const request = ++epoch;
      const valid = () => available() && request === epoch;
      state.offset = offset;
      state.stale = true;
      state.loading = !background;
      state.updating = background;
      if (!background) { clearSelection(); state.page = null; }
      // Keep uncertain outcomes until an explicit read actually succeeds.
      if (!keepMessage && !state.uncertain) state.message = '';
      publish();
      try {
        let next;
        // A queue can shrink while its last page is open. Correct the offset
        // once; repeated churn goes through the ordinary read-recovery path.
        for (let attempt = 0; attempt < 2; attempt++) {
          if (!valid()) return false;
          const flight = (async () => call('queue', { params: { room: zone }, query: { offset, limit: PAGE_SIZE } }))();
          reads.add(flight);
          let response;
          try { response = await flight; } finally { reads.delete(flight); }
          if (!valid()) return false;
          next = pageOf(response, offset);
          if (offset === 0 || offset < next.total) break;
          if (attempt) throw new Error('The queue changed again while paging. Refresh to read it again.');
          offset = Math.min(MAX_INDEX - PAGE_SIZE, Math.floor(Math.max(0, next.total - 1) / PAGE_SIZE) * PAGE_SIZE);
        }
        if (!sameEntries(previous, next)) clearSelection();
        state.offset = offset;
        state.page = next;
        state.stale = false;
        state.uncertain = false;
        if (!keepMessage) state.message = '';
        return true;
      } catch (error) {
        if (!valid()) return false;
        state.message = `${state.uncertain ? 'The previous queue command is still unconfirmed. ' : ''}${error.hint ? `${error.message} ${error.hint}` : error.message}`;
        if (background) throw error; // The shared live controller owns backoff.
        return false;
      } finally {
        if (!disposed && current() && request === epoch) {
          state.loading = state.updating = false;
          publish();
        }
      }
    }

    async function load(offset = state.offset, keepMessage = false) {
      if (disposed || !current() || !active) return false;
      if (!index(offset) || offset > MAX_INDEX - PAGE_SIZE) return false;
      visible = true;
      if (!available()) return false;
      return readPage(offset, false, keepMessage);
    }

    function reconcile() {
      if (reconciliation) return reconciliation;
      if (!available() || state.uncertain || !state.stale) return Promise.resolve(false);
      // Wait for manual reads too. Live notifications can supersede a sample,
      // but must not multiply concurrent queue requests while it is pending.
      const running = (async () => {
        while (reads.size) {
          await Promise.allSettled([...reads]);
          if (!available()) return false;
        }
        if (!available() || state.uncertain || !state.stale) return false;
        return readPage(state.offset, true, false);
      })();
      reconciliation = running;
      const clear = () => { if (reconciliation === running) reconciliation = null; };
      running.then(clear, clear);
      return running;
    }

    async function write(route, body, validate, message) {
      state.writing = true;
      state.stale = true;
      state.page = null;
      clearSelection();
      state.message = message;
      const request = ++epoch;
      publish();
      let success = false, entered = false;
      let nextOffset = state.offset;
      try {
        await execute(async () => {
          // A declined lane or a disposed/closed view must never dispatch late.
          if (!state.writing || entered || disposed || !current() || !active || !visible || request !== epoch) return;
          entered = true;
          const result = await call(route, { body });
          if (!result || typeof result.done !== 'string' || !validate(result)) {
            throw new Error('The action returned an unreadable result. Its outcome is uncertain.');
          }
          const notes = (Array.isArray(result.notes) ? result.notes : [])
            .filter((note) => note && typeof note.detail === 'string');
          state.message = [result.done, ...notes.map((note) => note.detail)].join(' ');
          if (route === 'queueRemove') {
            nextOffset = Math.min(nextOffset, Math.floor(Math.max(0, result.remaining - 1) / PAGE_SIZE) * PAGE_SIZE);
          }
          if (route === 'queueMove') {
            nextOffset = Math.min(Math.floor((result.position - 1) / PAGE_SIZE) * PAGE_SIZE, MAX_INDEX - PAGE_SIZE);
          }
          if (route === 'queueAdd') state.source = '';
          success = true;
        });
        if (!entered) state.message = 'Another playback control is busy. Refresh the queue before trying again; no queue command was sent.';
      } catch (error) {
        if (entered) state.uncertain = true;
        state.message = `${error.message}${error.hint ? ` ${error.hint}` : ''} Refresh before trying again; the request was not repeated.`;
      } finally {
        state.writing = false;
        publish();
      }
      if (!disposed && current() && entered) {
        // A failed status refresh must not turn an acknowledged edit into a retry.
        try { Promise.resolve(played()).catch(() => {}); } catch (_) { /* read-only callback */ }
        // Only refresh reads after success; an uncertain write needs an explicit fresh read.
        if (success && visible && active) await load(nextOffset, true);
      }
      return success;
    }

    async function play(position) {
      const page = ready();
      const item = page && page.items.find((each) => each.position === position);
      if (!item || !identity(item.uri) || !page.revision || state.pending) return false;
      const body = { zone, position, expected_uri: item.uri, expected_revision: page.revision };
      return write('queuePlay', body, (result) => typeof result.changed === 'boolean',
        'Starting the selected queue entry…');
    }

    async function add(source, next = false) {
      const page = ready();
      if (!page || !page.revision || state.pending || !nonblank(source)) return false;
      return write('queueAdd', { zone, source_uri: source.trim(), next: Boolean(next) },
        (result) => index(result.first) && result.first > 0 && index(result.count)
          && typeof result.playing_queue === 'boolean',
        'Adding the Spotify item to the queue…');
    }

    function select(position, checked) {
      const page = ready();
      const item = page && page.items.find((each) => each.position === position);
      if (!item || !identity(item.uri) || !page.revision) return false;
      state.pending = null;
      if (checked) state.selected.add(position);
      else state.selected.delete(position);
      state.message = '';
      publish();
      return true;
    }

    function removable() {
      const page = ready();
      if (!page || !page.revision || !state.selected.size) return null;
      const items = page.items.filter((item) => state.selected.has(item.position));
      if (items.length !== state.selected.size || items.some((item, i) =>
        !identity(item.uri) || item.position !== items[0].position + i)) return null;
      return items;
    }

    function reviewRemoval() {
      const items = removable();
      if (!items) return false;
      // Freeze the exact selection the person reviewed. Never reselect by title.
      state.pending = Object.freeze({
        kind: 'remove',
        titles: Object.freeze(items.map((item) => item.title || 'Untitled track')),
        remaining: state.page.total - items.length,
        body: Object.freeze({ zone, position: items[0].position,
          expected_uris: Object.freeze(items.map((item) => item.uri)),
          expected_revision: state.page.revision }),
      });
      publish();
      return true;
    }

    function cancelRemoval() {
      state.pending = null;
      publish();
    }

    async function remove(pending = state.pending) {
      const page = ready();
      if (!page || !pending || pending !== state.pending || pending.kind !== 'remove'
          || page.revision !== pending.body.expected_revision) return false;
      const body = pending.body;
      return write('queueRemove', body, (result) => result.position === body.position
        && result.count === body.expected_uris.length && result.remaining === pending.remaining
        && result.undoable === false && typeof result.recovered === 'boolean',
      'Removing the reviewed queue entries…');
    }

    function reviewMove(insertBefore) {
      const items = removable();
      if (!items) return false;
      state.pending = null;
      const start = items[0].position;
      if (!index(insertBefore) || insertBefore < 1 || insertBefore > state.page.total + 1) {
        state.message = 'Choose a one-based destination in the original queue, or its end.';
        publish();
        return false;
      }
      if (insertBefore >= start && insertBefore <= start + items.length) {
        state.message = 'The selection is already at that position. No move was sent.';
        publish();
        return false;
      }
      state.message = '';
      state.pending = Object.freeze({
        kind: 'move',
        titles: Object.freeze(items.map((item) => item.title || 'Untitled track')),
        total: state.page.total,
        position: insertBefore < start ? insertBefore : insertBefore - items.length,
        body: Object.freeze({ zone, position: start, insert_before: insertBefore,
          expected_uris: Object.freeze(items.map((item) => item.uri)),
          expected_revision: state.page.revision }),
      });
      publish();
      return true;
    }

    async function move(pending = state.pending) {
      const page = ready();
      if (!page || !pending || pending !== state.pending || pending.kind !== 'move'
          || page.revision !== pending.body.expected_revision) return false;
      const body = pending.body;
      return write('queueMove', body, (result) => result.position === pending.position
        && result.count === body.expected_uris.length && result.total === pending.total
        && nonblank(result.zone) && nonblank(result.revision)
        && result.revision !== body.expected_revision && result.changed === true
        && result.undoable === false && typeof result.recovered === 'boolean',
      'Moving the reviewed queue entries…');
    }

    function block() {
      if (blocked() && !state.writing) {
        // A native play/DJ/seek may invalidate this snapshot while a read is pending.
        ++epoch;
        state.loading = false;
        state.page = null;
        state.stale = true;
        state.updating = false;
        clearSelection();
        if (!state.uncertain) state.message = 'Playback is changing. Refresh the queue when the control finishes.';
      }
      publish();
    }

    function close() {
      visible = false;
      ++epoch;
      state.stale = true;
      state.loading = state.updating = false;
      state.page = null;
      clearSelection();
      publish();
    }

    return { state, load, play, add, select, removable, reviewRemoval, cancelRemoval, remove,
      reviewMove, move, block, stale, reconcile, close,
      suspend() { active = false; stale('Queue updates paused while this page is hidden.'); },
      resume() { if (!disposed) { active = true; stale(); } },
      dispose() { disposed = true; close(); } };
  }

  /** Lazy, bounded browsing inside the existing room card. No background poll. */
  function panel({ zone, call, el, played, execute, blocked = () => false, current }) {
    const summary = el('summary', { text: 'Queue' });
    const body = el('div', { class: 'queue-body' });
    const node = el('details', { class: 'queue' }, summary, body);
    const control = model({ zone, call, played, execute, blocked, current, changed: render });
    const button = (text, action, disabled = false) => el('button', {
      type: 'button', text, disabled, onclick: action, 'data-focus': text,
    });

    function render(state) {
      const active = document.activeElement;
      const focus = body.contains(active) ? active.getAttribute('data-focus') : null;
      const scroll = body.querySelector('.queue-items')?.scrollTop || 0;
      draw(state);
      const list = body.querySelector('.queue-items');
      if (list) list.scrollTop = scroll;
      const key = state.pending ? (state.pending.kind === 'move' ? 'Cancel move' : 'Cancel removal') : focus;
      const target = [...body.querySelectorAll('[data-focus]')]
        .find((each) => each.getAttribute('data-focus') === key);
      if (target && !target.disabled) target.focus({ preventScroll: true });
    }

    function draw(state) {
      body.replaceChildren();
      body.setAttribute('aria-busy', String(state.loading || state.updating || state.writing || blocked()));
      const busy = state.loading || state.writing || blocked();
      const deciding = Boolean(state.pending);
      const untrusted = state.stale || state.uncertain || state.updating;
      body.append(el('div', { class: 'row' }, button('Refresh queue', () => control.load(), busy)));
      if (state.message) body.append(el('p', { role: 'status', 'aria-live': 'polite', text: state.message }));
      if (state.loading) body.append(el('p', { text: 'Reading queue…' }));
      const page = state.page;
      const editable = page && page.revision && !busy && !untrusted && !deciding;
      const ownsPage = () => state.page === page && !state.stale && !state.uncertain;
      const source = el('input', { type: 'text', value: state.source, disabled: !editable,
        placeholder: 'Spotify track, album or playlist', 'aria-label': 'Spotify link or URI',
        'data-focus': 'source', class: 'queue-source' });
      const append = button('Add to end', () => { if (ownsPage()) return control.add(state.source); }, !editable || !nonblank(state.source));
      const next = button('Queue next', () => { if (ownsPage()) return control.add(state.source, true); }, !editable || !nonblank(state.source));
      source.addEventListener('input', () => {
        if (!ownsPage()) return;
        state.source = source.value;
        append.disabled = next.disabled = !editable || !nonblank(source.value);
      });
      body.append(el('label', {}, 'Add music from Spotify', source), el('div', { class: 'row' }, append, next));
      if (!page) return;
      if (!page.revision) body.append(el('p', { text: 'Read-only: this daemon did not return a queue revision.' }));
      if (!page.total) {
        body.append(el('p', { class: 'empty', text: 'The queue is empty.' }));
        return;
      }
      const first = page.items[0];
      const last = page.items[page.items.length - 1];
      const range = first ? `${first.position}–${last.position} of ${page.total}` : `No entries at this position (${page.total} total)`;
      body.append(el('p', { text: `${range}. ${page.playing ? `Queue position ${page.playing} is current.` : 'Not playing from this queue.'}` }));
      const list = el('ol', { class: 'queue-items', start: state.offset + 1 });
      for (const item of page.items) {
        const title = item.title || 'Untitled track';
        const current = page.playing === item.position;
        const play = button('Play', () => { if (ownsPage()) return control.play(item.position); }, busy || untrusted || deciding || !page.revision || !identity(item.uri));
        play.setAttribute('data-focus', `play-${item.position}`);
        const select = el('input', { type: 'checkbox', checked: state.selected.has(item.position),
          disabled: busy || untrusted || !page.revision || !identity(item.uri), 'data-focus': `select-${item.position}`,
          'aria-label': `Select queue position ${item.position}: ${title}`,
          onchange: (event) => { if (ownsPage()) control.select(item.position, event.target.checked); } });
        play.setAttribute('aria-label', `Play queue position ${item.position}: ${title}`);
        list.append(el('li', { 'aria-current': current ? 'true' : null },
          el('div', { class: 'track' },
            el('div', { class: 'title', text: `${current ? '▶ ' : ''}${title}` }),
            el('div', { class: 'by', text: [item.creator, item.album].filter(Boolean).join(' · ') })),
          el('div', { class: 'row' }, el('label', { class: 'queue-select' }, select, 'Select'), play)));
      }
      body.append(list);
      if (page.revision) {
        body.append(el('div', { class: 'row' },
          button('Review removal', () => control.reviewRemoval(), busy || deciding || !control.removable())));
        if (state.selected.size) {
          const destination = el('input', { type: 'text', inputmode: 'numeric', pattern: '[0-9]*', maxlength: 10,
            size: 8, class: 'queue-position', value: state.destination, disabled: !editable,
            'aria-label': 'Move before original queue position', 'data-focus': 'destination' });
          const canMove = () => editable && control.removable() && positionOf(destination.value) !== null
            && positionOf(destination.value) <= page.total + 1;
          const reviewMove = button('Review move', () => control.reviewMove(positionOf(state.destination)), !canMove());
          destination.addEventListener('input', () => {
            if (!ownsPage()) return;
            state.destination = destination.value;
            reviewMove.disabled = !canMove();
          });
          body.append(el('label', {}, 'Before original position ', destination),
            el('div', { class: 'row' }, reviewMove,
              button('To start…', () => control.reviewMove(1), !editable || !control.removable()),
              button('To end…', () => control.reviewMove(page.total + 1),
                !editable || !control.removable() || page.total === MAX_INDEX)));
        }
        if (state.selected.size && !control.removable()) {
          body.append(el('p', { text: 'Select consecutive entries on this page to move or remove them together.' }));
        }
      }
      if (state.pending) {
        const pending = state.pending;
        const moving = pending.kind === 'move';
        const destination = pending.body.insert_before === pending.total + 1
          ? 'to the end of the queue' : `before original position ${pending.body.insert_before}`;
        const review = el('div', { class: 'queue-review', role: 'group',
          'aria-label': moving ? 'Confirm queue move' : 'Confirm queue removal' },
          el('p', { text: moving
            ? `Move ${pending.titles.length} entry(s) from position ${pending.body.position} ${destination} in ${zone}? The block will start at ${pending.position}.`
            : `Remove ${pending.titles.length} entry(s) starting at position ${pending.body.position}?` }),
          el('p', { text: moving
            ? 'Only queue order changes; this does not start playback. Reordering cannot be undone. Other queue changes require a fresh review.'
            : 'This cannot be undone. Removing the current entry may advance or stop playback. An active DJ can add new music.' }));
        const titles = el('ul');
        for (const title of pending.titles) titles.append(el('li', { text: title }));
        review.append(titles, el('div', { class: 'row' },
          button(moving ? 'Cancel move' : 'Cancel removal', () => control.cancelRemoval()),
          button(moving ? 'Move entries' : 'Remove permanently',
            () => moving ? control.move(pending) : control.remove(pending), busy)));
        body.append(review);
      }
      body.append(el('div', { class: 'row' },
        button('Previous page', () => control.load(Math.max(0, state.offset - PAGE_SIZE)), busy || state.offset === 0),
        button('Next page', () => control.load(last.position), busy || !last || last.position >= page.total)));
      body.append(el('p', { class: 'empty', text: 'Live updates refresh this open queue. A changed queue revision clears its selection.' }));
    }

    node.addEventListener('toggle', () => { if (node.open) control.load(); else control.close(); });
    const visibility = () => { if (document.hidden) control.suspend(); else control.resume(); };
    const hide = () => control.suspend();
    document.addEventListener?.('visibilitychange', visibility);
    const win = typeof window === 'undefined' ? null : window;
    win?.addEventListener('pagehide', hide);
    win?.addEventListener('pageshow', visibility);
    if (document.hidden) control.suspend();
    render(control.state);
    return { node, block: control.block, stale: control.stale, reconcile: control.reconcile,
      dispose() {
        control.dispose();
        document.removeEventListener?.('visibilitychange', visibility);
        win?.removeEventListener('pagehide', hide);
        win?.removeEventListener('pageshow', visibility);
      },
    };
  }

  return { model, panel, PAGE_SIZE };
})();
