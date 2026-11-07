// Capture the current house through native scene storage, then use the existing recall review.
'use strict';
const DashboardScenes = (() => {
  const C = typeof DashboardCore !== 'undefined' ? DashboardCore : require('./dashboard-core.js');
  const S = typeof DashboardSession !== 'undefined' ? DashboardSession : require('./dashboard-session.js');
  const MAX_AGE = 45000;
  const fail = message => { throw new Error(message); };
  const nameKey = name => name.replace(/[A-Z]/g, letter => letter.toLowerCase());
  function nameOf(value) {
    if (typeof value !== 'string') return null;
    const name = value.trim();
    return name && [...name].length <= 64 ? name : null;
  }
  const signature = groups => S.stable(groups.map(C.shape).sort());
  function catalogOf(value) {
    const scenes = S.scenes(value);
    if (new Set(scenes.map(scene => nameKey(scene.name))).size !== scenes.length) {
      fail('Saved scene names are ambiguous. Resolve them in the full remote before capturing.');
    }
    return Object.freeze(scenes);
  }
  function houseOf(value, scope) {
    const groups = C.zones(value);
    if (!groups.length || signature(groups) !== scope) {
      fail('The house groups changed. Refresh playback and read the house again.');
    }
    return Object.freeze(groups);
  }
  function confirmed(value, review) {
    const scene = S.scene(value);
    if (scene.name !== review.name || scene.groups.some(group => group.source.kind === (review.backupQueues ? 'queue' : 'queue_backup'))) {
      fail('The reply did not confirm the reviewed scene name and queue capture option.');
    }
    return scene;
  }

  function model({ call, controller, changed = () => {}, saved = () => {}, now = () => performance.now() }) {
    let version = 0, active = true, disposed = false, reading = null, readAt = 0;
    const state = { open: false, target: '', scope: '', groups: null, scenes: [], stale: true,
      loading: false, writing: false, name: '', backupQueues: false, pending: null, result: null, message: '' };
    const publish = () => { if (!disposed) { try { changed(state); } catch (_) { /* Presentation only. */ } } };
    const live = () => !disposed && active && state.open;
    const scope = () => signature(controller.state.items.map(item => item.zone));
    const matching = () => state.target === controller.state.selected && state.scope === scope();
    const canRead = () => !!(live() && controller.selected()?.fresh && controller.state.active && controller.state.topologyFresh
      && !controller.state.busy && !controller.state.loading && !state.loading && !state.writing);
    const editable = () => !!(live() && matching() && state.groups && !state.stale && controller.state.topologyFresh
      && !state.loading && !state.writing && now() - readAt < MAX_AGE && controller.canControl());
    function retire(message, discard = false) {
      ++version; reading?.abort(); reading = null; state.loading = false;
      state.pending = null; state.stale = true;
      if (discard) { state.groups = null; state.scenes = []; readAt = 0; }
      if (message) state.message = message;
    }
    function observe() {
      if (disposed) return;
      if (!matching()) {
        retire('The selected group or house grouping changed. Read the house again.', true);
        state.target = controller.state.selected; state.scope = scope();
      } else if ((state.loading || state.pending || (state.groups && !state.stale))
          && (!controller.selected()?.fresh || !controller.state.active || !controller.state.topologyFresh
            || (state.groups && !state.stale && now() - readAt >= MAX_AGE))) {
        retire('Read the current house before saving. Refresh playback first if it is stale.');
      }
      publish();
    }
    async function read() {
      if (!canRead()) return false;
      retire(); state.target = controller.state.selected; state.scope = scope();
      const v = version, expected = state.scope, abort = new AbortController(), started = now();
      reading = abort; state.loading = true; state.result = null; state.message = 'Reading house groups and saved scenes…'; publish();
      const valid = () => {
        if (!live() || v !== version || !matching() || !controller.selected()?.fresh || !controller.state.active
            || controller.state.busy || !controller.state.topologyFresh || now() - started >= MAX_AGE) {
          fail('The house observation is no longer current. Read it again.');
        }
      };
      try {
        const options = { signal: abort.signal };
        const [groups, scenes] = await Promise.all([call('/zones', undefined, options), call('/scenes', undefined, options)]);
        valid();
        const next = houseOf(groups, expected), catalog = catalogOf(scenes);
        state.groups = next; state.scenes = catalog; state.stale = false; readAt = started; state.message = ''; return true;
      } catch (error) {
        if (live() && v === version) state.message = C.problem(error);
        return false;
      } finally {
        if (reading === abort) { reading = null; state.loading = false; publish(); }
      }
    }
    function edit(field, value) {
      if (!live() || state.loading || state.writing || (field === 'name' ? typeof value !== 'string'
        : field !== 'backupQueues' || typeof value !== 'boolean')) return false;
      ++version; state.pending = null; state.result = null; state[field] = value;
      state.message = field === 'name' && value.trim() && !nameOf(value) ? 'Use a scene name of 1 to 64 characters.' : '';
      publish(); return true;
    }
    const canReview = () => !!(editable() && nameOf(state.name));
    function review() {
      if (!canReview()) return null;
      const asked = nameOf(state.name), existing = state.scenes.find(scene => nameKey(scene.name) === nameKey(asked)) || null;
      // Reuse the stored spelling: a case-only edit must not create a second saved name.
      state.pending = Object.freeze({ version, target: state.target, scope: state.scope, groups: state.groups,
        name: existing?.name || asked, existing, backupQueues: state.backupQueues,
        body: Object.freeze(state.backupQueues ? { backup_queues: true } : {}) });
      state.message = ''; state.result = null; publish(); return state.pending;
    }
    const canApply = () => !!(editable() && state.pending && state.pending.version === version);
    async function apply(reviewed = state.pending) {
      if (!canApply() || reviewed !== state.pending) return false;
      const v = version;
      let captured = null, ok = false;
      state.pending = null; state.writing = true; state.result = null; state.message = 'Checking the saved name and house groups…'; publish();
      try {
        ok = await controller.mutate(async ({ target, check, send }) => {
          const valid = () => {
            check();
            if (!live() || v !== version || target !== reviewed.target || !matching() || !controller.state.topologyFresh
                || now() - readAt >= MAX_AGE) fail('The capture review is no longer current. Read the house and review again.');
          };
          valid();
          const [groups, scenes] = await Promise.all([call('/zones'), call('/scenes')]); valid();
          houseOf(groups, reviewed.scope);
          const existing = catalogOf(scenes).find(scene => nameKey(scene.name) === nameKey(reviewed.name)) || null;
          if (S.stable(existing) !== S.stable(reviewed.existing)) {
            fail('That saved scene changed while you reviewed it. Read the house and review the replacement again.');
          }
          valid();
          captured = confirmed(await send('/scenes/' + encodeURIComponent(reviewed.name), reviewed.body, { method: 'PUT' }), reviewed);
          return { done: `Saved scene “${captured.name}”. Playback was not changed. Review the saved scene separately to recall it.`, changed: true };
        }, { wholeHouse: true });
        if (ok) { state.result = captured; state.message = controller.state.message; }
        else state.message = controller.state.message || 'No scene was saved. Refresh playback and read the house again.';
      } finally {
        state.writing = false; retire(); publish();
      }
      if (ok) {
        try { await saved(captured); }
        catch (_) { state.message += ' Refresh the scene list before recalling it.'; }
        publish();
      }
      return ok;
    }
    return { state, read, canRead, editable, edit, canReview, review, canApply, apply, observe,
      cancelReview() { if (state.writing) return false; ++version; state.pending = null; publish(); return true; },
      open() {
        if (disposed || !active) return Promise.resolve(false);
        retire(undefined, true); state.open = true; state.target = controller.state.selected; state.scope = scope(); publish(); return read();
      },
      close() { state.open = false; retire(undefined, true); publish(); },
      suspend() { active = false; state.open = false; retire(undefined, true); publish(); },
      resume() { if (!disposed) { active = true; observe(); } },
      dispose() { disposed = true; active = false; state.open = false; retire(undefined, true); },
    };
  }

  function panel({ call, controller, session, document: doc = document, now }) {
    const $ = id => doc.getElementById(id), dialog = $('capture-scene-dialog'), trigger = $('open-capture-scene'), preview = $('capture-scene-preview');
    let opener = null, reviewFocus = null, shown = null, groupsShown = null, existingShown = null, savedShown = null;
    const control = model({ call, controller, now, saved: () => session.catalog(), changed: render });
    const element = (tag, text) => { const node = doc.createElement(tag); if (text !== undefined) node.textContent = text; return node; };
    function detail(scene) {
      const fragment = element('div');
      for (const group of scene.groups) {
        fragment.append(element('p', `${[group.coordinator, ...group.members].join(' + ')} · ${S.sceneSource(group.source)} · ${group.playing == null ? 'playback unchanged on recall' : group.playing ? 'playing' : 'paused or stopped'}`));
      }
      const levels = Object.entries(scene.volumes).map(([room, value]) => `${room}: ${value}`).join(' · ');
      const mutes = Object.entries(scene.mutes).map(([room, value]) => `${room}: ${value ? 'muted' : 'unmuted'}`).join(' · ');
      if (levels) fragment.append(element('p', 'Room levels: ' + levels));
      if (mutes) fragment.append(element('p', 'Room mutes: ' + mutes));
      return fragment;
    }
    function render(s) {
      trigger.disabled = !controller.selected() || !controller.state.active || controller.state.busy;
      trigger.setAttribute('aria-expanded', String(s.open));
      if (s.open && !dialog.open) dialog.showModal();
      else if (!s.open && dialog.open) { dialog.close(); if (opener && !opener.disabled) opener.focus(); }
      $('capture-scene-read').disabled = !control.canRead();
      $('capture-scene-name').disabled = $('capture-scene-backup').disabled = s.loading || s.writing;
      if ($('capture-scene-name').value !== s.name) $('capture-scene-name').value = s.name;
      $('capture-scene-backup').checked = s.backupQueues;
      $('capture-scene-review').disabled = !control.canReview();
      $('capture-scene-message').textContent = s.message;
      if (groupsShown !== s.groups) {
        groupsShown = s.groups; $('capture-scene-groups').replaceChildren();
        for (const group of s.groups || []) $('capture-scene-groups').append(element('li', group.members.join(' + ') + ' · ' + group.household));
      }
      $('capture-scene-context').textContent = s.groups
        ? `${s.groups.length} listening group${s.groups.length === 1 ? '' : 's'} across ${new Set(s.groups.map(group => group.household)).size} household(s)${s.stale ? ' · read again before saving' : ''}.`
        : 'Read the current house groups and saved scenes to begin.';
      preview.hidden = !s.pending;
      if (s.pending) {
        const p = s.pending;
        $('capture-scene-preview-title').textContent = `${p.existing ? 'Replace' : 'Create'} scene “${p.name}”?`;
        $('capture-scene-preview-detail').textContent = p.backupQueues
          ? 'Save complete ordinary queues, up to 1,000 entries and 4 MiB per queue. Recall rebuilds their contents on the original players and replaces those players’ current queues. Action-history undo cannot recover the replaced contents.'
          : 'Save queue bookmarks. Recall can restore the selected track, position and mode only while each original player’s queue remains unchanged. Queue contents are not saved.';
        $('capture-scene-confirm').textContent = p.existing ? 'Replace saved scene' : 'Save scene';
        $('capture-scene-existing').hidden = !p.existing;
        if (existingShown !== p.existing) {
          existingShown = p.existing; $('capture-scene-existing-detail').replaceChildren(...(p.existing ? [detail(p.existing)] : []));
        }
      }
      $('capture-scene-confirm').disabled = !control.canApply();
      $('capture-scene-back').disabled = s.writing;
      $('capture-scene-result').hidden = !s.result;
      if (savedShown !== s.result) {
        savedShown = s.result; $('capture-scene-result-detail').replaceChildren(...(s.result ? [detail(s.result)] : []));
        $('capture-scene-result-title').textContent = s.result ? `Saved “${s.result.name}”` : '';
      }
      $('capture-scene-recall').disabled = !s.result || !controller.canControl()
        || !session.state.scenes.some(scene => scene.name === s.result.name);
      if (s.pending && s.pending !== shown) { reviewFocus = doc.activeElement; preview.focus(); }
      else if (!s.pending && shown && s.open && reviewFocus && !reviewFocus.disabled) reviewFocus.focus();
      shown = s.pending;
    }
    trigger.addEventListener('click', () => { opener = doc.activeElement; void control.open(); });
    $('capture-scene-read').addEventListener('click', () => { void control.read(); });
    $('capture-scene-name').addEventListener('input', event => control.edit('name', event.target.value));
    $('capture-scene-backup').addEventListener('change', event => control.edit('backupQueues', event.target.checked));
    $('capture-scene-review').addEventListener('click', () => control.review());
    $('capture-scene-back').addEventListener('click', () => control.cancelReview());
    $('capture-scene-confirm').addEventListener('click', () => { void control.apply(); });
    $('capture-scene-recall').addEventListener('click', () => {
      const scene = control.state.result;
      if (!scene || $('capture-scene-recall').disabled) return;
      control.close(); void session.reviewScene(scene.name);
    });
    $('close-capture-scene').addEventListener('click', () => control.close());
    dialog.addEventListener('cancel', event => { event.preventDefault(); control.close(); });
    dialog.addEventListener('close', () => { if (control.state.open) control.close(); });
    render(control.state); return control;
  }
  return { MAX_AGE, nameOf, catalogOf, signature, confirmed, model, panel };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = DashboardScenes;
