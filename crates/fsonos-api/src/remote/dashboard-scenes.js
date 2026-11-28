// Capture the current house through native scene storage, then use the existing recall review.
'use strict';
const DashboardScenes = (() => {
  const C = typeof DashboardCore !== 'undefined' ? DashboardCore : require('./dashboard-core.js');
  const S = typeof DashboardSession !== 'undefined' ? DashboardSession : require('./dashboard-session.js');
  const Q = typeof SceneQueue !== 'undefined' ? SceneQueue : require('./scene-queue.js');
  const player = () => typeof SourcePlayerRemote !== 'undefined' ? SourcePlayerRemote : require('./source-player.js');
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
    if (scenes.some(scene => typeof scene.revision !== 'string' || !scene.revision.trim()
        || scene.revision.length > 4096 || scene.revision === 'absent' || /[\u0000-\u001f\u007f]/.test(scene.revision))) {
      fail('A saved scene has no usable revision. Read the current scenes before changing them.');
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

  function selectionOf(value) {
    if (value?.kind === 'sonos_playlist_tracks') return player().nativeSceneSelection(value);
    const group = value?.group;
    if (value?.kind !== 'tracks' || !group || typeof group.target !== 'string'
        || !Array.isArray(group.members) || !group.members.length || group.members.length > 512
        || !group.members.every(room => typeof room === 'string' && room.trim() && room.length <= 1024)
        || new Set(group.members).size !== group.members.length || !group.members.includes(group.target)) {
      fail('Choose a listening group and its selected tracks again.');
    }
    return Object.freeze({ kind: 'tracks', tracks: Q.readTracks(value.tracks),
      group: Object.freeze({ target: group.target, members: Object.freeze([...group.members].sort()) }) });
  }
  function selectionGroup(selection, groups) {
    const group = groups.find(group => C.key(group) === selection.group.target);
    if (!group || S.stable(group.members.map(room => `${room}@${group.household}`).sort()) !== S.stable(selection.group.members)) {
      fail('The selected listening group changed. Choose the tracks and group again from the library.');
    }
    return group;
  }
  function selectedDefinition(selection, groups, playing) {
    selectionGroup(selection, groups);
    return Object.freeze({ groups: Object.freeze([Object.freeze({ coordinator: selection.group.target,
      members: Object.freeze(selection.group.members.filter(room => room !== selection.group.target)),
      source: Object.freeze(selection.kind === 'sonos_playlist_tracks' ? player().nativeSceneSource(selection)
        : { kind: 'tracks', tracks: selection.tracks }), playing })]),
    volumes: Object.freeze({}), mutes: Object.freeze({}) });
  }
  async function confirmSelection(scene, review, call) {
    const wanted = review.body.groups[0], group = scene.groups[0];
    const native = review.selection.kind === 'sonos_playlist_tracks', expected = native ? player().nativeSceneSource(review.selection, true) : wanted.source;
    const tracksSignature = native ? Q.nativeTracksSignature : Q.tracksSignature;
    if (scene.groups.length !== 1 || group.source.kind !== expected.kind || group.playing !== wanted.playing
        || S.stable(tracksSignature(group.source.tracks)) !== S.stable(tracksSignature(expected.tracks))
        || native && (group.source.name !== expected.name || group.source.playlist.id !== expected.playlist.id
          || group.source.playlist.title !== expected.playlist.title
          || group.coordinator !== `${review.selectionRoom.room}@${group.source.playlist.household}`)
        || Object.keys(scene.volumes).length || Object.keys(scene.mutes).length) {
      fail('The reply did not confirm the reviewed track selection and playback setting.');
    }
    const room = async target => {
      if (!native && [wanted.coordinator, ...wanted.members].includes(target)) return target;
      const value = await call('/rooms/resolve?target=' + encodeURIComponent(target));
      const identity = value?.rooms?.[0];
      if (value?.target !== target || value?.matched !== 'room' || value.rooms.length !== 1
          || !identity || typeof identity.room !== 'string' || typeof identity.household !== 'string') {
        fail('The saved selection room identity is unreadable.');
      }
      if (native && target === group.coordinator && S.stable(roomIdentity(value, target)) !== S.stable(review.selectionRoom)) {
        fail('The saved native scene names a different destination player.');
      }
      return `${identity.room}@${identity.household}`;
    };
    const coordinator = await room(group.coordinator), members = await Promise.all(group.members.map(room));
    if (coordinator !== wanted.coordinator || S.stable(members.sort()) !== S.stable([...wanted.members].sort())) {
      fail('The saved scene names a different listening group.');
    }
  }

  function roomIdentity(value, target) {
    const room = value?.rooms?.[0];
    if (value?.target !== target || value?.matched !== 'room' || value?.rooms?.length !== 1 || !room
        || !['room', 'household', 'player', 'coordinator'].every(key => typeof room[key] === 'string' && room[key].trim())) {
      fail('The native scene destination identity is unreadable.');
    }
    return Object.freeze(Object.fromEntries(['room', 'household', 'player', 'coordinator'].map(key => [key, room[key]])));
  }

  function model({ call, controller, changed = () => {}, saved = () => {}, now = () => performance.now() }) {
    let version = 0, active = true, disposed = false, reading = null, readAt = 0;
    const state = { open: false, target: '', scope: '', groups: null, scenes: [], stale: true,
      loading: false, writing: false, name: '', backupQueues: false, selection: null, selectionRoom: null, playing: true,
      pending: null, result: null, message: '' };
    const publish = () => { if (!disposed) { try { changed(state); } catch (_) { /* Presentation only. */ } } };
    const live = () => !disposed && active && state.open;
    const scope = () => signature(controller.state.items.map(item => item.zone));
    const matching = () => state.target === controller.state.selected && state.scope === scope();
    const canRead = () => !!(live() && controller.selected()?.fresh && controller.state.active && controller.state.topologyFresh
      && !controller.state.busy && !controller.state.loading && !state.loading && !state.writing);
    const editable = () => !!(live() && matching() && state.groups && !state.stale && controller.state.topologyFresh
      && !state.loading && !state.writing && now() - readAt < MAX_AGE && controller.canControl());
    const nativeCall = (name, args = {}) => {
      if (name === 'zones') return call('/zones');
      if (name !== 'sourcePlaylistContents') fail('Unknown native scene read.');
      return call('/sonos/playlists/contents?' + new URLSearchParams(args.query));
    };
    async function readNative(selection, valid) {
      await player().verifyNativeSceneSelection({ selection, call: nativeCall, valid });
      const target = selection.group.target;
      const identity = roomIdentity(await call('/rooms/resolve?target=' + encodeURIComponent(target)), target); valid();
      if (`${identity.room}@${identity.household}` !== target) fail('The native scene destination changed.');
      return identity;
    }
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
        if (state.selection) {
          if (state.selection.group.target !== state.target) fail('The selected listening group changed. Choose the tracks and group again from the library.');
          selectionGroup(state.selection, next);
          if (state.selection.kind === 'sonos_playlist_tracks') state.selectionRoom = await readNative(state.selection, valid);
        }
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
        : !['backupQueues', 'playing'].includes(field) || typeof value !== 'boolean')
        || field === 'backupQueues' && state.selection || field === 'playing' && !state.selection) return false;
      ++version; state.pending = null; state.result = null; state[field] = value;
      state.message = field === 'name' && value.trim() && !nameOf(value) ? 'Use a scene name of 1 to 64 characters.' : '';
      publish(); return true;
    }
    const canReview = () => !!(editable() && nameOf(state.name));
    function reviewReady() {
      if (!canReview()) return null;
      const asked = nameOf(state.name), existing = state.scenes.find(scene => nameKey(scene.name) === nameKey(asked)) || null;
      // Reuse the stored spelling: a case-only edit must not create a second saved name.
      state.pending = Object.freeze({ version, target: state.target, scope: state.scope, groups: state.groups,
        name: existing?.name || asked, existing, expectedRevision: existing?.revision || 'absent', backupQueues: state.backupQueues,
        selection: state.selection, selectionRoom: state.selectionRoom,
        body: state.selection ? selectedDefinition(state.selection, state.groups, state.playing)
          : Object.freeze(state.backupQueues ? { backup_queues: true } : {}) });
      state.message = ''; state.result = null; publish(); return state.pending;
    }
    function review() {
      if (state.selection?.kind !== 'sonos_playlist_tracks') return reviewReady();
      if (!canReview()) return Promise.resolve(null);
      const v = ++version, selection = state.selection;
      state.pending = null; state.loading = true; state.message = 'Checking the exact native occurrences before review…'; publish();
      const valid = () => {
        if (!live() || v !== version || !matching() || !controller.canControl() || !controller.state.topologyFresh) {
          fail('The native scene review changed or closed. Read the selection again.');
        }
      };
      return (async () => {
        try {
          const identity = await readNative(selection, valid); valid();
          if (S.stable(identity) !== S.stable(state.selectionRoom)) fail('The native scene destination changed. Read the selection again.');
          state.loading = false;
          return reviewReady();
        } catch (error) { if (live() && v === version) state.message = C.problem(error); return null; }
        finally { if (v === version) { state.loading = false; publish(); } }
      })();
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
          if (reviewed.selection) selectionGroup(reviewed.selection, C.zones(groups));
          if (reviewed.selection?.kind === 'sonos_playlist_tracks') {
            const identity = await readNative(reviewed.selection, valid); valid();
            if (S.stable(identity) !== S.stable(reviewed.selectionRoom)) fail('The native scene destination changed after review.');
          }
          const existing = catalogOf(scenes).find(scene => nameKey(scene.name) === nameKey(reviewed.name)) || null;
          if (S.stable(existing) !== S.stable(reviewed.existing)) {
            fail('That saved scene changed while you reviewed it. Read the house and review the replacement again.');
          }
          valid();
          captured = confirmed(await send('/scenes/' + encodeURIComponent(reviewed.name),
            { ...reviewed.body, expected_revision: reviewed.expectedRevision }, { method: 'PUT' }), reviewed);
          if (reviewed.selection) await confirmSelection(captured, reviewed, call);
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
        state.selection = null;
        retire(undefined, true); state.open = true; state.target = controller.state.selected; state.scope = scope(); publish(); return read();
      },
      openSelection(value) {
        if (disposed || !active || state.open || state.loading || state.writing || !controller.canControl()) return Promise.resolve(false);
        try {
          const selection = selectionOf(value);
          if (selection.group.target !== controller.state.selected) fail('Select the same listening group before saving its tracks.');
          selectionGroup(selection, controller.state.items.map(item => item.zone));
          retire(undefined, true); state.selection = selection; state.playing = true; state.backupQueues = false; state.name = '';
          state.open = true; state.target = controller.state.selected; state.scope = scope(); publish(); return read();
        } catch (error) { state.message = C.problem(error); publish(); return Promise.resolve(false); }
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
      $('capture-scene-title').textContent = s.selection ? 'Save selected tracks.' : 'Save this moment.';
      $('capture-scene-description').textContent = s.selection
        ? 'Keep this exact ordered selection for the displayed listening group. Choose whether recall plays it or loads it silently. Saving leaves music, queues, volume and mute unchanged.'
        : 'Capture every listening group, its source and playback state, and each room’s volume and mute. Saving reads the current house and stores a scene without changing playback.';
      $('capture-scene-backup-option').hidden = !!s.selection;
      $('capture-scene-playback-option').hidden = !s.selection;
      $('capture-scene-playback').disabled = s.loading || s.writing || !!s.pending;
      $('capture-scene-playback').value = s.playing ? 'play' : 'pause';
      $('capture-scene-tracks').hidden = !s.selection;
      $('capture-scene-tracks').textContent = !s.selection ? '' : s.selection.kind === 'sonos_playlist_tracks'
        ? Q.describeNativeTracks(s.selection.tracks, player().nativeSceneSource(s.selection, true).playlist) : Q.describeTracks(s.selection.tracks);
      $('capture-scene-review').textContent = s.selection ? 'Review selected scene' : 'Review capture';
      $('capture-scene-name').disabled = $('capture-scene-backup').disabled = s.loading || s.writing;
      if ($('capture-scene-name').value !== s.name) $('capture-scene-name').value = s.name;
      $('capture-scene-backup').checked = s.backupQueues;
      $('capture-scene-review').disabled = !control.canReview();
      $('capture-scene-message').textContent = s.message;
      if (groupsShown !== s.groups) {
        groupsShown = s.groups; $('capture-scene-groups').replaceChildren();
        for (const group of s.groups || []) {
          if (!s.selection || C.key(group) === s.selection.group.target) $('capture-scene-groups').append(element('li', group.members.join(' + ') + ' · ' + group.household));
        }
      }
      $('capture-scene-context').textContent = s.selection
        ? `Selection for ${s.selection.group.members.join(' + ')}${s.stale ? ' · read again before saving' : ''}. Recall restores this saved grouping; volume and mute are left unspecified.`
        : s.groups
        ? `${s.groups.length} listening group${s.groups.length === 1 ? '' : 's'} across ${new Set(s.groups.map(group => group.household)).size} household(s)${s.stale ? ' · read again before saving' : ''}.`
        : 'Read the current house groups and saved scenes to begin.';
      preview.hidden = !s.pending;
      if (s.pending) {
        const p = s.pending;
        $('capture-scene-preview-title').textContent = `${p.existing ? 'Replace' : 'Create'} scene “${p.name}”?`;
        $('capture-scene-preview-detail').textContent = p.selection
          ? `Recall restores this saved grouping and replaces its queue with the ${p.selection.tracks.length} entries shown in order. ${p.body.groups[0].playing ? 'Recall starts playback.' : 'Recall loads the queue without starting playback.'} Saving does not change any speaker.`
          : p.backupQueues
          ? 'Save complete ordinary queues, up to 1,000 entries and 4 MiB per queue. Recall rebuilds their contents on the original players and replaces those players’ current queues. Action-history undo cannot recover the replaced contents.'
          : 'Save queue bookmarks. Recall can restore the selected track, position and mode only while each original player’s queue remains unchanged. Queue contents are not saved.';
        $('capture-scene-confirm').textContent = p.existing ? 'Replace saved scene' : 'Save scene';
        $('capture-scene-existing').hidden = !p.existing;
        if (existingShown !== p.existing) {
          existingShown = p.existing; $('capture-scene-existing-detail').replaceChildren(...(p.existing ? [detail(p.existing)] : []));
        }
        $('capture-scene-review-note').textContent = p.selection
          ? 'The daemon checks that the saved definition still matches this review when storing it. Apply the scene separately or choose it in Routines. Later edits to the scene change what those routines do.'
          : 'The daemon checks that the saved definition still matches this review when storing it. Live groups and playback can change during capture after confirmation.';
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
    $('capture-scene-playback').addEventListener('change', event => control.edit('playing', event.target.value === 'play'));
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
    const fromNative = event => {
      if (event.detail?.kind !== 'sonos_playlist_tracks' || doc.hidden || control.state.open || control.state.loading || control.state.writing) return;
      void control.openSelection(event.detail);
      if (control.state.open) { opener = doc.activeElement; event.preventDefault(); }
    };
    doc.addEventListener?.('fsonos:scene-selection', fromNative);
    const dispose = control.dispose;
    control.dispose = () => { doc.removeEventListener?.('fsonos:scene-selection', fromNative); dispose(); };
    render(control.state); return control;
  }
  return { MAX_AGE, nameOf, catalogOf, signature, confirmed, selectionOf, model, panel };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = DashboardScenes;
