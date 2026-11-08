// Standing taste uses the daemon's preferences, independent of online rooms.
'use strict';
const DashboardPreferences = (() => {
  const C = typeof DashboardCore !== 'undefined' ? DashboardCore : require('./dashboard-core.js');
  const P = typeof DjPreferencesRemote !== 'undefined' ? DjPreferencesRemote : require('./dj-preferences.js');
  const MAX_AGE = 60000;
  const groups = [
    ['favor', 'Favor', 'Give these more weight.'],
    ['avoid', 'Avoid', 'Set these aside when other music fits. Avoids can relax if nothing else is left.'],
    ['pin', 'Pin', 'Keep these strongly favored. Pins outweigh avoids.'],
    ['ban', 'Ban', 'Exclude these from ordinary selection. Bans do not relax.'],
  ];
  const label = key => key.replace('.', ' ').replace(/^./, value => value.toUpperCase());
  const valueOf = (preferences, key) => key.includes('.')
    ? preferences[key.split('.')[0]][key.split('.')[1]] : preferences[key];
  const signature = (preferences, key) => JSON.stringify(valueOf(preferences, key));
  const fail = message => { throw new Error(message); };
  function description(body) {
    if (body.key === 'energy') return body.unset ? 'Follow time-of-day energy.' : `Set default energy to ${body.value} out of 100.`;
    if (body.key === 'explicit') return body.unset || body.value === 'false' ? 'Keep explicit tracks out.' : 'Allow explicit tracks.';
    return `${body.unset ? 'Remove' : 'Add'} “${body.value}” ${body.unset ? 'from' : 'to'} ${label(body.key).toLowerCase()}.`;
  }
  function model({ call, controller, changed = () => {}, now = () => performance.now() }) {
    let version = 0, active = true, disposed = false, reading = null, readAt = 0;
    const state = { open: false, loaded: false, loading: false, writing: false, preferences: null,
      pending: null, result: null, message: '', draft: { key: 'favor.genres', value: '', energy: '', explicit: 'false' } };
    const publish = () => { if (!disposed) { try { changed(state); } catch (_) { /* Presentation only. */ } } };
    const live = () => !disposed && active && state.open && controller.state.active;
    const canRead = () => live() && !state.loading && !state.writing && !controller.state.busy;
    const editable = () => live() && state.loaded && !state.loading && !state.writing
      && now() - readAt < MAX_AGE && controller.canManage();
    function retire() {
      ++version; reading?.abort(); reading = null;
      state.loading = false; state.loaded = false; state.pending = null;
    }
    function observe() {
      if (!state.writing && state.loaded && now() - readAt >= MAX_AGE) {
        retire(); state.message = 'Refresh preferences before another change.';
      } else if (!state.writing && state.pending && (!live() || !controller.canManage())) {
        state.pending = null;
      }
      publish();
    }
    function accept(preferences) {
      state.preferences = preferences;
      state.draft.energy = preferences.energy === null ? '' : String(preferences.energy);
      state.draft.explicit = String(preferences.explicit);
    }
    async function read(explicit = true) {
      if (!canRead()) return false;
      retire();
      const v = version, abort = new AbortController(), started = now();
      reading = abort; state.loading = true; state.message = 'Reading your standing preferences…'; publish();
      const valid = () => {
        if (!live() || v !== version || now() - started >= MAX_AGE) fail('This preference read is no longer current. Refresh again.');
      };
      try {
        const preferences = await controller.refreshStored('preferences', value => { valid(); return P.preferencesOf(value); },
          { explicit, signal: abort.signal });
        valid();
        if (preferences === null) fail('The dashboard changed while reading preferences. Refresh them again.');
        accept(preferences); state.loaded = true; readAt = started;
        state.message = controller.state.uncertain
          ? 'An earlier command is uncertain. Use its refresh control before making another change.' : '';
        return true;
      } catch (error) {
        if (live() && v === version) state.message = C.problem(error);
        return false;
      } finally {
        if (reading === abort) { reading = null; state.loading = false; publish(); }
      }
    }
    function edit(field, value) {
      if (!editable() || !['key', 'value', 'energy', 'explicit'].includes(field) || typeof value !== 'string'
          || value.length > 512 || (field === 'key' && !P.listKeys.includes(value))) return false;
      state.draft[field] = value; state.pending = null; state.message = ''; publish(); return true;
    }
    function review(key, value, unset = false) {
      if (!editable()) return null;
      state.pending = null;
      try {
        const body = P.changeOf(key, value, unset);
        state.pending = Object.freeze({ body, before: signature(state.preferences, key),
          description: description(body), version });
        state.message = ''; state.result = null;
      } catch (error) { state.message = C.problem(error); }
      publish(); return state.pending;
    }
    const canApply = () => !!(editable() && state.pending && state.pending.version === version);
    async function apply(reviewed = state.pending) {
      if (!canApply() || reviewed !== state.pending) return false;
      const v = version, abort = new AbortController();
      reading = abort; state.pending = null; state.writing = true; state.result = null;
      state.message = 'Checking the preference you reviewed…'; publish();
      let receipt = null;
      try {
        const ok = await controller.mutate(async ({ check, send }) => {
          const valid = () => {
            check();
            if (!live() || v !== version || now() - readAt >= MAX_AGE) fail('This review is no longer current. Refresh preferences and review again.');
          };
          valid();
          const current = P.preferencesOf(await call('/dj/preferences', undefined, { signal: abort.signal })); valid();
          if (signature(current, reviewed.body.key) !== reviewed.before) {
            fail('This preference changed since you reviewed it. Refresh preferences and review again.');
          }
          // Other keys may change concurrently: send only the reviewed key.
          valid(); reading = null;
          const answer = await send('/dj/preferences', reviewed.body);
          if (!answer || typeof answer.done !== 'string' || !answer.done.trim() || typeof answer.changed !== 'boolean') {
            fail('The preference save response was incomplete.');
          }
          receipt = { done: answer.done, changed: answer.changed, preferences: P.preferencesOf(answer.preferences) };
          return { done: receipt.done, changed: receipt.changed };
        }, { stored: 'preferences' });
        if (!disposed) {
          if (ok) {
            accept(receipt.preferences); readAt = now();
            state.result = Object.freeze({ done: receipt.done, changed: receipt.changed });
          }
          state.loaded = ok && live() && v === version;
          state.message = controller.state.message || 'The dashboard is busy. No preference was changed.';
        }
        return ok;
      } finally {
        if (reading === abort) reading = null;
        state.writing = false; publish();
      }
    }
    return { state, canRead, editable, read, edit, review, canApply, apply, observe,
      cancel() { if (state.writing) return false; state.pending = null; publish(); return true; },
      open() { if (disposed || !active || !controller.state.active) return false; state.open = true; publish(); return true; },
      close() { retire(); state.open = false; publish(); },
      suspend() { active = false; retire(); state.open = false; publish(); },
      resume() { if (!disposed) { active = true; publish(); } },
      dispose() { active = false; disposed = true; retire(); state.open = false; } };
  }

  function panel({ call, controller, document: doc = document, now }) {
    const $ = id => doc.getElementById(id), dialog = $('preferences-dialog');
    const trigger = $('open-preferences'), lists = $('preferences-lists');
    const input = Object.fromEntries(['key', 'value', 'energy', 'explicit'].map(field => [field, $('preferences-' + field)]));
    let observed = null, renderedReady = null, shown = null, previousFocus = null;
    const el = (tag, cls, text) => {
      const node = doc.createElement(tag);
      if (cls) node.className = cls;
      if (text != null) node.textContent = text;
      return node;
    };
    for (const key of P.listKeys) { const option = el('option', '', label(key)); option.value = key; input.key.append(option); }
    const control = model({ call, controller, now, changed: render });
    function render(state) {
      const ready = control.editable();
      trigger.disabled = !controller.state.active || (controller.state.busy && !state.writing);
      trigger.setAttribute('aria-expanded', String(state.open));
      if (state.open && !dialog.open) dialog.showModal();
      else if (!state.open && dialog.open) { dialog.close(); if (!trigger.disabled) trigger.focus(); }
      $('preferences-read').disabled = !control.canRead();
      for (const [field, node] of Object.entries(input)) { node.disabled = !ready; if (node.value !== state.draft[field]) node.value = state.draft[field]; }
      for (const name of ['energy-save', 'energy-reset', 'explicit-save', 'add']) $('preferences-' + name).disabled = !ready;
      $('preferences-message').textContent = state.message;
      $('preferences-freshness').textContent = state.loading ? 'Reading the daemon’s current preferences…'
        : state.loaded ? 'Standing taste · applies to every group’s next DJ selections.'
          : state.preferences ? 'Last observation · refresh before making changes.' : 'Read preferences to see your current taste.';
      const preferences = state.preferences;
      if (preferences !== observed || ready !== renderedReady) {
        const focused = doc.activeElement?.getAttribute('data-preference-item');
        observed = preferences; renderedReady = ready; lists.replaceChildren();
        if (preferences) for (const [group, title, help] of groups) {
          const section = el('section', 'preference-group'); section.dataset.kind = group;
          section.append(el('h3', '', title), el('p', 'small-note', help));
          const rows = el('ul', 'preference-items'); let count = 0;
          for (const key of P.listKeys.filter(key => key.startsWith(group + '.'))) {
            for (const [index, value] of valueOf(preferences, key).entries()) {
              ++count;
              const item = el('li'), copy = el('span', 'preference-item-copy');
              copy.append(el('small', '', key.split('.')[1]), el('span', '', value));
              const remove = el('button', 'text-button', 'Remove'); remove.type = 'button'; remove.disabled = !ready;
              remove.setAttribute('aria-label', `Remove ${value} from ${label(key).toLowerCase()}`);
              remove.setAttribute('data-preference-item', key + ':' + index);
              remove.addEventListener('click', () => control.review(key, value, true)); item.append(copy, remove); rows.append(item);
            }
          }
          section.append(count ? rows : el('p', 'preference-empty', 'Nothing added yet.')); lists.append(section);
        }
        if (focused) [...lists.querySelectorAll('[data-preference-item]')].find(node => node.getAttribute('data-preference-item') === focused && !node.disabled)?.focus({ preventScroll: true });
      }
      const pending = state.pending;
      $('preferences-preview').hidden = !pending;
      if (pending) {
        $('preferences-preview-title').textContent = pending.description;
        const before = valueOf(preferences, pending.body.key);
        $('preferences-preview-before').textContent = pending.body.key === 'energy'
          ? `Current default: ${before === null ? 'time of day' : before + ' out of 100'}.`
          : pending.body.key === 'explicit' ? `Currently ${before ? 'allowing' : 'excluding'} explicit tracks.`
            : `Current ${label(pending.body.key).toLowerCase()}: ${before.length ? before.join(' · ') : 'none'}.`;
      }
      $('preferences-confirm').disabled = !control.canApply(); $('preferences-back').disabled = state.writing;
      $('preferences-result').hidden = !state.result;
      $('preferences-result').textContent = state.result ? `${state.result.done} Applies from future DJ picks; queued music keeps playing.` : '';
      if (pending !== shown) {
        if (pending) { previousFocus = doc.activeElement; $('preferences-preview').focus({ preventScroll: true }); }
        else if (shown && state.open && !state.writing && previousFocus?.isConnected && !previousFocus.disabled) previousFocus.focus({ preventScroll: true });
        shown = pending;
      }
    }
    trigger.addEventListener('click', () => { if (control.open()) void control.read(false); });
    $('close-preferences').addEventListener('click', control.close);
    $('preferences-read').addEventListener('click', () => void control.read(true));
    for (const [field, node] of Object.entries(input)) {
      node.addEventListener(field === 'key' || field === 'explicit' ? 'change' : 'input', () => control.edit(field, node.value));
    }
    $('preferences-form').addEventListener('submit', event => {
      event.preventDefault(); control.review(control.state.draft.key, control.state.draft.value);
    });
    $('preferences-energy-save').addEventListener('click', () => control.review('energy', control.state.draft.energy));
    $('preferences-energy-reset').addEventListener('click', () => control.review('energy', null, true));
    $('preferences-explicit-save').addEventListener('click', () => control.review('explicit', control.state.draft.explicit));
    $('preferences-back').addEventListener('click', control.cancel);
    $('preferences-confirm').addEventListener('click', () => void control.apply());
    dialog.addEventListener('cancel', event => { event.preventDefault(); control.close(); });
    dialog.addEventListener('close', () => { if (control.state.open) control.close(); });
    render(control.state);
    return { control, observe: control.observe, suspend: control.suspend, resume: control.resume,
      dispose() { control.dispose(); if (dialog.open) dialog.close(); } };
  }
  return { MAX_AGE, model, panel };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = DashboardPreferences;
