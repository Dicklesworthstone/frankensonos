// A reviewed, reload-aware composer over the existing scene and schedule APIs.
'use strict';

const FavoriteRoutinePanel = (() => {
  const JOURNAL_KEY = 'fsonos.favorite-routine.v1';
  const MAX_JOURNAL = 262144;

  // Session-scoped: survives reload in this tab, not closing the tab or clearing
  // its data. Access storage lazily so an unavailable store becomes visible UI.
  function sessionJournal(storage = () => globalThis.sessionStorage) {
    return {
      load() {
        const raw = storage().getItem(JOURNAL_KEY);
        if (raw === null) return null;
        if (typeof raw !== 'string' || raw.length > MAX_JOURNAL) {
          throw new Error('The saved routine attempt is too large to inspect. Check Scenes and Routines before clearing this tab’s data.');
        }
        try { return JSON.parse(raw); }
        catch (_) { throw new Error('The saved routine attempt is unreadable. Check Scenes and Routines before clearing this tab’s data.'); }
      },
      save(value) {
        const raw = JSON.stringify(value);
        if (typeof raw !== 'string' || raw.length > MAX_JOURNAL) throw new Error('This routine is too large to save safely in this tab.');
        storage().setItem(JOURNAL_KEY, raw);
        if (storage().getItem(JOURNAL_KEY) !== raw) throw new Error('This tab could not keep the routine’s recovery record. No further setup will be sent.');
      },
      clear() { storage().removeItem(JOURNAL_KEY); },
    };
  }

  function panel({ core, call, el, doc = globalThis.document, journal = sessionJournal(), nonce }) {
    const content = el('div', { class: 'favorite-routine-content' });
    const heading = el('summary', { text: 'Favorite routines' });
    const node = el('details', { id: 'favorite-routines-section', class: 'favorite-routines' }, heading, content);
    const root = el('section', { id: 'favorite-routines-root' }, node);
    let draft = { label: '', cadence: 'weekdays', time: '07:30', minutes: '45', favorite: '' };
    let lastPending = null, resumeFocus = 'review', wasBusy = false;
    const control = core.model({ call, journal, ...(nonce ? { nonce } : {}),
      active: () => node.open && node.isConnected && !doc.hidden && !node.closest('[hidden], [inert]'),
      changed: (state) => render(state) });
    const button = (label, key, handler, disabled = false) => el('button', {
      type: 'button', text: label, disabled, 'data-routine-focus': key, onclick: handler,
    });
    const paragraph = (text) => el('p', { text });
    const phaseText = {
      idle: 'Choose a favorite routine', choosing: 'Choose a favorite routine', checking: 'Checking the reviewed group and favorite',
      scene_sending: 'Saving the scene', schedule_sending: 'Creating the routine',
      scene_ready: 'Scene saved; routine not submitted', unknown: 'Setup needs inspection',
      blocked: 'Setup blocked', complete: 'Routine saved or observed',
    };

    function select(label, key, value, options, handler, disabled) {
      const input = el('select', { disabled, 'data-routine-focus': key, onchange: (event) => handler(event.target.value) });
      for (const [v, text] of options) input.append(el('option', { value: v, text }));
      input.value = value;
      return el('label', {}, label, input);
    }

    function field(label, key, attrs, disabled) {
      const input = el('input', { ...attrs, value: draft[key], disabled, 'data-routine-focus': key,
        oninput: (event) => { draft[key] = event.target.value; } });
      return el('label', {}, label, input);
    }

    function form(state) {
      const disabled = state.busy || !!state.pending || !state.open;
      const formNode = el('form', { class: 'favorite-routine-form', onsubmit: (event) => {
        event.preventDefault(); control.review({ ...draft });
      } });
      formNode.append(
        field('Routine name', 'label', { type: 'text', maxlength: 80, autocomplete: 'off', required: true }, disabled),
        select('Group to save', 'group', state.group?.target || '', [['', 'Choose a current group…'],
          ...state.groups.map((g) => [g.target, g.label])], (target) => {
          draft.favorite = ''; void control.choose(target);
        }, disabled),
        select('Playable favorite', 'favorite', draft.favorite, [['', 'Choose a favorite…'],
          ...state.favorites.map((f) => [f.id, `${f.title} · ${f.kind}${f.description ? ` · ${f.description}` : ''}`])],
        (value) => { draft.favorite = value; }, disabled || !state.group || !state.favorites.length),
        select('When', 'cadence', draft.cadence, [['weekdays', 'Weekdays'], ['daily', 'Every day'],
          ['weekends', 'Weekends'], ['delay', 'Once after a delay']], (value) => {
          draft.cadence = value; render(control.state);
        }, disabled),
      );
      if (draft.cadence === 'delay') formNode.append(
        field('Delay in minutes', 'minutes', { type: 'number', min: 1, max: 10080, step: 1, required: true }, disabled),
        paragraph('The delay starts when the daemon accepts the routine, not when you open this form. Maximum: seven days.'),
      );
      else formNode.append(
        field('House time', 'time', { type: 'time', step: 60, required: true }, disabled),
        paragraph('Uses the daemon host’s time zone, not this browser’s. The daemon handles daylight-saving changes.'),
      );
      if (state.group && !state.busy && !state.favorites.length) formNode.append(paragraph('No playable favorites were read for this group. Add one in its household, then refresh the choices.'));
      formNode.append(el('div', { class: 'row' },
        button('Refresh choices', 'refresh', () => { draft.favorite = ''; void control.refresh(); }, state.busy || !!state.pending),
        button('Review favorite routine', 'review', () => control.review({ ...draft }), disabled || !state.group || !state.favorites.length),
      ));
      return formNode;
    }

    function review(plan, state) {
      const continuing = !!state.attempt;
      const section = el('section', { class: 'favorite-routine-review', 'aria-label': 'Review favorite routine' },
        el('h3', { text: continuing ? 'Schedule this saved scene?' : 'Save this scene and create an enabled routine?' }),
        paragraph(`${plan.favorite.title} · ${plan.favorite.kind}`),
        paragraph(`When: ${plan.when}${plan.when.startsWith('in ') ? ' after daemon acceptance' : ' in the house’s time zone'}`),
        paragraph(`Coordinator: ${plan.group.target}`),
        paragraph(`Exact saved group: ${plan.group.members.join(' + ')}`),
        paragraph('Each run restores this exact grouping. Listed rooms may leave other groups; rooms added later may be detached. It then starts the favorite, replacing whatever that group was playing.'),
        paragraph(plan.favorite.kind === 'container'
          ? 'This album or playlist replaces the group’s queue at run time; the routine is not a backup of that queue.'
          : 'The routine switches the group to this track or stream at run time.'),
        paragraph('This scene sets no volume or mute levels. Current levels remain subject to the house policy; a muted room may stay silent.'),
        paragraph('The scene and favorite are not frozen copies. Later scene edits, renamed favorites, changed playlists or account availability can affect future runs.'),
        paragraph('The daemon must be running. The routine uses its creator’s rights, and occurrences more than ten minutes late are skipped. No playback command is sent during setup.'),
        paragraph('Setup uses two separate saved changes, not an atomic transaction. A scene may remain without a routine. Closing this panel stops later submissions, not a request already sent.'),
        el('details', {}, el('summary', { text: 'Dedicated scene name' }), el('p', { class: 'favorite-routine-identity', text: plan.name })),
      );
      section.append(el('div', { class: 'row' },
        button('Cancel review', 'cancel', () => control.cancel(), state.busy),
        button(continuing ? 'Create routine for saved scene' : 'Save scene and create routine', 'confirm',
          () => { if (continuing) void control.continue(plan); else void control.confirm(plan); }, state.busy),
      ));
      return section;
    }

    function render(state) {
      const active = doc.activeElement;
      let key = content.contains(active) ? active.getAttribute('data-routine-focus') : null;
      const caret = active?.type === 'text' ? active.selectionStart : null;
      const newReview = state.pending && state.pending !== lastPending;
      if (newReview) { resumeFocus = key || 'review'; key = 'cancel'; }
      else if (lastPending && !state.pending && !state.busy) key = resumeFocus;
      else if (wasBusy && !state.busy && state.attempt && !key) key = 'inspect';
      lastPending = state.pending; wasBusy = state.busy;
      content.replaceChildren();
      content.setAttribute('aria-busy', String(state.busy));
      content.append(
        paragraph('Choose a favorite once, then let the daemon play it on schedule. Setup saves a dedicated scene and a routine; it does not play music now.'),
        el('p', { role: 'status', 'aria-live': 'polite', text: `${phaseText[state.phase] || 'Routine setup'}${state.message ? ` — ${state.message}` : ''}` }),
      );
      if (state.busy) content.append(paragraph('A reviewed step is in progress. Do not repeat setup from another tab. A lost response will be inspected, never automatically resubmitted.'));
      if (!state.attempt && state.phase !== 'blocked') content.append(form(state));
      if (state.attempt) {
        content.append(el('p', { class: 'favorite-routine-identity', text: `Saved setup: ${state.attempt.plan.name}` }));
        if (state.result && state.phase === 'complete') content.append(
          paragraph(`Routine #${state.result.id}: ${state.result.enabled ? 'enabled' : 'paused'}. Stored time: ${state.result.when}.`),
          paragraph(state.result.next ? `Next run reported by the daemon: ${state.result.next}` : 'No next run was reported; a one-time routine may already be finished.'),
        );
        const actions = el('div', { class: 'row' }, button('Inspect saved state', 'inspect', () => { void control.recover(); }, state.busy || !!state.pending));
        if (state.phase === 'scene_ready') actions.append(button('Review scheduling saved scene', 'review-continue', () => control.reviewContinue(), state.busy || !!state.pending));
        if (state.phase === 'complete') actions.append(button('New favorite routine', 'new', () => {
          draft = { label: '', cadence: 'weekdays', time: '07:30', minutes: '45', favorite: '' };
          void control.newRoutine();
        }, state.busy));
        content.append(actions, paragraph('Manage, pause or remove the saved routine in Routines. The dedicated scene is retained in Scenes; removing a routine does not delete its scene.'));
      }
      if (state.pending) content.append(review(state.pending, state));
      content.append(paragraph('Recovery is kept in this tab’s session storage. Reloading this tab preserves it; closing the tab or clearing its data may not. Another tab is not a shared lock. Inspect existing Scenes and Routines before recreating uncertain setup.'));
      const next = [...content.querySelectorAll('[data-routine-focus]')].find((item) => item.getAttribute('data-routine-focus') === key);
      if (node.open && next && !next.disabled) {
        next.focus({ preventScroll: true });
        if (next.type === 'text' && Number.isInteger(caret)) next.setSelectionRange(caret, caret);
      }
    }

    const toggle = () => { if (node.open && !doc.hidden) void control.refresh(); else control.close(); };
    const hide = () => { if (doc.hidden) { node.open = false; control.close(); } };
    const pagehide = () => { node.open = false; control.close(); };
    const workspaceChanged = () => {
      if (node.closest('[hidden], [inert]')) { node.open = false; control.close(); }
    };
    const escape = (event) => {
      if (event.key === 'Escape' && control.state.pending && !control.state.busy) {
        event.preventDefault(); control.cancel();
      }
    };
    node.addEventListener('toggle', toggle);
    node.addEventListener('keydown', escape);
    doc.addEventListener('visibilitychange', hide);
    doc.addEventListener('fsonos:studio-selection', workspaceChanged);
    doc.defaultView?.addEventListener('pagehide', pagehide);
    render(control.state);
    return { node: root, dispose() {
      node.removeEventListener('toggle', toggle);
      node.removeEventListener('keydown', escape);
      doc.removeEventListener('visibilitychange', hide);
      doc.removeEventListener('fsonos:studio-selection', workspaceChanged);
      doc.defaultView?.removeEventListener('pagehide', pagehide);
      control.dispose();
    } };
  }
  return { JOURNAL_KEY, MAX_JOURNAL, sessionJournal, panel };
})();

if (typeof module !== 'undefined' && module.exports) module.exports = FavoriteRoutinePanel;
else {
  Object.assign(ROUTES, FavoriteRoutines.routes);
  document.getElementById('scenes-section')?.before(FavoriteRoutinePanel.panel({ core: FavoriteRoutines, call, el }).node);
}
