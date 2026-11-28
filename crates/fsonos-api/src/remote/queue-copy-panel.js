// Copy reviewed native queue occurrences through each remote's shared command lane.
'use strict';
const QueueCopyPanel = (() => {
  const Q = typeof QueueCopyRemote !== 'undefined' ? QueueCopyRemote : require('./queue-copy.js');
  const T = typeof SourceTargetsRemote !== 'undefined' ? SourceTargetsRemote : require('./source-targets.js');

  function panel({ call, executeSet, document: doc = document, canOpen = () => true }) {
    const el = (tag, text, className) => {
      const node = doc.createElement(tag);
      if (text != null) node.textContent = text;
      if (className) node.className = className;
      return node;
    };
    const node = el('section', null, 'queue-copy-panel');
    node.hidden = true; node.setAttribute('aria-label', 'Copy selected queue tracks');
    const title = el('h3', 'Copy selected queue tracks');
    const source = el('p');
    const help = el('p', 'Choose the destination, order and action, then review every affected group. Copying leaves the original queue and playback intact unless its group is also a destination.');
    const fields = el('div', null, 'queue-copy-fields');
    const destinationLabel = el('label', 'Destination room or room set');
    const destination = el('input'); destination.type = 'text'; destination.maxLength = 1024;
    destination.setAttribute('aria-label', 'Queue copy destination');
    destination.placeholder = 'Room, saved alias, or all@household';
    destinationLabel.append(destination);
    const actionLabel = el('label', 'Action');
    const action = el('select'); action.setAttribute('aria-label', 'Queue copy action');
    for (const [value, label] of [['play', 'Play now · replace destination queues'], ['next', 'Play next · keep current playback'], ['end', 'Append · keep current playback']]) {
      const option = el('option', label); option.value = value; action.append(option);
    }
    actionLabel.append(action); fields.append(destinationLabel, actionLabel);
    const tracks = el('ol', null, 'queue-copy-tracks'); tracks.setAttribute('aria-label', 'Tracks to copy in order');
    const controls = el('div', null, 'queue-copy-actions');
    const button = (label, run) => {
      const button = el('button', label); button.type = 'button'; button.addEventListener('click', run); return button;
    };
    const review = button('Review copy', () => { if (usable()) void control.review(); });
    const close = button('Close copy', () => { if (!control.state.writing) control.close(); });
    controls.append(review, close);
    const message = el('p'); message.setAttribute('role', 'status'); message.setAttribute('aria-live', 'polite');
    const consent = el('section', null, 'queue-copy-review'); consent.hidden = true;
    consent.setAttribute('aria-label', 'Review queue copy'); consent.tabIndex = -1;
    const result = el('p', null, 'queue-copy-result'); result.setAttribute('role', 'status');
    result.setAttribute('aria-live', 'polite');
    node.append(title, source, help, fields, tracks, controls, message, consent, result);
    let disposed = false, shownSelection = null, shownPending = null, enabled = false;
    const visible = () => !disposed && !doc.hidden && !node.hidden
      && !node.parentElement?.closest?.('[hidden], [inert]');
    const usable = () => visible() && control.state.open;
    const checkedCall = async (...args) => {
      if (!usable()) throw new Error('The queue-copy view closed or became hidden. Review the selection again.');
      const value = await call(...args);
      // A write acknowledgement belongs to its command even after navigation.
      if (!args[1]?.body && !usable()) throw new Error('The queue-copy view changed during its read. Review again.');
      return value;
    };
    const control = Q.model({ call: checkedCall, executeSet, changed: render });
    destination.addEventListener('input', event => { if (!event.isComposing && usable()) control.edit('target', destination.value); });
    destination.addEventListener('compositionend', () => { if (usable()) control.edit('target', destination.value); });
    action.addEventListener('change', () => { if (usable()) control.edit('mode', action.value); });

    function render(state) {
      if (disposed) return;
      node.hidden = !state.open && !state.result;
      node.setAttribute('aria-busy', String(state.reading || state.writing));
      source.textContent = state.selection ? `Original queue: ${state.selection.zone} · ${state.selection.tracks.length} selected occurrences` : '';
      source.hidden = help.hidden = fields.hidden = tracks.hidden = controls.hidden = !state.open;
      if (destination.value !== state.target) destination.value = state.target || '';
      action.value = state.mode;
      const busy = state.reading || state.writing;
      destination.disabled = action.disabled = busy || !!state.pending;
      review.disabled = busy || !!state.pending || !state.selection?.tracks.length || !state.target?.trim();
      close.disabled = state.writing;
      message.textContent = state.message || '';
      result.textContent = state.result?.detail || '';
      result.hidden = !state.result;
      const editable = !busy && !state.pending;
      if (shownSelection !== state.selection || enabled !== editable) {
        shownSelection = state.selection; enabled = editable;
        tracks.replaceChildren();
        for (const [index, track] of (state.selection?.tracks || []).entries()) {
          const selection = state.selection, row = el('li');
          row.append(el('strong', `#${track.position} · ${track.title || 'Untitled track'}`));
          const description = [track.creator, track.album].filter(Boolean).join(' · ');
          if (description) row.append(el('p', description));
          const changes = el('div', null, 'queue-copy-actions');
          for (const [name, label, allowed] of [['up', 'Move up', index > 0], ['down', 'Move down', index + 1 < selection.tracks.length],
            ['repeat', 'Repeat', selection.tracks.length < 10], ['remove', 'Remove', true]]) {
            const change = button(label, () => {
              if (usable() && control.state.selection === selection) control.changeSelection(name, index);
            });
            change.disabled = !editable || !allowed;
            change.setAttribute('aria-label', `${label} copied occurrence ${index + 1}: ${track.title || 'Untitled track'}`);
            changes.append(change);
          }
          row.append(changes); tracks.append(row);
        }
      }
      if (shownPending !== state.pending) {
        shownPending = state.pending; consent.replaceChildren(); consent.hidden = !state.pending;
        if (state.pending) {
          const pending = state.pending;
          consent.append(el('h4', 'Review destinations'), el('p', `Copy the ${state.selection.tracks.length} occurrences above in exactly that order from ${state.selection.zone}.`));
          const groups = el('ul');
          for (const detail of T.reviewGroups(pending.targets)) groups.append(el('li', detail));
          consent.append(groups, el('p', pending.mode === 'play'
            ? 'This replaces the destination queues and starts their first selected track with shuffle and repeat off.'
            : 'This adds the selection without starting or resuming playback.'));
          const actions = el('div', null, 'queue-copy-actions');
          const cancel = button('Cancel copy review', () => { if (usable() && control.state.pending === pending) control.cancel(); });
          const confirm = button(pending.mode === 'play' ? 'Copy and play' : 'Copy into queues', () => {
            if (usable() && control.state.pending === pending) void control.confirm(pending);
          });
          actions.append(cancel, confirm); consent.append(actions);
          if (visible()) { cancel.focus?.({ preventScroll: true }); consent.scrollIntoView?.({ block: 'nearest' }); }
        }
      }
    }

    function open(value) {
      if (disposed || doc.hidden || !canOpen() || node.parentElement?.closest?.('[hidden], [inert]')
          || control.state.reading || control.state.writing || control.state.pending) return null;
      let selection;
      try { selection = Q.selectionOf(value); } catch (_) { return null; }
      node.hidden = false;
      const opening = control.open(selection);
      node.scrollIntoView?.({ block: 'nearest' }); destination.focus?.({ preventScroll: true });
      return opening;
    }
    const offer = event => {
      if (!event.detail || typeof event.detail !== 'object') return;
      const opening = open(event.detail.selection);
      if (opening !== null) { event.preventDefault(); event.detail.response = opening; }
    };
    const suspend = () => control.suspend();
    const resume = () => { if (!doc.hidden) control.resume(); };
    const visibility = () => doc.hidden ? suspend() : resume();
    doc.addEventListener('fsonos:queue-selection', offer);
    doc.addEventListener('visibilitychange', visibility);
    const win = doc.defaultView;
    win?.addEventListener('pagehide', suspend); win?.addEventListener('pageshow', resume);
    render(control.state);
    return { node, state: control.state, open, suspend, resume,
      dispose() {
        disposed = true; control.dispose();
        doc.removeEventListener('fsonos:queue-selection', offer); doc.removeEventListener('visibilitychange', visibility);
        win?.removeEventListener('pagehide', suspend); win?.removeEventListener('pageshow', resume);
      } };
  }

  function dashboard({ call, controller, completed = () => {}, canOpen = () => true, document: doc = document }) {
    let lane = null, view;
    const P = typeof SourcePlayerRemote !== 'undefined' ? SourcePlayerRemote : require('./source-player.js');
    const routes = { ...P.routes, ...T.routes, ...Q.routes, zones: ['GET', '/zones'], state: ['GET', '/zones/{room}/state'] };
    view = panel({ document: doc, canOpen: () => canOpen() && controller.canControl(),
      call: async (route, options = {}) => {
        const [method, template] = routes[route] || [];
        if (!template) throw new Error('The queue-copy operation is unavailable. Reload this page.');
        const path = template.replace(/\{(\w+)\}/g, (_, key) => encodeURIComponent(options.params?.[key]));
        const url = path + (options.query ? '?' + new URLSearchParams(options.query) : '');
        if (method === 'POST') {
          if (!lane) throw new Error('Review the queue copy before sending it.');
          return lane.send(url, options.body);
        }
        const held = lane; held?.check();
        const value = await call(url, undefined, options.signal ? { signal: options.signal } : undefined);
        held?.check(); return value;
      },
      async executeSet(groups, work) {
        let receipt;
        const ok = await controller.mutate(async context => {
          lane = context;
          try {
            receipt = await work();
            return { done: receipt.detail, changed: receipt.changed };
          } finally { lane = null; }
        });
        if (!ok) throw new Error(controller.state.message || 'The queue-copy command was not confirmed. Read the queues before another action.');
        try { await completed(groups.filter(group => !group.sourceOnly).map(group => group.target)); } catch (_) { /* Receipt remains authoritative. */ }
        return receipt;
      } });
    return view;
  }
  return { panel, dashboard };
})();

if (typeof module !== 'undefined' && module.exports) module.exports = QueueCopyPanel;
else if (typeof ROUTES !== 'undefined' && typeof house !== 'undefined') {
  Object.assign(ROUTES, QueueCopyRemote.routes);
  const host = document.getElementById('households');
  if (host) {
    const levels = RoomSetsRemote.levelLanes({ cards: () => house.cards.values(), flights: volumeFlights });
    registerCardPanel(parts => {
      const observe = () => levels.observe(parts); observe();
      return { node: el('span', { hidden: true }), observe, block: observe };
    });
    const copy = QueueCopyPanel.panel({ call,
      executeSet: RoomSetsRemote.coordinated({ cards: () => house.cards.values(), refresh: loadZones,
        begin: beginAction, end: endAction, shape: groupShape, levels }) });
    host.before(copy.node);
  }
}
