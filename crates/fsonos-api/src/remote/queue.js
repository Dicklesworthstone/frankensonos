// Revision-bound queue browsing. Served before remote.js in the same asset.
'use strict';

const QueueRemote = (() => {
  const PAGE_SIZE = 25;
  const MAX_INDEX = 4294967295;
  const nonblank = (value) => typeof value === 'string' && value.trim() !== '';
  const identity = (value) => nonblank(value) && value !== 'NOT_IMPLEMENTED';
  const index = (value) => Number.isInteger(value) && value >= 0 && value <= MAX_INDEX;

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
  function model({ zone, call, changed = () => {}, played = () => {} }) {
    let epoch = 0;
    let disposed = false;
    const state = { offset: 0, page: null, loading: false, writing: false, message: '' };
    const publish = () => { if (!disposed) changed(state); };
    const ready = () => !disposed && !state.loading && !state.writing && state.page;

    async function load(offset = state.offset, keepMessage = false) {
      if (disposed || state.writing) return false;
      if (!index(offset) || offset > MAX_INDEX - PAGE_SIZE) return false;
      const request = ++epoch;
      state.offset = offset;
      state.page = null;
      state.loading = true;
      if (!keepMessage) state.message = '';
      publish();
      try {
        const response = await call('queue', { params: { room: zone }, query: { offset, limit: PAGE_SIZE } });
        if (disposed || request !== epoch) return false;
        state.page = pageOf(response, offset);
        return true;
      } catch (error) {
        if (!disposed && request === epoch) state.message = error.hint ? `${error.message} ${error.hint}` : error.message;
        return false;
      } finally {
        if (!disposed && request === epoch) {
          state.loading = false;
          publish();
        }
      }
    }

    async function play(position) {
      const page = ready();
      const item = page && page.items.find((each) => each.position === position);
      if (!item || !identity(item.uri) || !page.revision) return false;
      const body = { zone, position, expected_uri: item.uri, expected_revision: page.revision };
      state.writing = true;
      state.page = null;
      state.message = 'Starting the selected queue entry…';
      ++epoch;
      publish();
      let success = false;
      try {
        const result = await call('queuePlay', { body });
        if (!result || typeof result.done !== 'string' || typeof result.changed !== 'boolean') {
          throw new Error('Playback returned an unreadable result. Its outcome is uncertain.');
        }
        const notes = (result.notes || []).filter((note) => note && typeof note.detail === 'string');
        state.message = [result.done, ...notes.map((note) => note.detail)].join(' ');
        success = true;
      } catch (error) {
        state.message = `${error.message}${error.hint ? ` ${error.hint}` : ''} Refresh before trying again; the request was not repeated.`;
      } finally {
        state.writing = false;
        publish();
      }
      if (!disposed) {
        played();
        // Only refresh reads after success; an uncertain write needs an explicit fresh read.
        if (success) await load(state.offset, true);
      }
      return success;
    }

    return { state, load, play, dispose() { disposed = true; ++epoch; } };
  }

  /** Lazy, bounded browsing inside the existing room card. No background poll. */
  function panel({ zone, call, el, played }) {
    const summary = el('summary', { text: 'Queue' });
    const body = el('div', { class: 'queue-body' });
    const node = el('details', { class: 'queue' }, summary, body);
    const control = model({ zone, call, played, changed: render });
    const button = (text, action, disabled = false) => el('button', {
      type: 'button', text, disabled, onclick: action,
    });

    function render(state) {
      body.replaceChildren();
      body.setAttribute('aria-busy', String(state.loading || state.writing));
      const busy = state.loading || state.writing;
      body.append(el('div', { class: 'row' }, button('Refresh queue', () => control.load(), busy)));
      if (state.message) body.append(el('p', { role: 'status', 'aria-live': 'polite', text: state.message }));
      if (state.loading) body.append(el('p', { text: 'Reading queue…' }));
      const page = state.page;
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
        const play = button('Play', () => control.play(item.position), busy || !page.revision || !identity(item.uri));
        play.setAttribute('aria-label', `Play queue position ${item.position}: ${title}`);
        list.append(el('li', { 'aria-current': current ? 'true' : null },
          el('div', { class: 'track' },
            el('div', { class: 'title', text: `${current ? '▶ ' : ''}${title}` }),
            el('div', { class: 'by', text: [item.creator, item.album].filter(Boolean).join(' · ') })),
          play));
      }
      body.append(list, el('div', { class: 'row' },
        button('Previous page', () => control.load(Math.max(0, state.offset - PAGE_SIZE)), busy || state.offset === 0),
        button('Next page', () => control.load(last.position), busy || !last || last.position >= page.total)));
      body.append(el('p', { class: 'empty', text: 'This is a queue snapshot. Refresh after changes from another controller.' }));
    }

    node.addEventListener('toggle', () => { if (node.open) control.load(); });
    render(control.state);
    return { node, dispose: control.dispose };
  }

  return { model, panel, PAGE_SIZE };
})();
