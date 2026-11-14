// The phone and Listening Studio share the same room-set DJ review and receipt.
'use strict';
const DjTargetsPanel = (() => {
  const T = typeof DjTargetsRemote !== 'undefined' ? DjTargetsRemote : require('./dj-targets.js');
  const D = typeof DjDirection !== 'undefined' ? DjDirection : require('./dj-direction.js');
  let nextId = 0;
  function panel({ create, document: doc = document }) {
    const id = `dj-targets-${++nextId}`, fields = new Map();
    const el = (tag, attrs = {}, ...children) => {
      const node = doc.createElement(tag);
      for (const [key, value] of Object.entries(attrs)) {
        if (value == null || value === false) continue;
        if (key === 'text') node.textContent = value;
        else if (key === 'class') node.className = value;
        else if (key.startsWith('on')) node.addEventListener(key.slice(2), value);
        else if (['id', 'value', 'disabled', 'checked', 'hidden', 'open'].includes(key)) node[key] = value;
        else node.setAttribute(key, value === true ? '' : String(value));
      }
      if (attrs.id) fields.set(attrs.id, node);
      node.append(...children); return node;
    };
    const button = (text, click) => el('button', { type: 'button', text, onclick: click });
    const field = (label, input) => el('label', { for: input.id }, label, input);
    const suggestions = el('select', { id: `${id}-suggestions`, 'aria-label': 'Known DJ room targets' });
    const input = el('input', { id: `${id}-target`, type: 'text', maxlength: 128, autocomplete: 'off',
      placeholder: 'Room, saved alias, all@S2, or everywhere' });
    const action = el('select', { id: `${id}-action` });
    for (const [value, text] of Object.entries(T.actions)) action.append(el('option', { value, text }));
    const mode = el('select', { id: `${id}-direction-mode` },
      el('option', { value: 'keep', text: 'Keep each group’s current direction' }),
      el('option', { value: 'replace', text: 'Use the same new direction in every group' }));
    const mood = el('select', { id: `${id}-mood` });
    const message = el('p', { class: 'dj-targets-message', role: 'status', 'aria-live': 'polite' });
    const refresh = button('Refresh rooms and moods', () => { void control.refresh(); });
    const review = el('button', { type: 'submit', text: 'Review affected groups' });
    const direction = D.editor({ draft: D.draft(), el, idPrefix: `${id}-filters`, open: false,
      change: (key, value) => control.editDirection(key, value),
      note: 'Use one entry per line. A new duration begins when this command saves the direction; leave it blank to keep the direction until cleared. Existing group deadlines are replaced. Use the single-group editor to preserve one group’s deadline.' });
    const modeField = field('Direction on start', mode);
    const directionFields = el('div', { class: 'dj-targets-direction' }, field('Mood', mood), direction);
    const form = el('form', { class: 'dj-targets-form' },
      el('p', { text: 'Choose existing groups by room, alias, household or everywhere. Each group runs its own DJ; this keeps their grouping unchanged.' }),
      field('Known targets (suggestions only)', suggestions), field('DJ target', input), refresh,
      field('Action', action), modeField, directionFields,
      el('p', { class: 'dj-targets-help', text: 'Read every affected group before confirming. Names are qualified by household, and the daemon binds the command to the reviewed player identities.' }), review);
    const preview = el('section', { class: 'dj-targets-review', tabindex: -1, hidden: true, 'aria-label': 'Review DJ command' });
    const result = el('section', { class: 'dj-targets-result', role: 'status', 'aria-live': 'polite', hidden: true });
    const recover = button('Read affected rooms', () => { void control.observeResult(); });
    const observation = el('section', { class: 'dj-targets-observation', hidden: true, 'aria-label': 'Current affected rooms' });
    const node = el('div', { class: 'dj-targets-panel' }, form, message, preview, result, recover, observation);
    let shown = null, shownResult = null, shownObservation = null, known = null, moods = null, confirm = null, focus = null;
    const control = create(render);
    function groupCard(entry, selected = null) {
      const { group } = entry;
      const card = el('article', { class: 'dj-targets-group' }, el('h4', { text: group.target }),
        el('p', { text: `Affected rooms: ${group.members.map(room => `${room.room}@${room.household}`).join(', ')}` }));
      if (selected) {
        const outside = group.members.filter(room => !selected.has(JSON.stringify([room.household, room.player])));
        if (outside.length) card.append(el('p', { class: 'dj-targets-warning', text: `Also affected through grouping: ${outside.map(room => `${room.room}@${room.household}`).join(', ')}` }));
      }
      if (entry.error) { card.append(el('p', { class: 'dj-targets-warning', text: entry.error })); return card; }
      card.append(el('p', { text: `Current source: ${entry.playback.label}` }),
        el('p', { text: `DJ ${entry.status.running ? 'running' : 'idle'}. ${T.directionText(entry.status)}` }));
      if (entry.queue) card.append(el('p', { text: `${entry.queue.total} retained queue entries${entry.queue.playing == null ? '; not currently playing its queue' : `; playing entry ${entry.queue.playing}`}.` }));
      if (entry.status.now) card.append(el('p', { text: `Current whole work: ${entry.status.now.title}${entry.status.now.composer ? ' · ' + entry.status.now.composer : ''}${entry.status.now.movement == null ? '' : ` · movement ${entry.status.now.movement}/${entry.status.now.movements}`}` }));
      if (entry.status.next.length) card.append(el('p', { text: 'Next queued works: ' + entry.status.next.map(work => work.title).join('; ') }));
      if (entry.status.problem) card.append(el('p', { class: 'dj-targets-warning', text: entry.status.problem }));
      return card;
    }
    function render(state) {
      node.setAttribute('aria-busy', String(state.reading || state.writing));
      for (const [key, value] of [[input, state.input], [action, state.action], [mode, state.directionMode]]) if (key.value !== value) key.value = value;
      if (known !== state.suggestions) {
        known = state.suggestions;
        suggestions.replaceChildren(el('option', { value: '', text: 'Choose a suggestion or type a target…' }),
          ...known.map(item => el('option', { value: item.query, text: item.label })));
      }
      if (moods !== state.moods) {
        moods = state.moods;
        mood.replaceChildren(el('option', { value: '', text: 'Time-of-day mood with these filters' }),
          ...moods.map(item => el('option', { value: item.name, text: item.name })));
        if (state.mood && !moods.some(item => item.name === state.mood)) mood.append(el('option', { value: state.mood, text: `${state.mood} (refresh before use)` }));
      }
      if (mood.value !== state.mood) mood.value = state.mood;
      modeField.hidden = state.action !== 'start';
      const directed = state.action === 'steer' || state.action === 'start' && state.directionMode === 'replace';
      directionFields.hidden = !directed;
      for (const field of [input, action, mode, mood, suggestions]) field.disabled = state.writing;
      for (const [key, value] of Object.entries(state.direction)) {
        const field = fields.get(`${id}-filters-${key}`);
        if (!field) continue;
        field.disabled = state.writing || !directed;
        if (typeof value === 'boolean') field.checked = value;
        else if (field.value !== value) field.value = value;
      }
      refresh.disabled = state.reading || state.writing;
      review.disabled = !control.canReview() || !state.input.trim();
      message.textContent = state.message;
      if (shown !== state.pending) {
        const previous = shown; shown = state.pending; preview.replaceChildren(); preview.hidden = !shown; confirm = null;
        if (shown) {
          const pending = shown, selected = new Set(pending.before.view.rooms.map(room => JSON.stringify([room.household, room.player])));
          preview.append(el('h3', { text: `${T.actions[pending.action]} · ${pending.body.zone}` }),
            el('p', { text: `${pending.before.view.rooms.length} selected room(s), ${pending.before.groups.length} independent group(s). Matched as ${pending.before.view.matched}.` }));
          if (pending.before.view.matched === 'prefix') preview.append(el('p', { class: 'dj-targets-warning', text: 'This target is a unique-prefix match. Check every room before confirming.' }));
          preview.append(...pending.before.entries.map(entry => groupCard(entry, selected)),
            el('p', { class: 'dj-targets-effect', text: T.effect(pending.action, pending.body) }));
          if (pending.body.constraints) preview.append(el('p', { text: 'New direction: ' + D.describe({ constraints: pending.body.constraints,
            ...(pending.body.for_secs ? { for_secs: pending.body.for_secs } : {}) }, pending.body.mood).replace('on each start', 'from this command')
              + (pending.body.for_secs ? '.' : '; until cleared.') }));
          const readiness = pending.action === 'start' ? 'The daemon checks every group’s exact direction, library, renderer and required volume reads before starting any group. '
            : pending.action === 'steer' ? 'The daemon checks direction readiness for every group before saving any session. '
              : pending.action === 'clear' ? 'Clearing remains available when direction or library configuration needs repair. '
                : 'Each existing group is attempted once; an idle or failing group is reported while other groups can complete. ';
          preview.append(el('p', { class: 'dj-targets-help', text: readiness + 'A later failure can leave some groups completed; inspect the returned group reports before a new command.' }));
          confirm = button(`Confirm: ${T.actions[pending.action]}`, () => { void control.confirm(pending); });
          preview.append(el('div', { class: 'row' }, confirm, button('Cancel review', () => control.cancel())));
          if (state.open) { focus = doc.activeElement; preview.focus({ preventScroll: true }); }
        } else if (previous && !state.writing && state.open && focus && !focus.disabled) focus.focus();
      }
      if (confirm) confirm.disabled = !control.canConfirm(shown);
      if (shownResult !== state.result) {
        shownResult = state.result; result.replaceChildren(); result.hidden = !shownResult;
        if (shownResult) {
          const r = shownResult;
          result.setAttribute('role', r.status === 'uncertain' || r.partial ? 'alert' : 'status');
          result.append(el('h3', { text: r.status === 'uncertain' ? 'DJ outcome uncertain' : r.status === 'not_sent' ? 'No DJ command sent'
            : r.partial ? 'Incomplete DJ request' : r.status === 'observed' ? 'Current rooms observed' : 'DJ command accepted' }),
          el('p', { text: `Reviewed groups: ${r.groups.map(group => group.target).join(', ')}` }),
          el('p', { class: 'dj-targets-receipt', text: r.done || r.detail }));
          if (r.notes.length) {
            const notes = el('ul', { 'aria-label': 'Per-group failures and daemon notes' });
            for (const note of r.notes) notes.append(el('li', { text: `${note.code}: ${note.detail}` }));
            result.append(notes);
          }
          if (r.partial) result.append(el('p', { class: 'dj-targets-warning', text: 'Some groups failed and may already have changed. Do not repeat this batch. Read affected rooms, then review any new action separately. An incomplete batch has no safe whole-batch undo.' }));
          if (r.status === 'observed') result.append(el('p', { text: 'The original receipt is retained. The observations below do not prove which earlier operations completed.' }));
        }
      }
      recover.hidden = !state.recovery; recover.disabled = state.reading || state.writing;
      if (shownObservation !== state.observation) {
        shownObservation = state.observation; observation.replaceChildren(); observation.hidden = !shownObservation;
        if (shownObservation) {
          observation.append(el('h3', { text: 'Current groups containing the original players' }),
            ...shownObservation.entries.map(entry => groupCard(entry)));
          if (shownObservation.missing.length) observation.append(el('p', { class: 'dj-targets-warning', text: 'Original players no longer addressable: ' + shownObservation.missing.join(', ') + '. Their earlier outcome is still unknown.' }));
        }
      }
    }
    form.addEventListener('submit', event => { event.preventDefault(); void control.review(); });
    input.addEventListener('input', () => control.edit('input', input.value));
    action.addEventListener('change', () => control.edit('action', action.value));
    mode.addEventListener('change', () => control.edit('directionMode', mode.value));
    mood.addEventListener('change', () => control.edit('mood', mood.value));
    suggestions.addEventListener('change', () => { if (suggestions.value) control.edit('input', suggestions.value); suggestions.value = ''; });
    render(control.state);
    return { node, control, state: control.state, observe: () => control.observe?.(),
      open(target) { control.open(target); input.focus({ preventScroll: true }); if (!control.state.suggestions.length) void control.refresh(); },
      close: control.close, suspend: control.close, resume: () => control.notify(), dispose: control.dispose };
  }

  function dashboard({ call, controller, demo, completed, now, document: doc = document }) {
    const trigger = doc.getElementById('open-dj-targets'), dialog = doc.getElementById('dj-targets-dialog');
    let opener = null;
    const view = panel({ document: doc, create: changed => T.dashboard({ call, controller, demo, completed, now, changed }) });
    doc.getElementById('dj-targets-content').append(view.node);
    const close = () => {
      view.close(); if (dialog.open) dialog.close(); trigger.setAttribute('aria-expanded', 'false');
      if (opener && !opener.disabled) opener.focus();
    };
    trigger.addEventListener('click', () => {
      if (demo || !controller.state.active || controller.state.busy) return;
      opener = doc.activeElement; dialog.showModal(); trigger.setAttribute('aria-expanded', 'true'); view.open(controller.state.selected);
    });
    doc.getElementById('close-dj-targets').addEventListener('click', close);
    dialog.addEventListener('cancel', event => { event.preventDefault(); close(); });
    dialog.addEventListener('close', () => { view.close(); trigger.setAttribute('aria-expanded', 'false'); });
    return { ...view, observe() {
      trigger.disabled = !!(demo || !controller.state.active || controller.state.busy);
      view.observe();
    }, suspend: close, dispose() { close(); view.dispose(); } };
  }
  return { panel, dashboard };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = DjTargetsPanel;
else if (typeof ROUTES !== 'undefined') {
  Object.assign(ROUTES, DjTargetsRemote.routes);
  const host = document.getElementById('households');
  if (host) {
    const levels = RoomSetsRemote.levelLanes({ cards: () => house.cards.values(), flights: volumeFlights });
    const view = DjTargetsPanel.panel({ create: changed => DjTargetsRemote.model({ call, changed,
      available: () => !document.hidden, readable: () => !document.hidden,
      execute: (groups, work) => RoomSetsRemote.coordinated({ cards: () => house.cards.values(), refresh: loadZones,
        begin: beginAction, end: endAction, shape: groupShape, levels })(groups, work),
      completed: groups => { for (const group of groups) document.dispatchEvent(new CustomEvent('fsonos:dj-changed', { detail: { zone: group.target } })); },
    }) });
    const details = el('details', { class: 'zone dj-targets-controls' }, el('summary', { text: 'DJ across rooms' }), view.node);
    const root = el('section', { id: 'dj-targets-controls', class: 'scenes', 'aria-label': 'DJ across rooms' }, details);
    host.before(root);
    details.addEventListener('toggle', event => { if (event.target === details) { if (details.open) view.open(); else view.close(); } });
    registerCardPanel(parts => ({ node: el('span', { hidden: true }),
      observe(value) { levels.observe(parts); view.control.observePlayback(value); },
      block() { levels.observe(parts); }, stale() { view.control.invalidate(parts.zone); }, dispose() { view.control.invalidate(parts.zone); },
    }));
    document.addEventListener('fsonos:dj-changed', () => view.control.invalidate(null, 'A DJ session changed. Read the current target again.'));
    document.addEventListener('fsonos:catalog-changed', change => {
      if (change.detail?.moods !== false) view.control.invalidate(null, 'DJ configuration changed. Review the current target and mood again.');
    });
    document.addEventListener('visibilitychange', () => { if (document.hidden) view.close(); else if (details.open) view.control.open(); });
    window.addEventListener('pagehide', () => view.close());
    window.addEventListener('pageshow', () => { if (!document.hidden && details.open) view.control.open(); });
  }
}
