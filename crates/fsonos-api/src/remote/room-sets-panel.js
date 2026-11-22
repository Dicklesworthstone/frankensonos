// The room-set editor uses the ordinary policy-checked command routes.
'use strict';
const RoomSetsPanel = (() => {
  function mount({ api = RoomSetsRemote, call, el, execute, document: doc = document,
    actions = api.actions, initialAction = 'pause', available = () => true, id = 'room-set-controls',
    title = 'Control a room set or household' }) {
    const input = el('input', { type: 'text', autocomplete: 'off', spellcheck: 'false',
      'aria-label': 'Room or alias to control', placeholder: 'Room, alias, here, or all@S1' });
    const suggestions = el('select', { 'aria-label': 'Known room-set suggestions' });
    const action = el('select', { 'aria-label': 'Collective room action' });
    for (const [value, label] of Object.entries(actions)) action.append(el('option', { value, text: label }));
    const amount = el('input', { type: 'number', min: 0, max: 100, step: 1, value: '5',
      'aria-label': 'Volume level or step' });
    const amountLabel = el('label', { hidden: true }, el('span', { text: 'Volume level or step' }), amount);
    const duration = el('input', { type: 'text', maxlength: 64, value: '45m', 'aria-label': 'Room-set sleep duration' });
    const durationLabel = el('label', { hidden: true }, 'Sleep duration (45m, 1h30m or 90s; maximum 23 hours)', duration);
    const seconds = el('input', { type: 'number', min: 1, max: 60, step: 1, value: '5', 'aria-label': 'Room-set fade seconds' });
    const secondsLabel = el('label', { hidden: true }, 'Fade duration (1–60 seconds)', seconds);
    const refresh = el('button', { type: 'button', text: 'Read room suggestions' });
    const review = el('button', { type: 'button', text: 'Resolve and review' });
    const status = el('p', { role: 'status', 'aria-live': 'polite' });
    const receipt = el('p', { class: 'room-set-receipt', role: 'status', 'aria-live': 'polite' });
    const consent = el('div', { class: 'room-set-review', hidden: true });
    const details = el('details', { class: 'zone room-set-controls' },
      el('summary', { text: title }),
      el('div', {},
        el('p', { text: api.bedtime(initialAction)
          ? 'Set, extend or cancel sleep timers, or fade out and pause a saved room set or household. Review every group before one request.'
          : 'Control independent groups, set or change their sleep timers, or fade out and pause a room set in one daemon action. Rooms keep their existing groups.' }),
        el('label', {}, 'Known targets (suggestions only)', suggestions),
        el('label', {}, 'Target', input), refresh,
        el('label', {}, 'Action', action), amountLabel, durationLabel, secondsLabel, review, status, consent, receipt));
    const node = el('section', { id, class: 'scenes', 'aria-label': 'Room sets and household controls' }, details);
    const control = api.model({ call, execute, available, initialAction, changed: render });
    let shownSuggestions = null, shown = null, confirmation = null;
    function render(s) {
      if (shownSuggestions !== s.suggestions) {
        shownSuggestions = s.suggestions;
        suggestions.replaceChildren(el('option', { value: '', text: 'Choose a suggestion or type a target…' }));
        for (const entry of s.suggestions) suggestions.append(el('option', { value: entry.query, text: entry.label }));
      }
      for (const [field, value] of [[input, s.input], [action, s.action], [amount, s.amount], [duration, s.duration], [seconds, s.seconds]]) if (field.value !== value) field.value = value;
      const busy = s.reading || s.writing || !available();
      for (const field of [input, action, suggestions, amount, duration, seconds]) field.disabled = s.writing || !available();
      refresh.disabled = busy; review.disabled = busy || !s.input.trim();
      amountLabel.hidden = !['lower', 'raise', 'volume'].includes(s.action);
      durationLabel.hidden = !['sleep_set', 'sleep_extend'].includes(s.action);
      secondsLabel.hidden = s.action !== 'fade_pause';
      amount.min = s.action === 'volume' ? 0 : 1;
      amountLabel.firstChild.textContent = s.action === 'volume' ? 'Volume for each room (0–100)' : 'Change each room by (1–100)';
      node.setAttribute('aria-busy', String(busy));
      status.textContent = s.message || (s.reading ? 'Reading the target…' : '');
      receipt.textContent = s.result ? `Last collective request${s.result.partial ? ' · partial result' : ''}: ${s.result.detail}` : '';
      receipt.setAttribute('role', s.result?.status === 'uncertain' ? 'alert' : 'status');
      if (confirmation) confirmation.disabled = busy;
      if (shown === s.preview) return;
      shown = s.preview; confirmation = null; consent.replaceChildren(); consent.hidden = !shown;
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
        consent.append(el('p', { text: api.bedtime(request.action)
          ? 'This action affects these complete groups, including members outside the alias. Receipts identify each group’s timer or fade result. Their queues and grouping are not replaced.'
          : 'Transport acts on these complete groups, including members outside the named alias. Their queues are not replaced.' }));
      } else {
        const effect = request.action === 'volume' ? `Request volume ${request.body.volume} for each selected room.`
          : request.body.delta != null ? `Request ${request.body.delta > 0 ? '+' : ''}${request.body.delta} from each room’s own current level.`
            : `${request.body.mute ? 'Mute' : 'Unmute'} only the selected rooms, not their other group members.`;
        consent.append(el('p', { text: effect }), el('p', { text: 'House policy is applied separately at execution. Levels may be capped; changing volume or mute can end an active sleep fade.' }));
      }
      consent.append(el('p', { class: 'source-help', text: 'Only currently addressable rooms are listed. The target and group identities are checked again before sending. This is not a reservation: another controller can still change the house afterward. Read every returned note for skipped rooms or limits.' }));
      const confirm = el('button', { type: 'button', text: 'Confirm collective action', disabled: busy });
      confirmation = confirm;
      const cancel = el('button', { type: 'button', text: 'Cancel review' });
      confirm.addEventListener('click', () => { void control.confirm(preview); });
      cancel.addEventListener('click', () => control.cancel());
      consent.append(el('div', { class: 'row' }, confirm, cancel));
    }
    input.addEventListener('input', () => control.edit('input', input.value));
    action.addEventListener('change', () => control.edit('action', action.value));
    amount.addEventListener('input', () => control.edit('amount', amount.value));
    duration.addEventListener('input', () => control.edit('duration', duration.value));
    seconds.addEventListener('input', () => control.edit('seconds', seconds.value));
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
else if (typeof ROUTES !== 'undefined') {
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
