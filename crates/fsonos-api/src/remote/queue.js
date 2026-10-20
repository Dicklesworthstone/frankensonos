// Revision-bound queue browsing. Served before remote.js in the same asset.
'use strict';

const QueueRemote = (() => {
  const PAGE_SIZE = 25;
  // Match the existing native move/remove request limit, never split a write.
  const MAX_SELECTION = 100;
  // Each explicit search examines one bounded section. Continue is deliberate.
  const SEARCH_PAGE = 100, SEARCH_REQUESTS = 100, SEARCH_MATCHES = 200;
  const searchText = value => value.normalize('NFKD').replace(/\p{M}/gu, '').toLowerCase();
  const MAX_INDEX = 4294967295;
  const nonblank = (value) => typeof value === 'string' && value.trim() !== '';
  const identity = (value) => nonblank(value) && value !== 'NOT_IMPLEMENTED';
  const index = (value) => Number.isInteger(value) && value >= 0 && value <= MAX_INDEX;

  const positionOf = (value) => typeof value === 'string' && /^[0-9]{1,10}$/.test(value)
    && index(Number(value)) && Number(value) > 0 ? Number(value) : null;

  function pageOf(value, offset, limit = PAGE_SIZE) {
    if (!value || !index(value.total) || !Array.isArray(value.items)
        || value.items.length > limit
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
    const selectedItems = new Map();
    let selectionRevision = null, selectionTotal = null;
    const state = { offset: 0, page: null, loading: false, writing: false, message: '',
      stale: true, updating: false, uncertain: false,
      selected: new Set(), pending: null, source: '', destination: '',
      selecting: false, navigating: false, jump: '', rangeStart: '', rangeEnd: '',
      searching: false, query: '', search: null, found: null };
    const clearSearch = () => { state.search = null; state.found = null; };
    const clearSelection = () => {
      state.selected.clear(); selectedItems.clear();
      selectionRevision = selectionTotal = null;
      state.pending = null; state.destination = '';
    };
    const publish = () => {
      if (!disposed && current()) { try { changed(state); } catch (_) { /* View only. */ } }
    };
    const ready = () => !disposed && current() && visible && active && !blocked()
      && !state.stale && !state.uncertain && !state.loading && !state.writing && !state.selecting && !state.navigating && !state.searching && state.page;

    // Live reads share the household's coalesced refresh, not a second stream
    // or per-card polling loop. A snapshot is never edited while it is stale.
    function stale(message = 'Queue changed; waiting for a fresh snapshot.') {
      const notify = !state.stale || state.pending || state.loading || state.updating || state.search;
      clearSearch();
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

    const sameQueue = (a, b) => a && b && a.revision && a.revision === b.revision && a.total === b.total;
    const agreesWithSelection = page => !state.selected.size || (selectionRevision === page.revision
      && selectionTotal === page.total && page.items.every(item => !selectedItems.has(item.position)
        || selectedItems.get(item.position).uri === item.uri));
    const pageOffset = position => Math.min(Math.floor((position - 1) / PAGE_SIZE) * PAGE_SIZE, MAX_INDEX - PAGE_SIZE);

    async function readPage(offset, background, keepMessage, preserve = false, expected = null) {
      const previous = background || preserve ? state.page : null;
      const request = ++epoch;
      const valid = () => available() && request === epoch;
      if (!preserve) state.offset = offset;
      state.pending = null;
      state.found = null;
      state.stale = true;
      state.loading = !background;
      state.updating = background;
      if (!background && !preserve) { clearSelection(); clearSearch(); state.page = null; }
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
          if (expected && !sameQueue(expected, next)) {
            throw new Error('The queue changed while locating that entry. Refresh before navigating again.');
          }
          if (expected?.item && !next.items.some(item => item.position === expected.item.position
              && item.uri === expected.item.uri)) {
            throw new Error('The found entry no longer agrees with this queue. Search again before using it.');
          }
          if (offset === 0 || offset < next.total) break;
          if (attempt) throw new Error('The queue changed again while paging. Refresh to read it again.');
          offset = Math.min(MAX_INDEX - PAGE_SIZE, Math.floor(Math.max(0, next.total - 1) / PAGE_SIZE) * PAGE_SIZE);
        }
        if (sameQueue(previous, next) && !agreesWithSelection(next)) {
          clearSelection();
          throw new Error('Selected entries disagree within this queue revision. Refresh before editing.');
        }
        const retain = preserve ? sameQueue(previous, next) : sameEntries(previous, next);
        const cleared = state.selected.size > 0 && !retain;
        if (!retain) clearSelection();
        if (state.search && !sameQueue(state.search, next)) clearSearch();
        state.offset = offset;
        state.page = next;
        state.stale = false;
        state.uncertain = false;
        if (!keepMessage) state.message = cleared ? 'Queue contents changed; the previous selection was cleared.' : '';
        return true;
      } catch (error) {
        if (!valid()) return false;
        if (preserve) clearSelection();
        clearSearch();
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
      if (disposed || !current() || !active || state.selecting || state.navigating || state.searching) return false;
      if (!index(offset) || offset > MAX_INDEX - PAGE_SIZE) return false;
      visible = true;
      if (!available()) return false;
      return readPage(offset, false, keepMessage);
    }

    async function browse(offset) {
      if (!ready() || reads.size || !index(offset) || offset > MAX_INDEX - PAGE_SIZE) return false;
      state.navigating = true;
      try { return await readPage(offset, false, false, true); }
      finally { state.navigating = false; publish(); }
    }

    function jumpTo(position) {
      const page = ready();
      if (!page || !index(position) || position < 1 || position > page.total) return Promise.resolve(false);
      return browse(pageOffset(position));
    }

    async function showCurrent() {
      const previous = ready();
      if (!previous || reads.size) return false;
      const version = ++epoch;
      const valid = () => available() && epoch === version;
      state.navigating = true; state.pending = null;
      state.message = 'Locating the current queue entry…'; publish();
      let flight;
      try {
        if (!valid()) return false;
        flight = (async () => call('queue', { params: { room: zone }, query: { offset: 0, limit: 1 } }))();
        reads.add(flight);
        const head = pageOf(await flight, 0, 1);
        reads.delete(flight);
        if (!valid()) return false;
        if (!sameQueue(previous, head)) { clearSelection(); state.stale = true; }
        else if (!agreesWithSelection(head)) {
          throw new Error('Selected entries disagree within this queue revision. Refresh before editing.');
        }
        if (!head.playing) {
          if (!state.stale) state.page = { ...previous, playing: null };
          state.message = 'No current queue entry was reported. Playback may be using another source.';
          return false;
        }
        // Two bounded reads, never a loop chasing an advancing player. The
        // destination page must belong to the queue reported by the head read.
        const loaded = await readPage(pageOffset(head.playing), false, false, true, head);
        if (loaded && state.page.playing && !state.page.items.some(item => item.position === state.page.playing)) {
          state.message = 'Playback advanced while locating it. Show current entry again to follow it; no playback command was sent.';
        }
        return loaded;
      } catch (error) {
        if (valid()) {
          clearSelection(); state.stale = true;
          state.message = `${error.message}${error.hint ? ` ${error.hint}` : ''} No playback command was sent.`;
        }
        return false;
      } finally {
        if (flight) reads.delete(flight);
        state.navigating = false; publish();
      }
    }

    function editSearch(query) {
      if (!ready() || typeof query !== 'string' || query.length > 1024) return false;
      state.query = query; state.pending = null; clearSearch(); publish(); return true;
    }

    function cancelSearch() {
      if (disposed || !current() || !state.searching || !state.search) return false;
      ++epoch; clearSearch();
      state.message = 'Search cancelled. Waiting for the outstanding read to finish; no queue command was sent.';
      publish(); return true;
    }

    async function search(query = state.query, continuation = null) {
      const page = ready();
      if (!page || !page.revision || reads.size) return false;
      if (continuation && (continuation !== state.search || continuation.complete
          || continuation.running || !sameQueue(continuation, page) || query !== continuation.query)) return false;
      state.pending = null; clearSearch();
      const trimmed = typeof query === 'string' && query.length <= 1024 ? query.trim() : '';
      const terms = trimmed && [...trimmed].length <= 256 ? searchText(trimmed).split(/\s+/).filter(Boolean) : [];
      if (!terms.length || terms.length > 32) {
        state.message = 'Search for 1–32 words, at most 256 characters, in titles, artists or albums.';
        publish(); return false;
      }
      state.query = trimmed; state.searching = true;
      const version = ++epoch, start = continuation ? continuation.scanned : 0;
      const valid = () => available() && epoch === version && !state.stale && state.page === page;
      const matches = [];
      let offset = start, requests = 0, flight;
      const snapshot = running => Object.freeze({ query: trimmed, revision: page.revision, total: page.total,
        start, scanned: offset, complete: offset === page.total, running,
        matches: Object.freeze([...matches]) });
      state.search = snapshot(true); state.message = 'Searching the observed queue…'; publish();
      try {
        while (valid() && offset < page.total && requests < SEARCH_REQUESTS && matches.length < SEARCH_MATCHES) {
          const limit = Math.min(SEARCH_PAGE, page.total - offset);
          flight = (async () => call('queue', { params: { room: zone }, query: { offset, limit } }))();
          reads.add(flight); requests++;
          let response;
          try { response = await flight; } finally { reads.delete(flight); }
          if (!valid()) return false;
          const next = pageOf(response, offset, limit);
          if (!sameQueue(page, next) || !agreesWithSelection(next)
              || page.items.some(item => item.position > offset && item.position <= offset + next.items.length
                && item.uri !== next.items[item.position - offset - 1].uri)) {
            throw new Error('The queue changed during search. Refresh and search its new contents.');
          }
          for (const item of next.items) {
            // Limit retained metadata as well as result count. Preserve exact
            // renderer identity; search normalization is only for matching text.
            if ([item.title, item.creator, item.album].some(value => value.length > 16384)
                || (item.uri != null && (typeof item.uri !== 'string' || item.uri.length > 16384))) {
              throw new Error('A search entry is unreadable or too large. Refresh before searching again.');
            }
            const words = searchText([item.title, item.creator, item.album].join(' '));
            offset = item.position;
            if (terms.every(term => words.includes(term))) matches.push(Object.freeze({ ...item }));
            if (matches.length === SEARCH_MATCHES) break;
          }
          state.search = snapshot(true);
          state.message = `Searched ${offset - start} entries; ${matches.length} matches so far. No queue command was sent.`;
          publish();
        }
        if (!valid()) return false;
        state.search = snapshot(false);
        state.message = offset === page.total
          ? 'Search reached the end of the observed queue. Open a result to inspect its current page.'
          : 'Search stopped at its result or read limit. Continue explicitly to examine the next section.';
        return true;
      } catch (error) {
        if (valid()) {
          clearSearch(); clearSelection(); state.stale = true;
          state.message = `${error.message}${error.hint ? ` ${error.hint}` : ''} No queue command was sent.`;
        }
        return false;
      } finally {
        if (flight) reads.delete(flight);
        state.searching = false; publish();
      }
    }

    async function openMatch(item) {
      const page = ready(), result = state.search;
      if (!page || reads.size || !result || result.running || !result.matches.includes(item)
          || !identity(item.uri) || !sameQueue(page, result) || state.query !== result.query) return false;
      state.navigating = true;
      try {
        const loaded = await readPage(pageOffset(item.position), false, false, true, { ...result, item });
        if (loaded) {
          state.found = item.position;
          state.message = `Found entry ${item.position} is shown below. Inspect or play it with the ordinary queue controls; nothing was changed.`;
        }
        return loaded;
      } finally { state.navigating = false; publish(); }
    }

    function reconcile() {
      if (reconciliation) return reconciliation;
      if (!available() || state.selecting || state.navigating || state.searching || state.uncertain || !state.stale) return Promise.resolve(false);
      // Wait for manual reads too. Live notifications can supersede a sample,
      // but must not multiply concurrent queue requests while it is pending.
      const running = (async () => {
        while (reads.size) {
          await Promise.allSettled([...reads]);
          if (!available()) return false;
        }
        if (!available() || state.selecting || state.navigating || state.searching || state.uncertain || !state.stale) return false;
        return readPage(state.offset, true, false);
      })();
      reconciliation = running;
      const clear = () => { if (reconciliation === running) reconciliation = null; };
      running.then(clear, clear);
      return running;
    }

    async function write(route, body, validate, message) {
      clearSearch();
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
      if (checked && !state.selected.has(position) && state.selected.size >= MAX_SELECTION) {
        state.message = `Select at most ${MAX_SELECTION} entries for one native edit.`;
        publish(); return false;
      }
      selectionRevision = page.revision; selectionTotal = page.total;
      if (checked) {
        state.selected.add(position);
        selectedItems.set(position, Object.freeze({ ...item }));
      } else {
        state.selected.delete(position); selectedItems.delete(position);
      }
      state.message = '';
      publish();
      return true;
    }

    function removable() {
      const page = ready();
      if (!page || !page.revision || !state.selected.size || state.selected.size > MAX_SELECTION
          || selectionRevision !== page.revision || selectionTotal !== page.total) return null;
      const items = [...state.selected].sort((a, b) => a - b).map(at => selectedItems.get(at));
      if (items.some((item, i) => !item || !identity(item.uri)
          || item.position !== items[0].position + i)) return null;
      return items;
    }

    function clearSelected() {
      if (disposed || !current() || state.writing || state.selecting) return false;
      clearSelection(); publish(); return true;
    }

    function editRange(first, last) {
      if (!ready() || typeof first !== 'string' || typeof last !== 'string') return false;
      clearSelection();
      state.rangeStart = first; state.rangeEnd = last;
      state.message = 'Range changed; read and select it before reviewing an edit.';
      publish(); return true;
    }

    async function selectRange(first, last) {
      const page = ready();
      if (!page || !page.revision || state.pending || reads.size) return false;
      clearSelection();
      const count = last - first + 1;
      if (!index(first) || first < 1 || !index(last) || last < first || last > page.total
          || count > MAX_SELECTION) {
        state.message = `Choose consecutive positions within this queue, at most ${MAX_SELECTION} entries. No command was sent.`;
        publish(); return false;
      }
      const version = ++epoch;
      const valid = () => available() && version === epoch && !state.stale && state.page === page;
      // Retain this slot through stale/close/suspend until the read settles.
      // A late range can neither select another queue nor start another read.
      state.selecting = true;
      state.rangeStart = String(first); state.rangeEnd = String(last);
      state.message = `Reading positions ${first}–${last} from the observed queue…`;
      publish();
      let flight;
      try {
        if (!valid()) return false;
        flight = (async () => call('queue', {
          params: { room: zone }, query: { offset: first - 1, limit: count },
        }))();
        reads.add(flight);
        const response = await flight;
        if (!valid()) return false;
        const range = pageOf(response, first - 1, count);
        if (range.revision !== page.revision || range.total !== page.total
            || range.items.length !== count || range.items.some(item => !identity(item.uri))
            || page.items.some(item => item.position >= first && item.position <= last
              && item.uri !== range.items[item.position - first].uri)) {
          throw new Error('The queue changed or the range is incomplete. Refresh and select it again.');
        }
        selectionRevision = range.revision; selectionTotal = range.total;
        for (const item of range.items) {
          state.selected.add(item.position);
          selectedItems.set(item.position, Object.freeze({ ...item }));
        }
        state.message = `${count} entries selected at positions ${first}–${last}. Review before moving or removing them; nothing was changed.`;
        return true;
      } catch (error) {
        if (valid()) {
          clearSelection(); state.stale = true;
          state.message = `${error.message}${error.hint ? ` ${error.hint}` : ''} No queue command was sent.`;
        }
        return false;
      } finally {
        if (flight) reads.delete(flight);
        state.selecting = false;
        publish();
      }
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

    // Everyday destinations use the same original-coordinate move review. A
    // row always refers to that single occurrence; the selection toolbar keeps
    // an explicitly selected contiguous block together, including across pages.
    function moveItems(position = null) {
      const page = ready();
      if (!page || !page.revision || state.pending) return null;
      if (position === null) return removable();
      const item = page.items.find(each => each.position === position);
      return item && identity(item.uri) ? [item] : null;
    }

    function moveOptions(position = null) {
      const items = moveItems(position);
      if (!items) return [];
      const first = items[0].position, after = first + items.length;
      const playing = state.page.playing;
      const currentInBlock = playing !== null && playing >= first && playing < after;
      return [
        { action: 'earlier', label: 'Move earlier', before: first - 1 },
        { action: 'later', label: 'Move later', before: after + 1 },
        { action: 'next', label: 'After current entry', before: playing === null || currentInBlock ? null : playing + 1 },
        { action: 'start', label: 'To start…', before: 1 },
        { action: 'end', label: 'To end…', before: state.page.total + 1 },
      ].map(option => ({ ...option, enabled: index(option.before) && option.before > 0
        && option.before <= state.page.total + 1 && (option.before < first || option.before > after) }));
    }

    function reviewReorder(action, position = null) {
      const items = moveItems(position);
      const option = moveOptions(position).find(each => each.action === action && each.enabled);
      if (!items || !option) return false;
      // Do not publish an intermediate selection. The only visible result is
      // the ordinary confirmation, bound to the precise source occurrence(s).
      clearSelection();
      selectionRevision = state.page.revision; selectionTotal = state.page.total;
      for (const item of items) {
        state.selected.add(item.position);
        selectedItems.set(item.position, Object.freeze({ ...item }));
      }
      state.destination = String(option.before);
      return reviewMove(option.before);
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
        clearSearch();
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
      clearSearch();
      ++epoch;
      state.stale = true;
      state.loading = state.updating = false;
      state.page = null;
      clearSelection();
      publish();
    }

    return { state, load, browse, jumpTo, showCurrent, play, add, select, selectRange, editRange, clearSelected, removable, reviewRemoval, cancelRemoval, remove,
      reviewMove, moveOptions, reviewReorder, move, search, editSearch, cancelSearch, openMatch, block, stale, reconcile, close,
      suspend() { active = false; stale('Queue updates paused while this page is hidden.'); },
      resume() { if (!disposed) { active = true; stale(); } },
      dispose() { disposed = true; close(); } };
  }

  /** Lazy, bounded browsing inside the existing room card. No background poll. */
  function panel({ zone, call, el, played, execute, blocked = () => false, current }) {
    const summary = el('summary', { text: 'Queue' });
    const body = el('div', { class: 'queue-body' });
    const node = el('details', { class: 'queue' }, summary, body);
    let shownFound = null, previousReview = null, reviewFocus = null;
    const control = model({ zone, call, played, execute, blocked, current, changed: render });
    const button = (text, action, disabled = false) => el('button', {
      type: 'button', text, disabled, onclick: action, 'data-focus': text,
    });

    function render(state) {
      const active = document.activeElement;
      const focus = body.contains(active) ? active.getAttribute('data-focus') : null;
      const returnFocus = focus ? active.getAttribute('data-return-focus') : null;
      // Every redraw rebuilds the panel's inputs: put the caret back where
      // it was in whichever one had focus, or the next digit typed into a
      // range position can land in front of the last ("2", "4" -> "42").
      const caret = focus !== null && typeof active.selectionStart === 'number'
        ? [active.selectionStart, active.selectionEnd] : null;
      const found = state.found !== null && state.found !== shownFound;
      shownFound = state.found;
      const scroll = body.querySelector('.queue-items')?.scrollTop || 0;
      const searchScroll = body.querySelector('.queue-search-results')?.scrollTop || 0;
      const leavingReview = previousReview && !state.pending;
      if (state.pending && !previousReview) reviewFocus = returnFocus || focus;
      previousReview = state.pending;
      draw(state);
      const list = body.querySelector('.queue-items');
      if (list) list.scrollTop = scroll;
      const results = body.querySelector('.queue-search-results');
      if (results) results.scrollTop = searchScroll;
      const key = found ? `found-${state.found}`
        : state.pending ? (state.pending.kind === 'move' ? 'Cancel move' : 'Cancel removal')
          : leavingReview ? reviewFocus : returnFocus || focus;
      const target = [...body.querySelectorAll('[data-focus]')]
        .find((each) => each.getAttribute('data-focus') === key);
      if (target && !target.disabled) {
        target.focus({ preventScroll: true });
        if (caret && key === focus && typeof target.setSelectionRange === 'function') {
          target.setSelectionRange(...caret);
        }
        if (found) target.scrollIntoView({ block: 'nearest', behavior: 'instant' });
      } else if (leavingReview || returnFocus || focus === 'Cancel queue search') summary.focus({ preventScroll: true });
    }

    function moveButtons(position, disabled, returnFocus) {
      const page = control.state.page;
      const options = control.moveOptions(position);
      return options.map(option => {
        const move = button(option.label, () => {
          if (control.state.page === page) return control.reviewReorder(option.action, position);
        }, disabled || !option.enabled);
        move.setAttribute('data-focus', `move-${position ?? 'selection'}-${option.action}`);
        if (returnFocus) move.setAttribute('data-return-focus', returnFocus);
        return move;
      });
    }

    function entryMoves(item, editable, ownsPage) {
      const key = `move-${item.position}`;
      const actions = el('div', { class: 'queue-move-options', role: 'group', hidden: true,
        'aria-label': `Move queue position ${item.position}: ${item.title || 'Untitled track'}` },
        ...moveButtons(item.position, !editable, key),
        el('p', { text: 'Choose a position, then review the move before the queue changes.' }));
      const toggle = button('Move…', () => {
        if (!editable || !ownsPage()) return;
        actions.hidden = !actions.hidden;
        toggle.setAttribute('aria-expanded', String(!actions.hidden));
        if (!actions.hidden) actions.scrollIntoView?.({ block: 'nearest', behavior: 'instant' });
      }, !editable || !identity(item.uri));
      toggle.setAttribute('aria-label', `Move queue position ${item.position}: ${item.title || 'Untitled track'}`);
      toggle.setAttribute('aria-expanded', 'false');
      toggle.setAttribute('data-focus', key);
      actions.addEventListener('keydown', event => {
        if (event.key === 'Escape') {
          event.preventDefault(); event.stopPropagation();
          actions.hidden = true; toggle.setAttribute('aria-expanded', 'false'); toggle.focus();
        }
      });
      return { toggle, actions };
    }

    function draw(state) {
      body.replaceChildren();
      body.setAttribute('aria-busy', String(state.loading || state.updating || state.writing || state.selecting || state.navigating || state.searching || blocked()));
      const busy = state.loading || state.writing || state.selecting || state.navigating || state.searching || blocked();
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
      drawSearch(state, page, busy, untrusted);
      const first = page.items[0];
      const last = page.items[page.items.length - 1];
      const range = first ? `${first.position}–${last.position} of ${page.total}` : `No entries at this position (${page.total} total)`;
      body.append(el('p', { text: `${range}. ${page.playing ? `Queue position ${page.playing} is current.` : 'Not playing from this queue.'}` }));
      const position = el('input', { type: 'text', inputmode: 'numeric', pattern: '[0-9]*', maxlength: 10,
        class: 'queue-position', value: state.jump, disabled: busy || untrusted,
        'aria-label': 'Queue position to show', 'data-focus': 'jump' });
      const canJump = () => !busy && !untrusted && positionOf(position.value) !== null && positionOf(position.value) <= page.total;
      const jump = button('Go to position', () => { if (ownsPage()) return control.jumpTo(positionOf(state.jump)); }, !canJump());
      position.addEventListener('input', () => { state.jump = position.value; jump.disabled = !canJump(); });
      body.append(el('div', { class: 'row' }, el('label', {}, 'Show position ', position), jump,
        button('Show current entry', () => control.showCurrent(), busy || untrusted),
        button('Show selection start', () => control.jumpTo(Math.min(...state.selected)), busy || untrusted || !state.selected.size)));
      if (page.revision) {
        const disabled = busy || untrusted;
        const firstInput = el('input', { type: 'text', inputmode: 'numeric', pattern: '[0-9]*', maxlength: 10,
          class: 'queue-position', value: state.rangeStart, disabled,
          'aria-label': 'First range position', 'data-focus': 'range-first' });
        const lastInput = el('input', { type: 'text', inputmode: 'numeric', pattern: '[0-9]*', maxlength: 10,
          class: 'queue-position', value: state.rangeEnd, disabled,
          'aria-label': 'Last range position', 'data-focus': 'range-last' });
        const edit = () => { if (ownsPage()) control.editRange(firstInput.value, lastInput.value); };
        firstInput.addEventListener('input', edit); lastInput.addEventListener('input', edit);
        const from = positionOf(state.rangeStart), to = positionOf(state.rangeEnd);
        body.append(el('div', { class: 'queue-range', role: 'group', 'aria-label': 'Select across queue pages' },
          el('p', { text: `Select up to ${MAX_SELECTION} consecutive entries, including across pages. Choose the boundaries yourself; titles do not establish a whole work.` }),
          el('div', { class: 'row' }, el('label', {}, 'First ', firstInput), el('label', {}, 'Last ', lastInput),
            button('Read and select range', () => { if (ownsPage()) return control.selectRange(from, to); },
              !editable || from === null || to === null || from > to || to > page.total || to - from + 1 > MAX_SELECTION))));
      }
      if (state.selected.size) {
        const elsewhere = [...state.selected].filter(at => !page.items.some(item => item.position === at)).length;
        body.append(el('p', { text: `${state.selected.size} selected; ${elsewhere} outside this page. The confirmation lists every selected title.` }),
          button('Clear selection', () => control.clearSelected(), busy));
        const moves = moveButtons(null, !editable);
        if (moves.length) body.append(el('section', { class: 'queue-selection-moves',
          'aria-label': 'Move selected queue entries' },
          el('strong', { text: state.selected.size === 1 ? 'Move selected entry' : `Move ${state.selected.size} entries together` }),
          el('div', { class: 'row' }, ...moves)));
      }
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
        const moves = entryMoves(item, editable, ownsPage);
        list.append(el('li', { 'aria-current': current ? 'true' : null,
          class: state.found === item.position ? 'queue-found' : null,
          tabindex: -1, 'data-focus': `found-${item.position}`,
          'aria-label': state.found === item.position ? `Found queue entry ${item.position}: ${title}` : null },
          ...(state.found === item.position ? [el('strong', { text: 'Found result' })] : []),
          el('div', { class: 'track' },
            el('div', { class: 'title', text: `${current ? '▶ ' : ''}${title}` }),
            el('div', { class: 'by', text: [item.creator, item.album].filter(Boolean).join(' · ') })),
          el('div', { class: 'row queue-entry-actions' }, el('label', { class: 'queue-select' }, select, 'Select'), play, moves.toggle),
          moves.actions));
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
            el('div', { class: 'row' }, reviewMove));
        }
        if (state.selected.size && !control.removable()) {
          body.append(el('p', { text: 'Select consecutive entries to move or remove them together. A stale queue needs a fresh read.' }));
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
        button('Previous page', () => control.browse(Math.max(0, state.offset - PAGE_SIZE)), busy || untrusted || state.offset === 0),
        button('Next page', () => control.browse(last.position), busy || untrusted || !last || last.position >= page.total)));
      body.append(el('p', { class: 'empty', text: 'Browsing keeps up to 100 selected identities only while the queue revision agrees. Refresh queue clears the selection; navigation cancels any earlier confirmation.' }));
    }

    function drawSearch(state, page, busy, untrusted) {
      const usable = Boolean(page.revision && !busy && !untrusted);
      const query = el('input', { type: 'text', value: state.query, maxlength: 256,
        disabled: !usable, class: 'queue-source', 'data-focus': 'queue-search',
        'aria-label': 'Search queue titles, artists or albums', placeholder: 'Title, artist or album' });
      const run = () => { if (body.contains(query)) void control.search(state.query); };
      const submit = button('Search queue', run, !usable || !state.query.trim());
      query.addEventListener('input', event => {
        if (!event.isComposing && body.contains(query)) control.editSearch(query.value);
      });
      query.addEventListener('compositionend', () => {
        if (body.contains(query)) control.editSearch(query.value);
      });
      query.addEventListener('keydown', event => {
        if (event.key === 'Enter' && !event.isComposing) { event.preventDefault(); run(); }
      });
      const search = el('section', { class: 'queue-search', role: 'search', 'aria-label': 'Find music in this queue' },
        el('strong', { text: 'Find music in this queue' }),
        el('p', { text: 'Search every word across titles, artists and albums, ignoring case and accents. This inspects queued music; it does not add or play anything.' }),
        query, el('div', { class: 'row' }, submit,
          button('Cancel queue search', () => control.cancelSearch(), !state.searching || !state.search)));
      const result = state.search;
      if (result) {
        const coverage = result.scanned > result.start
          ? `Positions ${result.start + 1}–${result.scanned} of ${result.total} examined in this section.`
          : 'No entries examined yet.';
        search.append(el('p', { text: `“${result.query}”: ${result.matches.length} matches. ${coverage}` }));
        if (!result.running) {
          const end = result.complete ? (result.start === 0 ? 'The entire observed queue was searched.' : 'This section reached the end of the queue.')
            : 'More entries remain. Search the next section explicitly; these results are not the whole queue.';
          search.append(el('p', { text: end }));
          if (!result.matches.length) search.append(el('p', { text: 'No matches in the examined section.' }));
        }
        if (result.matches.length) {
          const list = el('ol', { class: 'queue-search-results', 'aria-label': 'Matching queue occurrences' });
          for (const item of result.matches) {
            const show = button(`Show position ${item.position}`, () => { void control.openMatch(item); },
              !usable || result.running || !identity(item.uri));
            show.setAttribute('aria-label', `Show queue position ${item.position}: ${item.title || 'Untitled track'}`);
            list.append(el('li', { value: item.position }, el('strong', { text: item.title || 'Untitled track' }),
              el('p', { text: [item.creator, item.album].filter(Boolean).join(' · ') || 'Artist and album unavailable' }), show,
              ...(!identity(item.uri) ? [el('p', { text: 'No renderer identity was reported; this result cannot open an actionable queue entry.' })] : [])));
          }
          search.append(list);
        }
        if (!result.running && !result.complete) search.append(button('Search next section',
          () => { void control.search(result.query, result); }, !usable));
      }
      search.append(el('p', { class: 'empty', text: `Each request examines at most ${SEARCH_REQUESTS * SEARCH_PAGE} entries in ${SEARCH_REQUESTS} page reads and keeps ${SEARCH_MATCHES} matches. Show position rechecks that exact occurrence before opening the ordinary queue controls.` }));
      body.append(search);
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

  return { model, panel, PAGE_SIZE, MAX_SELECTION, SEARCH_PAGE, SEARCH_REQUESTS, SEARCH_MATCHES };
})();
