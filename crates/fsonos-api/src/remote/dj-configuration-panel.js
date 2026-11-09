// One editor on the phone and dashboard; opening it never starts playback.
'use strict';
const DjConfigurationPanel = (() => {
  const M = typeof DjConfiguration !== 'undefined' ? DjConfiguration : require('./dj-configuration.js');
  const D = typeof DjDirection !== 'undefined' ? DjDirection : require('./dj-direction.js');
  let nextId = 0;
  function panel({ call, document: doc = document, canEdit = () => true, saved = () => {} }) {
    const id = `dj-configuration-${++nextId}`, focusKey = 'data-config-focus';
    const el = (tag, attrs = {}, ...children) => {
      const node = doc.createElement(tag);
      for (const [key, value] of Object.entries(attrs)) {
        if (value === undefined || value === null || value === false) continue;
        if (key === 'text') node.textContent = value;
        else if (key === 'class') node.className = value;
        else if (key.startsWith('on')) node.addEventListener(key.slice(2), value);
        else node.setAttribute(key, value === true ? '' : String(value));
      }
      for (const child of children) if (child != null) node.append(child);
      return node;
    };
    const button = (text, action, attrs = {}) => el('button', { type: 'button', text, onclick: action, ...attrs });
    const labeled = (label, input) => el('label', { for: input.id }, label, input);
    const trigger = button('Moods & daily programs', () => { void control.open(); }, {
      class: 'text-button dj-config-trigger', 'aria-haspopup': 'dialog', 'aria-controls': id, 'aria-expanded': 'false' });
    const dialog = el('dialog', { id, class: 'dj-config-dialog', 'aria-labelledby': `${id}-title` });
    const status = el('p', { class: 'dj-config-status', role: 'status', 'aria-live': 'polite' });
    const refresh = button('Check saved settings', () => { void control.load(); });
    const discard = button('Discard draft & reload', () => { void control.load({ discard: true }); });
    const close = button('Close', () => control.close());
    const moodSelect = el('select', { id: `${id}-mood`, [focusKey]: 'mood-select', onchange: event => control.select(event.target.value) });
    const newName = el('input', { id: `${id}-new-name`, type: 'text', maxlength: 256, placeholder: 'e.g. late-night-jazz', [focusKey]: 'new-name' });
    const create = button('Create mood', () => { if (control.create(newName.value)) newName.value = ''; });
    const moodKind = el('p', { class: 'dj-config-note' });
    const remove = button('Remove custom mood', () => control.remove(control.state.selected));
    const moodFields = el('div', { class: 'dj-config-mood-fields' });
    const selectedPreview = el('pre', { class: 'dj-config-json', tabindex: 0 });
    const mode = el('select', { id: `${id}-mode`, onchange: event => control.programMode(event.target.value) },
      el('option', { value: 'default', text: 'Use the built-in daily program' }),
      el('option', { value: 'disabled', text: 'Disable automatic mood changes' }),
      el('option', { value: 'custom', text: 'Use my ordered daily program' }));
    const rows = el('div', { class: 'dj-config-programs' });
    const programNote = el('p', { class: 'dj-config-note' });
    const add = button('Add time window', () => control.addProgram());
    const previewDay = el('select', { id: `${id}-preview-day` }, ...M.days.map(day => el('option', { value: day, text: day.toUpperCase() })));
    const previewTime = el('input', { id: `${id}-preview-time`, type: 'time', value: '08:00' });
    const previewResult = el('p', { class: 'dj-config-program-result' });
    const allPreview = el('pre', { class: 'dj-config-json', tabindex: 0 });
    const latestPreview = el('pre', { class: 'dj-config-json', tabindex: 0 });
    const draftPreview = el('pre', { class: 'dj-config-json', tabindex: 0 });
    const comparison = el('details', { class: 'dj-config-comparison', hidden: true },
      el('summary', { text: 'Compare retained draft with latest saved settings' }),
      el('h3', { text: 'Your retained draft' }), draftPreview, el('h3', { text: 'Latest saved settings' }), latestPreview);
    const review = button('Review changes', () => control.review(), { class: 'primary-button' });
    const reviewBox = el('div', { class: 'dj-config-review', hidden: true, tabindex: -1 });
    const reviewSummary = el('p'), reviewJson = el('pre', { class: 'dj-config-json', tabindex: 0 });
    const confirm = button('Save for every DJ', () => { void control.save(); }, { class: 'primary-button' });
    const back = button('Keep editing', () => control.cancelReview());
    reviewBox.append(el('h3', { text: 'Save this configuration for the house?' }), reviewSummary,
      el('p', { text: 'Applies to future DJ choices. Music already queued keeps playing. These settings are shared by every room.' }),
      el('details', {}, el('summary', { text: 'Full configuration to save' }), reviewJson),
      el('div', { class: 'dj-config-actions' }, confirm, back));
    let moodKey = '', catalogueKey = '', shown = null, filtersOpen = true;
    const programNodes = [];
    const control = M.model({ call, canEdit, saved, changed: render });

    function restoreFocus(container, rebuild) {
      const active = doc.activeElement, key = active?.getAttribute(focusKey);
      const start = active?.selectionStart, end = active?.selectionEnd;
      rebuild();
      if (!key) return;
      const next = [...container.querySelectorAll(`[${focusKey}]`)].find(node => node.getAttribute(focusKey) === key);
      if (next && !next.disabled) {
        next.focus({ preventScroll: true });
        if (typeof start === 'number') { try { next.setSelectionRange(start, end); } catch (_) { /* Numeric controls have no text caret. */ } }
      }
    }

    function programRow(index) {
      const node = el('fieldset', { class: 'dj-config-program' }), legend = el('legend');
      const dayInputs = M.days.map(day => el('input', { type: 'checkbox', value: day,
        'aria-label': `Program ${index + 1} starts on ${day}`, onchange: () => control.editProgram(index, 'days', dayInputs.filter(input => input.checked).map(input => input.value)) }));
      const from = el('input', { id: `${id}-from-${index}`, type: 'text', inputmode: 'numeric', maxlength: 5,
        placeholder: 'HH:MM', oninput: event => control.editProgram(index, 'from', event.target.value) });
      const to = el('input', { id: `${id}-to-${index}`, type: 'text', inputmode: 'numeric', maxlength: 5,
        placeholder: 'HH:MM', oninput: event => control.editProgram(index, 'to', event.target.value) });
      const mood = el('select', { id: `${id}-program-mood-${index}`, onchange: event => control.editProgram(index, 'mood', event.target.value) });
      const up = button('Move up', () => control.moveProgram(index, -1), { 'aria-label': `Move program ${index + 1} up` });
      const down = button('Move down', () => control.moveProgram(index, 1), { 'aria-label': `Move program ${index + 1} down` });
      const remove = button('Remove window', () => control.removeProgram(index), { 'aria-label': `Remove program ${index + 1}` });
      node.append(legend, el('div', { class: 'dj-config-days' }, ...dayInputs.map((input, at) => el('label', {}, input, M.days[at].toUpperCase()))),
        el('div', { class: 'dj-config-times' }, labeled('From', from), labeled('Until', to), labeled('Mood', mood)),
        el('div', { class: 'dj-config-actions' }, up, down, remove));
      return { node, legend, dayInputs, from, to, mood, up, down, remove, names: '' };
    }

    function renderPrograms(state, enabled) {
      const custom = state.draft?.programs;
      mode.value = custom === null || custom === undefined ? 'default' : custom.length ? 'custom' : 'disabled';
      mode.disabled = !enabled; add.disabled = !enabled;
      add.hidden = custom === null || custom === undefined;
      const list = custom ?? [];
      while (programNodes.length < list.length) { const row = programRow(programNodes.length); programNodes.push(row); rows.append(row.node); }
      while (programNodes.length > list.length) programNodes.pop().node.remove();
      const names = Object.keys(control.effective()).sort();
      for (const [index, row] of list.entries()) {
        const parts = programNodes[index]; parts.legend.textContent = `Priority ${index + 1}`;
        for (const input of parts.dayInputs) { input.checked = row.days.includes(input.value); input.disabled = !enabled; }
        for (const key of ['from', 'to']) { if (parts[key].value !== row[key]) parts[key].value = row[key]; parts[key].disabled = !enabled; }
        const available = names.includes(row.mood) ? names : [row.mood, ...names], key = JSON.stringify(available);
        if (parts.names !== key) {
          parts.names = key; parts.mood.replaceChildren(...available.map(name => el('option', { value: name,
            text: names.includes(name) ? name : `${name} (unavailable)` })));
        }
        parts.mood.value = row.mood; parts.mood.disabled = !enabled;
        parts.up.disabled = !enabled || index === 0; parts.down.disabled = !enabled || index === list.length - 1; parts.remove.disabled = !enabled;
      }
      programNote.textContent = custom === null || custom === undefined
        ? 'Following the daemon’s built-in daily program. Its complete windows appear in the effective preview.'
        : custom.length ? 'The first matching window wins. Overnight windows belong to their starting day. Equal start and end times cover that whole day.'
          : 'Automatic mood selection is disabled. DJ playback can still use an explicitly selected mood or its usual unconstrained selection.';
    }

    function renderPreview() {
      try {
        const value = control.preview();
        if (!value) { allPreview.textContent = 'Read the saved settings to see their definitions.'; return; }
        allPreview.textContent = JSON.stringify({ moods: value.moods, programs: value.programs }, null, 2);
        selectedPreview.textContent = control.state.selected ? JSON.stringify(value.moods[control.state.selected], null, 2) : 'Choose a mood to see its complete definition.';
        const at = M.programAt(value.programs, previewDay.value, previewTime.value);
        previewResult.textContent = at ? `${previewDay.value.toUpperCase()} ${previewTime.value} → ${at.mood}`
          : `${previewDay.value.toUpperCase()} ${previewTime.value} → no automatic mood in this window`;
      } catch (error) {
        previewResult.textContent = error.message;
        selectedPreview.textContent = error.message;
        allPreview.textContent = 'Finish the fields described above to preview the complete effective configuration.';
      }
    }

    function render(state) {
      trigger.disabled = !control.canOpen(); trigger.setAttribute('aria-expanded', String(state.open));
      trigger.textContent = `Moods & daily programs${control.dirty() ? ' · draft' : ''}`;
      if (state.open && !dialog.open) dialog.showModal();
      else if (!state.open && dialog.open) { dialog.close(); if (!trigger.disabled) trigger.focus(); }
      status.textContent = state.message;
      const enabled = control.editable();
      refresh.disabled = state.loading || state.writing;
      discard.disabled = state.loading || state.writing || !control.dirty();
      moodSelect.disabled = !enabled; newName.disabled = !enabled; create.disabled = !enabled;
      const names = Object.keys(control.effective()).sort(), catalogue = JSON.stringify([names, state.selected]);
      if (catalogue !== catalogueKey) {
        catalogueKey = catalogue;
        moodSelect.replaceChildren(el('option', { value: '', text: 'Choose a reusable mood', disabled: true }),
          ...names.map(name => el('option', { value: name, text: name })));
        moodSelect.value = state.selected;
      }
      const builtin = !!state.base && Object.hasOwn(state.base.builtin_moods, state.selected);
      const override = !!state.draft && Object.hasOwn(state.draft.moods, state.selected);
      moodKind.textContent = !state.selected ? 'Create a mood, or choose one to edit its complete reusable definition.'
        : builtin ? override ? 'You have an override of this built-in mood.' : 'Built-in mood. Editing creates a saved override.' : 'Your custom reusable mood.';
      remove.hidden = !state.selected || !override; remove.disabled = !enabled;
      remove.textContent = builtin ? 'Restore built-in default' : 'Remove custom mood';
      const key = JSON.stringify([state.selected, state.mood, enabled]);
      if (moodKey !== key) {
        moodKey = key;
        restoreFocus(moodFields, () => {
          moodFields.replaceChildren();
          if (!state.mood) return;
          moodFields.append(D.editor({ draft: state.mood.direction, change: control.editMood, disabled: !enabled,
            el, idPrefix: `${id}-filters`, focusAttribute: focusKey, open: filtersOpen, toggle: open => { filtersOpen = open; },
            includeDuration: false, note: 'These are the complete rules for this reusable mood. Session steering and standing taste are applied when the DJ selects music.' }));
          const songs = el('textarea', { id: `${id}-song-keywords`, rows: 3, maxlength: 16384, disabled: !enabled,
            [focusKey]: 'song-keywords', oninput: event => control.editMood('song_keywords', event.target.value) }); songs.value = state.mood.song_keywords;
          const expiry = el('input', { id: `${id}-expiry`, type: 'text', inputmode: 'numeric', value: state.mood.expires_at,
            maxlength: 24, placeholder: 'No expiry', disabled: !enabled, [focusKey]: 'expiry', oninput: event => control.editMood('expires_at', event.target.value) });
          moodFields.append(el('div', { class: 'dj-config-extra' }, labeled('Song alternatives to classical periods (one keyword per line)', songs),
            el('p', { class: 'dj-config-note', text: 'Classical works use the selected periods; songs instead match any of these keywords in their title, album, artists or genres.' }),
            labeled('Absolute rule expiry (Unix seconds, optional)', expiry),
            el('p', { class: 'dj-config-note', text: 'Blank keeps these reusable rules indefinitely. An existing exact expiry is preserved unless you edit this field.' })));
        });
      }
      renderPrograms(state, enabled); renderPreview();
      review.disabled = !enabled || state.stale || state.uncertain || !control.dirty();
      reviewBox.hidden = !state.pending; confirm.disabled = !enabled || !state.pending; back.disabled = state.writing;
      if (state.pending) {
        reviewSummary.textContent = `${Object.keys(state.pending.config.moods).length} saved mood definitions; ${state.pending.config.programs === null ? 'built-in daily program' : state.pending.config.programs.length ? state.pending.config.programs.length + ' ordered time windows' : 'automatic program disabled'}.`;
        reviewJson.textContent = JSON.stringify(state.pending.config, null, 2);
      }
      comparison.hidden = !state.stale && !state.uncertain;
      draftPreview.textContent = state.draft ? JSON.stringify(state.draft, null, 2) : '';
      latestPreview.textContent = state.latest ? JSON.stringify(state.latest.config, null, 2) : 'No valid saved configuration has been read.';
      if (state.pending !== shown) { shown = state.pending; if (shown) reviewBox.focus({ preventScroll: true }); }
    }

    previewDay.addEventListener('change', renderPreview); previewTime.addEventListener('input', renderPreview);
    dialog.addEventListener('cancel', event => { event.preventDefault(); control.close(); });
    dialog.addEventListener('close', () => { if (control.state.open) control.close(); });
    const moodSection = el('section', { class: 'dj-config-moods' }, el('h3', { text: 'Reusable moods' }),
      labeled('Mood definition', moodSelect), el('div', { class: 'dj-config-create' }, labeled('New mood name', newName), create),
      moodKind, remove, moodFields,
      el('details', {}, el('summary', { text: 'Complete selected mood definition' }), selectedPreview));
    const programSection = el('section', {}, el('h3', { text: 'Automatic daily program' }), labeled('Program behavior', mode),
      el('p', { class: 'dj-config-note', text: 'The program follows the daemon’s local clock. A group follows it whenever no temporary session mood is selected.' }),
      programNote, rows, add, el('h3', { text: 'Try a day and time' }),
      el('div', { class: 'dj-config-times' }, labeled('Day at the daemon', previewDay), labeled('Time at the daemon', previewTime)), previewResult);
    dialog.append(el('header', { class: 'dj-config-heading' }, el('div', {}, el('h2', { id: `${id}-title`, text: 'Your moods, throughout the day' }),
      el('p', { text: 'Make reusable listening rules and choose when the house’s DJ follows each one.' })), close),
      el('div', { class: 'dj-config-actions' }, refresh, discard), status,
      el('div', { class: 'dj-config-columns' }, moodSection, programSection),
      el('details', { class: 'dj-config-effective' }, el('summary', { text: 'Preview all effective definitions and time windows' }), allPreview),
      comparison, el('div', { class: 'dj-config-actions' }, review), reviewBox);
    render(control.state);
    return { node: el('div', { class: 'dj-configuration' }, trigger, dialog), control,
      suspend: control.suspend, resume: control.resume,
      dispose() { control.dispose(); if (dialog.open) dialog.close(); } };
  }
  return Object.freeze({ panel });
})();
if (typeof module !== 'undefined' && module.exports) module.exports = DjConfigurationPanel;
if (typeof document !== 'undefined' && typeof ROUTES !== 'undefined') {
  Object.assign(ROUTES, DjConfiguration.routes);
  const configurationMain = document.getElementById('households');
  if (configurationMain) {
    const configurationPanel = DjConfigurationPanel.panel({
      call: (url, body, options) => call(body === undefined ? 'djConfigurationRead' : 'djConfigurationWrite', { body, signal: options?.signal }),
      saved: () => { if (typeof catalogs !== 'undefined') catalogs.refresh(); },
    });
    const configurationRoot = el('section', { class: 'scenes', 'aria-label': 'Reusable DJ moods and daily programs' });
    configurationRoot.append(configurationPanel.node); configurationMain.before(configurationRoot);
    const resumeConfiguration = () => { if (!document.hidden) configurationPanel.resume(); };
    document.addEventListener('visibilitychange', () => document.hidden ? configurationPanel.suspend() : resumeConfiguration());
    window.addEventListener('pagehide', configurationPanel.suspend); window.addEventListener('pageshow', resumeConfiguration);
    if (document.hidden) configurationPanel.suspend();
  }
}
