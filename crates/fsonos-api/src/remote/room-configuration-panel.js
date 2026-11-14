// One plain-form room configuration editor on the phone and dashboard.
'use strict';
const RoomConfigurationPanel = (() => {
  const M = typeof RoomConfiguration !== 'undefined' ? RoomConfiguration : require('./room-configuration.js');
  let nextId = 0;
  function panel({ call, execute, readConfiguration, canEdit, canRead, blockedReason, saved, document: doc = document }) {
    const id = `room-configuration-${++nextId}`, focusKey = 'data-room-config-focus';
    const el = (tag, attrs = {}, ...children) => {
      const node = doc.createElement(tag);
      for (const [key, value] of Object.entries(attrs)) {
        if (key === 'text') node.textContent = value;
        else if (key.startsWith('on')) node.addEventListener(key.slice(2), value);
        else if (['value', 'checked', 'disabled', 'open', 'hidden', 'readOnly'].includes(key)) node[key] = value;
        else node.setAttribute(key, value);
      }
      node.append(...children); return node;
    };
    const button = (label, key, action, disabled = false, cls = '') => el('button', {
      type: 'button', class: cls, text: label, [focusKey]: key, disabled, onclick: action });
    const dialog = el('dialog', { id, class: 'room-config-dialog', 'aria-labelledby': `${id}-title` });
    const body = el('div', { class: 'room-config-body' });
    const trigger = button('Aliases & default rooms', 'open', () => control.open(), false, 'text-button room-config-trigger');
    trigger.setAttribute('aria-haspopup', 'dialog'); trigger.setAttribute('aria-controls', id); trigger.setAttribute('aria-expanded', 'false');
    const control = M.model({ call, execute, readConfiguration, canEdit, canRead, blockedReason, saved, changed: render });
    const field = (label, key, value, options = {}) => el('label', { class: 'room-config-field', for: `${id}-${key}` }, label,
      el('input', { id: `${id}-${key}`, type: 'text', value, maxlength: key === 'client' ? 1024 : 256,
        [focusKey]: key, disabled: !control.editable(), oninput: event => control.edit(key, event.target.value), ...options }));
    function roomSuggestions(state) {
      return el('datalist', { id: `${id}-rooms` }, ...state.rooms.map(room => el('option', { value: room.target, text: room.label })));
    }
    function draftForm(state) {
      const draft = state.draft, alias = ['set_alias', 'remove_alias'].includes(draft.kind), removing = draft.kind.startsWith('remove') || draft.kind.startsWith('clear');
      const title = alias ? `${removing ? 'Remove' : draft.existing_name === null ? 'Create' : 'Edit'} alias${draft.existing_name === null ? '' : ` “${draft.name}”`}`
        : `${removing ? 'Clear' : 'Set'} ${draft.client === null ? 'the global' : 'a client’s'} default room`;
      const section = el('section', { class: 'room-config-draft', 'aria-label': 'Room configuration draft' }, el('h3', { text: title }));
      if (state.stale) section.append(el('p', { class: 'room-config-warning', text: 'This draft still belongs to the settings you originally read. The current saved settings are listed below. Discard this draft or open a new edit to use them.' }));
      if (alias) {
        section.append(field('Alias name', 'name', draft.name, { readOnly: draft.existing_name !== null }));
        const members = el('ol', { class: 'room-config-members', 'aria-label': 'Alias room names' });
        draft.rooms.forEach((room, index) => members.append(el('li', {}, el('span', { text: room }),
          ...(removing ? [] : [button('Remove room', `remove-room-${index}`, () => control.removeRoom(index), !control.editable())]))));
        section.append(members);
        if (!removing) section.append(el('div', { class: 'room-config-add-room' },
          field('Physical room to add', 'room_input', draft.room_input, { list: `${id}-rooms`, placeholder: 'Choose a room or enter its exact saved name' }),
          button('Add room', 'add-room', () => control.addRoom(), !control.editable())),
          el('p', { class: 'room-config-note', text: 'Add each physical room separately. Saved names are kept exactly, including offline rooms and repeated entries. Aliases do not expand other aliases.' }));
      } else {
        if (draft.client !== null) section.append(field('Exact client key', 'client', draft.client, { readOnly: draft.client_locked,
          placeholder: 'Client key as configured by the owner' }));
        else section.append(el('p', { text: 'The global default applies when a client has no individual default.' }));
        if (!removing) section.append(field('Default physical room', 'room', draft.room, { list: `${id}-rooms`, placeholder: 'Choose a room or enter its exact saved name' }));
        else section.append(el('p', { text: `Current saved room: ${draft.room}` }));
        section.append(el('p', { class: 'room-config-note', text: 'Client keys and room names are kept exactly. A default identifies a physical room, not an alias or the words here, all, or everywhere.' }));
      }
      section.append(roomSuggestions(state), el('div', { class: 'room-config-actions' },
        button('Discard draft', 'discard', () => control.discard(), state.writing),
        button('Review change', 'review', () => control.review(), !control.editable() || state.stale, 'primary-button')));
      return section;
    }
    function preview(planned, state) {
      return el('section', { class: 'room-config-review', tabindex: '-1', 'aria-label': 'Review room configuration change' },
        el('h3', { text: 'Save this change for the house?' }), el('p', { text: planned.prior }), el('p', { text: planned.summary }),
        el('p', { text: 'This changes how future commands and scheduled routines resolve their targets. It does not start music, move a session, or regroup speakers. Other saved aliases and defaults are retained.' }),
        el('div', { class: 'room-config-actions' }, button('Keep editing', 'back', () => control.cancelReview(), state.writing),
          button('Save room configuration', 'confirm', () => void control.save(planned), !control.editable(), 'primary-button')));
    }
    function savedList(state) {
      const config = state.configuration;
      if (!config) return el('p', { text: 'Read the house’s stored configuration to see its aliases and defaults.' });
      const aliases = el('section', { class: 'room-config-aliases', 'aria-label': 'Saved aliases' }, el('h3', { text: 'Saved aliases' }));
      if (!config.aliases.length) aliases.append(el('p', { text: 'No aliases are saved.' }));
      config.aliases.forEach((alias, index) => aliases.append(el('article', { class: 'room-config-card' },
        el('h4', { text: alias.name }), el('ul', {}, ...alias.rooms.map(room => el('li', { text: room }))),
        el('div', { class: 'room-config-actions' },
          button('Edit alias', `edit-alias-${index}`, () => control.beginAlias(alias.name), !control.editable()),
          button('Remove alias', `remove-alias-${index}`, () => control.beginAlias(alias.name, true), !control.editable())))));
      const defaults = el('section', { class: 'room-config-defaults', 'aria-label': 'Saved default rooms' }, el('h3', { text: 'Default rooms' }));
      defaults.append(el('p', { class: 'room-config-note', text: 'An individual client default takes precedence over the global default when a command uses here or omits its room.' }));
      const defaultCard = (client, room, index) => el('article', { class: 'room-config-card' },
        el('h4', { text: client === null ? 'Global default' : `Client: ${client}` }), el('p', { text: room === null ? 'Not set' : room }),
        el('div', { class: 'room-config-actions' },
          button(room === null ? 'Set default' : 'Edit default', `edit-default-${index}`, () => control.beginDefault(client), !control.editable()),
          button('Clear default', `clear-default-${index}`, () => control.beginDefault(client, true), !control.editable() || room === null)));
      defaults.append(defaultCard(null, config.default_room, 'global'));
      Object.entries(config.client_defaults).forEach(([client, room], index) => defaults.append(defaultCard(client, room, index)));
      const columns = el('div', { class: 'room-config-columns' }, aliases, defaults);
      const result = el('section', { 'aria-label': 'Last observed room configuration' }, columns);
      if (config.warnings.length) result.append(el('section', { class: 'room-config-warnings', 'aria-label': 'Configuration warnings' },
        el('h3', { text: 'Room names to check' }), el('p', { text: 'Offline or ambiguous room names remain saved. The house reports:' }),
        el('ul', {}, ...config.warnings.map(warning => el('li', { text: `Line ${warning.line}: ${warning.message}` })))));
      return result;
    }
    function render(state) {
      trigger.disabled = state.writing;
      trigger.setAttribute('aria-expanded', String(state.open));
      if (state.open && !dialog.open) dialog.showModal();
      else if (!state.open && dialog.open) dialog.close();
      const active = doc.activeElement, focused = active?.getAttribute(focusKey), start = active?.selectionStart, end = active?.selectionEnd;
      body.replaceChildren(); body.setAttribute('aria-busy', String(state.loading || state.writing));
      body.append(el('div', { class: 'room-config-actions' },
        button('Refresh room configuration', 'refresh', () => void control.load({ explicit: true }), !control.readable()),
        button('Create alias', 'new-alias', () => control.beginAlias(), !control.editable()),
        button('Add client default', 'new-client', () => control.beginDefault(''), !control.editable())));
      if (state.message) body.append(el('p', { class: 'room-config-status', role: 'status', text: state.message }));
      if (state.roomMessage) body.append(el('p', { class: 'room-config-note', text: state.roomMessage }));
      if (state.result) body.append(el('p', { class: 'room-config-result', role: 'status', text: state.result.detail }));
      if (!state.loaded && !state.loading) body.append(el('p', { class: 'room-config-note', text: 'Refresh the saved configuration before another change.' }));
      if (state.draft) body.append(draftForm(state));
      if (state.pending) body.append(preview(state.pending, state));
      body.append(savedList(state));
      const next = [...body.querySelectorAll(`[${focusKey}]`)].find(node => node.getAttribute(focusKey) === focused);
      if (next && !next.disabled) {
        next.focus({ preventScroll: true });
        if (typeof start === 'number') { try { next.setSelectionRange(start, end); } catch (_) { /* Non-text controls have no caret. */ } }
      }
    }
    dialog.append(el('header', { class: 'room-config-heading' },
      el('div', {}, el('h2', { id: `${id}-title`, text: 'Aliases & default rooms' }),
        el('p', { text: 'Give room sets reusable names and choose each client’s fallback room. These settings are stored by the house.' })),
      button('Close', 'close', () => control.close())),
      el('p', { class: 'room-config-note', text: 'Saving requires permission to configure the house. Changes affect future target resolution, including scheduled routines; no playback changes now.' }), body);
    dialog.addEventListener('cancel', event => { event.preventDefault(); control.close(); });
    dialog.addEventListener('close', () => { if (control.state.open) control.close(); });
    render(control.state);
    return { node: el('div', { class: 'room-configuration' }, trigger, dialog), control,
      observe: control.observe, suspend: control.suspend, resume: control.resume,
      dispose() { control.dispose(); if (dialog.open) dialog.close(); } };
  }
  return Object.freeze({ panel });
})();
if (typeof module !== 'undefined' && module.exports) module.exports = RoomConfigurationPanel;
if (typeof document !== 'undefined' && typeof ROUTES !== 'undefined') {
  Object.assign(ROUTES, RoomConfiguration.routes);
  const roomConfigurationHost = document.getElementById('room-set-controls') || document.getElementById('room-groups') || document.getElementById('households');
  if (roomConfigurationHost) {
    const roomConfigurationPanel = RoomConfigurationPanel.panel({ call: (url, body, options) => call(
      url === '/rooms' ? 'roomConfigurationRooms' : body === undefined ? 'roomConfigurationRead' : 'roomConfigurationWrite',
      { body, signal: options?.signal }) });
    const roomConfigurationRoot = el('section', { id: 'room-configuration', class: 'scenes', 'aria-label': 'Aliases and default rooms' });
    roomConfigurationRoot.append(roomConfigurationPanel.node); roomConfigurationHost.before(roomConfigurationRoot);
    const resumeRoomConfiguration = () => { if (!document.hidden) roomConfigurationPanel.resume(); };
    document.addEventListener('visibilitychange', () => document.hidden ? roomConfigurationPanel.suspend() : resumeRoomConfiguration());
    window.addEventListener('pagehide', roomConfigurationPanel.suspend); window.addEventListener('pageshow', resumeRoomConfiguration);
    if (document.hidden) roomConfigurationPanel.suspend();
  }
}
