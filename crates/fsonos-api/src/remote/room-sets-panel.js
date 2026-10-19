// The room-set editor uses the ordinary policy-checked command routes.
'use strict';
const RoomSetsPanel = (() => {
  function mount({ api = RoomSetsRemote, call, el, execute, document: doc = document }) {
    const input = el('input', { type: 'text', autocomplete: 'off', spellcheck: 'false',
      'aria-label': 'Room or alias to control', placeholder: 'Room, alias, here, or all@S1' });
    const suggestions = el('select', { 'aria-label': 'Known room-set suggestions' });
    const action = el('select', { 'aria-label': 'Collective room action' });
    for (const [value, label] of Object.entries(api.actions)) action.append(el('option', { value, text: label }));
    const amount = el('input', { type: 'number', min: 0, max: 100, step: 1, value: '5',
      'aria-label': 'Volume level or step' });
    const amountLabel = el('label', { hidden: true }, el('span', { text: 'Volume level or step' }), amount);
    const refresh = el('button', { type: 'button', text: 'Read room suggestions' });
    const review = el('button', { type: 'button', text: 'Resolve and review' });
    const status = el('p', { role: 'status', 'aria-live': 'polite' });
    const receipt = el('p', { role: 'status', 'aria-live': 'polite' });
    const consent = el('div', { class: 'room-set-review', hidden: true });
    const details = el('details', { class: 'zone room-set-controls' },
      el('summary', { text: 'Control a room set or household' }),
      el('div', {},
        el('p', { text: 'Pause or resume several independent groups, or change the selected rooms’ mute and volume, in one daemon action. This does not group rooms together.' }),
        el('label', {}, 'Known targets (suggestions only)', suggestions),
        el('label', {}, 'Target', input), refresh,
        el('label', {}, 'Action', action), amountLabel, review, status, consent, receipt));
    const node = el('section', { id: 'room-set-controls', class: 'scenes', 'aria-label': 'Room sets and household controls' }, details);
    const control = api.model({ call, execute, changed: render });
    let shownSuggestions = null, shown = null;
    function render(s) {
      if (shownSuggestions !== s.suggestions) {
        shownSuggestions = s.suggestions;
        suggestions.replaceChildren(el('option', { value: '', text: 'Choose a suggestion or type a target…' }));
        for (const entry of s.suggestions) suggestions.append(el('option', { value: entry.query, text: entry.label }));
      }
      for (const [field, value] of [[input, s.input], [action, s.action], [amount, s.amount]]) if (field.value !== value) field.value = value;
      const busy = s.reading || s.writing;
      for (const field of [input, action, suggestions, amount]) field.disabled = s.writing;
      refresh.disabled = busy; review.disabled = busy || !s.input.trim();
      amountLabel.hidden = !['lower', 'raise', 'volume'].includes(s.action);
      amount.min = s.action === 'volume' ? 0 : 1;
      amountLabel.firstChild.textContent = s.action === 'volume' ? 'Volume for each room (0–100)' : 'Change each room by (1–100)';
      node.setAttribute('aria-busy', String(busy));
      status.textContent = s.message || (s.reading ? 'Reading the target…' : '');
      receipt.textContent = s.result ? `Last collective request${s.result.partial ? ' · partial result' : ''}: ${s.result.detail}` : '';
      receipt.setAttribute('role', s.result?.status === 'uncertain' ? 'alert' : 'status');
      if (shown === s.preview) return;
      shown = s.preview; consent.replaceChildren(); consent.hidden = !shown;
      if (!shown) return;
      const preview = shown, { view, request, sources } = preview;
      const full = api.transport(request.action);
      const affected = full ? view.groups.flatMap(g => g.members) : view.rooms;
      consent.append(el('h3', { text: `${api.actions[request.action]} · ${view.target}` }),
        el('p', { text: `The daemon matched this as ${view.matched}: ${view.rooms.length} selected room(s), in ${view.groups.length} group(s).` }));
      if (view.matched === 'prefix') consent.append(el('p', { text: 'This is a unique-prefix match, not an exact name. Check the resolved rooms carefully.' }));
      const selected = new Set(view.rooms.map(api.roomName));
      const list = el('ul', { 'aria-label': 'Rooms affected by this action' });
      for (const room of affected) list.append(el('li', { text: `${api.roomName(room)}${full && !selected.has(api.roomName(room)) ? ' · also affected because it shares the group' : ''}` }));
      consent.append(list);
      if (full) {
        for (const group of view.groups) consent.append(el('p', { text: `Group: ${group.members.map(r => r.room).join(' + ')} (${group.household}); coordinator ${group.coordinator_room}.` }));
        for (const source of sources) consent.append(el('p', { text: source.label }));
        consent.append(el('p', { text: 'Transport acts on these complete groups, including members outside the named alias. Their queues are not replaced.' }));
      } else {
        const effect = request.action === 'volume' ? `Request volume ${request.body.volume} for each selected room.`
          : request.body.delta != null ? `Request ${request.body.delta > 0 ? '+' : ''}${request.body.delta} from each room’s own current level.`
            : `${request.body.mute ? 'Mute' : 'Unmute'} only the selected rooms, not their other group members.`;
        consent.append(el('p', { text: effect }), el('p', { text: 'House policy is applied separately at execution. Levels may be capped; changing volume or mute can end an active sleep fade.' }));
      }
      consent.append(el('p', { class: 'source-help', text: 'Only currently addressable rooms are listed. The target and group identities are checked again before sending. This is not a reservation: another controller can still change the house afterward. Read every returned note for skipped rooms or limits.' }));
      const confirm = el('button', { type: 'button', text: 'Confirm collective action' });
      const cancel = el('button', { type: 'button', text: 'Cancel review' });
      confirm.addEventListener('click', () => { void control.confirm(preview); });
      cancel.addEventListener('click', () => control.cancel());
      consent.append(el('div', { class: 'row' }, confirm, cancel));
    }
    input.addEventListener('input', () => control.edit('input', input.value));
    action.addEventListener('change', () => control.edit('action', action.value));
    amount.addEventListener('input', () => control.edit('amount', amount.value));
    suggestions.addEventListener('change', () => { if (suggestions.value) control.edit('input', suggestions.value); suggestions.value = ''; });
    refresh.addEventListener('click', () => { void control.refresh(); });
    review.addEventListener('click', () => { void control.review(); });
    details.addEventListener('toggle', event => {
      if (event.target !== details) return;
      if (details.open) { control.open(); if (!control.state.suggestions.length) void control.refresh(); }
      else control.close();
    });
    const hide = () => { if (doc.hidden) control.close(); else if (details.open) control.open(); };
    const pagehide = () => control.close(), pageshow = () => { if (!doc.hidden && details.open) control.open(); };
    doc.addEventListener('visibilitychange', hide);
    doc.defaultView?.addEventListener('pagehide', pagehide);
    doc.defaultView?.addEventListener('pageshow', pageshow);
    render(control.state);
    return { node, ...control, dispose() {
      control.dispose(); doc.removeEventListener('visibilitychange', hide);
      doc.defaultView?.removeEventListener('pagehide', pagehide);
      doc.defaultView?.removeEventListener('pageshow', pageshow);
    } };
  }
  return { mount };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = RoomSetsPanel;
else {
  Object.assign(ROUTES, RoomSetsRemote.routes);
  const host = document.getElementById('households');
  if (host) {
    const levels = RoomSetsRemote.levelLanes({ cards: () => house.cards.values(), flights: volumeFlights });
    registerCardPanel(parts => {
      levels.observe(parts);
      const observe = () => levels.observe(parts);
      return { node: el('span', { hidden: true }), observe, block: observe };
    });
    host.before(RoomSetsPanel.mount({ call, el, execute: RoomSetsRemote.coordinated({
      cards: () => house.cards.values(), refresh: loadZones, begin: beginAction, end: endAction, shape: groupShape, levels,
    }) }).node);
  }
}
