// Save an observed queue as a new native playlist, through one scoped command.
'use strict';
const DashboardPlaylistSave = (() => {
  const C = typeof DashboardCore !== 'undefined' ? DashboardCore : require('./dashboard-core.js');
  const P = typeof SourcePlayerRemote !== 'undefined' ? SourcePlayerRemote : require('./source-player.js');
  const MAX_TITLE_BYTES = 1023, MAX_AGE = 45000;
  const fail = message => { throw new Error(message); };
  function titleOf(value) {
    if (typeof value !== 'string') fail('Enter a title for the new Sonos playlist.');
    // Rust str::trim uses Unicode White_Space: NEL is whitespace, BOM is not.
    const title = value.replace(/^\p{White_Space}+|\p{White_Space}+$/gu, '');
    if (!title) fail('Enter a title for the new Sonos playlist.');
    if (/[\p{Cc}]/u.test(title)) fail('The playlist title cannot contain control characters.');
    if ([...title].some(ch => ch.length === 1 && ch.charCodeAt(0) >= 0xd800 && ch.charCodeAt(0) <= 0xdfff)) {
      fail('The playlist title contains an incomplete Unicode character.');
    }
    if (new TextEncoder().encode(title).length > MAX_TITLE_BYTES) fail('The playlist title cannot exceed 1023 UTF-8 bytes.');
    return title;
  }
  const decodeCatalog = value => P.playlists(value);
  function receiptOf(value, reviewed) {
    if (!value || typeof value.done !== 'string' || !value.done.trim() || value.done.length > 4096
        || value.zone !== reviewed.group.coordinator || typeof value.logged !== 'boolean'
        || !value.playlist || typeof value.playlist.id !== 'string' || !/^SQ:[0-9]+$/.test(value.playlist.id)
        || value.playlist.title !== reviewed.title) {
      fail('The native playlist save reply did not confirm the reviewed title and destination.');
    }
    return Object.freeze({ done: value.done, logged: value.logged,
      playlist: Object.freeze({ id: value.playlist.id, title: value.playlist.title }) });
  }
  function description(reviewed) {
    return `Create a new Sonos playlist from the ${reviewed.queue.total.toLocaleString()} tracks in ${reviewed.group.label}’s queue. `
      + 'An existing playlist with this title stays separate. A retained queue can be saved while radio or another source plays. '
      + 'Playback, volume and the DJ continue unchanged. Sonos saves the queue when it handles the request; another controller can still edit it after this review.';
  }

  function model({ call, controller, changed = () => {}, saved = () => {}, now = () => performance.now() }) {
    let version = 0, active = true, disposed = false, reading = null, catalogReading = null, catalogVersion = 0;
    const state = { open: false, target: '', scope: '', group: null, title: '', queue: null, loaded: false,
      observedAt: null, loading: false, writing: false, pending: null, result: null, message: '',
      catalog: [], catalogTarget: '', catalogStatus: 'idle' };
    const publish = () => { if (!disposed) { try { changed(state); } catch (_) { /* Presentation only. */ } } };
    const live = () => !disposed && active && state.open && controller.state.active;
    const target = () => controller.state.selected || '';
    const scope = () => controller.selected() ? C.shape(controller.selected().zone) : '';
    const matching = () => state.target === target() && state.scope === scope();
    const editable = () => !!(live() && state.group && !state.loading && !state.writing
      && state.catalogStatus !== 'loading' && !controller.state.busy);
    const destinationReady = () => !!(state.group && matching() && controller.state.topologyFresh
      && controller.selected()?.fresh && controller.canControl());
    const canRead = () => !!(editable() && destinationReady());
    const observed = () => !!(state.loaded && state.queue && state.observedAt !== null
      && now() - state.observedAt < MAX_AGE);
    function retire() {
      ++version; reading?.abort(); reading = null;
      state.loading = false; state.loaded = false; state.pending = null;
    }
    function stopCatalog(clear = false) {
      ++catalogVersion; catalogReading?.abort(); catalogReading = null;
      if (clear) { state.catalog = []; state.catalogTarget = ''; state.catalogStatus = 'idle'; }
      else if (state.catalogStatus === 'loading') state.catalogStatus = 'idle';
    }
    function destination() {
      state.target = target(); state.scope = scope();
      try { state.group = controller.selected() ? P.groups([controller.selected().zone])[0] : null; }
      catch (error) { state.group = null; state.message = C.problem(error); }
    }
    function observe() {
      if (disposed) return;
      if (!matching()) {
        retire(); state.queue = null; state.observedAt = null; destination();
        state.message = 'The listening group changed. Read its current queue before reviewing a save.';
      } else if (!state.writing && (state.loaded || state.loading || state.pending)
          && (!live() || !destinationReady() || (state.loaded && !observed()))) {
        retire(); state.message = 'Read the current queue again before reviewing a save.';
      }
      publish();
    }
    function edit(field, value) {
      if (!editable() || field !== 'title' || typeof value !== 'string' || value.length > 8192) return false;
      ++version; state.pending = null; state.title = value; state.message = ''; publish(); return true;
    }
    async function inspect(proposed, options, valid) {
      valid();
      const groups = P.groups(C.zones(await call('/zones', undefined, options))); valid();
      if (!groups.some(group => group.target === proposed.target && group.signature === proposed.group.signature)) {
        fail('The destination group changed. Refresh playback and read its queue again.');
      }
      const value = await call('/zones/' + encodeURIComponent(proposed.target) + '/queue?offset=0&limit=1', undefined, options);
      valid(); return P.queue(value, proposed.group);
    }
    async function read() {
      if (!canRead()) return false;
      retire();
      const v = version, started = now(), proposed = { target: state.target, group: state.group };
      const abort = new AbortController(); reading = abort;
      state.loading = true; state.message = 'Reading the group’s current queue…'; publish();
      const valid = () => {
        if (!live() || v !== version || abort.signal.aborted || !matching() || !destinationReady()
            || now() - started >= MAX_AGE) fail('This queue observation is no longer current. Read the queue again.');
      };
      try {
        const queue = await inspect(proposed, { signal: abort.signal }, valid); valid();
        state.queue = queue; state.loaded = true; state.observedAt = started;
        state.message = queue.total ? '' : 'This group’s queue is empty. Queue some music before saving a playlist.';
        return true;
      } catch (error) {
        if (live() && v === version) state.message = C.problem(error);
        return false;
      } finally {
        if (reading === abort) { reading = null; state.loading = false; publish(); }
      }
    }
    const canReview = () => {
      if (!editable() || !destinationReady() || !observed() || !state.queue.total) return false;
      try { titleOf(state.title); return true; } catch (_) { return false; }
    };
    async function review() {
      if (!editable()) return null;
      let title;
      try { title = titleOf(state.title); }
      catch (error) { state.message = C.problem(error); publish(); return null; }
      if (!canReview()) return null;
      const proposed = { target: state.target, scope: state.scope, group: state.group, title,
        queue: state.queue, body: Object.freeze({ zone: state.target, title }), at: state.observedAt, version };
      state.pending = Object.freeze({ ...proposed, description: description(proposed) });
      state.result = null; stopCatalog(true); state.message = ''; publish(); return state.pending;
    }
    const canApply = () => !!(canReview() && state.pending && state.pending.version === version
      && now() - state.pending.at < MAX_AGE);
    async function apply(reviewed = state.pending) {
      if (!canApply() || reviewed !== state.pending) return false;
      const v = version, abort = new AbortController();
      let receipt = null, ok = false;
      reading = abort; state.pending = null; state.writing = true; state.result = null;
      stopCatalog(true); state.message = 'Checking the reviewed group and queue again…'; publish();
      try {
        ok = await controller.mutate(async ({ target: selected, check, send }) => {
          const valid = () => {
            check();
            if (!live() || v !== version || abort.signal.aborted || selected !== reviewed.target || !matching()
                || !controller.state.topologyFresh || !controller.selected()?.fresh || now() - reviewed.at >= MAX_AGE) {
              fail('This queue-save review is no longer current. Read the queue and review again.');
            }
          };
          const queue = await inspect(reviewed, { signal: abort.signal }, valid); valid();
          if (!queue.total || queue.revision !== reviewed.queue.revision || queue.total !== reviewed.queue.total) {
            fail('The queue changed since you reviewed it. Read its current contents before saving.');
          }
          state.message = 'Saving the queue as a new Sonos playlist…'; publish(); valid(); reading = null;
          receipt = receiptOf(await send('/sonos/playlists/save', reviewed.body), reviewed);
          return { done: receipt.done, changed: true, notes: receipt.logged ? [] : [
            { detail: 'The playlist was saved, but the action log could not be written. Do not repeat the save.' },
          ] };
        }, { stored: 'playlists', storeTarget: reviewed.target });
        if (!disposed) {
          state.result = Object.freeze({ status: ok ? 'accepted' : controller.state.uncertain ? 'uncertain' : 'not_sent',
            target: reviewed.target, group: reviewed.group, label: reviewed.group.label, title: reviewed.title,
            playlist: ok ? receipt.playlist : null, logged: ok ? receipt.logged : null,
            detail: controller.state.message || 'No save was sent. Read the queue and review again.' });
          state.message = state.result.detail;
        }
      } finally {
        state.writing = false; retire(); publish();
      }
      if (ok && !disposed) {
        try { await saved({ target: reviewed.target, playlist: receipt.playlist, logged: receipt.logged }); }
        catch (_) { state.message += ' Refresh saved Sonos playlists to see the new entry.'; }
        publish();
      }
      return ok;
    }
    const canCheckCatalog = () => !!(live() && state.result && state.result.status !== 'not_sent'
      && !state.writing && !state.loading && state.catalogStatus !== 'loading' && !controller.state.busy);
    async function checkCatalog() {
      if (!canCheckCatalog()) return null;
      const result = state.result, v = ++catalogVersion, abort = new AbortController();
      catalogReading = abort; state.catalogStatus = 'loading'; state.catalogTarget = result.target;
      state.message = 'Reading saved Sonos playlists for ' + result.target + '…'; publish();
      const valid = () => {
        if (!live() || v !== catalogVersion || abort.signal.aborted || state.result !== result) {
          fail('This saved-playlist observation is no longer current. Read the catalog again.');
        }
      };
      try {
        const catalog = await controller.refreshStored('playlists', value => { valid(); return decodeCatalog(value); },
          { explicit: true, storeTarget: result.target, signal: abort.signal });
        valid();
        if (catalog === null) fail('The saved-playlist catalog was not refreshed. Read it again.');
        state.catalog = catalog; state.catalogStatus = 'ready'; retire();
        state.message = 'Saved Sonos playlists refreshed for ' + result.target + '. '
          + (result.status === 'uncertain' ? 'Inspect this observation; a matching title does not prove that the earlier save succeeded. Nothing was retried.'
            : 'The catalog is an observation of what is saved on this household.');
        return catalog;
      } catch (error) {
        if (live() && v === catalogVersion) { state.catalogStatus = 'error'; state.message = C.problem(error); }
        return null;
      } finally {
        if (catalogReading === abort) { catalogReading = null; publish(); }
      }
    }
    function close() { state.open = false; retire(); stopCatalog(); publish(); }
    return { state, open() {
      if (disposed || !active || !controller.state.active) return false;
      state.open = true;
      if (!state.writing) {
        if (!matching()) { state.queue = null; state.observedAt = null; }
        retire(); destination();
        state.message = controller.state.uncertain
          ? state.result?.status === 'uncertain' ? 'Check saved Sonos playlists for the original group before another command.'
            : controller.state.message || 'Resolve the earlier uncertain command before saving a playlist.'
          : destinationReady() ? '' : 'Select a listening group and refresh playback before reading its queue.';
      }
      publish(); return true;
    }, read, edit, editable, canRead, canReview, review, canApply, apply, canCheckCatalog, checkCatalog, observe, close,
      cancelReview() { if (state.writing) return false; state.pending = null; state.message = ''; publish(); return true; },
      suspend() { active = false; close(); },
      resume() { if (!disposed) { active = true; observe(); } },
      dispose() { close(); disposed = true; active = false; },
    };
  }
  return { model, titleOf, receiptOf, description, decodeCatalog, MAX_TITLE_BYTES, MAX_AGE };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = DashboardPlaylistSave;
