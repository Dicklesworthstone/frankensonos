// Copy exact native queue occurrences through one reviewed destination request.
'use strict';
const QueueCopyRemote = (() => {
  const P = typeof SourcePlayerRemote !== 'undefined' ? SourcePlayerRemote : require('./source-player.js');
  const T = typeof SourceTargetsRemote !== 'undefined' ? SourceTargetsRemote : require('./source-targets.js');
  const R = typeof RoomSetsRemote !== 'undefined' ? RoomSetsRemote : require('./room-sets.js');
  const routes = { ...T.routes, zones: ['GET', '/zones'], state: ['GET', '/zones/{room}/state'],
    sourceQueueState: ['GET', '/zones/{room}/queue'], queueCopy: ['POST', '/queue/selection'] };
  const REVIEW_AGE_MS = 45000;
  const fail = message => { throw new Error(message); };
  const bytes = value => new TextEncoder().encode(value).length;
  const uint = value => Number.isInteger(value) && value >= 0 && value <= 0xffffffff;
  const keys = (value, allowed) => value && typeof value === 'object' && !Array.isArray(value)
    && Object.keys(value).every(key => allowed.includes(key));
  const revisionOf = value => typeof value === 'string' && value.trim() !== ''
    && value.length <= 1024 && bytes(value) <= 1024 && !/[\p{Cc}]/u.test(value);
  const labelOf = value => typeof value === 'string' && value.length <= 16384 && bytes(value) <= 16384
    && !/[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f-\u009f]/u.test(value);
  const sameTrack = (a, b) => a.position === b.position && a.title === b.title && a.uri === b.uri
    && a.creator === b.creator && a.album === b.album;

  function trackOf(value, total) {
    if (!keys(value, ['position', 'title', 'uri', 'creator', 'album']) || !uint(value.position)
        || value.position < 1 || value.position > Math.min(total, 100000) || !labelOf(value.title)
        || typeof value.uri !== 'string' || !value.uri.trim() || value.uri === 'NOT_IMPLEMENTED'
        || value.uri.length > 65536 || bytes(value.uri) > 65536 || /[\p{Cc}]/u.test(value.uri)
        || !labelOf(value.creator ?? '') || !labelOf(value.album ?? '')) {
      fail('Select readable native queue occurrences with their original positions and titles. Refresh the source queue.');
    }
    return Object.freeze({ position: value.position, title: value.title, uri: value.uri,
      creator: value.creator ?? '', album: value.album ?? '' });
  }

  function selectionOf(value) {
    if (!keys(value, ['zone', 'revision', 'total', 'tracks']) || typeof value.zone !== 'string'
        || T.query(value.zone) !== value.zone || value.zone.lastIndexOf('@') < 1 || value.zone.endsWith('@')
        || !revisionOf(value.revision) || !uint(value.total) || !value.total
        || !Array.isArray(value.tracks) || !value.tracks.length || value.tracks.length > 10) {
      fail('Choose 1–10 tracks from a qualified room’s current queue, then read its queue again.');
    }
    const tracks = value.tracks.map(track => trackOf(track, value.total)), positions = new Map();
    for (const track of tracks) {
      if (positions.has(track.position) && !sameTrack(positions.get(track.position), track)) {
        fail('Repeated positions must identify the same original queue occurrence. Read the queue again.');
      }
      positions.set(track.position, track);
    }
    return Object.freeze({ zone: value.zone, revision: value.revision, total: value.total, tracks: Object.freeze(tracks) });
  }

  async function sourceResolution(call, zone, valid) {
    valid();
    const value = await call('sourceTargetResolve', { query: { target: zone } }); valid();
    const view = R.resolutionOf(value, zone);
    if (view.groups.length !== 1 || view.rooms.length !== 1
        || `${view.rooms[0].room}@${view.rooms[0].household}` !== zone
        || typeof value.revision !== 'string' || !/^[a-f0-9]{64}$/.test(value.revision)) {
      fail('The original queue room could not be bound to its current physical group. Refresh the source queue.');
    }
    return { view, signature: JSON.stringify([view.signature, value.revision]), revision: value.revision };
  }

  async function readSourceGroup({ call, selection, valid = () => {} }) {
    valid(); const zones = P.groups(await call('zones')); valid();
    const physical = await sourceResolution(call, selection.zone, valid), resolved = physical.view.groups[0];
    const group = zones.find(group => group.signature === resolved.shape);
    if (!group) fail('The original queue group changed while it was read. Refresh the source queue.');
    return Object.freeze({ ...group, shape: resolved.shape, coordinator_id: resolved.coordinator,
      members: resolved.members, sourceSignature: physical.signature, sourceRevision: physical.revision });
  }

  async function readQueueSelection({ call, item, group, valid = () => {} }) {
    if (!group || item?.kind !== 'native_queue_tracks' || !Array.isArray(item.tracks) || !item.tracks.length) {
      fail('Read the original queue and its selected occurrences again.');
    }
    const sourceUnchanged = async () => {
      if ((await sourceResolution(call, item.zone, valid)).signature !== group.sourceSignature) {
        fail('The original queue’s physical group changed. Refresh the source queue and select its tracks again.');
      }
    };
    await sourceUnchanged();
    // One-item reads cannot lose a selected occurrence to a speaker’s short
    // page. Repeated positions share a read; order stays in item.tracks.
    const selected = [...new Map(item.tracks.map(track => [track.position, track])).values()];
    for (const track of selected) {
      valid();
      const value = await call('sourceQueueState', { params: { room: item.zone }, query: { offset: track.position - 1, limit: 1 } });
      valid();
      if (!value || value.zone !== group.coordinator || value.revision !== item.revision || value.total !== item.total
          || !Array.isArray(value.items) || value.items.length !== 1 || !sameTrack(trackOf(value.items[0], item.total), track)) {
        fail('The selected source queue occurrences changed. Refresh the original queue and select its tracks again.');
      }
    }
    await sourceUnchanged();
    return item;
  }

  function queueSelectionBody(choice, sourceGroup) {
    const item = choice?.item;
    if (item?.kind !== 'native_queue_tracks' || !sourceGroup || !['play', 'next', 'end'].includes(choice.mode)) {
      fail('Review this exact queue selection and its source group again.');
    }
    const selection = selectionOf({ zone: item.zone, revision: item.revision, total: item.total, tracks: item.tracks });
    if (!sourceGroup.members.some(room => `${room.room}@${room.household}` === selection.zone)) {
      fail('The original queue does not belong to the reviewed source group.');
    }
    return Object.freeze({ zone: selection.zone, positions: Object.freeze(selection.tracks.map(track => track.position)),
      expected_revision: selection.revision, mode: choice.mode === 'end' ? 'append' : choice.mode });
  }
  const player = { ...P, readQueueSelection, queueSelectionBody };
  const itemOf = selection => Object.freeze({ ...selection, kind: 'native_queue_tracks', container: true,
    positions: Object.freeze(selection.tracks.map(track => track.position)) });

  function model({ call, executeSet = (_, work) => work(), changed = () => {}, now = () => performance.now() }) {
    const state = { open: false, reading: false, writing: false, selection: null, sourceGroup: null,
      target: '', mode: 'play', pending: null, result: null, message: '' };
    let epoch = 0, active = true, disposed = false, reader = null;
    const publish = () => { if (!disposed) { try { changed(state); } catch (_) { /* Rendering cannot change a command outcome. */ } } };
    const usable = () => active && !disposed && state.open;
    const invalidate = () => { epoch++; reader?.abort(); reader = null; state.reading = false; state.pending = null; };
    const validFor = (generation, selection) => () => {
      if (!usable() || epoch !== generation || state.selection !== selection) {
        fail('The queue-copy selection or view changed. Review the current selection again.');
      }
    };
    const readStart = () => { reader = new AbortController(); state.reading = true; state.message = ''; publish(); return reader; };
    const readWith = (abort, valid) => async (route, options = {}) => {
      valid(); const value = await call(route, { ...options, signal: abort.signal }); valid(); return value;
    };
    const errorText = error => [error?.message || 'The queue copy was not confirmed.', error?.hint].filter(value => typeof value === 'string').join(' ');

    async function open(value) {
      if (disposed || !active || state.writing) return false;
      let selection;
      try { selection = selectionOf(value); }
      catch (error) { state.message = errorText(error); publish(); return false; }
      invalidate(); state.open = true; state.selection = selection; state.sourceGroup = null;
      state.target = selection.zone; state.mode = 'play'; state.result = null;
      const generation = epoch, valid = validFor(generation, selection), abort = readStart(), read = readWith(abort, valid);
      try {
        const group = await readSourceGroup({ call: read, selection, valid });
        await readQueueSelection({ call: read, item: itemOf(selection), group, valid }); valid();
        state.sourceGroup = group; return true;
      } catch (error) { if (epoch === generation && !disposed) state.message = errorText(error); return false; }
      finally { if (epoch === generation) { reader = null; state.reading = false; publish(); } }
    }

    function edit(field, value) {
      if (!usable() || state.writing || !['target', 'mode'].includes(field)
          || typeof value !== 'string' || (field === 'mode' && !['play', 'next', 'end'].includes(value))) return false;
      if (state[field] === value) return true;
      invalidate(); state[field] = value; state.message = ''; publish(); return true;
    }

    function changeSelection(action, index) {
      if (!usable() || state.reading || state.writing || !state.selection
          || !Number.isInteger(index) || index < 0 || index >= state.selection.tracks.length) return false;
      const tracks = [...state.selection.tracks];
      if (action === 'up' && index > 0) [tracks[index - 1], tracks[index]] = [tracks[index], tracks[index - 1]];
      else if (action === 'down' && index + 1 < tracks.length) [tracks[index], tracks[index + 1]] = [tracks[index + 1], tracks[index]];
      else if (action === 'repeat' && tracks.length < 10) tracks.splice(index + 1, 0, tracks[index]);
      else if (action === 'remove') tracks.splice(index, 1);
      else return false;
      invalidate(); state.selection = Object.freeze({ ...state.selection, tracks: Object.freeze(tracks) });
      state.message = tracks.length ? '' : 'Choose tracks from the source queue to begin another copy.'; publish(); return true;
    }

    async function review() {
      if (!usable() || state.reading || state.writing || !state.selection?.tracks.length || !state.sourceGroup) return false;
      invalidate();
      const selection = state.selection, sourceGroup = state.sourceGroup, mode = state.mode;
      const generation = epoch, valid = validFor(generation, selection), abort = readStart(), read = readWith(abort, valid);
      try {
        const item = itemOf(selection), choice = { item, mode };
        const targets = await T.inspect({ call: read, player, target: state.target, item, mode, sourceGroup, valid }); valid();
        const request = T.request(choice, targets, player);
        state.pending = Object.freeze({ selection, item, targets, mode, ...request, reviewedAt: now() });
        state.result = null; return true;
      } catch (error) { if (epoch === generation && !disposed) state.message = errorText(error); return false; }
      finally { if (epoch === generation) { reader = null; state.reading = false; publish(); } }
    }

    async function confirm(pending) {
      if (!usable() || state.reading || state.writing || !pending || state.pending !== pending) return false;
      state.pending = null;
      if (!Number.isFinite(pending.reviewedAt) || now() - pending.reviewedAt < 0 || now() - pending.reviewedAt > REVIEW_AGE_MS) {
        state.message = 'This queue-copy review expired. Review the current queues again.'; publish(); return false;
      }
      const generation = epoch, valid = validFor(generation, pending.selection);
      const sourceGroup = pending.targets.sourceGroup;
      const reservations = [...pending.targets.groups];
      if (!reservations.some(group => group.household === sourceGroup.household && group.coordinator_id === sourceGroup.coordinator_id)) {
        reservations.push(Object.freeze({ ...sourceGroup, sourceOnly: true }));
      }
      state.writing = true; state.message = ''; publish();
      let sent = false, accepted = false, worked = false, workError = null;
      try {
        await executeSet(Object.freeze(reservations), async () => {
          try {
            if (worked) fail('This queue-copy command has already been consumed. Read the destination queues before another action.');
            worked = true; valid();
            const fresh = await T.inspect({ call, player, target: pending.targets.target, item: pending.item,
              mode: pending.mode, sourceGroup, valid }); valid();
            if (fresh.signature !== pending.targets.signature) {
              fail('The destination groups, their queues or playback changed. Review the current queues again.');
            }
            if (now() - pending.reviewedAt > REVIEW_AGE_MS) fail('This queue-copy review expired. Review the current queues again.');
            const request = T.request({ item: pending.item, mode: pending.mode }, fresh, player);
            valid(); sent = true;
            const value = await call(request.route, { body: request.body });
            const receipt = T.result(value, pending.targets, player);
            state.result = Object.freeze({ ...receipt, changed: value.changed,
              uncertain: receipt.uncertain === true || (value.queue_batch?.failed || []).some(row => row.error?.code === 'OUTCOME_UNCERTAIN') });
            accepted = true; publish(); return state.result;
          } catch (error) { workError = error; throw error; }
        });
        if (!worked) fail('The destination groups could not be reserved. Review their current state again.');
      } catch (error) {
        // Global lanes must see the original failure after a send; navigation
        // cannot erase either a verified acknowledgement or an uncertain write.
        if (!accepted) {
          const actual = workError || error;
          state.result = sent && T.failure(actual, pending.targets, player) || Object.freeze({
            status: sent ? 'uncertain' : 'not_sent', complete: false, partial: sent, uncertain: sent,
            detail: errorText(actual) + (sent ? ' This request was not repeated. Read every destination queue before another action.' : ' No queue-copy request was sent.'),
          });
          state.message = state.result.detail;
        }
      } finally { state.writing = false; publish(); }
      return accepted;
    }

    function cancel() { if (!disposed && !state.writing) { invalidate(); state.message = ''; publish(); } }
    function close() { if (!disposed) { invalidate(); state.open = false; publish(); } }
    function suspend() { if (!disposed) { active = false; invalidate(); publish(); } }
    function resume() { if (!disposed) { active = true; publish(); } }
    function dispose() { if (!disposed) { active = false; invalidate(); state.open = false; disposed = true; } }
    return { state, open, edit, changeSelection, review, confirm, cancel, close, suspend, resume, dispose };
  }
  return { routes, REVIEW_AGE_MS, selectionOf, readSourceGroup, readQueueSelection, queueSelectionBody, model };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = QueueCopyRemote;
