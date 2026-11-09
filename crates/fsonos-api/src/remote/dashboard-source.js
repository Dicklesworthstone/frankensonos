// Links and saved Sonos playlists share the dashboard command lane.
'use strict';
const DashboardSource = (() => {
  const C = typeof DashboardCore !== 'undefined' ? DashboardCore : require('./dashboard-core.js');
  const P = typeof SourcePlayerRemote !== 'undefined' ? SourcePlayerRemote : require('./source-player.js');
  const MAX_AGE = 45000;
  const fail = message => { throw new Error(message); };

  function observation(value, queue, zone, group, mode) {
    const before = P.playback(C.playback(value, zone), group);
    if (queue === null) return before;
    const snapshot = P.queue(queue, group);
    return Object.freeze({ ...before, queue: snapshot, signature: JSON.stringify(
      mode === 'end' ? [snapshot.revision, snapshot.total]
        : [before.signature, snapshot.revision, snapshot.total, snapshot.playing]) });
  }
  function effect(review) {
    const q = review.before.queue;
    if (review.mode === 'play' && review.item.kind === 'sonos_playlist') {
      return 'Replace the current queue with this saved Sonos playlist and start playback. The speakers supply its current contents.';
    }
    if (review.mode === 'play') return review.item.container
      ? 'Replace the current queue with this album or playlist and start playback. The live source can contain music that is not in the local cache.'
      : 'Replace the current source and start playback. The existing queue remains available; any DJ session can remain stored while this source plays.';
    return review.mode === 'next' && q.playing !== null
      ? 'Add after the track playing when the daemon executes the command. Existing playback and any running DJ session stay active.'
      : 'Append to the existing queue without starting or resuming playback. Any running DJ session stays active.';
  }

  function model({ call, controller, changed = () => {}, completed = () => {}, now = () => performance.now() }) {
    let version = 0, active = true, disposed = false, reading = null;
    const state = { open: false, target: '', scope: '', group: null, sourceType: 'link', input: '', title: '', mode: 'play',
      playlists: [], playlist: '', playlistTarget: '', playlistStatus: 'idle', contents: null, contentsHistory: [],
      loading: false, writing: false, pending: null, result: null, message: '' };
    const publish = () => { if (!disposed) { try { changed(state); } catch (_) { /* Presentation only. */ } } };
    const live = () => !disposed && active && state.open;
    const target = () => controller.state.selected || '';
    const scope = () => controller.selected() ? C.shape(controller.selected().zone) : '';
    const matching = () => state.target === target() && state.scope === scope();
    const editable = () => !!(live() && !state.writing && !controller.state.busy);
    const ready = () => !!(editable() && !state.loading && state.group && matching()
      && controller.state.topologyFresh && controller.canControl());
    function retire(message) {
      ++version; reading?.abort(); reading = null; state.loading = false; state.pending = null;
      if (state.playlistStatus === 'loading') state.playlistStatus = 'idle';
      if (message) state.message = message;
    }
    const clearContents = () => { state.contents = null; state.contentsHistory = []; };
    const clearPlaylists = () => { state.playlists = []; state.playlist = ''; state.playlistTarget = ''; state.playlistStatus = 'idle'; clearContents(); };
    function destination() {
      if (!matching()) clearPlaylists();
      state.target = target(); state.scope = scope();
      try { state.group = controller.selected() ? P.groups([controller.selected().zone])[0] : null; }
      catch (error) { state.group = null; state.message = C.problem(error); }
    }
    function observe() {
      if (disposed) return;
      if (!matching()) {
        retire('The selected listening group changed. Review this source for the new group.');
        destination();
      } else if (!state.writing && (state.loading || state.pending)
          && (!controller.selected()?.fresh || !controller.state.active || !controller.state.topologyFresh
            || (state.pending && now() - state.pending.at >= MAX_AGE))) {
        retire('Refresh playback and review the current destination again.');
      }
      publish();
    }
    function edit(field, value) {
      if (!editable() || typeof value !== 'string' || !['sourceType', 'playlist', 'input', 'title', 'mode'].includes(field)
          || (field === 'mode' && !Object.hasOwn(P.modes, value))) return false;
      if (field === 'sourceType' && !['link', 'playlist'].includes(value)) return false;
      if (field === 'playlist' && (state.playlistStatus !== 'ready' || state.playlistTarget !== state.target
          || !state.playlists.some(item => item.id === value))) return false;
      retire(); if (field === 'sourceType') clearPlaylists();
      if (field === 'playlist') clearContents();
      state[field] = value; state.message = ''; publish(); return true;
    }
    async function loadPlaylists() {
      if (!editable() || state.sourceType !== 'playlist' || !state.group || !matching()
          || !controller.state.topologyFresh || !controller.canControl()) return false;
      retire(); clearPlaylists();
      const v = version, selected = state.target, abort = new AbortController();
      reading = abort; state.loading = true; state.playlistStatus = 'loading'; state.message = 'Reading saved Sonos playlists…'; publish();
      try {
        const value = await call('/sonos/playlists?zone=' + encodeURIComponent(selected), undefined, { signal: abort.signal });
        if (!live() || v !== version || abort.signal.aborted || !matching()) return false;
        state.playlists = P.playlists(value); state.playlistTarget = selected; state.playlistStatus = 'ready'; state.message = '';
        return true;
      } catch (error) {
        if (live() && v === version) { state.playlistStatus = 'error'; state.message = C.problem(error); }
        return false;
      } finally {
        if (reading === abort) { reading = null; state.loading = false; publish(); }
      }
    }

    async function loadContents(direction = 'refresh') {
      if (!ready() || state.sourceType !== 'playlist' || state.playlistStatus !== 'ready'
          || state.playlistTarget !== state.target) return false;
      const item = state.playlists.find(item => item.id === state.playlist);
      if (!item) return false;
      const request = P.contentsRequest(state.contents, state.contentsHistory, direction, item.id);
      if (!request) return false;
      retire();
      const v = version, selected = state.target, abort = new AbortController();
      reading = abort; state.loading = true; state.message = 'Reading saved tracks…'; publish();
      try {
        const query = new URLSearchParams({ zone: selected, ...request.query });
        const value = await call('/sonos/playlists/contents?' + query, undefined, { signal: abort.signal });
        if (!live() || v !== version || abort.signal.aborted || !matching()) return false;
        state.contents = P.playlistContents(value, request.query, item, state.group); state.contentsHistory = request.history;
        state.message = ''; return true;
      } catch (error) {
        if (live() && v === version) { clearContents(); state.message = C.problem(error); }
        return false;
      } finally {
        if (reading === abort) { reading = null; state.loading = false; publish(); }
      }
    }

    async function inspect(reviewed, options, valid) {
      valid();
      const groups = P.groups(C.zones(await call('/zones', undefined, options))); valid();
      if (!groups.some(group => group.target === reviewed.target && group.signature === reviewed.group.signature)) {
        fail('The destination group changed. Refresh playback and review again.');
      }
      if (reviewed.item.kind === 'sonos_playlist') {
        const catalog = await call('/sonos/playlists?zone=' + encodeURIComponent(reviewed.target), undefined, options); valid();
        P.verifyPlaylist(catalog, reviewed.item);
      }
      const base = '/zones/' + encodeURIComponent(reviewed.target);
      const queueNeeded = reviewed.mode !== 'play' || reviewed.item.container;
      const [playback, queue] = await Promise.all([
        call(base + '/state', undefined, options),
        queueNeeded ? call(base + '/queue?offset=0&limit=1', undefined, options) : Promise.resolve(null),
      ]);
      valid();
      return observation(playback, queue, reviewed.zone, reviewed.group, reviewed.mode);
    }
    const canReview = () => !!(ready() && (state.sourceType === 'playlist'
      ? state.playlist && state.playlistStatus === 'ready' && state.playlistTarget === state.target : state.input.trim()));
    async function review() {
      if (!canReview()) return null;
      retire();
      const v = version, at = now(), abort = new AbortController();
      let choice;
      try { choice = state.sourceType === 'playlist' ? P.preparePlaylist(state.playlist, state.playlists, state.mode)
        : P.prepare(state.input, state.title, state.mode); }
      catch (error) { state.message = C.problem(error); publish(); return null; }
      const proposed = { ...choice, target: state.target, scope: state.scope, group: state.group,
        zone: controller.selected().zone, version: v, at };
      reading = abort; state.loading = true; state.message = 'Reading the destination…'; publish();
      const valid = () => {
        if (!live() || v !== version || abort.signal.aborted || !matching() || !controller.canControl()
            || !controller.state.topologyFresh || now() - at >= MAX_AGE) {
          fail('The destination observation is no longer current. Review again.');
        }
      };
      try {
        const before = await inspect(proposed, { signal: abort.signal }, valid); valid();
        const body = P.commandBody(proposed, proposed.target);
        state.pending = Object.freeze({ ...proposed, before, body: Object.freeze(body) });
        state.message = ''; return state.pending;
      } catch (error) {
        if (live() && v === version) state.message = C.problem(error);
        return null;
      } finally {
        if (reading === abort) { reading = null; state.loading = false; publish(); }
      }
    }
    const canApply = () => !!(ready() && state.pending && state.pending.version === version && now() - state.pending.at < MAX_AGE);
    async function apply(reviewed = state.pending) {
      if (!canApply() || reviewed !== state.pending) return false;
      const v = version, abort = new AbortController();
      let ok = false, detail = '';
      reading = abort; state.pending = null; state.writing = true; state.message = 'Checking the destination again…'; publish();
      try {
        ok = await controller.mutate(async ({ target, check, send }) => {
          const valid = () => {
            check();
            if (!live() || v !== version || abort.signal.aborted || target !== reviewed.target || !matching()
                || !controller.state.topologyFresh || now() - reviewed.at >= MAX_AGE) {
              fail('The source review is no longer current. Review the destination again.');
            }
          };
          valid();
          const before = await inspect(reviewed, { signal: abort.signal }, valid); valid();
          if (before.signature !== reviewed.before.signature) {
            fail('Playback or the queue changed since your review. Review the destination again.');
          }
          state.message = reviewed.mode === 'play' ? 'Starting the reviewed source…' : 'Adding the reviewed source to the queue…'; publish();
          valid(); reading = null;
          const route = reviewed.mode === 'play' ? reviewed.item.kind === 'sonos_playlist' ? '/sonos/playlists/play' : '/play' : '/queue';
          const value = await send(route, reviewed.body);
          if (reviewed.mode === 'play') { detail = P.outcome(value); return value; }
          detail = P.queued(value, reviewed.group);
          return { done: detail, changed: true };
        }, { wholeHouse: false });
        if (!disposed) {
          state.result = Object.freeze({ status: ok ? 'accepted' : controller.state.uncertain ? 'uncertain' : 'not_sent',
            mode: reviewed.mode, target: reviewed.group.label, source: reviewed.item.id || reviewed.item.uri, title: reviewed.title,
            detail: ok ? detail : controller.state.message || 'No command was sent. Review the source again.' });
          state.message = state.result.detail;
        }
      } finally {
        state.writing = false; retire(); publish();
      }
      if (ok && !disposed) {
        try { await completed({ target: reviewed.target, mode: reviewed.mode }); }
        catch (_) { state.message += ' Refresh the queue and DJ status to see the latest state.'; }
        publish();
      }
      return ok;
    }
    function open() {
      if (disposed || !active) return false;
      state.open = true;
      if (!state.writing) {
        retire(); destination();
        state.message = controller.canControl() && state.group ? ''
          : controller.state.message || 'Select a listening group and refresh playback before reviewing a source.';
      }
      publish(); return true;
    }
    return { state, observe, editable, edit, loadPlaylists, loadContents, canReview, review, canApply, apply, open,
      cancelReview() { if (state.writing) return false; retire(); state.message = ''; publish(); return true; },
      clearDraft() {
        if (!editable()) return false;
        retire(); clearPlaylists(); state.sourceType = 'link'; state.input = ''; state.title = ''; state.mode = 'play';
        state.message = 'Source draft cleared. No playback command was sent.'; publish(); return true;
      },
      offer(value) {
        if (disposed || !active || state.writing || controller.state.busy) return false;
        let offered;
        try { offered = P.offeredSource(value); }
        catch (error) { state.message = C.problem(error); publish(); return false; }
        if (state.loading || state.pending || state.input.trim() || state.title.trim() || state.playlist) {
          state.open = true; state.message = 'Finish this source action, or clear its draft before importing another source.'; publish(); return false;
        }
        open(); clearPlaylists(); state.sourceType = 'link'; state.input = offered.input; state.title = offered.title; state.mode = 'play'; publish(); return true;
      },
      close() { state.open = false; retire(); publish(); },
      suspend() { active = false; state.open = false; retire(); publish(); },
      resume() { if (!disposed) { active = true; observe(); } },
      dispose() { disposed = true; active = false; state.open = false; retire(); },
    };
  }

  function panel({ call, controller, completed, document: doc = document, now }) {
    const $ = id => doc.getElementById(id), dialog = $('source-dialog'), trigger = $('open-source'), preview = $('source-preview');
    let opener = null, reviewFocus = null, shown = null, shownPlaylists = null, shownPlaylistStatus = '', shownContents = null;
    const control = model({ call, controller, completed, now, changed: render });
    function render(s) {
      trigger.disabled = !controller.state.active || (controller.state.busy && !s.writing) || (!controller.selected() && !s.result);
      trigger.setAttribute('aria-expanded', String(s.open));
      if (s.open && !dialog.open) dialog.showModal();
      else if (!s.open && dialog.open) { dialog.close(); if (opener && !opener.disabled) opener.focus(); }
      for (const field of ['input', 'title', 'mode']) {
        const input = $('source-' + field); input.disabled = !control.editable();
        if (input.value !== s[field]) input.value = s[field];
      }
      $('source-type').value = s.sourceType; $('source-type').disabled = !control.editable();
      const saved = s.sourceType === 'playlist';
      for (const id of ['source-link-field', 'source-title-field', 'source-kinds']) $(id).hidden = saved;
      for (const id of ['source-playlist-field', 'source-playlist-controls']) $(id).hidden = !saved;
      if (shownPlaylists !== s.playlists || shownPlaylistStatus !== s.playlistStatus) {
        shownPlaylists = s.playlists; shownPlaylistStatus = s.playlistStatus;
        const caption = s.playlistStatus === 'loading' ? 'Loading playlists…'
          : s.playlistStatus === 'ready' ? s.playlists.length ? 'Choose a playlist…' : 'No saved Sonos playlists'
            : 'Refresh playlists for this group';
        const placeholder = doc.createElement('option'); placeholder.value = ''; placeholder.textContent = caption; placeholder.disabled = true;
        $('source-playlist').replaceChildren(placeholder);
        for (const item of s.playlists) {
          const option = doc.createElement('option'); option.value = item.id; option.textContent = item.title || item.id;
          $('source-playlist').append(option);
        }
      }
      $('source-playlist').value = s.playlist;
      $('source-playlist').disabled = !control.editable() || s.playlistStatus !== 'ready' || !s.playlists.length;
      $('source-playlist-refresh').disabled = !control.editable() || s.loading || !s.group || !controller.canControl();
      $('source-playlist-help').textContent = s.playlistStatus === 'ready' ? s.playlists.length
        ? 'Saved on the speakers in this household. Read its tracks before choosing how to play it.'
        : 'This household has no saved Sonos playlists. Open Speaker queue → Save queue as playlist, then refresh here.'
        : s.group ? 'Read saved playlists for the selected group.' : 'Select a listening group first.';
      $('source-contents').hidden = !saved || !s.playlist || s.playlistStatus !== 'ready';
      $('source-contents-read').disabled = !control.canReview();
      $('source-contents-read').textContent = s.contents ? 'Refresh tracks' : 'Read tracks';
      $('source-contents-previous').hidden = !s.contentsHistory.length;
      $('source-contents-next').hidden = s.contents?.next_offset == null;
      for (const id of ['source-contents-previous', 'source-contents-next']) $(id).disabled = !control.canReview();
      $('source-contents-help').textContent = P.contentsSummary(s.contents);
      if (shownContents !== s.contents) { shownContents = s.contents; P.renderContents($('source-contents-items'), s.contents, doc); }
      $('source-context').textContent = s.group ? 'Destination: ' + s.group.label : 'Select a listening group first.';
      $('source-review').textContent = s.mode === 'play' ? 'Review playback' : 'Review queue addition';
      $('source-review').disabled = !control.canReview(); $('source-clear').disabled = !control.editable();
      $('source-message').textContent = s.message;
      dialog.setAttribute('aria-busy', String(s.loading || s.writing));
      preview.hidden = !s.pending;
      if (s.pending) {
        const p = s.pending, q = p.before.queue;
        $('source-preview-title').textContent = P.modes[p.mode] + ': ' + (p.title || p.item.uri);
        $('source-preview-source').textContent = p.item.kind === 'sonos_playlist' ? 'Saved Sonos playlist · ' + p.item.id : p.item.uri;
        $('source-preview-target').textContent = p.group.label;
        $('source-preview-current').textContent = p.before.label;
        $('source-preview-effect').textContent = effect(p);
        $('source-preview-queue').hidden = !q;
        $('source-preview-queue').textContent = q ? q.playing === null
          ? q.total + ' queue entries; this group is not currently using its queue.'
          : q.total + ' queue entries; reported current occurrence: ' + q.playing + '.' : '';
        $('source-confirm').textContent = P.modes[p.mode];
      }
      $('source-confirm').disabled = !control.canApply(); $('source-back').disabled = s.writing;
      $('source-result').hidden = !s.result;
      $('source-result-title').textContent = s.result
        ? (s.result.status === 'accepted' ? 'Command accepted' : s.result.status === 'uncertain' ? 'Check the command outcome' : 'No command sent')
          + ' · ' + s.result.target : '';
      $('source-result-detail').textContent = s.result ? s.result.detail : '';
      if (s.pending && s.pending !== shown) { reviewFocus = doc.activeElement; preview.focus(); }
      else if (!s.pending && shown && s.open && reviewFocus && !reviewFocus.disabled) reviewFocus.focus();
      shown = s.pending;
    }
    trigger.addEventListener('click', () => {
      opener = doc.activeElement;
      if (control.open() && control.state.sourceType === 'playlist') void control.loadPlaylists();
    });
    $('source-type').addEventListener('change', event => {
      if (control.edit('sourceType', event.target.value) && control.state.sourceType === 'playlist') void control.loadPlaylists();
    });
    $('source-playlist').addEventListener('change', event => control.edit('playlist', event.target.value));
    $('source-playlist-refresh').addEventListener('click', () => { void control.loadPlaylists(); });
    $('source-contents-read').addEventListener('click', () => { void control.loadContents(); });
    $('source-contents-previous').addEventListener('click', () => { void control.loadContents('previous'); });
    $('source-contents-next').addEventListener('click', () => { void control.loadContents('next'); });
    for (const field of ['input', 'title', 'mode']) {
      $('source-' + field).addEventListener(field === 'mode' ? 'change' : 'input', event => control.edit(field, event.target.value));
    }
    $('source-review').addEventListener('click', () => { void control.review(); });
    $('source-back').addEventListener('click', () => control.cancelReview());
    $('source-confirm').addEventListener('click', () => { void control.apply(); });
    $('source-clear').addEventListener('click', () => control.clearDraft());
    $('close-source').addEventListener('click', () => control.close());
    dialog.addEventListener('cancel', event => { event.preventDefault(); control.close(); });
    dialog.addEventListener('close', () => { if (control.state.open) control.close(); });
    render(control.state); return control;
  }
  return { MAX_AGE, observation, effect, model, panel };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = DashboardSource;
