// Music sources and native inputs share the dashboard command lane.
'use strict';
const DashboardSource = (() => {
  const C = typeof DashboardCore !== 'undefined' ? DashboardCore : require('./dashboard-core.js');
  const P = typeof SourcePlayerRemote !== 'undefined' ? SourcePlayerRemote : require('./source-player.js');
  const T = typeof SourceTargetsRemote !== 'undefined' ? SourceTargetsRemote
    : typeof require === 'function' ? require('./source-targets.js') : null;
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
    if (review.targets) {
      const count = review.targets.groups.length;
      if (review.mode !== 'play') return `Add this source to ${count} existing group(s) without starting or resuming playback. Each group uses its own queue and current occurrence, shown below.`;
      const native = ['playlist_query', 'favorite_query'].includes(review.item.kind);
      return `Start this source in ${count} existing group(s). ${native
        ? 'Each household uses the catalog entry shown below.'
        : review.item.container ? 'Each group’s queue is replaced with the selected music.' : 'Each group’s current source is replaced.'} Groups keep their existing membership.`;
    }
    const q = review.before.queue;
    if (review.item.kind === 'line_in') return `Play the analog input from ${review.item.room} in the selected group. The current source is replaced and the existing queue remains available. Cable and signal state are not reported; start your source device yourself.`;
    if (review.item.kind === 'artist' || review.item.kind === 'show') {
      const selection = review.item.kind === 'show'
        ? 'Uses up to 10 available episodes from the show’s first episode page, in Spotify’s order. Requires Spotify sign-in.'
        : 'Uses up to 10 tracks from the artist’s top tracks or albums, in Spotify’s order. Requires Spotify sign-in.';
      if (review.mode === 'play') return 'Replace the current queue with this ' + review.item.kind + ' selection and start playback in order. ' + selection;
      return (review.mode === 'next' && q.playing !== null
        ? 'Add after the playing track without restarting playback. '
        : 'Append to the existing queue without starting or resuming playback. ') + selection;
    }
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
      destinationMode: 'selected', destinationInput: '', nativeQuery: '', suggestions: [],
      playlists: [], playlist: '', playlistTarget: '', playlistStatus: 'idle', contents: null, contentsHistory: [],
      lineInSources: [], lineInUnavailable: [], lineIn: '', lineInTarget: '', lineInStatus: 'idle',
      loading: false, writing: false, pending: null, result: null, message: '' };
    const publish = () => { if (!disposed) { try { changed(state); } catch (_) { /* Presentation only. */ } } };
    const live = () => !disposed && active && state.open;
    const target = () => controller.state.selected || '';
    const scope = () => controller.selected() ? C.shape(controller.selected().zone) : '';
    const matching = () => state.target === target() && state.scope === scope();
    const editable = () => !!(live() && !state.writing && !controller.state.busy);
    const targetSet = () => state.destinationMode === 'set';
    const nativeQuery = () => state.sourceType === 'favorite' || targetSet() && state.sourceType === 'playlist';
    const expanded = () => targetSet() || state.sourceType === 'favorite';
    const targetCall = (name, args = {}) => {
      const route = { ...P.routes, ...T.routes, state: ['GET', '/zones/{room}/state'], zones: ['GET', '/zones'] }[name];
      if (!route) throw new Error('Unknown source read.');
      let url = route[1].replace(/\{([^}]+)\}/g, (_, key) => encodeURIComponent(args.params?.[key] || ''));
      if (args.query) url += '?' + new URLSearchParams(args.query);
      return call(url, args.body, { signal: args.signal });
    };
    const catalog = P.catalogModel({
      call: (query, options) => call('/spotify/search?' + new URLSearchParams(query), undefined, options),
      available: () => editable() && state.sourceType === 'catalog' && !state.loading,
      changed: publish,
    });
    state.catalog = catalog.state;
    const ready = () => !!(editable() && !state.loading && state.group && matching()
      && controller.state.topologyFresh && controller.canControl());
    function retire(message) {
      ++version; reading?.abort(); reading = null; state.loading = false; state.pending = null;
      catalog.retire();
      if (state.playlistStatus === 'loading') state.playlistStatus = 'idle';
      if (state.lineInStatus === 'loading') state.lineInStatus = 'idle';
      if (message) state.message = message;
    }
    const clearContents = () => { state.contents = null; state.contentsHistory = []; };
    const clearPlaylists = () => { state.playlists = []; state.playlist = ''; state.playlistTarget = ''; state.playlistStatus = 'idle'; clearContents(); };
    const clearLineIn = () => { state.lineInSources = []; state.lineInUnavailable = []; state.lineIn = ''; state.lineInTarget = ''; state.lineInStatus = 'idle'; };
    function destination() {
      if (!matching()) { clearPlaylists(); clearLineIn(); }
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
      if (!editable() || typeof value !== 'string' || !['sourceType', 'playlist', 'lineIn', 'input', 'title', 'mode', 'destinationMode', 'destinationInput', 'nativeQuery'].includes(field)
          || (field === 'mode' && (!Object.hasOwn(P.modes, value) || state.sourceType === 'line_in' && value !== 'play'))) return false;
      if (field === 'destinationMode' && !['selected', 'set'].includes(value)) return false;
      if (field === 'sourceType' && !['link', 'playlist', 'catalog', 'line_in', 'favorite'].includes(value)) return false;
      if ((field === 'destinationMode' && value === 'set' && state.sourceType === 'line_in')
          || (field === 'sourceType' && value === 'line_in' && targetSet())) {
        state.message = 'Choose the selected listening group to play a physical line-in source.'; publish(); return false;
      }
      if (!T && (field === 'destinationMode' && value === 'set' || field === 'sourceType' && value === 'favorite')) return false;
      if (field === 'playlist' && (state.playlistStatus !== 'ready' || state.playlistTarget !== state.target
          || !state.playlists.some(item => item.id === value))) return false;
      if (field === 'lineIn' && (state.lineInStatus !== 'ready' || state.lineInTarget !== state.target
          || !state.lineInSources.some(item => item.player === value))) return false;
      retire(); if (field === 'sourceType' || field === 'destinationMode' || field === 'destinationInput') { clearPlaylists(); clearLineIn(); }
      if (field === 'sourceType' && value === 'line_in') state.mode = 'play';
      if (field === 'playlist') clearContents();
      state[field] = value; state.message = ''; publish(); return true;
    }
    async function loadTargets() {
      if (!editable() || state.loading || !T) return false;
      retire();
      const v = version, abort = new AbortController(); reading = abort;
      state.loading = true; state.message = 'Reading room and alias suggestions…'; publish();
      try {
        const value = await call('/rooms', undefined, { signal: abort.signal });
        if (!live() || v !== version || abort.signal.aborted) return false;
        state.suggestions = T.suggestions(value); state.message = ''; return true;
      } catch (error) {
        if (live() && v === version) state.message = C.problem(error);
        return false;
      } finally {
        if (reading === abort) { reading = null; state.loading = false; publish(); }
      }
    }
    function chooseCatalog(item, items) {
      const incoming = catalog.select(item, items);
      if (!incoming) return false;
      retire(); clearPlaylists(); clearLineIn(); state.sourceType = 'link';
      state.input = incoming.input; state.title = incoming.title;
      state.message = 'Spotify source selected. Review the destination and action before sending it.';
      publish(); return true;
    }
    async function loadPlaylists() {
      if (!editable() || targetSet() || state.sourceType !== 'playlist' || !state.group || !matching()
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

    async function loadLineInSources() {
      if (!ready() || targetSet() || state.sourceType !== 'line_in') return false;
      retire(); clearLineIn();
      const v = version, selected = state.target, abort = new AbortController();
      reading = abort; state.loading = true; state.lineInStatus = 'loading'; state.message = 'Reading line-in sources…'; publish();
      try {
        const value = await call('/line-in?zone=' + encodeURIComponent(selected), undefined, { signal: abort.signal });
        if (!live() || v !== version || abort.signal.aborted || !matching()) return false;
        const catalog = P.lineInSources(value);
        state.lineInSources = catalog.sources; state.lineInUnavailable = catalog.unavailable;
        state.lineInTarget = selected; state.lineInStatus = 'ready'; state.message = ''; return true;
      } catch (error) {
        if (live() && v === version) { state.lineInStatus = 'error'; state.message = C.problem(error); }
        return false;
      } finally {
        if (reading === abort) { reading = null; state.loading = false; publish(); }
      }
    }

    async function loadContents(direction = 'refresh') {
      if (!ready() || targetSet() || state.sourceType !== 'playlist' || state.playlistStatus !== 'ready'
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
      if (reviewed.expanded) return T.inspect({ call: targetCall, player: P, target: reviewed.target,
        item: reviewed.item, mode: reviewed.mode, signal: options.signal, valid });
      const groups = P.groups(C.zones(await call('/zones', undefined, options))); valid();
      if (!groups.some(group => group.target === reviewed.target && group.signature === reviewed.group.signature)) {
        fail('The destination group changed. Refresh playback and review again.');
      }
      if (reviewed.item.kind === 'sonos_playlist') {
        const catalog = await call('/sonos/playlists?zone=' + encodeURIComponent(reviewed.target), undefined, options); valid();
        P.verifyPlaylist(catalog, reviewed.item);
      }
      if (reviewed.item.kind === 'line_in') {
        const catalog = await call('/line-in?zone=' + encodeURIComponent(reviewed.target), undefined, options); valid();
        P.verifyLineIn(catalog, reviewed.item);
      }
      if (reviewed.item.kind === 'sonos_playlist_track') {
        const query = new URLSearchParams(P.trackQuery(reviewed.item, reviewed.target));
        const value = await call('/sonos/playlists/contents?' + query, undefined, options); valid();
        P.verifyPlaylistTrack(value, reviewed.item, reviewed.group);
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
    const canReview = () => !!(ready() && state.sourceType !== 'catalog' && (!targetSet() || state.destinationInput.trim()) && (nativeQuery()
      ? state.nativeQuery.trim() : state.sourceType === 'playlist'
      ? state.playlist && state.playlistStatus === 'ready' && state.playlistTarget === state.target
      : state.sourceType === 'line_in' ? state.lineIn && state.lineInStatus === 'ready' && state.lineInTarget === state.target : state.input.trim()));
    async function review(prepared = null) {
      if (!canReview()) return null;
      retire();
      const v = version, at = now(), abort = new AbortController();
      let choice;
      try { choice = prepared || (nativeQuery() ? T.prepareNative(state.sourceType, state.nativeQuery, state.mode)
        : state.sourceType === 'playlist' ? P.preparePlaylist(state.playlist, state.playlists, state.mode)
        : state.sourceType === 'line_in' ? P.prepareLineIn(state.lineIn, state.lineInSources, state.mode)
          : P.prepare(state.input, state.title, state.mode)); }
      catch (error) { state.message = C.problem(error); publish(); return null; }
      const proposed = { ...choice, target: targetSet() ? state.destinationInput.trim() : state.target,
        selectedTarget: state.target, expanded: expanded(), scope: state.scope, group: state.group,
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
        const request = proposed.expanded ? T.request(proposed, proposed.target, P) : null;
        const body = request ? request.body : P.commandBody(proposed, proposed.target);
        state.pending = Object.freeze({ ...proposed, before, targets: proposed.expanded ? before : null,
          request, body: Object.freeze(body) });
        state.message = ''; return state.pending;
      } catch (error) {
        if (live() && v === version) state.message = C.problem(error);
        return null;
      } finally {
        if (reading === abort) { reading = null; state.loading = false; publish(); }
      }
    }
    async function reviewTrack(position, mode, page = state.contents) {
      if (!canReview() || targetSet() || state.sourceType !== 'playlist' || !page || page !== state.contents
          || page.playlist.id !== state.playlist) return null;
      let choice;
      try { choice = P.preparePlaylistTrack(page, position, mode); }
      catch (error) { retire(); state.message = C.problem(error); publish(); return null; }
      return review(choice);
    }
    const canApply = () => !!(ready() && state.pending && state.pending.version === version && now() - state.pending.at < MAX_AGE);
    async function apply(reviewed = state.pending) {
      if (!canApply() || reviewed !== state.pending) return false;
      const v = version, abort = new AbortController();
      let ok = false, detail = '', receipt = null;
      reading = abort; state.pending = null; state.writing = true; state.message = 'Checking the destination again…'; publish();
      try {
        ok = await controller.mutate(async ({ target, check, send }) => {
          const valid = () => {
            check();
            if (!live() || v !== version || abort.signal.aborted || target !== reviewed.selectedTarget || !matching()
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
          if (reviewed.targets) {
            const route = { ...P.routes, ...T.routes }[reviewed.request.route];
            if (!route || route[0] !== 'POST') fail('The source command is unavailable.');
            const value = await send(route[1], reviewed.body);
            receipt = T.result(value, reviewed.targets, P); detail = receipt.detail;
            return { done: detail, changed: reviewed.mode === 'play' ? value.changed : value.added ? value.added.length > 0 : true };
          }
          const route = reviewed.item.kind === 'sonos_playlist_track' ? '/sonos/playlists/track'
            : reviewed.item.kind === 'line_in' ? '/line-in/play'
            : reviewed.mode === 'play' ? reviewed.item.kind === 'sonos_playlist' ? '/sonos/playlists/play' : '/play' : '/queue';
          const value = await send(route, reviewed.body);
          if (reviewed.item.kind === 'sonos_playlist_track') { detail = P.trackOutcome(value, reviewed); return { ...value, done: detail }; }
          if (reviewed.mode === 'play') { detail = P.outcome(value); return value; }
          detail = P.queued(value, reviewed.group);
          return { done: detail, changed: true };
        }, { wholeHouse: !!reviewed.targets });
        if (!disposed) {
          state.result = Object.freeze({ status: ok ? 'accepted' : controller.state.uncertain ? 'uncertain' : 'not_sent',
            partial: receipt?.partial || false, complete: receipt?.complete,
            mode: reviewed.mode, target: reviewed.targets ? reviewed.target : reviewed.group.label,
            source: reviewed.item.player || reviewed.item.id || reviewed.item.uri || reviewed.item.query, title: reviewed.title,
            detail: ok ? detail : controller.state.message || 'No command was sent. Review the source again.' });
          state.message = state.result.detail;
        }
      } finally {
        state.writing = false; retire(); publish();
      }
      if (ok && !disposed) {
        try { await completed({ target: reviewed.selectedTarget, mode: reviewed.mode,
          ...(reviewed.targets ? { targets: reviewed.targets.groups.map(group => group.target) } : {}) }); }
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
    return { state, catalog, chooseCatalog, observe, editable, edit, loadTargets, loadPlaylists, loadLineInSources, loadContents, canReview, review, reviewTrack, canApply, apply, open,
      catalogChanged(change = null) {
        const favorite = state.sourceType === 'favorite';
        if (disposed || (!favorite && state.sourceType !== 'playlist')
            || (favorite ? change?.favorites === false : change?.playlists === false)
            || !targetSet() && change?.household && state.group?.household !== change.household) return;
        retire('The household catalog changed. Review its current source again.');
        clearPlaylists(); publish();
      },
      cancelReview() { if (state.writing) return false; retire(); state.message = ''; publish(); return true; },
      clearDraft() {
        if (!editable()) return false;
        retire(); clearPlaylists(); clearLineIn(); state.sourceType = 'link'; state.input = ''; state.title = ''; state.mode = 'play';
        state.destinationMode = 'selected'; state.destinationInput = ''; state.nativeQuery = '';
        state.message = 'Source draft cleared. No playback command was sent.'; publish(); return true;
      },
      offer(value) {
        if (disposed || !active || state.writing || controller.state.busy) return false;
        let offered;
        try { offered = P.offeredSource(value); }
        catch (error) { state.message = C.problem(error); publish(); return false; }
        if (state.loading || state.pending || state.input.trim() || state.title.trim() || state.nativeQuery.trim() || state.playlist || state.lineIn) {
          state.open = true; state.message = 'Finish this source action, or clear its draft before importing another source.'; publish(); return false;
        }
        open(); clearPlaylists(); clearLineIn(); state.sourceType = 'link'; state.input = offered.input; state.title = offered.title; state.mode = 'play'; publish(); return true;
      },
      close() { state.open = false; retire(); publish(); },
      suspend() { active = false; state.open = false; retire(); publish(); },
      resume() { if (!disposed) { active = true; observe(); } },
      dispose() { disposed = true; active = false; state.open = false; retire(); },
    };
  }

  function panel({ call, controller, completed, document: doc = document, now }) {
    const $ = id => doc.getElementById(id), dialog = $('source-dialog'), trigger = $('open-source'), preview = $('source-preview');
    let opener = null, reviewFocus = null, shown = null, shownPlaylists = null, shownPlaylistStatus = '', shownContents = null, shownContentsReady = null;
    let shownCatalog = null, shownCatalogReady = null;
    let shownLineIn = null, shownLineInStatus = '';
    let shownSuggestions = null;
    const control = model({ call, controller, completed, now, changed: render });
    function render(s) {
      trigger.disabled = !controller.state.active || (controller.state.busy && !s.writing);
      trigger.setAttribute('aria-expanded', String(s.open));
      if (s.open && !dialog.open) dialog.showModal();
      else if (!s.open && dialog.open) { dialog.close(); if (opener && !opener.disabled) opener.focus(); }
      for (const field of ['input', 'title', 'mode']) {
        const input = $('source-' + field); input.disabled = !control.editable();
        if (input.value !== s[field]) input.value = s[field];
      }
      $('source-type').value = s.sourceType; $('source-type').disabled = !control.editable();
      const saved = s.sourceType === 'playlist', online = s.sourceType === 'catalog', analog = s.sourceType === 'line_in';
      const set = s.destinationMode === 'set', native = s.sourceType === 'favorite' || set && saved;
      $('source-destination-mode').value = s.destinationMode;
      $('source-destination-mode').disabled = !control.editable() || analog;
      $('source-target-fields').hidden = !set;
      for (const [id, value] of [['source-destination-input', s.destinationInput], ['source-native-query', s.nativeQuery]]) {
        if ($(id).value !== value) $(id).value = value;
        $(id).disabled = !control.editable();
      }
      $('source-target-refresh').disabled = !control.editable() || s.loading;
      $('source-target-suggestions').disabled = !control.editable();
      if (shownSuggestions !== s.suggestions) {
        shownSuggestions = s.suggestions;
        const empty = doc.createElement('option'); empty.value = ''; empty.textContent = 'Choose a suggestion or type a target…';
        $('source-target-suggestions').replaceChildren(empty);
        for (const suggestion of s.suggestions) {
          const option = doc.createElement('option'); option.value = suggestion.query; option.textContent = suggestion.label;
          $('source-target-suggestions').append(option);
        }
      }
      $('source-native-field').hidden = !native;
      $('source-native-label').textContent = s.sourceType === 'favorite' ? 'Favorite title' : 'Saved playlist title';
      for (const id of ['source-link-field', 'source-title-field', 'source-kinds']) $(id).hidden = saved || online || analog || native;
      for (const id of ['source-playlist-field', 'source-playlist-controls']) $(id).hidden = !saved || native;
      for (const id of ['source-line-in-field', 'source-line-in-controls']) $(id).hidden = !analog;
      $('source-mode-field').hidden = analog;
      $('source-mode').disabled = !control.editable() || analog;
      $('source-online').hidden = !online;
      if ($('source-catalog-query').value !== s.catalog.query) $('source-catalog-query').value = s.catalog.query;
      $('source-catalog-kind').value = s.catalog.kind;
      const catalogReady = control.editable() && !s.loading;
      for (const id of ['source-catalog-query', 'source-catalog-kind']) $(id).disabled = !catalogReady;
      $('source-catalog-search').disabled = !catalogReady || s.catalog.status === 'loading';
      $('source-catalog-cancel').hidden = s.catalog.status !== 'loading';
      $('source-catalog-message').textContent = s.catalog.message;
      $('source-online').setAttribute('aria-busy', String(s.catalog.status === 'loading'));
      if (shownCatalog !== s.catalog.items || shownCatalogReady !== catalogReady) {
        shownCatalog = s.catalog.items; shownCatalogReady = catalogReady;
        P.renderCatalog($('source-catalog-items'), s.catalog, doc, { choose: control.chooseCatalog, disabled: !catalogReady });
      }
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
      if (shownLineIn !== s.lineInSources || shownLineInStatus !== s.lineInStatus) {
        shownLineIn = s.lineInSources; shownLineInStatus = s.lineInStatus;
        const caption = s.lineInStatus === 'loading' ? 'Reading inputs…'
          : s.lineInStatus === 'ready' ? s.lineInSources.length ? 'Choose a line-in source…' : 'No available inputs'
            : 'Read inputs for this destination';
        const placeholder = doc.createElement('option'); placeholder.value = ''; placeholder.textContent = caption; placeholder.disabled = true;
        $('source-line-in').replaceChildren(placeholder);
        for (const item of s.lineInSources) {
          const option = doc.createElement('option'); option.value = item.player; option.textContent = P.lineInLabel(item);
          $('source-line-in').append(option);
        }
      }
      $('source-line-in').value = s.lineIn;
      $('source-line-in').disabled = !control.editable() || s.loading || s.lineInStatus !== 'ready' || !s.lineInSources.length;
      $('source-line-in-refresh').disabled = !control.editable() || s.loading || !s.group || !controller.canControl();
      $('source-line-in-refresh').textContent = s.lineInStatus === 'ready' ? 'Refresh line-in sources' : 'Read line-in sources';
      $('source-line-in-help').textContent = P.lineInHelp(s);
      $('source-contents').hidden = !saved || native || !s.playlist || s.playlistStatus !== 'ready';
      $('source-contents-read').disabled = !control.canReview();
      $('source-contents-read').textContent = s.contents ? 'Refresh tracks' : 'Read tracks';
      $('source-contents-previous').hidden = !s.contentsHistory.length;
      $('source-contents-next').hidden = s.contents?.next_offset == null;
      for (const id of ['source-contents-previous', 'source-contents-next']) $(id).disabled = !control.canReview();
      $('source-contents-help').textContent = P.contentsSummary(s.contents);
      if (shownContents !== s.contents || shownContentsReady !== control.canReview()) {
        shownContents = s.contents; shownContentsReady = control.canReview();
        P.renderContents($('source-contents-items'), s.contents, doc, { review: control.reviewTrack, disabled: !shownContentsReady });
      }
      $('source-context').textContent = set ? 'Resolve a room, alias, here, all@S1, all@S2, or everywhere before sending one request.' : s.group ? 'Destination: ' + s.group.label
        : 'Search Spotify or prepare a source now. Select a listening group before reviewing playback.';
      $('source-review').textContent = s.mode === 'play' ? 'Review playback' : 'Review queue addition';
      $('source-review').disabled = !control.canReview(); $('source-clear').disabled = !control.editable();
      $('source-message').textContent = s.message;
      dialog.setAttribute('aria-busy', String(s.loading || s.writing));
      preview.hidden = !s.pending;
      if (s.pending) {
        const p = s.pending, q = p.before.queue;
        $('source-preview-title').textContent = P.modes[p.mode] + ': ' + (p.title || p.item.uri);
        $('source-preview-source').textContent = p.item.kind === 'sonos_playlist_track'
          ? `Track ${p.item.position} · ${p.item.playlistTitle} · ${p.item.id}`
          : p.item.kind === 'line_in' ? 'Line-in · ' + P.lineInLabel(p.item)
          : p.item.kind === 'favorite_query' ? 'Household favorite · ' + p.item.query
          : p.item.kind === 'playlist_query' ? 'Saved Sonos playlist · ' + p.item.query
          : p.item.kind === 'sonos_playlist' ? 'Saved Sonos playlist · ' + p.item.id : p.item.uri;
        $('source-preview-target').textContent = p.targets ? `${p.target} · ${p.targets.groups.length} existing group(s)` : p.group.label;
        $('source-preview-current').textContent = p.targets ? 'Each affected group is shown below.' : p.before.label;
        $('source-preview-effect').textContent = effect(p);
        $('source-preview-queue').hidden = !q;
        $('source-preview-queue').textContent = q ? q.playing === null
          ? q.total + ' queue entries; this group is not currently using its queue.'
          : q.total + ' queue entries; reported current occurrence: ' + q.playing + '.' : '';
        const groups = $('source-preview-groups'); groups.hidden = !p.targets;
        if (p !== shown) {
          groups.replaceChildren();
          for (const entry of p.targets?.entries || []) {
            const row = doc.createElement('li'), name = doc.createElement('strong'); name.textContent = entry.group.label;
            const current = doc.createElement('p'); current.textContent = 'Current playback: ' + entry.before.label;
            const source = doc.createElement('p'); source.textContent = 'Source: ' + entry.sourceLabel;
            row.append(name, current, source);
            if (entry.before.queue) {
              const queue = entry.before.queue, effect = doc.createElement('p');
              effect.textContent = p.mode === 'play' ? `${queue.total} existing queue entries; playback will use the reviewed source.`
                : p.mode === 'next' && queue.playing !== null ? `Add after queue occurrence ${queue.playing}; ${queue.total} existing entries.`
                  : `Append after ${queue.total} existing entries; playback stays as it is.`;
              row.append(effect);
            }
            groups.append(row);
          }
        }
        $('source-confirm').textContent = P.modes[p.mode];
      }
      $('source-confirm').disabled = !control.canApply(); $('source-back').disabled = s.writing;
      $('source-result').hidden = !s.result;
      $('source-result-title').textContent = s.result
        ? (s.result.status === 'accepted' ? s.result.partial ? 'Incomplete request' : 'Command accepted' : s.result.status === 'uncertain' ? 'Check the command outcome' : 'No command sent')
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
    $('source-destination-mode').addEventListener('change', event => control.edit('destinationMode', event.target.value));
    $('source-destination-input').addEventListener('input', event => control.edit('destinationInput', event.target.value));
    $('source-native-query').addEventListener('input', event => control.edit('nativeQuery', event.target.value));
    $('source-target-refresh').addEventListener('click', () => { void control.loadTargets(); });
    $('source-target-suggestions').addEventListener('change', event => {
      if (event.target.value) control.edit('destinationInput', event.target.value);
      event.target.value = '';
    });
    $('source-catalog-query').addEventListener('input', event => control.catalog.edit('query', event.target.value));
    $('source-catalog-kind').addEventListener('change', event => control.catalog.edit('kind', event.target.value));
    $('source-catalog-form').addEventListener('submit', event => { event.preventDefault(); void control.catalog.search(); });
    $('source-catalog-cancel').addEventListener('click', () => control.catalog.cancel());
    $('source-playlist').addEventListener('change', event => control.edit('playlist', event.target.value));
    $('source-playlist-refresh').addEventListener('click', () => { void control.loadPlaylists(); });
    $('source-line-in').addEventListener('change', event => control.edit('lineIn', event.target.value));
    $('source-line-in-refresh').addEventListener('click', () => { void control.loadLineInSources(); });
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
