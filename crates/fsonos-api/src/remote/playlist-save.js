// The phone remote shares the dashboard's reviewed native queue-save workflow.
'use strict';
const PlaylistSaveRemote = (() => {
  const C = typeof DashboardCore !== 'undefined' ? DashboardCore : require('./dashboard-core.js');
  const Panel = typeof DashboardPlaylistSavePanel !== 'undefined' ? DashboardPlaylistSavePanel : require('./dashboard-playlist-save-panel.js');
  const routes = Object.freeze({
    nativePlaylistSave: ['POST', '/sonos/playlists/save'],
    nativePlaylistCatalog: ['GET', '/sonos/playlists'],
  });
  function client(call) {
    return (url, body, options = {}) => {
      const signal = options.signal;
      if (url === '/zones' && body === undefined) return call('zones', { signal });
      const state = /^\/zones\/([^/]+)\/state$/.exec(url);
      if (state && body === undefined) return call('state', { params: { room: decodeURIComponent(state[1]) }, signal });
      const queue = /^\/zones\/([^/]+)\/queue\?offset=0&limit=1$/.exec(url);
      if (queue && body === undefined) return call('queue', { params: { room: decodeURIComponent(queue[1]) },
        query: { offset: 0, limit: 1 }, signal });
      const catalog = /^\/sonos\/playlists\?zone=([^&]+)$/.exec(url);
      if (catalog && body === undefined) return call('nativePlaylistCatalog', { query: { zone: decodeURIComponent(catalog[1]) }, signal });
      if (url === '/sonos/playlists/save' && body !== undefined) return call('nativePlaylistSave', { body });
      throw new Error('The queue-save request is not supported by this remote.');
    };
  }

  // Keep this controller for the whole page, not for a disposable room card.
  // An uncertain native save belongs to its original qualified destination.
  function bridge({ call, cards, current, busy, begin, end, changed = () => {}, now, demo = false }) {
    let held = null, lifetime = 0, disposed = false;
    const state = { opening: false, target: '', message: '' };
    const publish = () => { if (!disposed) { try { changed(); } catch (_) { /* Presentation only. */ } } };
    const request = client(call);
    const base = C.model({ call: request, changed: publish, now, demo });
    const card = () => {
      const item = base.selected();
      return item ? [...cards()].find(parts => current(parts) && C.shape(parts.zone) === C.shape(item.zone)) : null;
    };
    const controller = { ...base,
      canControl() {
        const parts = card();
        return !!(!state.opening && base.canControl() && parts && !parts.writing && !busy(parts));
      },
      async mutate(operation, options) {
        if (!controller.canControl()) return false;
        const parts = card(), signature = C.shape(parts.zone);
        held = parts;
        if (!begin(parts)) { held = null; state.message = 'Another command is using this group. Read its queue and review again.'; publish(); return false; }
        try {
          return await base.mutate(context => {
            const check = () => {
              context.check();
              if (!current(parts) || C.shape(parts.zone) !== signature || card() !== parts) {
                throw new Error('The original listening group changed. Read its queue and review again.');
              }
            };
            return operation({ ...context, check, send(url, body, init) {
              check(); return context.send(url, body, init);
            } });
          }, options);
        } finally {
          // Closing the dialog or replacing its card cannot release this lane
          // while a submitted request is still outstanding.
          held = null; end(parts); publish();
        }
      },
    };
    async function choose(zone) {
      if (disposed || state.opening || !base.state.active || base.state.busy) return false;
      let expected;
      try { expected = C.zones([zone])[0]; }
      catch (error) { state.message = C.problem(error); publish(); return false; }
      const origin = [...cards()].find(parts => current(parts) && C.shape(parts.zone) === C.shape(expected));
      if (!origin) { state.message = 'The original group is no longer available. Choose its current queue.'; publish(); return false; }
      const version = lifetime;
      state.target = C.key(expected); state.opening = true;
      state.message = 'Reading the selected listening group…'; publish();
      try {
        // A playback read never acknowledges a possibly completed catalog write.
        await base.refresh(false);
        if (disposed || version !== lifetime || !base.state.active) return false;
        const found = base.state.items.find(item => C.shape(item.zone) === C.shape(expected));
        if (!current(origin) || !found?.fresh || !base.state.topologyFresh
            || !base.select(C.key(found.zone)) || card() !== origin) {
          state.message = 'The selected group is unavailable or changed. Refresh rooms and choose its queue again.';
          return false;
        }
        state.message = ''; return true;
      } finally {
        state.opening = false; publish();
      }
    }
    function invalidate(parts = null) {
      if (disposed) return;
      if (parts && base.state.selected !== C.key(parts.zone)) return;
      base.invalidate(parts ? [C.key(parts.zone)] : null);
    }
    return { state, controller, call: request, choose,
      canPrepareRead() {
        const parts = card();
        return !!(!demo && !disposed && !state.opening && base.state.active && !base.state.busy
          && !base.state.loading && !base.state.uncertain && parts && !parts.writing && !busy(parts));
      },
      observe(parts) {
        if (disposed || held === parts) return;
        if (base.state.selected === C.key(parts.zone)
            && (!current(parts) || parts.writing || busy(parts) || card() !== parts)) invalidate(parts);
        else publish();
      },
      invalidate,
      suspend() { ++lifetime; base.suspend(); },
      resume() { if (!disposed) base.resume(); },
      dispose() { ++lifetime; disposed = true; base.dispose(); },
    };
  }

  function mount({ call, cards, register, current, busy, begin, end, el,
    document: doc = document, now }) {
    const host = doc.getElementById('households');
    if (!host) return null;
    const trigger = el('button', { id: 'open-playlist-save', type: 'button',
      text: 'Review the last queue save', disabled: true, 'aria-haspopup': 'dialog',
      'aria-controls': 'playlist-save-dialog' });
    const summary = el('summary', { text: 'Save queues as Sonos playlists' });
    const status = el('p', { class: 'by', role: 'status', 'aria-live': 'polite' });
    const details = el('details', { class: 'playlist-save-remote' }, summary,
      el('p', { text: 'Open a listening group’s Queue and choose Save queue as playlist. The result stays here even if that group changes or goes offline.' }),
      trigger, status);
    const section = el('section', { class: 'scenes', 'aria-label': 'Save queues as Sonos playlists' }, details);
    const dialog = el('dialog', { id: 'playlist-save-dialog', class: 'playlist-save-dialog playlist-save-remote-dialog',
      'aria-labelledby': 'playlist-save-title' });
    host.before(section);
    doc.body.append(dialog);
    let panel = null, disposed = false, studioScope = null;
    const entries = new Set(), listeners = [];
    const on = (node, event, fn) => { node?.addEventListener(event, fn); if (node) listeners.push([node, event, fn]); };
    const demo = doc.body.dataset.preview === 'true';
    const event = (type, detail, cancelable = false) => new (doc.defaultView?.CustomEvent || CustomEvent)(type, { detail, cancelable });
    function update() {
      if (!panel || disposed) return;
      const result = panel.control.state.result;
      trigger.textContent = result?.status === 'uncertain' ? 'Check the last save outcome' : 'Review the last queue save';
      status.textContent = adapter.state.message || (result ? result.detail : 'No queue has been selected for saving.');
      for (const entry of entries) {
        entry.button.disabled = demo || adapter.state.opening || adapter.controller.state.busy
          || !adapter.controller.state.active || !current(entry.parts) || entry.parts.writing || busy(entry.parts);
        entry.notice.textContent = adapter.state.target === C.key(entry.parts.zone) ? adapter.state.message : '';
      }
    }
    const adapter = bridge({ call, cards, current, busy, begin, end, now, demo,
      changed() { panel?.observe(); update(); } });
    panel = Panel.panel({ call: adapter.call, controller: adapter.controller, document: doc, now, demo, changed: update,
      canPrepareRead: adapter.canPrepareRead,
      prepareRead: () => adapter.choose(adapter.controller.selected().zone),
      saved() {
        const group = panel.control.state.result?.group;
        doc.dispatchEvent(event('fsonos:catalog-changed', {
          household: group?.household, playlists: true, favorites: false,
        }));
      },
      async browse(receipt) {
        const handoff = { selection: { target: receipt.target, signature: receipt.group.signature,
          playlist: receipt.playlist }, response: null };
        if (doc.dispatchEvent(event('fsonos:playlist-browse', handoff, true))) return false;
        return await handoff.response === true ? true
          : 'The source editor could not select the saved playlist. Check its message or clear its existing draft, then browse again. The save receipt is unchanged.';
      },
    });
    register(parts => {
      const button = el('button', { type: 'button', text: 'Save queue as playlist', 'aria-haspopup': 'dialog',
        'aria-controls': 'playlist-save-dialog' });
      const notice = el('p', { class: 'by', role: 'status', 'aria-live': 'polite' });
      const node = el('div', { class: 'queue-save-action' }, button, notice);
      const entry = { parts, button, notice }; entries.add(entry);
      const open = async () => {
        if (button.disabled || disposed) return;
        if (await adapter.choose(parts.zone)) panel.open();
      };
      button.addEventListener('click', open);
      // Queue redraws replace their body, so this action is a stable sibling.
      parts.queue.node.append(node);
      adapter.observe(parts);
      return { node: el('span', { hidden: true }),
        observe() { adapter.observe(parts); },
        block() { adapter.observe(parts); },
        stale() { adapter.invalidate(parts); },
        dispose() { entries.delete(entry); button.removeEventListener('click', open); adapter.invalidate(parts); },
      };
    });
    const suspend = () => { adapter.suspend(); panel.suspend(); };
    const resume = () => { if (!doc.hidden) { adapter.resume(); panel.resume(); } };
    on(doc, 'visibilitychange', () => { if (doc.hidden) suspend(); else resume(); });
    on(doc.defaultView, 'pagehide', suspend);
    on(doc.defaultView, 'pageshow', resume);
    on(doc, 'fsonos:catalog-changed', event => {
      panel.control.catalogChanged(event.detail);
    });
    on(doc, 'fsonos:studio-selection', event => {
      const next = JSON.stringify([event.detail?.key, event.detail?.workspace]);
      if (studioScope !== null && next !== studioScope) { adapter.invalidate(); panel.control.close(); }
      studioScope = next;
    });
    if (doc.hidden) suspend();
    return { adapter, panel, node: section,
      dispose() {
        if (disposed) return;
        panel.dispose(); adapter.dispose(); disposed = true;
        for (const [node, event, fn] of listeners) node.removeEventListener(event, fn);
      },
    };
  }
  return { routes, client, bridge, mount };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = PlaylistSaveRemote;
else if (typeof registerCardPanel === 'function') {
  Object.assign(ROUTES, PlaylistSaveRemote.routes);
  PlaylistSaveRemote.mount({ call, cards: () => house.cards.values(), register: registerCardPanel,
    current: ownsCard, busy: transportBusy, begin: beginAction, end: endAction, el });
}
