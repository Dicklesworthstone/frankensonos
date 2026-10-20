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
    const heading = el('summary', { text: 'Music routines' });
    const node = el('details', { id: 'favorite-routines-section', class: 'favorite-routines' }, heading, content);
    const root = el('section', { id: 'favorite-routines-root' }, node);
    const defaults = () => ({ label: '', cadence: 'weekdays', time: '07:30', minutes: '45',
      source_kind: 'favorite', source_uri: '', favorite: '', volume: '', unmute: false });
    let draft = defaults();
    let lastPending = null, resumeFocus = 'review', wasBusy = false;
    let incoming = null, imported = null, importing = false, offerMessage = '', skipOpenToggle = false, disposed = false;
    const control = core.model({ call, journal, ...(nonce ? { nonce } : {}),
      active: () => node.open && node.isConnected && !doc.hidden && !node.closest('[hidden], [inert]'),
      changed: (state) => render(state) });
    const button = (label, key, handler, disabled = false) => el('button', {
      type: 'button', text: label, disabled, 'data-routine-focus': key, onclick: handler,
    });
    const paragraph = (text) => el('p', { text });
    const hasDraft = () => !!control.state.group || Object.entries(defaults()).some(([key, value]) => draft[key] !== value);
    const visible = () => !disposed && node.open && node.isConnected && !doc.hidden && !node.closest('[hidden], [inert]');
    const phaseText = {
      idle: 'Choose a music routine', choosing: 'Choose a music routine', checking: 'Checking the reviewed group and music source',
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
        oninput: (event) => {
          draft[key] = event.target.value;
          if (key === 'source_uri' && imported) { imported = null; render(control.state); }
        } });
      return el('label', {}, label, input);
    }

    function form(state) {
      const disabled = state.busy || importing || !!state.pending || !state.open;
      const formNode = el('form', { class: 'favorite-routine-form', onsubmit: (event) => {
        event.preventDefault(); control.review({ ...draft });
      } });
      formNode.append(
        field('Routine name', 'label', { type: 'text', maxlength: 80, autocomplete: 'off', required: true }, disabled),
        select('Music source', 'source_kind', draft.source_kind, [['favorite', 'Sonos favorite'],
          ['spotify', 'Spotify track, album or playlist'], ['stream', 'Radio stream URL']], (value) => {
          imported = null;
          draft.source_kind = value; draft.favorite = '';
          void control.choose(state.group?.target || '', value);
        }, disabled),
        select('Group to save', 'group', state.group?.target || '', [['', 'Choose a current group…'],
          ...state.groups.map((g) => [g.target, g.label])], async (target) => {
          const selection = imported;
          draft.favorite = ''; await control.choose(target, draft.source_kind);
          if (visible() && imported === selection && draft.source_kind === 'favorite' && selection) {
            draft.favorite = core.libraryFavorite(selection, control.state.favorites, control.state.group)?.id || '';
            render(control.state);
          }
        }, disabled),
      );
      if (draft.source_kind === 'favorite') formNode.append(
        select('Playable favorite', 'favorite', draft.favorite, [['', 'Choose a favorite…'],
          ...state.favorites.map((f) => [f.id, `${f.title} · ${f.kind}${f.description ? ` · ${f.description}` : ''}`])],
        (value) => { draft.favorite = value; imported = null; render(control.state); }, disabled || !state.group || !state.favorites.length),
      );
      else formNode.append(
        field(draft.source_kind === 'spotify' ? 'Spotify URI or full share link' : 'Radio stream URL', 'source_uri',
          { type: 'text', maxlength: 2048, autocomplete: 'off', spellcheck: 'false', required: true }, disabled),
        paragraph(draft.source_kind === 'spotify'
          ? 'Paste a track, album or playlist link. The review shows its canonical Spotify URI. Short links are not expanded; this browser never contacts Spotify.'
          : 'Use the station’s direct HTTP or HTTPS audio stream, not its web page. The speaker must be able to reach it. Avoid private access keys: the source is saved in the scene and this tab’s recovery record.'),
      );
      formNode.append(
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
      formNode.append(
        field('Volume for each saved room (optional)', 'volume',
          { type: 'number', min: 0, max: 100, step: 1, placeholder: 'Keep current levels' }, disabled),
        el('label', { class: 'favorite-routine-unmute' },
          el('input', { type: 'checkbox', checked: draft.unmute, disabled, 'data-routine-focus': 'unmute',
            onchange: (event) => { draft.unmute = event.target.checked; } }),
          'Unmute the saved rooms when this routine runs'),
        paragraph('Leave volume blank and unmute unchecked to preserve current settings. A requested level applies to every listed room, subject to the house policy. Zero means silence, not “keep”.'),
      );
      if (draft.source_kind === 'favorite' && state.group && !state.busy && !state.favorites.length) formNode.append(paragraph('No playable favorites were read for this group. Add one in its household, then refresh the choices.'));
      formNode.append(el('div', { class: 'row' },
        button('Refresh choices', 'refresh', () => { draft.favorite = ''; void control.refresh(); }, state.busy || importing || !!state.pending),
        button('Review music routine', 'review', () => control.review({ ...draft }),
          disabled || !state.group || (draft.source_kind === 'favorite' && !state.favorites.length)),
      ));
      return formNode;
    }

    async function useIncoming(expected, keepGroup = true) {
      const state = control.state;
      if (incoming !== expected || importing || state.busy || state.pending || state.attempt || state.phase === 'blocked' || !visible()) return false;
      importing = true; offerMessage = ''; render(state);
      let result = null;
      try {
        result = await control.importSource(expected, { keepGroup, current: () => incoming === expected && visible() });
        if (!result || incoming !== expected || !visible()) return false;
        draft = { ...draft, source_kind: result.source_kind, source_uri: result.source_uri,
          favorite: result.favorite, label: draft.label.trim() ? draft.label : expected.title.slice(0, 80) };
        imported = result.selection; incoming = null;
        return true;
      } finally {
        importing = false;
        if (!result && incoming === expected) offerMessage = control.state.message || 'Music could not be filled in yet. Try again when the composer is available.';
        if (!disposed) render(control.state);
        if (result && !incoming && visible()) {
          content.querySelector(`[data-routine-focus="${control.state.group ? 'cadence' : 'group'}"]`)?.focus({ preventScroll: true });
        }
      }
    }

    function offerSource(payload) {
      if (disposed || doc.hidden || !root.isConnected) return false;
      let selection;
      try { selection = core.librarySourceOf(payload); } catch (_) { return false; }
      const fresh = !hasDraft() && !control.state.pending && !control.state.attempt && control.state.phase !== 'blocked';
      // Reuse the Studio's own navigation so hidden workspaces and accessible
      // navigation state stay synchronized; standalone remote panels also work.
      const workspace = root.closest('[data-studio-workspace]');
      if (workspace?.hidden) {
        [...doc.querySelectorAll('#studio-nav [data-workspace]')]
          .find((button) => button.dataset.workspace === workspace.dataset.studioWorkspace)?.click();
      }
      if (root.closest('[hidden], [inert]')) return false;
      incoming = selection; offerMessage = '';
      if (!node.open) { skipOpenToggle = true; node.open = true; }
      control.resumeDraft();
      node.scrollIntoView({ block: 'start', behavior: 'auto' });
      heading.focus({ preventScroll: true });
      if (fresh && !control.state.busy) void useIncoming(selection, false);
      return true;
    }

    function libraryMusic(state) {
      if (incoming) {
        const expected = incoming;
        const waiting = state.attempt || state.phase === 'blocked';
        const section = el('section', { class: 'routine-library-offer', 'aria-label': 'Music from library' },
          el('span', { class: 'routine-library-eyebrow', text: 'From your library' }),
          el('h3', { text: expected.title }),
          ...(expected.subtitle ? [paragraph(expected.subtitle)] : []),
          paragraph(expected.kind === 'favorite' ? `Sonos favorite · ${expected.household}` : `Spotify ${expected.source_uri.split(':')[1]}`));
        if (expected.kind === 'favorite') section.append(paragraph(`Favorite group: ${expected.group.target}. Its household and favorite are checked before filling the form.`));
        section.append(paragraph(importing ? 'Reading current rooms and checking this music…'
          : waiting ? 'Your saved setup is kept intact. Inspect or finish it before starting another routine.'
            : state.pending ? 'Your review is kept intact. Cancel that review before changing its music.'
              : 'Your current draft is kept below. Use this music when ready; your routine name, timing and sound settings stay the same.'));
        if (offerMessage) section.append(el('p', { role: 'alert', text: offerMessage }));
        const actions = el('div', { class: 'row' });
        if (state.attempt && state.phase === 'complete') actions.append(button('New routine with this music', 'library-new', async () => {
          if (incoming !== expected || state.busy) return;
          await control.newRoutine();
          if (!control.state.attempt && control.state.phase !== 'blocked' && incoming === expected) {
            draft = defaults(); imported = null;
            await useIncoming(expected, false);
          }
        }, state.busy || importing));
        else actions.append(button(expected.kind === 'favorite' ? 'Use this favorite and group' : 'Use this music', 'library-use',
          () => useIncoming(expected), state.busy || importing || !!state.pending || !!waiting));
        actions.append(button(state.attempt ? 'Keep current setup' : 'Keep current draft', 'library-dismiss', () => {
          if (incoming === expected && !importing) { incoming = null; offerMessage = ''; render(control.state); }
        }, importing));
        section.append(actions);
        return section;
      }
      if (!imported || state.attempt) return null;
      const section = el('section', { class: 'routine-library-selected', 'aria-label': 'Selected library music' },
        el('span', { class: 'routine-library-eyebrow', text: 'Music from your library' }),
        el('strong', { text: imported.title }), ...(imported.subtitle ? [paragraph(imported.subtitle)] : []));
      if (imported.kind === 'favorite' && !draft.favorite) section.append(paragraph(`Choose a current group and matching favorite in ${imported.household}.`));
      return section;
    }

    function review(plan, state) {
      const continuing = !!state.attempt;
      const settings = plan.settings || { volume: null, unmute: false };
      const container = plan.favorite?.kind === 'container' || /^spotify:(album|playlist):/.test(plan.source?.uri || '');
      const section = el('section', { class: 'favorite-routine-review', 'aria-label': 'Review music routine' },
        el('h3', { text: continuing ? 'Schedule this saved scene?' : 'Save this scene and create an enabled routine?' }),
        ...(imported && (plan.favorite?.id === imported.favorite || plan.source?.uri === imported.source_uri)
          ? [paragraph(imported.title)] : []),
        paragraph(core.sourceLabel(plan)),
        paragraph(`When: ${plan.when}${plan.when.startsWith('in ') ? ' after daemon acceptance' : ' in the house’s time zone'}`),
        paragraph(`Coordinator: ${plan.group.target}`),
        paragraph(`Exact saved group: ${plan.group.members.join(' + ')}`),
        paragraph('Each run restores this exact grouping. Listed rooms may leave other groups; rooms added later may be detached. It then starts the selected music source, replacing whatever that group was playing.'),
        paragraph(container
          ? 'This album or playlist replaces the group’s queue at run time; the routine is not a backup of that queue.'
          : 'The routine switches the group to this track or stream at run time.'),
        paragraph(settings.volume === null
          ? 'Volume: keep current levels, subject to the house policy.'
          : `Volume: request ${settings.volume} for each saved room, subject to its policy cap and fade. This can raise or lower the level.${settings.volume === 0 ? ' Zero is silent.' : ''}`),
        paragraph(settings.unmute
          ? 'Mute: explicitly unmute every saved room when the routine runs.'
          : 'Mute: keep current mute settings. A muted room may stay silent.'),
        paragraph('The scene is not a frozen copy of the music. Later scene edits, renamed favorites, changed playlists, stream or account availability can affect future runs. Saving is not a playback test.'),
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
        paragraph('Choose a favorite, Spotify music or a radio stream, then let the daemon play it on schedule. Setup saves a dedicated scene and a routine; it does not play music now.'),
        el('p', { role: 'status', 'aria-live': 'polite', text: `${phaseText[state.phase] || 'Routine setup'}${state.message ? ` — ${state.message}` : ''}` }),
      );
      const library = libraryMusic(state);
      if (library) content.append(library);
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
        if (state.phase === 'complete') actions.append(button('New music routine', 'new', () => {
          draft = defaults(); imported = null;
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
      if (newReview && node.open) content.querySelector('.favorite-routine-review')?.scrollIntoView({ block: 'start', behavior: 'auto' });
    }

    const toggle = () => {
      if (node.open && !doc.hidden) {
        if (skipOpenToggle) { skipOpenToggle = false; return; }
        if (hasDraft() || control.state.attempt || control.state.pending) control.resumeDraft();
        else void control.refresh();
      } else { skipOpenToggle = false; control.close(); }
    };
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
    const fromLibrary = (event) => { if (offerSource(event.detail)) event.preventDefault(); };
    node.addEventListener('toggle', toggle);
    node.addEventListener('keydown', escape);
    doc.addEventListener('visibilitychange', hide);
    doc.addEventListener('fsonos:studio-selection', workspaceChanged);
    doc.addEventListener('fsonos:routine-source', fromLibrary);
    doc.defaultView?.addEventListener('pagehide', pagehide);
    render(control.state);
    return { node: root, offerSource, dispose() {
      disposed = true;
      node.removeEventListener('toggle', toggle);
      node.removeEventListener('keydown', escape);
      doc.removeEventListener('visibilitychange', hide);
      doc.removeEventListener('fsonos:studio-selection', workspaceChanged);
      doc.removeEventListener('fsonos:routine-source', fromLibrary);
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
