// The same house policy editor serves the phone remote and listening dashboard.
'use strict';
const PolicyConfigurationPanel = (() => {
  const M = typeof PolicyConfiguration !== 'undefined' ? PolicyConfiguration : require('./policy-configuration.js');
  let nextId = 0;
  function panel({ call, execute, readConfiguration, canEdit, canRead, blockedReason, document: doc = document }) {
    const id = `policy-configuration-${++nextId}`, focusKey = 'data-policy-config-focus';
    const el = (tag, attrs = {}, ...children) => {
      const node = doc.createElement(tag);
      for (const [key, value] of Object.entries(attrs)) {
        if (key === 'text') node.textContent = value;
        else if (key.startsWith('on')) node.addEventListener(key.slice(2), value);
        else if (['value', 'checked', 'selected', 'disabled', 'open', 'hidden', 'readOnly'].includes(key)) node[key] = value;
        else node.setAttribute(key, value);
      }
      node.append(...children); return node;
    };
    const button = (label, key, action, disabled = false, cls = '') => el('button', {
      type: 'button', text: label, class: cls, [focusKey]: key, disabled, onclick: action });
    const dialog = el('dialog', { id, class: 'policy-config-dialog', 'aria-labelledby': `${id}-title` });
    const body = el('div', { class: 'policy-config-body' });
    const trigger = button('House limits & permissions', 'open', () => control.open(), false, 'text-button policy-config-trigger');
    trigger.setAttribute('aria-haspopup', 'dialog'); trigger.setAttribute('aria-controls', id); trigger.setAttribute('aria-expanded', 'false');
    const control = M.model({ call, execute, readConfiguration, canEdit, canRead, blockedReason, changed: render });
    const note = text => el('p', { class: 'policy-config-note', text });
    function field(label, key, value, options = {}) {
      const { multiline = false, ...attributes } = options;
      return el('label', { class: 'policy-config-field', for: `${id}-${key}` }, label,
        el(multiline ? 'textarea' : 'input', { id: `${id}-${key}`, type: 'text', value, maxlength: multiline ? 65536 : 1024,
          [focusKey]: key, disabled: !control.editable(), oninput: event => control.edit(key, event.target.value), ...attributes }));
    }
    function choice(label, key, value, options) {
      return el('label', { class: 'policy-config-field', for: `${id}-${key}` }, label,
        el('select', { id: `${id}-${key}`, [focusKey]: key, disabled: !control.editable(), onchange: event => control.edit(key, event.target.value) },
          ...options.map(([entry, text]) => el('option', { value: entry, text, selected: entry === value }))));
    }
    const numeric = (label, key, value, placeholder = '') => field(label, key, value, { inputmode: 'numeric', placeholder });
    function clientFields(draft) {
      return el('div', {},
        choice('Allowed tools', 'allow_mode', draft.allow_mode, [['inherit', 'Use base policy'], ['only', 'Only the listed tools or classes']]),
        ...(draft.allow_mode === 'only' ? [field('Allowed tools or classes — one per line', 'allow', draft.allow, { multiline: true, rows: '4' }),
          note('An explicitly empty allowed list permits no tools unless a grant adds them.')] : []),
        field('Denied tools or classes — one per line', 'deny', draft.deny, { multiline: true, rows: '3' }),
        field('Additional grants — one per line', 'grant', draft.grant, { multiline: true, rows: '3' }),
        note('Classes include read, volume, playback, grouping, queue, sound, house-config, announce, dj, auth, and undo. Deny rules take precedence. A grant can add owner-only capabilities such as Spotify sign-in.'),
        choice('Volume caps', 'capped', draft.capped, [['inherit', 'Use caller defaults'], ['capped', 'Apply house and room caps'], ['uncapped', 'Allow uncapped volume']]),
        el('label', { class: 'policy-config-checkbox' }, el('input', { type: 'checkbox', checked: draft.manage_others,
          disabled: !control.editable(), [focusKey]: 'manage_others', onchange: event => control.edit('manage_others', event.target.checked) }),
          'Allow house administration'),
        note('House administrators may manage other people’s scenes and routines, aliases, and policy rules. Tool restrictions still apply. This does not change the house owner’s identity.'),
        choice('MCP tool list', 'mcp_profile', draft.mcp_profile, [['inherit', 'Use server default'], ['essentials', 'Essentials'], ['full', 'Full tool list']]),
        note('The MCP tool list changes what an agent is offered. Permissions still decide which tools it may use.'));
    }
    function draftForm(state) {
      const draft = state.draft, removing = ['clear_quiet_hours', 'remove_room', 'remove_client'].includes(draft.kind);
      const titles = { set_defaults: 'Default limits', set_quiet_hours: 'Quiet hours', clear_quiet_hours: 'Remove quiet hours',
        set_room: draft.existing === null ? 'Add room limits' : 'Edit room limits', remove_room: 'Remove room limits',
        set_client: draft.existing === null ? 'Add client rules' : 'Edit client rules', remove_client: 'Remove client rules' };
      const section = el('section', { class: 'policy-config-draft', 'aria-label': 'House policy draft' }, el('h3', { text: titles[draft.kind] }));
      if (state.stale) section.append(note('This draft belongs to the policy you originally read. Discard it or open an edit from the current saved rules below.'));
      if (draft.kind === 'set_defaults') section.append(
        numeric('Default maximum room volume (0–100)', 'max_volume', draft.max_volume),
        numeric('Largest single volume increase (0–100)', 'max_step', draft.max_step),
        numeric('Scheduled volume ramp (seconds)', 'fade_secs', draft.fade_secs),
        note('Room limits can override the defaults. Quiet hours can lower the maximum further. Caps apply to callers that are capped; the terminal owner is uncapped by default.'));
      else if (draft.kind === 'set_quiet_hours') section.append(
        field('Start on the house clock', 'start', draft.start, { type: 'time', step: '60' }),
        field('End on the house clock', 'end', draft.end, { type: 'time', step: '60' }),
        numeric('Quiet-hours maximum volume (0–100)', 'max_volume', draft.max_volume),
        note('Use the house’s local clock. The window may cross midnight.'));
      else if (['set_room', 'remove_room'].includes(draft.kind)) {
        section.append(field('Physical room name', 'room', draft.room, { readOnly: draft.existing !== null, placeholder: 'Exact Sonos room name, including offline rooms' }));
        if (!removing) section.append(numeric('Maximum volume (0–100)', 'max_volume', draft.max_volume, 'Blank inherits the default'),
          numeric('Largest volume increase (0–100)', 'max_step', draft.max_step, 'Blank inherits the default'),
          note('Use the room’s name, not an alias or household-qualified command target. Matching ignores case and normalizes apostrophes.'));
      } else if (['set_client', 'remove_client'].includes(draft.kind)) {
        section.append(field('Client key', 'client', draft.client, { readOnly: draft.existing !== null,
          placeholder: 'cli, mcp-stdio, loopback-http, or a tailnet principal' }));
        if (!removing) section.append(clientFields(draft));
      }
      if (removing) section.append(note(draft.kind === 'clear_quiet_hours'
        ? 'Default and room caps will continue to apply; the time-of-day cap will be removed.'
        : draft.kind === 'remove_room' ? 'This room will inherit the default limits and quiet-hours cap.'
          : 'This client will use the base policy. Removing its rule can remove restrictions as well as grants.'));
      section.append(el('div', { class: 'policy-config-actions' },
        button('Discard draft', 'discard', () => control.discard(), state.writing),
        button('Review change', 'review', () => control.review(), !control.editable() || state.stale, 'primary-button')));
      return section;
    }
    function preview(planned, state) {
      return el('section', { class: 'policy-config-review', tabindex: '-1', 'aria-label': 'Review house policy change' },
        el('h3', { text: 'Save this rule for the house?' }), el('h4', { text: 'Before' }), el('p', { text: planned.prior }),
        el('h4', { text: 'After' }), el('p', { text: planned.summary }),
        note('Future commands use the saved policy. Lowering a limit does not immediately turn down playing rooms. A permission change may prevent this caller from reading or editing policy again.'),
        el('div', { class: 'policy-config-actions' }, button('Keep editing', 'back', () => control.cancelReview(), state.writing),
          button('Save house policy', 'confirm', () => void control.save(planned), !control.editable(), 'primary-button')));
    }
    function savedList(state) {
      const config = state.configuration;
      if (!config) return note('The owner or an authorized house administrator can read the saved rules here.');
      const card = (title, text, actions) => el('article', { class: 'policy-config-card' }, el('h4', { text: title }),
        el('p', { text }), el('div', { class: 'policy-config-actions' }, ...actions));
      const edit = (label, key, kind, target = null) => button(label, key, () => control.begin(kind, target), !control.editable());
      const defaults = el('section', {}, el('h3', { text: 'House limits' }),
        card('Default limits', M.observation(config, { kind: 'set_defaults' }), [edit('Edit defaults', 'edit-defaults', 'set_defaults')]),
        card('Quiet hours', M.observation(config, { kind: 'set_quiet_hours' }), [edit(config.quiet_hours ? 'Edit quiet hours' : 'Set quiet hours', 'edit-quiet', 'set_quiet_hours'),
          ...(config.quiet_hours ? [edit('Remove quiet hours', 'remove-quiet', 'clear_quiet_hours')] : [])]));
      const rooms = el('section', {}, el('h3', { text: 'Room limits' }));
      if (!config.rooms.length) rooms.append(note('No room-specific limits are saved.'));
      config.rooms.forEach((row, index) => rooms.append(card(row.room, M.observation(config, { kind: 'set_room', room: row.room }), [
        edit('Edit room limits', `edit-room-${index}`, 'set_room', row.room), edit('Remove room limits', `remove-room-${index}`, 'remove_room', row.room)])));
      const clients = el('section', { 'aria-label': 'Saved client permissions' }, el('h3', { text: 'Client permissions' }));
      if (!config.clients.length) clients.append(note('No individual client rules are saved. Base policy applies.'));
      config.clients.forEach((row, index) => clients.append(card(row.client, M.observation(config, { kind: 'set_client', client: row.client }), [
        edit('Edit client rules', `edit-client-${index}`, 'set_client', row.client), edit('Remove client rules', `remove-client-${index}`, 'remove_client', row.client)])));
      return el('section', { 'aria-label': 'Last observed house policy' }, el('div', { class: 'policy-config-columns' }, defaults, rooms), clients);
    }
    function render(state) {
      trigger.disabled = state.writing; trigger.setAttribute('aria-expanded', String(state.open));
      if (state.open && !dialog.open) dialog.showModal(); else if (!state.open && dialog.open) dialog.close();
      const active = doc.activeElement, focused = active?.getAttribute(focusKey), start = active?.selectionStart, end = active?.selectionEnd;
      body.replaceChildren(); body.setAttribute('aria-busy', String(state.loading || state.writing));
      body.append(el('div', { class: 'policy-config-actions' }, button('Refresh house policy', 'refresh', () => void control.load({ explicit: true }), !control.readable()),
        button('Add room limits', 'new-room', () => control.begin('set_room'), !control.editable()),
        button('Add client rules', 'new-client', () => control.begin('set_client'), !control.editable())));
      if (state.message) body.append(el('p', { role: 'status', class: 'policy-config-status', text: state.message }));
      if (state.result) body.append(el('p', { role: 'status', class: 'policy-config-result', text: state.result.detail }));
      if (!state.loaded && !state.loading) body.append(note('Refresh saved policy before another change.'));
      if (state.draft) body.append(draftForm(state)); if (state.pending) body.append(preview(state.pending, state));
      body.append(savedList(state));
      const next = [...body.querySelectorAll(`[${focusKey}]`)].find(node => node.getAttribute(focusKey) === focused);
      if (next && !next.disabled) { next.focus({ preventScroll: true }); if (typeof start === 'number') { try { next.setSelectionRange(start, end); } catch (_) { /* Some controls have no caret. */ } } }
    }
    dialog.append(el('header', { class: 'policy-config-heading' }, el('div', {}, el('h2', { id: `${id}-title`, text: 'House limits & permissions' }),
      note('Set room caps, quiet hours, and what each client may do. Review one rule at a time.')),
      button('Close', 'close', () => control.close())), body);
    dialog.addEventListener('cancel', event => { event.preventDefault(); control.close(); });
    dialog.addEventListener('close', () => { if (control.state.open) control.close(); }); render(control.state);
    return { node: el('div', { class: 'policy-configuration' }, trigger, dialog), control,
      observe: control.observe, suspend: control.suspend, resume: control.resume,
      dispose() { control.dispose(); if (dialog.open) dialog.close(); } };
  }
  return Object.freeze({ panel });
})();
if (typeof module !== 'undefined' && module.exports) module.exports = PolicyConfigurationPanel;
if (typeof document !== 'undefined' && typeof ROUTES !== 'undefined') {
  Object.assign(ROUTES, PolicyConfiguration.routes);
  const policyConfigurationHost = document.getElementById('room-configuration') || document.getElementById('households');
  if (policyConfigurationHost) {
    const policyConfigurationPanel = PolicyConfigurationPanel.panel({ call: (url, body, options) => call(
      body === undefined ? 'policyConfigurationRead' : 'policyConfigurationWrite', { body, signal: options?.signal }) });
    const policyConfigurationRoot = el('section', { id: 'policy-configuration', class: 'scenes', 'aria-label': 'House limits and permissions' });
    policyConfigurationRoot.append(policyConfigurationPanel.node); policyConfigurationHost.before(policyConfigurationRoot);
    const resumePolicyConfiguration = () => { if (!document.hidden) policyConfigurationPanel.resume(); };
    document.addEventListener('visibilitychange', () => document.hidden ? policyConfigurationPanel.suspend() : resumePolicyConfiguration());
    window.addEventListener('pagehide', policyConfigurationPanel.suspend); window.addEventListener('pageshow', resumePolicyConfiguration);
    if (document.hidden) policyConfigurationPanel.suspend();
  }
}
