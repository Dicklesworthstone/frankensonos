// The DJ, whole works, scenes and library use native contracts, one shared write lane.
'use strict';
const DashboardSession = (() => {
  const C = typeof DashboardCore !== 'undefined' ? DashboardCore : require('./dashboard-core.js');
  const queueApi = () => typeof DashboardQueue !== 'undefined' ? DashboardQueue
    : typeof require === 'function' ? require('./dashboard-queue.js') : null;
  const libraryApi = () => typeof DashboardLibrary !== 'undefined' ? DashboardLibrary
    : typeof require === 'function' ? require('./dashboard-library.js') : null;
  const directionApi = () => typeof DjDirection !== 'undefined' ? DjDirection : require('./dj-direction.js');
  const sceneQueueApi = () => typeof SceneQueue !== 'undefined' ? SceneQueue : require('./scene-queue.js');
  const text = (v, max = 4096) => typeof v === 'string' && v.trim() !== '' && v.length <= max;
  const object = (v) => v !== null && typeof v === 'object' && !Array.isArray(v);
  const uint = (v) => Number.isInteger(v) && v >= 0 && v <= 0xffffffff;
  const list = (v, check, max = 128) => Array.isArray(v) && v.length <= max && v.every((entry) => check(entry));
  const fail = (message) => { throw new Error(message); };
  const clone = (v) => JSON.parse(JSON.stringify(v));
  function freeze(v) {
    if (v && typeof v === 'object') { Object.values(v).forEach(freeze); Object.freeze(v); }
    return v;
  }
  function stable(v) {
    if (Array.isArray(v)) return `[${v.map(stable).join(',')}]`;
    if (object(v)) return `{${Object.keys(v).sort().map((k) => `${JSON.stringify(k)}:${stable(v[k])}`).join(',')}}`;
    return JSON.stringify(v);
  }
  function work(v) {
    if (!object(v) || !text(v.title) || !text(v.composer) || !uint(v.movements) || !v.movements
        || (v.movement != null && (!uint(v.movement) || v.movement < 1 || v.movement > v.movements))
        || (v.minutes != null && !uint(v.minutes)) || (v.performers != null && !list(v.performers, text))
        || !object(v.reason) || typeof v.reason.summary !== 'string'
        || (v.reason.factors != null && !list(v.reason.factors, (f) => object(f) && text(f.factor)
          && Number.isInteger(f.weight) && Math.abs(f.weight) <= 2147483648))
        || (v.reason.relaxed != null && !list(v.reason.relaxed, text))) fail('The DJ work is unreadable. Refresh the session.');
    return clone(v);
  }
  function dj(v, zone) {
    if (!object(v) || v.zone !== zone.coordinator_room || typeof v.running !== 'boolean'
        || !object(v.steering) || !['session', 'program', 'none'].includes(v.steering.source)
        || (v.steering.mood != null && !text(v.steering.mood))
        || (v.steering.constraints != null && !object(v.steering.constraints))
        || (v.steering.added != null && !object(v.steering.added))
        || (v.steering.expires_at != null && !Number.isSafeInteger(v.steering.expires_at))
        || !list(v.next || [], object, 2)) fail('The DJ status does not match this room. Refresh the session.');
    return freeze({ ...clone(v), now: v.now == null ? null : work(v.now), next: (v.next || []).map(work) });
  }
  function djIdentity(value) {
    const v = clone(value);
    delete v.steering.expires_in_secs;
    return stable(v);
  }
  const playbackIdentity = (i) => stable([C.shape(i.zone), i.state?.transport_state, i.state?.track?.uri ?? null, i.state?.track?.queue_position ?? null]);
  function moods(v) {
    if (!object(v) || !list(v.moods, (m) => object(m) && text(m.name, 128) && object(m.constraints))) fail('The mood catalog is unreadable.');
    if (new Set(v.moods.map((m) => m.name)).size !== v.moods.length) fail('Mood names are ambiguous.');
    return freeze(clone(v.moods));
  }
  function scene(v) {
    if (!object(v) || !text(v.name, 256) || !Number.isSafeInteger(v.saved)
        || !list(v.groups, (g) => object(g) && text(g.coordinator) && list(g.members, text)
          && object(g.source) && ['keep', 'favorite', 'uri', 'dj', 'queue', 'queue_backup'].includes(g.source.kind)
          && (['queue', 'queue_backup'].includes(g.source.kind) || g.source.queue == null)
          && (g.source.kind === 'dj' || g.source.direction == null)
          && (g.source.kind !== 'favorite' || text(g.source.name))
          && (g.source.kind !== 'uri' || text(g.source.uri))
          && (g.source.mood == null || text(g.source.mood))
          && (g.playing == null || typeof g.playing === 'boolean'))
        || !object(v.volumes) || !object(v.mutes) || Object.keys(v.volumes).length > 256 || Object.keys(v.mutes).length > 256
        || Object.entries(v.volumes).some(([r, n]) => !text(r) || !Number.isInteger(n) || n < 0 || n > 100)
        || Object.entries(v.mutes).some(([r, m]) => !text(r) || typeof m !== 'boolean')) fail('The saved scene is unreadable.');
    const copy = clone(v);
    for (const group of copy.groups) {
      if (['queue', 'queue_backup'].includes(group.source.kind)) {
        group.source.queue = sceneQueueApi().read(group.source.queue);
        if ((group.source.kind === 'queue_backup') !== (group.source.queue.total != null)) fail('The saved queue source kind is unreadable.');
      }
      if (group.source.direction != null) group.source.direction = directionApi().read(group.source.direction, group.source.mood);
    }
    return freeze(copy);
  }
  function sceneSource(source) {
    return ['queue', 'queue_backup'].includes(source.kind) ? sceneQueueApi().describe(source.queue)
      : source.kind === 'dj' && source.direction != null ? directionApi().describe(source.direction, source.mood)
        : `${source.kind}${source.mood ? ` · ${source.mood}` : ''}${source.name ? ` · ${source.name}` : ''}${source.uri ? ` · ${source.uri}` : ''}`;
  }
  function scenes(v) {
    if (!Array.isArray(v) || v.length > 128) fail('The scene catalog is unreadable.');
    const result = v.map(scene);
    if (new Set(result.map((s) => s.name)).size !== result.length) fail('Scene names are ambiguous.');
    return result;
  }
  function hits(v) {
    if (!Array.isArray(v) || v.length > 8) fail('The search response is unreadable.');
    return v.map((h) => {
      if (!object(h) || !text(h.title) || !text(h.kind)) fail('A search result is unreadable.');
      const spotify = typeof h.source_uri === 'string' && /^spotify:(track|album|playlist):[a-zA-Z0-9]+$/.exec(h.source_uri);
      const playable = (spotify && spotify[1] === h.kind) || (h.kind === 'favorite' && /^FV:2\/\d+$/.test(h.favorite));
      return freeze({ title: h.title, kind: h.kind, subtitle: typeof h.subtitle === 'string' ? h.subtitle : '',
        source_uri: spotify ? h.source_uri : null, favorite: playable && h.kind === 'favorite' ? h.favorite : null, playable: !!playable });
    });
  }
  function model({ call, controller, changed = () => {}, demo = false, now = () => performance.now() }) {
    let version = 0, catalogVersion = 0, intent = 0, searchVersion = 0, disposed = false, active = true;
    const state = { target: '', dj: null, djAt: 0, loading: false, error: '', moods: [], scenes: [], catalogError: '',
      pending: null, result: null, query: '', hits: [], searching: false, searchError: '' };
    const emit = () => { if (!disposed) { try { changed(state); } catch (_) { /* View only. */ } } };
    const selection = () => controller.selected();
    const ready = () => active && !disposed && (demo || controller.canControl());
    const freshDj = () => state.dj && state.target === controller.state.selected && now() - state.djAt < 45000 && !state.loading;
    function cancel() { ++intent; state.pending = null; emit(); }
    async function load() {
      if (!active || disposed) return false;
      const item = selection(), target = controller.state.selected, v = ++version;
      if (!item) { state.target = ''; state.dj = null; state.loading = false; cancel(); emit(); return false; }
      if (state.target !== target) { state.dj = null; state.hits = []; state.query = ''; ++searchVersion; state.searching = false; cancel(); }
      state.target = target; state.loading = true; state.error = ''; emit();
      const current = () => active && !disposed && v === version && target === controller.state.selected;
      try {
        const next = dj(await call(`/zones/${encodeURIComponent(target)}/dj`), item.zone);
        if (!current()) return false;
        if (state.pending?.dj && state.pending.dj !== djIdentity(next)) cancel();
        state.dj = next; state.djAt = now(); return true;
      } catch (e) { if (current()) { state.dj = null; state.error = C.problem(e); cancel(); } return false; }
      finally { if (current()) { state.loading = false; emit(); } }
    }
    async function catalog() {
      if (!active || disposed) return;
      const v = ++catalogVersion;
      const values = await Promise.allSettled([call('/dj/moods').then(moods), call('/scenes').then(scenes)]);
      if (!active || disposed || v !== catalogVersion) return;
      state.moods = values[0].status === 'fulfilled' ? values[0].value : [];
      state.scenes = values[1].status === 'fulfilled' ? values[1].value : [];
      state.catalogError = values.filter((r) => r.status === 'rejected').map((r) => C.problem(r.reason)).join(' ');
      emit();
    }
    async function search(query) {
      if (!active || disposed) return false;
      const q = query.trim(), v = ++searchVersion, target = controller.state.selected;
      state.query = q; state.hits = []; state.searchError = ''; state.searching = false;
      if (!q || q.length > 512 || !target) { emit(); return false; }
      state.searching = true; emit();
      try {
        const value = hits(await call(`/library/search?${new URLSearchParams({ q, zone: target, limit: 8 })}`));
        if (disposed || !active || v !== searchVersion || target !== controller.state.selected) return false;
        state.hits = value; return true;
      } catch (e) { if (v === searchVersion && active) state.searchError = C.problem(e); return false; }
      finally { if (v === searchVersion && active) { state.searching = false; emit(); } }
    }
    function review(kind, args = {}) {
      if (!ready()) return null;
      const item = selection(); if (!item) return null;
      const target = C.key(item.zone), label = item.zone.members.join(' + ');
      const r = { kind, target, label, playback: playbackIdentity(item), title: '', body: { zone: target } };
      if (['start', 'steer', 'clear', 'skip', 'stop', 'like', 'dislike'].includes(kind)) {
        if (!freshDj()) return null;
        const running = state.dj.running;
        if (kind === 'start' ? running : !running) return null;
        r.dj = djIdentity(state.dj); r.work = state.dj.now;
        if (['like', 'dislike', 'skip'].includes(kind) && r.work?.movement_title
            && item.state?.track?.title && r.work.movement_title !== item.state.track.title) return null;
        r.url = `/dj/${['like', 'dislike'].includes(kind) ? 'feedback' : kind === 'clear' ? 'steer' : kind}`;
        if (kind === 'steer' || kind === 'start') {
          const mood = args.mood && state.moods.find((m) => m.name === args.mood);
          if ((kind === 'steer' || args.mood) && !mood) return null;
          if (args.for_secs != null && ![1800, 3600, 7200].includes(args.for_secs)) return null;
          if (mood) { r.body.mood = mood.name; r.mood = mood; if (args.for_secs != null) r.body.for_secs = args.for_secs; }
          r.title = running ? `Shift to ${mood.name}` : 'Start your personal DJ';
          r.detail = running ? 'Changes the next pick, not the movement playing. Replaces this group’s session steering; your standing preferences remain.'
            : 'Starts music from your library and lets the daemon keep the queue fed. This can replace what is playing.';
        } else if (kind === 'clear') {
          r.body.clear = true; r.title = 'Follow your time-of-day program'; r.detail = 'Clear this group’s session overrides. The program chooses future picks again.';
        } else if (kind === 'like' || kind === 'dislike') {
          if (!r.work) return null;
          r.body.signal = kind; r.title = kind === 'like' ? 'More like this' : 'Less like this';
          r.detail = 'Record feedback on this work, artist and recording. It shapes later picks; it does not skip or ban this music.';
        } else {
          r.title = kind === 'skip' ? 'Skip to the next whole work' : 'Stop the DJ';
          r.detail = kind === 'skip' ? 'Ends the current pick, including its remaining movements. The next whole work starts.' : 'Stop this group’s DJ session through the native daemon control.';
        }
      } else if (kind === 'play') {
        const hit = state.hits.find((h) => h === args.hit);
        if (!hit?.playable) return null;
        r.hit = hit; r.query = state.query; r.title = `Play ${hit.title}`;
        r.detail = `${hit.kind === 'album' || hit.kind === 'playlist' ? 'Replaces this group’s queue.' : 'Replaces the current source.'} The speaker renders it; this browser does not stream the audio.`;
        r.url = hit.favorite ? '/play/favorite' : '/play';
        if (hit.favorite) r.body.favorite = hit.favorite; else { r.body.source_uri = hit.source_uri; r.body.title = hit.title; }
      } else return null;
      ++intent; state.result = null; state.pending = freeze(r); emit(); return state.pending;
    }
    function reviewLibrary(value, action, destination) {
      const L = libraryApi();
      if (!L || !ready() || state.pending || !['play', 'append', 'next'].includes(action)) return null;
      const item = selection();
      if (!item || destination?.target !== C.key(item.zone) || destination?.shape !== C.shape(item.zone)) return null;
      let catalog;
      try { catalog = L.choiceOf(value); }
      catch (error) { state.error = error.message; emit(); return null; }
      const source = catalog.item, target = C.key(item.zone), name = source.title || `Untitled ${source.kind}`;
      const label = { play: 'Play now', append: 'Add to end', next: 'Queue next' }[action];
      const effect = action !== 'play' ? 'Adds to the speaker queue without starting playback.'
        : source.kind === 'track' ? 'Replaces the current source.' : 'Replaces current playback and the group’s queue.';
      const coverage = source.kind === 'album' ? `Uses the full album source, not just its ${source.cached_tracks} cached tracks.`
        : source.kind === 'playlist' ? 'Uses the playlist source; its live contents may differ from the cache.' : 'Uses this exact track source.';
      ++intent; state.result = null; state.error = '';
      state.pending = freeze({ kind: 'library', target, label: item.zone.members.join(' + '),
        title: `${label}: ${name}`, detail: `${source.subtitle ? `${source.subtitle}. ` : ''}${source.position ? `Playlist position ${source.position}. ` : ''}${effect} ${coverage} Source: ${source.source_uri}. The daemon’s policy and service availability still apply.`,
        playback: playbackIdentity(item), catalog, libraryAction: action,
        url: action === 'play' ? '/play' : '/queue',
        body: { zone: target, source_uri: source.source_uri, title: name, ...(action === 'play' ? {} : { next: action === 'next' }) } });
      emit(); return state.pending;
    }
    function reviewQueue(value, action, insertBefore) {
      const Q = queueApi();
      if (!Q || !ready() || state.pending) return null;
      const item = selection(); if (!item) return null;
      let operation;
      try {
        operation = Q.plan(value, action, insertBefore);
        if (operation.choice.target !== C.key(item.zone) || operation.choice.signature !== C.shape(item.zone)) {
          fail('The selected group changed. Read its queue and select again.');
        }
      } catch (error) { state.error = C.problem(error); emit(); return null; }
      const entries = operation.choice.rows.map((row) => `#${row.position}: ${(row.title || 'Untitled track').slice(0, 300)}${row.title.length > 300 ? '…' : ''}`).join('; ');
      ++intent; state.result = null; state.error = '';
      state.pending = freeze({ kind: 'queue', target: C.key(item.zone), label: item.zone.members.join(' + '),
        playback: playbackIdentity(item), queueEdit: operation, title: operation.title,
        detail: `${operation.detail} Selected entries: ${entries}.`, url: operation.url, body: operation.body });
      emit(); return state.pending;
    }
    async function reviewScene(name) {
      if (!ready() || !state.scenes.some((s) => s.name === name)) return null;
      cancel(); const v = intent, target = controller.state.selected;
      try {
        const [definition, groups] = await Promise.all([call(`/scenes/${encodeURIComponent(name)}`).then(scene), call('/zones').then(C.zones)]);
        if (!ready() || v !== intent || target !== controller.state.selected) return null;
        if (definition.name !== name) fail('The scene response named a different scene.');
        state.pending = freeze({ kind: 'scene', target, label: 'Whole-house scene', title: `Set the house to ${name}`,
          detail: 'Applies grouping, sources, levels and mutes below, subject to house policy. This is a multi-step operation, not an atomic change.',
          scene: definition, topology: stable(groups.map(C.shape).sort()), url: `/scenes/${encodeURIComponent(name)}/apply`, body: {} });
        state.result = null; emit(); return state.pending;
      } catch (e) { if (v === intent) { state.error = C.problem(e); emit(); } return null; }
    }
    async function confirm(r) {
      if (demo || !r || r !== state.pending || !ready() || r.target !== controller.state.selected) return false;
      const id = ++intent; state.pending = null; emit();
      const valid = (check) => { check(); if (!active || disposed || intent !== id) fail('The reviewed action was cancelled.'); };
      const ok = await controller.mutate(async ({ item, target, check, send }) => {
        valid(check);
        if (r.kind === 'scene') {
          const [definition, groups] = await Promise.all([call(`/scenes/${encodeURIComponent(r.scene.name)}`).then(scene), call('/zones').then(C.zones)]);
          valid(check);
          if (stable(definition) !== stable(r.scene) || stable(groups.map(C.shape).sort()) !== r.topology) fail('The scene or household changed. Review it again.');
        } else {
          const fresh = C.playback(await call(`/zones/${encodeURIComponent(target)}/state`), item.zone);
          valid(check);
          if (playbackIdentity({ zone: item.zone, state: fresh }) !== r.playback) fail('Playback changed since review. Choose the action again.');
          if (r.queueEdit) {
            await queueApi().verify(call, r.queueEdit, () => valid(check));
          } else if (r.dj) {
            const current = dj(await call(`/zones/${encodeURIComponent(target)}/dj`), item.zone);
            valid(check);
            if (djIdentity(current) !== r.dj) fail('The DJ session or current work changed. Review it again.');
            if (['like', 'dislike', 'skip'].includes(r.kind) && current.now?.movement_title
                && fresh.track?.title && current.now.movement_title !== fresh.track.title) {
              fail('The reported movement does not match playback. Refresh the session.');
            }
            if (r.mood) {
              const available = moods(await call('/dj/moods'));
              valid(check);
              if (stable(available.find((m) => m.name === r.mood.name)) !== stable(r.mood)) fail('The mood definition changed. Refresh moods.');
            }
          } else if (r.catalog) {
            const L = libraryApi();
            if (!L) fail('The library browser is unavailable. Refresh the page.');
            const page = L.pageOf(await call(L.urlOf(r.catalog.query)), r.catalog.query);
            valid(check);
            if (page.revision !== r.catalog.revision || !page.items.some((entry) => stable(entry) === stable(r.catalog.item))) {
              fail('The cached library selection changed. Refresh its listing and review again.');
            }
          } else if (r.hit) {
            const results = hits(await call(`/library/search?${new URLSearchParams({ q: r.query, zone: target, limit: 8 })}`));
            valid(check);
            if (!results.some((h) => stable(h) === stable(r.hit))) fail('That library result changed. Search again.');
          }
        }
        valid(check);
        if (r.queueEdit) queueApi().choiceOf(r.queueEdit.choice);
        const delivery = send(r.url, r.body);
        if (r.queueEdit) queueApi().submitted(r.queueEdit);
        const result = await delivery;
        if (r.queueEdit) return queueApi().receipt(result, r.queueEdit);
        if (r.kind === 'like' || r.kind === 'dislike') {
          if (!result || result.zone !== item.zone.coordinator_room || result.signal !== r.kind || !text(result.work)
              || result.composer !== r.work.composer || result.work !== `${r.work.composer}: ${r.work.title}` || !text(result.done)) fail('The feedback acknowledgement was incomplete.');
          return { done: result.done, changed: true };
        }
        if (r.catalog && r.libraryAction !== 'play') {
          if (!result || !text(result.done) || !uint(result.first) || result.first < 1
              || !uint(result.count) || result.count < 1 || typeof result.playing_queue !== 'boolean') {
            fail('The queue acknowledgement was incomplete.');
          }
          return { done: result.done, changed: true, notes: result.notes };
        }
        if (r.kind === 'scene') {
          if (!result || result.scene !== r.scene.name || typeof result.complete !== 'boolean'
              || !list(result.steps, text, 4096) || !list(result.skipped || [], text)
              || !list(result.failed || [], object)) fail('The scene acknowledgement was incomplete.');
          const incomplete = !result.complete || result.failed?.length || result.skipped?.length;
          state.result = freeze({ kind: 'scene', complete: !incomplete, steps: clone(result.steps),
            failed: clone(result.failed || []), skipped: clone(result.skipped || []) });
          return { ...result, done: `${result.done}${incomplete ? ' Partial result: check the failed or skipped steps below. No automatic retry.' : ''}` };
        }
        return result;
      });
      emit(); if (ok && active && !disposed) await load(); return ok;
    }
    return { state, load, catalog, search, review, reviewLibrary, reviewQueue, reviewScene, confirm, cancel,
      suspend() { active = false; ++version; ++searchVersion; ++catalogVersion; state.loading = state.searching = false; cancel(); },
      resume() { if (!disposed) active = true; },
      dispose() { disposed = true; active = false; ++version; ++intent; ++searchVersion; ++catalogVersion; },
    };
  }
  return { model, work, dj, djIdentity, playbackIdentity, moods, scene, sceneSource, scenes, hits, stable };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = DashboardSession;
