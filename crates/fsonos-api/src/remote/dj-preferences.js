// Standing DJ taste, edited one preference at a time through the public API.
'use strict';

const DjPreferencesRemote = (() => {
  const routes = {
    djPreferencesRead: ['GET', '/dj/preferences'],
    djPreferencesWrite: ['POST', '/dj/preferences'],
  };
  const groups = {
    favor: ['genres', 'artists', 'eras', 'moods'],
    avoid: ['genres', 'artists', 'eras', 'moods'],
    pin: ['artists', 'albums', 'tracks'],
    ban: ['artists', 'albums', 'tracks'],
  };
  const listKeys = Object.entries(groups).flatMap(([group, fields]) => fields.map((field) => `${group}.${field}`));
  const label = (key) => key.replace('.', ' ').replace(/^./, (c) => c.toUpperCase());
  const text = (value) => typeof value === 'string' && value.trim() !== '';
  const message = (error) => error && typeof error.message === 'string'
    ? `${error.message}${error.hint ? ` ${error.hint}` : ''}` : 'The preference request failed.';

  function preferencesOf(value) {
    if (!value || typeof value !== 'object' || typeof value.explicit !== 'boolean'
        || !(value.energy === null || (Number.isInteger(value.energy) && value.energy >= 0 && value.energy <= 100))) {
      throw new Error('The DJ preferences are unreadable. Refresh before editing.');
    }
    const result = { energy: value.energy, explicit: value.explicit };
    for (const [group, fields] of Object.entries(groups)) {
      if (!value[group] || typeof value[group] !== 'object') {
        throw new Error('The DJ preferences are incomplete. Refresh before editing.');
      }
      const lists = {};
      for (const field of fields) {
        const items = value[group][field];
        if (!Array.isArray(items) || items.length > 5000
            || !items.every((item) => text(item) && [...item].length <= 256)) {
          throw new Error(`The ${label(`${group}.${field}`)} list is unreadable. Refresh before editing.`);
        }
        lists[field] = Object.freeze([...items]);
      }
      result[group] = Object.freeze(lists);
    }
    // Unknown future fields are not sent back: each write changes one key only.
    return Object.freeze(result);
  }

  function changeOf(key, raw, unset = false) {
    if (key !== 'energy' && key !== 'explicit' && !listKeys.includes(key)) {
      throw new Error('Choose a known DJ preference.');
    }
    if (typeof unset !== 'boolean') throw new Error('Choose whether to add or remove a preference.');
    const scalar = key === 'energy' || key === 'explicit';
    if (scalar && unset) return Object.freeze({ key, unset: true });
    if (typeof raw !== 'string') throw new Error('Enter one value, not a list of values.');
    const value = raw.trim();
    if (!value || [...value].length > 256) throw new Error('Enter one value of 1–256 characters.');
    if (key === 'energy' && (!/^\d+$/.test(value) || Number(value) > 100)) {
      throw new Error('Energy must be a whole number from 0 to 100.');
    }
    if (key === 'explicit' && !['true', 'false'].includes(value)) {
      throw new Error('Choose whether explicit tracks are allowed.');
    }
    // List removal always names one exact item, never the entire list.
    return Object.freeze({ key, value: key === 'energy' ? String(Number(value)) : value, unset });
  }

  function description(body) {
    if (body.key === 'energy') return body.unset ? 'Use time-of-day energy again.' : `Set default energy to ${body.value}.`;
    if (body.key === 'explicit') return body.unset || body.value === 'false' ? 'Keep explicit tracks out.' : 'Allow explicit tracks.';
    return `${body.unset ? 'Remove' : 'Add'} “${body.value}” ${body.unset ? 'from' : 'to'} ${label(body.key).toLowerCase()}.`;
  }

  function model({ call, changed = () => {} }) {
    let epoch = 0, disposed = false;
    const state = { open: false, loading: false, writing: false, ready: false,
      preferences: null, pending: null, message: '' };
    const publish = () => { if (!disposed) changed(state); };
    const current = (version) => !disposed && state.open && version === epoch;

    async function load() {
      if (disposed) return false;
      state.open = true;
      if (state.writing) { publish(); return false; }
      const version = ++epoch;
      state.pending = null;
      state.ready = false;
      state.loading = true;
      state.message = '';
      publish();
      try {
        const preferences = preferencesOf(await call('djPreferencesRead'));
        if (!current(version)) return false;
        state.preferences = preferences;
        state.ready = true;
        return true;
      } catch (error) {
        if (current(version)) state.message = message(error);
        return false;
      } finally {
        if (current(version)) { state.loading = false; publish(); }
      }
    }

    function review(key, value, unset = false) {
      if (disposed || !state.open || !state.ready || state.loading || state.writing) return null;
      state.pending = null;
      try {
        const body = changeOf(key, value, unset);
        state.pending = Object.freeze({ body, description: description(body) });
        state.message = '';
      } catch (error) { state.message = message(error); }
      publish();
      return state.pending;
    }

    async function confirm(expected) {
      if (disposed || !state.open || !state.ready || state.loading || state.writing
          || !expected || state.pending !== expected) return false;
      state.pending = null;
      state.writing = true;
      state.ready = false;
      ++epoch;
      state.message = 'Saving one preference…';
      publish();
      try {
        const answer = await call('djPreferencesWrite', { body: expected.body });
        if (!answer || typeof answer.changed !== 'boolean' || !text(answer.done)) {
          throw new Error('The save response is unreadable.');
        }
        const preferences = preferencesOf(answer.preferences);
        if (disposed) return false;
        state.preferences = preferences;
        state.ready = state.open;
        state.message = answer.done;
        return true;
      } catch (error) {
        if (!disposed) state.message = `${message(error)} Refresh preferences before making another change; do not resend an uncertain save.`;
        return false;
      } finally {
        state.writing = false;
        publish();
      }
    }

    function cancel() {
      if (disposed || state.writing) return;
      state.pending = null;
      publish();
    }
    function close() {
      if (disposed) return;
      ++epoch;
      state.open = false;
      state.loading = false;
      state.ready = false;
      state.pending = null;
      publish();
    }
    function dispose() { close(); disposed = true; ++epoch; }
    return { state, load, review, confirm, cancel, close, dispose };
  }

  let nextId = 0;
  function panel({ call, el }) {
    const id = `dj-taste-${++nextId}`;
    const toggle = el('button', { type: 'button', text: 'DJ taste', 'aria-expanded': 'false', 'aria-controls': id });
    const box = el('div', { id, class: 'dj-taste-panel', hidden: true });
    const status = el('p', { role: 'status', 'aria-live': 'polite', class: 'dj-taste-status' });
    const refresh = el('button', { type: 'button', text: 'Refresh preferences' });
    const close = el('button', { type: 'button', text: 'Close' });
    const energy = el('input', { type: 'number', min: 0, max: 100, step: 1, id: `${id}-energy` });
    const energySave = el('button', { type: 'button', text: 'Review energy' });
    const energyReset = el('button', { type: 'button', text: 'Use time of day' });
    const explicit = el('select', { id: `${id}-explicit` },
      el('option', { value: 'false', text: 'Keep explicit tracks out' }),
      el('option', { value: 'true', text: 'Allow explicit tracks' }));
    const explicitSave = el('button', { type: 'button', text: 'Review explicit setting' });
    const key = el('select', { id: `${id}-key` }, ...listKeys.map((item) => el('option', { value: item, text: label(item) })));
    const value = el('input', { type: 'text', id: `${id}-value`, maxlength: 512,
      placeholder: 'One genre, artist, era, mood, title or Spotify URI' });
    const add = el('button', { type: 'submit', text: 'Review addition' });
    const form = el('form', { class: 'dj-taste-add' },
      el('label', { for: key.id, text: 'Preference' }), key,
      el('label', { for: value.id, text: 'One value (commas stay part of a name)' }), value, add);
    const lists = el('div', { class: 'dj-taste-lists' });
    const reviewBox = el('div', { class: 'dj-taste-review', hidden: true, tabindex: -1 });
    let observed = null, reviewed = null;
    const controls = [energy, energySave, energyReset, explicit, explicitSave, key, value, add];
    const editor = model({ call, changed: render });

    function render(state) {
      box.hidden = !state.open;
      toggle.setAttribute('aria-expanded', String(state.open));
      refresh.disabled = state.writing;
      for (const control of controls) control.disabled = !state.ready || state.loading || state.writing;
      for (const button of lists.querySelectorAll('button')) button.disabled = !state.ready || state.loading || state.writing;
      status.textContent = state.loading ? 'Reading standing preferences…' : state.message;
      if (state.preferences && observed !== state.preferences) {
        observed = state.preferences;
        energy.value = observed.energy === null ? '' : String(observed.energy);
        energy.placeholder = 'Time of day';
        explicit.value = String(observed.explicit);
        lists.replaceChildren();
        for (const [group, fields] of Object.entries(groups)) {
          const section = el('section', {}, el('h3', { text: label(group) }));
          let count = 0;
          for (const field of fields) {
            const items = observed[group][field];
            if (!items.length) continue;
            count += items.length;
            const row = el('ul', { class: 'dj-taste-items', 'aria-label': `${label(group)} ${field}` });
            for (const item of items) {
              const remove = el('button', { type: 'button', text: 'Remove', 'aria-label': `Remove ${item} from ${group} ${field}`,
                disabled: !state.ready || state.loading || state.writing,
                onclick: () => editor.review(`${group}.${field}`, item, true) });
              row.append(el('li', {}, el('span', { text: `${field}: ${item}` }), remove));
            }
            section.append(row);
          }
          if (!count) section.append(el('p', { text: 'None set.' }));
          lists.append(section);
        }
      }
      reviewBox.hidden = !state.pending;
      if (reviewed !== state.pending) {
        reviewed = state.pending;
        reviewBox.replaceChildren();
        if (reviewed) {
          const pending = reviewed;
          const save = el('button', { type: 'button', class: 'primary', text: 'Save for every DJ', onclick: () => { void editor.confirm(pending); } });
          reviewBox.append(el('p', { text: pending.description }),
            el('p', { text: 'This is a standing preference for every group, from its next DJ pick. It does not skip or restart music. Speaker undo does not revert it.' }),
            el('div', { class: 'row' }, save, el('button', { type: 'button', text: 'Cancel', onclick: () => editor.cancel() })));
          reviewBox.focus({ preventScroll: true });
        }
      }
    }

    toggle.addEventListener('click', () => { if (editor.state.open) editor.close(); else void editor.load(); });
    refresh.addEventListener('click', () => { void editor.load(); });
    close.addEventListener('click', () => { editor.close(); toggle.focus(); });
    form.addEventListener('submit', (event) => { event.preventDefault(); editor.review(key.value, value.value); });
    energySave.addEventListener('click', () => editor.review('energy', energy.value));
    energyReset.addEventListener('click', () => editor.review('energy', null, true));
    explicitSave.addEventListener('click', () => editor.review('explicit', explicit.value));
    // Editing a draft invalidates a confirmation that described an older value.
    for (const input of [energy, explicit, key, value]) {
      input.addEventListener('input', () => editor.cancel());
      input.addEventListener('change', () => editor.cancel());
    }
    box.append(el('h2', { text: 'Your standing DJ taste' }),
      el('p', { text: 'Favor nudges selection. Avoid can relax when nothing else is left; use Ban for a hard exclusion. Pins outweigh avoids, never bans. Per-room steering takes precedence over standing taste.' }),
      el('div', { class: 'row' }, refresh, close), status,
      el('div', { class: 'dj-taste-defaults' },
        el('label', { for: energy.id, text: 'Default energy (0–100)' }), energy,
        el('div', { class: 'row' }, energySave, energyReset),
        el('label', { for: explicit.id, text: 'Explicit tracks' }), explicit, explicitSave),
      form, reviewBox, lists);
    render(editor.state);
    return { node: el('div', { class: 'dj-taste' }, toggle, box), model: editor, dispose: editor.dispose };
  }

  return { routes, listKeys: Object.freeze(listKeys), preferencesOf, changeOf, model, panel };
})();

if (typeof module !== 'undefined' && module.exports) module.exports = DjPreferencesRemote;
if (typeof document !== 'undefined' && typeof ROUTES !== 'undefined') {
  Object.assign(ROUTES, DjPreferencesRemote.routes);
  const tasteMain = document.getElementById('households');
  if (tasteMain) {
    const tasteRoot = el('section', { class: 'scenes', 'aria-label': 'Standing DJ taste' });
    tasteRoot.append(DjPreferencesRemote.panel({ call, el }).node);
    tasteMain.before(tasteRoot);
  }
}
