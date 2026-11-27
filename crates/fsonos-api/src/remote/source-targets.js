// One reviewed source request for independent, authoritatively resolved groups.
'use strict';
const SourceTargetsRemote = (() => {
  const R = typeof RoomSetsRemote !== 'undefined' ? RoomSetsRemote : require('./room-sets.js');
  const routes = { sourceTargetRooms: ['GET', '/rooms'], sourceTargetResolve: ['GET', '/rooms/resolve'],
    sourceFavorites: ['GET', '/favorites'], sourceControl: ['POST', '/sources/control'] };
  const text = (value, max = 65536) => typeof value === 'string' && value.trim() !== ''
    && value.length <= max && !/[\p{Cc}]/u.test(value);
  const key = (household, coordinator) => JSON.stringify([household, coordinator]);
  const fail = message => { throw new Error(message); };
  function query(value) {
    if (!text(value, 1024) || [...value.trim()].length > 128) fail('Enter a room, alias, here, all@household, or everywhere (at most 128 characters).');
    return value.trim();
  }

  // The daemon's exact-title stage uses fsonos_types::text::normalize. Keep its
  // Latin folds and accidental names here; do not guess its fuzzy/prefix stage.
  function nameKey(value) {
    const folds = [['àáâãäåāăą', 'a'], ['æ', 'ae'], ['çćč', 'c'], ['ďđð', 'd'], ['èéêëēėęě', 'e'],
      ['ìíîïīįı', 'i'], ['łľĺ', 'l'], ['ñńň', 'n'], ['òóôõöøōő', 'o'], ['œ', 'oe'], ['řŕ', 'r'],
      ['śšşș', 's'], ['ß', 'ss'], ['ťţț', 't'], ['ùúûüūůűų', 'u'], ['ýÿ', 'y'], ['źżž', 'z'], ['þ', 'th']];
    return [...value.toLowerCase()].map(c => folds.find(([letters]) => letters.includes(c))?.[1]
      ?? (c === '♭' ? ' flat ' : c === '♯' ? ' sharp ' : /[\p{L}\p{N}]/u.test(c) ? c : ' ')).join('').trim().replace(/ +/g, ' ');
  }
  function prepareNative(kind, raw, mode) {
    if (!['playlist', 'favorite'].includes(kind) || !['play', 'next', 'end'].includes(mode)) fail('Choose a supported source and action.');
    if (!text(raw, 1024) || [...raw.trim()].length > 256 || !nameKey(raw)) fail('Enter the complete saved playlist or favorite title (at most 256 characters).');
    const title = raw.trim();
    if (/^\+?\d+$/.test(title) || /^(?:SQ:|FV:)/i.test(title)) fail('Use the full title for a room set. Native IDs and numbered positions belong to one household.');
    return Object.freeze({ item: Object.freeze({ kind: kind + '_query', query: title, container: true }), title, mode });
  }
  function prepareTracks(value, mode) {
    if (!['play', 'next', 'end'].includes(mode)) fail('Choose Play now, Queue next or Add to end.');
    const Q = typeof QueueTracksRemote !== 'undefined' ? QueueTracksRemote : require('./queue-tracks.js');
    return Object.freeze({ item: Object.freeze({ kind: 'tracks', tracks: Q.tracksOf(value), container: true }), mode });
  }
  function reviewGroups(reviewed) {
    return reviewed.entries.map(({ group, before }) => {
      const queue = before.queue;
      const effect = reviewed.mode === 'play' ? `Replace ${queue.total} queue entries and start the first selected track.`
        : reviewed.mode === 'next' && queue.playing !== null ? `Add after queue occurrence ${queue.playing}; ${queue.total} entries currently.`
          : `Append after ${queue.total} entries${reviewed.mode === 'next' ? '; this group is not playing its queue' : ''}.`;
      return `${group.label} · ${group.target}: ${before.label}. ${effect}`;
    });
  }
  const native = item => ['playlist_query', 'favorite_query'].includes(item.kind);
  function nativeItem(value, item, mode, player) {
    const favorite = item.kind === 'favorite_query';
    const list = favorite ? value : player.playlists(value);
    if (!Array.isArray(list) || list.length > 10000 || list.some(v => !v || !text(v.id, 256)
        || !text(v.title, 4096) || (favorite && !['track', 'stream', 'container', 'unplayable'].includes(v.kind)))
        || new Set(list.map(v => v.id)).size !== list.length) fail('A household source catalog is incomplete. Read the sources again.');
    const matches = list.filter(v => nameKey(v.title) === nameKey(item.query));
    if (matches.length !== 1) fail(`The title “${item.query}” must identify exactly one ${favorite ? 'favorite' : 'saved Sonos playlist'} in every selected household.`);
    const selected = matches[0];
    if (selected.kind === 'unplayable') fail(`“${selected.title}” is a browse shortcut and cannot play.`);
    if (mode !== 'play' && selected.kind === 'stream') fail(`“${selected.title}” is a live stream. Choose Play now; streams cannot join a queue.`);
    return Object.freeze({ kind: favorite ? 'favorite' : 'sonos_playlist', id: selected.id, title: selected.title,
      sourceKind: favorite ? selected.kind : 'container', container: !favorite || selected.kind === 'container' });
  }

  async function bounded(items, read, valid) {
    const result = new Array(items.length); let next = 0, failure = null;
    await Promise.all(Array.from({ length: Math.min(4, items.length) }, async () => {
      try {
        while (!failure && next < items.length) {
          valid(); const index = next++; result[index] = await read(items[index]); valid();
        }
      } catch (error) { failure ||= error; }
    }));
    if (failure) throw failure;
    return result;
  }
  async function resolveTarget(read, target) {
    const value = await read('sourceTargetResolve', { query: { target } });
    const view = R.resolutionOf(value, target);
    if (typeof value.revision !== 'string' || !/^[a-f0-9]{64}$/.test(value.revision)) {
      fail('This daemon cannot bind a source command to the reviewed room identities. Update the daemon before using source controls across rooms.');
    }
    return Object.freeze({ ...view, revision: value.revision,
      signature: JSON.stringify([view.signature, value.revision]) });
  }
  const targetGroups = resolution => Object.freeze(resolution.groups.map(g => Object.freeze({
    target: `${g.coordinator_room}@${g.household}`, household: g.household, coordinator: g.coordinator_room,
    coordinator_id: g.coordinator, label: `${g.members.map(r => r.room).join(' + ')} (${g.household})`,
    signature: g.shape, shape: g.shape, members: g.members,
  })));
  function lineInHousehold(groups) {
    if (!groups.length || new Set(groups.map(g => g.household)).size !== 1) {
      fail('A physical line-in can play only within its own household. Choose a room set in one household, or all@household.');
    }
    return groups[0];
  }
  async function lineInCatalog({ call, player, target, signal, valid = () => {} }) {
    target = query(target);
    const read = async (route, options = {}) => {
      valid(); const value = await call(route, { ...options, ...(signal ? { signal } : {}) }); valid(); return value;
    };
    const resolution = await resolveTarget(read, target);
    const group = lineInHousehold(targetGroups(resolution));
    const catalog = player.lineInSources(await read('sourceLineInSources', { query: { zone: group.target } }));
    if ((await resolveTarget(read, target)).signature !== resolution.signature) {
      fail('The resolved room set changed while reading inputs. Read its current line-in sources again.');
    }
    return catalog;
  }

  async function inspect({ call, player, target, item, mode, sourceGroup, signal, valid = () => {} }) {
    target = query(target);
    if (!['play', 'next', 'end'].includes(mode) || !item || ['sonos_playlist_track', 'sonos_playlist'].includes(item.kind)) {
      fail('Choose a source link or a complete household source title for this room set.');
    }
    if (mode !== 'play' && ['stream', 'line_in'].includes(item.kind)) fail('Live sources can play now but cannot join a queue.');
    const read = async (route, options = {}) => {
      valid(); const value = await call(route, { ...options, ...(signal ? { signal } : {}) }); valid(); return value;
    };
    const resolve = () => resolveTarget(read, target);
    const resolution = await resolve(), groups = targetGroups(resolution);
    if (item.kind === 'sonos_playlist_tracks') {
      if (!sourceGroup || groups.some(group => group.household !== sourceGroup.household)) {
        fail('Selected saved tracks can play only in their original household. Choose a room or room set in ' + (sourceGroup?.household || 'that household') + '.');
      }
      const sources = player.groups(await read('zones'));
      if (!sources.some(group => group.target === sourceGroup.target && group.signature === sourceGroup.signature)) {
        fail('The saved playlist’s original group changed. Read its current household and tracks again.');
      }
      await player.readPlaylistTracks({ call: read, item, group: sourceGroup, valid });
    }
    if (item.kind === 'line_in') {
      const group = lineInHousehold(groups);
      player.verifyLineIn(await read('sourceLineInSources', { query: { zone: group.target } }), item);
    }
    const sources = new Map();
    if (native(item)) {
      const homes = [...new Map(groups.map(g => [g.household, g])).values()];
      await bounded(homes, async group => {
        const value = await read(item.kind === 'favorite_query' ? 'sourceFavorites' : 'sourcePlaylists', { query: { zone: group.target } });
        try { sources.set(group.household, nativeItem(value, item, mode, player)); }
        catch (error) { fail(`${group.household}: ${error.message}`); }
      }, valid);
    }
    const entries = Object.freeze(await bounded(groups, async group => {
      const selected = sources.get(group.household) || item;
      const observed = player.playback(await read('state', { params: { room: group.target } }), group);
      let before = observed;
      if (mode !== 'play' || selected.container) {
        const queue = player.queue(await read('sourceQueueState', {
          params: { room: group.target }, query: { offset: 0, limit: 1 },
        }), group);
        before = Object.freeze({ ...observed, queue, signature: JSON.stringify([
          queue.revision, queue.total, ...(mode !== 'end' ? [queue.playing, observed.signature] : []),
        ]) });
      }
      return Object.freeze({ group, item: selected, before,
        sourceLabel: selected.kind === 'sonos_playlist_tracks' ? `${selected.tracks.length} selected track(s) from ${selected.playlistTitle} (${sourceGroup.target})`
          : selected.kind === 'tracks' ? `${selected.tracks.length} ordered track(s)`
          : selected.kind === 'line_in' ? player.lineInLabel(selected)
            : selected.id ? `${selected.title} (${selected.id})` : selected.uri });
    }, valid));
    if ((await resolve()).signature !== resolution.signature) fail('The resolved room set changed during review. Review its current groups again.');
    return Object.freeze({ target, item, mode, resolution, groups, entries, ...(sourceGroup ? { sourceGroup } : {}), signature: JSON.stringify([
      resolution.signature, sourceGroup?.signature, entries.map(e => [e.group.household, e.group.coordinator_id, e.item, e.before.signature]),
    ]) });
  }

  function request(choice, reviewed, player) {
    const target = query(reviewed?.target);
    const revision = reviewed?.resolution?.revision;
    if (typeof revision !== 'string' || !/^[a-f0-9]{64}$/.test(revision)
        || !['play', 'next', 'end'].includes(choice.mode) || reviewed.mode !== choice.mode) {
      fail('Review this source action and its exact room set again before sending it.');
    }
    if (choice.item.kind === 'sonos_playlist_tracks') {
      if (!reviewed.sourceGroup || reviewed.item !== choice.item
          || reviewed.groups.some(group => group.household !== reviewed.sourceGroup.household)) {
        fail('Review the exact saved selection, its original household and its destinations again.');
      }
      return Object.freeze({ route: 'sourcePlaylistTracks', body: Object.freeze({
        ...player.commandBody(choice, reviewed.sourceGroup.target), target, expected_target_revision: revision,
        ...(reviewed.groups.length === 1 ? { expected_queue_revision: reviewed.entries[0].before.queue.revision } : {}),
      }) });
    }
    let source;
    if (choice.item.kind === 'line_in') {
      lineInHousehold(reviewed.groups);
      if (choice.mode !== 'play' || reviewed.item.kind !== 'line_in' || reviewed.item.player !== choice.item.player
          || !reviewed.entries.length || reviewed.entries.some(entry => entry.item.player !== choice.item.player)) {
        fail('Review this exact line-in source and its destination groups again.');
      }
      source = { kind: 'line_in', source: player.commandBody(choice, target).source };
    } else if (choice.item.kind === 'tracks') {
      source = { kind: 'tracks', tracks: prepareTracks(choice.item.tracks, choice.mode).item.tracks };
    } else if (native(choice.item)) {
      const kind = choice.item.kind === 'favorite_query' ? 'favorite' : 'playlist';
      source = { kind, [kind]: choice.item.query };
    } else {
      const body = player.commandBody(choice, target);
      source = { kind: 'uri', source_uri: body.source_uri, ...(body.title != null ? { title: body.title } : {}) };
    }
    return Object.freeze({ route: 'sourceControl', body: Object.freeze({ zone: target,
      expected_revision: revision, mode: choice.mode === 'end' ? 'append' : choice.mode,
      source: Object.freeze(source) }) });
  }

  const groupLabel = group => `${group.coordinator}@${group.household}`;
  const receiptText = value => typeof value === 'string' && value.trim() !== '' && value.length <= 65536
    && !/[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f-\u009f]/u.test(value);
  function playbackReceipt(batch, reviewed, player) {
    const groups = reviewed.groups;
    const bad = () => fail(`The daemon did not return a consistent playback receipt for every reviewed group (${groups.map(groupLabel).join(', ')}). Read their current state before another action; this request was not repeated.`);
    if (!batch || typeof batch.complete !== 'boolean' || !Array.isArray(batch.results)
        || !groups.length || batch.results.length !== groups.length) bad();
    const expected = new Map(groups.map(group => [key(group.household, group.coordinator_id), group])), found = new Map();
    const notes = value => {
      if (value == null) return [];
      if (!Array.isArray(value) || value.length > 1024 || value.some(note => !note || !text(note.code, 256) || !receiptText(note.detail))) bad();
      return value;
    };
    for (const row of batch.results) {
      if (!row || !text(row.household, 4096) || !text(row.target, 256)) bad();
      const id = key(row.household, row.target), group = expected.get(id), result = row.result;
      if (!group || found.has(id) || row.zone !== group.coordinator || !result) bad();
      let status, detail, complete = false;
      if (result.status === 'succeeded') {
        if (!receiptText(result.done) || typeof result.changed !== 'boolean' || result.error != null
            || (result.volume != null && (!Number.isInteger(result.volume) || result.volume < 0 || result.volume > 100))) bad();
        const observedNotes = notes(result.notes);
        complete = !observedNotes.some(note => note.code === 'SKIPPED');
        status = complete ? 'confirmed' : 'incomplete';
        detail = `${complete ? 'Confirmed' : 'Incomplete'}: ${player.outcome(result)}`;
      } else if (result.status === 'failed') {
        const error = result.error;
        if (!error || !text(error.code, 256) || !receiptText(error.detail) || typeof error.retryable !== 'boolean'
            || (error.hint != null && (typeof error.hint !== 'string' || error.hint.length > 65536))
            || error.batch != null) bad();
        status = error.code === 'OUTCOME_UNCERTAIN' ? 'uncertain' : 'failed';
        detail = `${status === 'uncertain' ? 'Outcome uncertain' : 'Failed'}: ${error.code}: ${error.detail}${error.hint ? ' ' + error.hint : ''}`;
      } else if (result.status === 'not_attempted') {
        status = 'not_attempted'; detail = 'Not attempted after an earlier failure.';
      } else bad();
      found.set(id, Object.freeze({ household: group.household, coordinator: group.coordinator_id,
        zone: group.coordinator, status, complete, detail: `${groupLabel(group)}: ${detail}` }));
    }
    const targets = Object.freeze(groups.map(group => found.get(key(group.household, group.coordinator_id))));
    const complete = targets.every(target => target.complete);
    if (batch.complete !== complete) bad();
    return Object.freeze({ targets, complete, partial: !complete, uncertain: targets.some(target => target.status === 'uncertain') });
  }

  function playbackResult(value, reviewed, player) {
    const detail = player.outcome(value);
    if (reviewed.groups.length === 1 && value.batch == null) {
      const partial = (value.notes || []).some(note => note.code === 'SKIPPED');
      return Object.freeze({ status: 'accepted', detail, partial, complete: !partial });
    }
    const receipt = playbackReceipt(value.batch, reviewed, player);
    if (receipt.complete && (value.notes || []).some(note => note.code === 'SKIPPED')) {
      fail('The playback summary and group receipts disagree. Read every reviewed group before another action; this request was not repeated.');
    }
    // Target notes contain their own caps and recovery details. Keep extra
    // action-wide notes (including logging/undo barriers) without doubling them.
    const targetNotes = new Set(value.batch.results.flatMap(row => (row.result.status === 'succeeded' ? row.result.notes || [] : [])
      .map(note => JSON.stringify([note.code, note.detail]))));
    const extra = (value.notes || []).filter(note => note.code !== 'SKIPPED'
      && !targetNotes.has(JSON.stringify([note.code, note.detail]))).map(note => note.detail);
    return Object.freeze({ status: 'accepted', ...receipt,
      detail: [value.done, ...receipt.targets.map(target => target.detail), ...extra,
        ...(!receipt.complete ? ['Read the failed or unattempted groups before another action. Do not repeat the whole batch.'] : [])].join('\n') });
  }

  // An error response can still prove which commands failed or never ran.
  // Malformed receipts remain uncertain; never present a mismatched row as the
  // result of a reviewed group, or turn the error into a successful mutation.
  function failure(error, reviewed, player) {
    if (reviewed.mode !== 'play' || error?.batch == null) return null;
    try {
      const receipt = playbackReceipt(error.batch, reviewed, player);
      if (receipt.complete) fail('The failed request contradicts its playback receipts. Read every reviewed group before another action; this request was not repeated.');
      return Object.freeze({ ...receipt, status: receipt.uncertain ? 'uncertain' : 'failed',
        detail: ['Playback did not complete in every reviewed group.', ...receipt.targets.map(target => target.detail),
          'Read each failed or unattempted group before another action. Do not repeat the whole batch.'].join('\n') });
    } catch (invalid) {
      return Object.freeze({ status: 'uncertain', complete: false, partial: true, uncertain: true, detail: invalid.message });
    }
  }

  function result(value, reviewed, player) {
    if (reviewed.item?.kind === 'sonos_playlist_tracks') {
      const item = reviewed.item, mode = reviewed.mode === 'end' ? 'append' : reviewed.mode;
      const zone = reviewed.groups.length === 1 && (mode === 'play' || value?.queue_batch?.complete === true)
        ? reviewed.groups[0].coordinator : reviewed.target;
      if (!value || value.zone !== zone || value.playlist !== item.id || value.mode !== mode
          || JSON.stringify(value.positions) !== JSON.stringify(item.positions)
          || JSON.stringify(value.titles) !== JSON.stringify(item.tracks.map(track => track.title))) {
        fail('The native selection receipt does not match the reviewed tracks and destinations. Read every destination before another action; this request was not repeated.');
      }
      player.outcome(value);
      if (mode === 'play') {
        if (value.queue_batch != null) fail('The playback receipt contains an unexpected queue addition. Read the destinations before another action.');
      } else {
        if (value.batch != null || !value.queue_batch) fail('The daemon did not return the native selection’s per-group queue receipts. Read the queues before another add.');
        value = value.queue_batch;
      }
    }
    if (reviewed.mode === 'play') return playbackResult(value, reviewed, player);
    const groups = reviewed.groups;
    const bad = () => fail('The daemon did not return one complete queue receipt for every reviewed group. Read their queues before another add; this request was not repeated.');
    const queued = (value, group) => {
      if (['tracks', 'sonos_playlist_tracks'].includes(reviewed.item?.kind)) {
        const before = reviewed.entries.find(entry => entry.group === group)?.before.queue;
        if (!before || value?.count !== reviewed.item.tracks.length || value.first > before.total + 1) bad();
      }
      return player.queued(value, group);
    };
    if (groups.length === 1 && value && !Object.hasOwn(value, 'added') && !Object.hasOwn(value, 'failed')) {
      return Object.freeze({ status: 'accepted', detail: queued(value, groups[0]), partial: false, complete: true });
    }
    if (!value || !text(value.done) || typeof value.complete !== 'boolean' || !Array.isArray(value.added)
        || !Array.isArray(value.failed) || typeof value.logged !== 'boolean' || typeof value.undoable !== 'boolean'
        || value.complete !== (value.failed.length === 0) || (value.undoable && !value.logged)
        || value.added.length + value.failed.length !== groups.length) bad();
    const expected = new Map(groups.map(g => [key(g.household, g.coordinator_id), g])), seen = new Set();
    const claim = row => {
      if (!row || !text(row.household, 4096) || !text(row.coordinator, 256)) bad();
      const id = key(row.household, row.coordinator), group = expected.get(id);
      if (!group || seen.has(id)) bad(); seen.add(id); return group;
    };
    const details = [value.done];
    for (const added of value.added) {
      const group = claim(added);
      details.push(`${group.coordinator}@${group.household}: ${queued(added.queue, group)}`);
    }
    for (const failed of value.failed) {
      const group = claim(failed), error = failed.error;
      if (failed.zone !== group.coordinator || !error || !text(error.code, 256) || !text(error.detail)
          || typeof error.retryable !== 'boolean' || error.retryable || (error.hint != null && !text(error.hint))) bad();
      details.push(`${group.coordinator}@${group.household}: ${error.code}: ${error.detail}${error.hint ? ' ' + error.hint : ''}`);
    }
    if (!value.logged) details.push('The action was not logged; no batch undo is available.');
    else if (!value.undoable) details.push('No safe batch undo is available. Inspect the queues before another add.');
    if (value.failed.length) details.push('Check each failed group before another action. Do not repeat the whole batch.');
    return Object.freeze({ status: 'accepted', detail: details.join('\n'), partial: !value.complete, complete: value.complete,
      logged: value.logged, undoable: value.undoable });
  }
  return { routes, query, nameKey, prepareNative, prepareTracks, reviewGroups, lineInCatalog, inspect, request, result, failure, suggestions: R.suggestionsOf };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = SourceTargetsRemote;
