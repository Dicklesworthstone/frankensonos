// One reviewed DJ command for the daemon's existing, independent room groups.
'use strict';
const DjTargetsRemote = (() => {
  const R = typeof RoomSetsRemote !== 'undefined' ? RoomSetsRemote : require('./room-sets.js');
  const D = typeof DjDirection !== 'undefined' ? DjDirection : require('./dj-direction.js');
  const S = typeof DjSteeringRemote !== 'undefined' ? DjSteeringRemote : require('./dj-steering.js');
  const P = typeof SourcePlayerRemote !== 'undefined' ? SourcePlayerRemote : require('./source-player.js');
  const routes = Object.freeze({ djTargetsRooms: ['GET', '/rooms'], djTargetsResolve: ['GET', '/rooms/resolve'],
    djTargetsState: ['GET', '/zones/{room}/state'], djTargetsStatus: ['GET', '/zones/{room}/dj'],
    djTargetsQueue: ['GET', '/zones/{room}/queue'], djTargetsMoods: ['GET', '/dj/moods'],
    djTargetsControl: ['POST', '/dj/control'] });
  const actions = Object.freeze({ start: 'Start DJs', steer: 'Save DJ direction', clear: 'Return to programs',
    skip: 'Skip current whole works', stop: 'Stop DJs' });
  const MAX_AGE = 45000;
  const fail = message => { throw new Error(message); };
  const problem = error => [error?.message || 'The DJ request failed.', error?.hint].filter(Boolean).join(' ');
  const playerKey = room => JSON.stringify([room.household, room.player]);
  const mutablePlayback = action => ['start', 'skip', 'stop'].includes(action);
  function resolution(value, target) {
    const view = R.resolutionOf(value, target);
    if (typeof value.revision !== 'string' || !/^[a-f0-9]{64}$/.test(value.revision)) {
      fail('This daemon cannot bind a DJ command to the reviewed room identities. Update the daemon before using DJ across rooms.');
    }
    return Object.freeze({ ...view, revision: value.revision });
  }
  function groupsOf(view) {
    return Object.freeze(view.groups.map(group => Object.freeze({ ...group,
      target: `${group.coordinator_room}@${group.household}`, room: group.coordinator_room,
      // Queue and playback readers use the name; the resolver retains the UUID separately.
      coordinator_id: group.coordinator, coordinator: group.coordinator_room,
      signature: group.shape, label: `${group.members.map(room => room.room).join(' + ')} (${group.household})` })));
  }
  function moodsOf(value) {
    S.moodsOf(value);
    return Object.freeze(value.moods.map(mood => Object.freeze({ name: mood.name,
      constraints: D.constraintsOf(mood.constraints) })));
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
    return Object.freeze(result);
  }
  function request(state) {
    const target = state.input.trim();
    if (!target || [...target].length > 128 || /[\p{Cc}]/u.test(target)) fail('Enter a room, saved alias, all@household, or everywhere (at most 128 characters).');
    if (!Object.hasOwn(actions, state.action)) fail('Choose a DJ action.');
    const body = { zone: target, action: state.action === 'clear' ? 'steer' : state.action };
    if (state.action === 'clear') body.clear = true;
    else if (state.action === 'steer' || state.action === 'start' && state.directionMode === 'replace') {
      const mood = state.mood || null, direction = D.fromDraft(state.direction, mood);
      if (!mood && !Object.keys(direction.constraints).length) fail('Choose a mood or filter, or use Return to programs to clear temporary steering.');
      if (mood) body.mood = mood;
      body.constraints = direction.constraints;
      if (direction.for_secs != null) body.for_secs = direction.for_secs;
    }
    return Object.freeze(body);
  }
  const workIdentity = work => work ? [work.composer, work.title, work.album, work.performers, work.movements] : null;
  function entrySignature(entry, action) {
    return [entry.group.household, entry.group.coordinator_id, entry.status.running, S.signatureOf(entry.status.steering),
      ...(mutablePlayback(action) ? [entry.playback.signature] : []),
      ...(action === 'skip' ? [workIdentity(entry.status.now), entry.status.next.map(workIdentity)] : []),
      ...(entry.queue ? [entry.queue.revision, entry.queue.total, action === 'skip' ? entry.queue.playing : null] : [])];
  }
  async function inspect({ call, body, action, signal, valid = () => {} }) {
    const read = async (name, args = {}) => {
      valid(); const value = await call(name, { ...args, ...(signal ? { signal } : {}) }); valid(); return value;
    };
    const resolve = async () => resolution(await read('djTargetsResolve', { query: { target: body.zone } }), body.zone);
    const view = await resolve(), groups = groupsOf(view);
    let mood = null;
    if (body.mood) {
      mood = moodsOf(await read('djTargetsMoods')).find(item => item.name === body.mood);
      if (!mood) fail('The chosen mood is no longer available. Refresh moods and review again.');
    }
    const entries = await bounded(groups, async group => {
      try {
        const playback = P.playback(await read('djTargetsState', { params: { room: group.target } }), group);
        const status = S.statusOf(await read('djTargetsStatus', { params: { room: group.target } }), group);
        const queue = ['start', 'skip'].includes(action) ? P.queue(await read('djTargetsQueue', {
          params: { room: group.target }, query: { offset: 0, limit: 1 } }), group) : null;
        return Object.freeze({ group, playback, status, queue });
      } catch (error) { fail(`${group.target}: ${problem(error)}`); }
    }, valid);
    const after = await resolve();
    if (after.signature !== view.signature || after.revision !== view.revision) fail('The resolved room set changed during review. Read its current groups again.');
    return Object.freeze({ view, groups, entries, mood, signature: JSON.stringify([
      view.signature, view.revision, mood, entries.map(entry => entrySignature(entry, action)),
    ]) });
  }
  function effect(action, body) {
    if (action === 'clear') return 'Clear temporary steering in every listed group. Each returns to its own automatic program; this does not start playback.';
    if (action === 'steer') return 'Replace each group’s temporary direction for future DJ selections. Already queued works continue; idle DJs stay idle.';
    if (action === 'skip') return 'Advance each running DJ past its current whole work, including its remaining movements. Idle groups are reported separately.';
    if (action === 'stop') return 'Stop the DJ’s selected work where it still owns playback and release each feed. Queued music is retained; a different source already playing is left alone. Idle groups are reported separately.';
    return 'Start an independent DJ in each listed group, preserving retained queued works. This can change its current source. Already running feeds remain active. '
      + (body.constraints || body.mood ? 'The same reviewed direction applies to every selected group.' : 'Each group keeps its current direction or automatic program.');
  }
  function directionText(status) {
    const steering = status.steering;
    const filters = Object.entries(steering.constraints).map(([key, value]) => D.lists[key]
      ? `${D.lists[key]}: ${value.join('; ')}` : key === 'strict' ? 'Strict filters; new picks wait if none match'
        : key === 'allow_long_works' ? 'Long works allowed' : key === 'energy_bias' ? `Energy shift ${value > 0 ? '+' : ''}${value}`
          : `${key === 'min_work_minutes' ? 'Minimum' : 'Maximum'} work length ${value} minutes`).join(' · ');
    const expiry = steering.source === 'session' ? steering.expires_at === null ? ' · until cleared'
      : ` · deadline ${new Date(steering.expires_at * 1000).toLocaleString()}` : '';
    return `${steering.source === 'session' ? 'Temporary session' : steering.source === 'program' ? 'Automatic program'
      : steering.source === 'unavailable' ? 'Direction unavailable' : 'No direction'} · ${steering.mood || 'Time-of-day mood'}${filters ? ' · ' + filters : ''}${expiry}`;
  }

  function model({ call, execute, changed = () => {}, available = () => true, readable = () => true,
    completed = () => {}, recovered = () => true, now = Date.now }) {
    let epoch = 0, disposed = false, reader = null, dispatched = false;
    const state = { open: false, input: '', action: 'start', directionMode: 'keep', mood: '', direction: D.draft(),
      suggestions: [], moods: [], reading: false, writing: false, pending: null, result: null, recovery: null,
      observation: null, message: '' };
    const publish = () => { if (!disposed) { try { changed(state); } catch (_) { /* Presentation only. */ } } };
    const live = () => !disposed && state.open;
    const busy = () => state.reading || state.writing;
    const retire = () => { ++epoch; reader?.abort(); reader = null; state.reading = false; state.pending = null; };
    const validFor = (version, abort) => {
      if (!live() || version !== epoch || abort?.signal.aborted) fail('This DJ review closed or changed. Read the target and review again.');
    };
    const canReview = () => live() && !busy() && !state.recovery && available();
    function edit(field, value) {
      if (!live() || state.writing || !['input', 'action', 'directionMode', 'mood'].includes(field) || typeof value !== 'string'
          || value.length > 1024 || field === 'action' && !Object.hasOwn(actions, value)
          || field === 'directionMode' && !['keep', 'replace'].includes(value)) return false;
      retire(); state[field] = value; state.message = ''; publish(); return true;
    }
    function editDirection(field, value) {
      if (!live() || state.writing) return false;
      try { const next = D.editDraft(state.direction, field, value); retire(); state.direction = next; state.message = ''; publish(); return true; }
      catch (error) { state.message = problem(error); publish(); return false; }
    }
    async function refresh() {
      if (!live() || busy() || !readable()) return false;
      retire(); const version = epoch, abort = new AbortController(); reader = abort; state.reading = true;
      state.message = 'Reading room suggestions and available moods…'; publish();
      const answers = await Promise.allSettled([call('djTargetsRooms', { signal: abort.signal }), call('djTargetsMoods', { signal: abort.signal })]);
      try {
        validFor(version, abort); const errors = [];
        try { if (answers[0].status === 'rejected') throw answers[0].reason; state.suggestions = R.suggestionsOf(answers[0].value); }
        catch (error) { errors.push(problem(error)); }
        try { if (answers[1].status === 'rejected') throw answers[1].reason; state.moods = moodsOf(answers[1].value); }
        catch (error) { errors.push(problem(error)); }
        state.message = errors.join(' '); return errors.length === 0;
      } catch (_) { return false; }
      finally { if (reader === abort) { reader = null; state.reading = false; publish(); } }
    }
    async function review() {
      if (!canReview()) return null;
      retire(); const version = epoch, abort = new AbortController(); reader = abort; state.reading = true;
      state.message = 'Reading every affected group and its DJ…'; publish();
      try {
        const body = request(state), action = state.action, started = now();
        const valid = () => { validFor(version, abort); if (!available() || now() - started >= MAX_AGE) fail('Refresh and review the DJ target again.'); };
        const before = await inspect({ call, body, action, signal: abort.signal, valid }); valid();
        state.pending = Object.freeze({ body: Object.freeze({ ...body, expected_revision: before.view.revision }),
          action, before, at: started, version });
        state.message = ''; return state.pending;
      } catch (error) { if (version === epoch && live()) state.message = problem(error); return null; }
      finally { if (reader === abort) { reader = null; state.reading = false; publish(); } }
    }
    const canConfirm = pending => !!(canReview() && pending && pending === state.pending
      && pending.version === epoch && now() - pending.at < MAX_AGE);
    async function confirm(pending = state.pending) {
      if (!canConfirm(pending)) return false;
      const version = epoch, abort = new AbortController(); reader = abort; state.pending = null; state.writing = true; dispatched = false;
      state.observation = null; state.message = 'Checking the same groups, current sources and DJ sessions…'; publish();
      let receipt = null, caught = null;
      try {
        const work = async ({ check = () => {}, send = (name, body) => call(name, { body }) } = {}) => {
          try {
            const valid = () => {
              check(); validFor(version, abort);
              if (now() - pending.at >= MAX_AGE) fail('This DJ review expired. Read the current groups and review again.');
            };
            valid(); const before = await inspect({ call, body: pending.body, action: pending.action, signal: abort.signal, valid }); valid();
            if (before.signature !== pending.before.signature) fail('The rooms, current source, DJ session, queue or mood changed. Nothing was sent; review the current groups again.');
            reader = null; // A dispatched write is never cancelled or repeated by a view change.
            if (dispatched) fail('This DJ review already dispatched its command.');
            dispatched = true;
            const value = await send('djTargetsControl', pending.body);
            // Omitted notes mean none; an explicit malformed field cannot prove
            // that every selected group completed its command.
            if (value && Object.hasOwn(value, 'notes') && !Array.isArray(value.notes)) {
              fail('The daemon did not return a complete DJ command receipt.');
            }
            const result = R.outcomeOf(value);
            receipt = Object.freeze({ ...result, done: value.done, target: pending.body.zone, groups: pending.before.groups,
              action: pending.action, detail: result.detail + (result.partial ? ' Read affected rooms before another action. Do not repeat the whole batch.' : '') });
            return value;
          } catch (error) { caught = error; throw error; }
        };
        if (execute) await execute(pending.before.groups, work); else await work();
        if (!receipt) throw caught || new Error('The DJ command lane was unavailable. Refresh and review again.');
        state.result = receipt;
        state.recovery = receipt.partial ? receipt : null;
        state.message = '';
        return true;
      } catch (error) {
        state.result = Object.freeze({ status: dispatched ? 'uncertain' : 'not_sent', target: pending.body.zone,
          action: pending.action, groups: pending.before.groups, notes: [], partial: false,
          detail: problem(caught || error) + (dispatched
            ? ' The result is uncertain. Read affected rooms before any new action. This request was not repeated.' : ' No DJ command was sent.') });
        if (dispatched) state.recovery = state.result;
        state.message = ''; return false;
      } finally {
        const sent = dispatched; dispatched = false; state.writing = false; retire(); publish();
        if (sent) { try { await completed(pending.before.groups); } catch (_) { /* Preserve the original receipt. */ } }
      }
    }
    async function observeResult() {
      const recovery = state.recovery;
      if (!live() || busy() || !readable() || !recovery) return false;
      retire(); const version = epoch, abort = new AbortController(); reader = abort; state.reading = true;
      state.message = 'Finding the originally affected players and reading their current groups…'; publish();
      const valid = () => validFor(version, abort);
      const read = async (name, args = {}) => { valid(); const value = await call(name, { ...args, signal: abort.signal }); valid(); return value; };
      try {
        // The alias may now name different rooms. Follow the original physical
        // players through today's topology instead of inspecting that alias again.
        const view = resolution(await read('djTargetsResolve', { query: { target: 'everywhere' } }), 'everywhere');
        const original = new Map(recovery.groups.flatMap(group => group.members.map(room => [playerKey(room), room])));
        const groups = groupsOf(view).filter(group => group.members.some(room => original.has(playerKey(room))));
        const present = new Set(groups.flatMap(group => group.members.map(playerKey)));
        const missing = Object.freeze([...original].filter(([key]) => !present.has(key)).map(([, room]) => R.roomName(room)));
        const entries = await bounded(groups, async group => {
          try {
            const playback = P.playback(await read('djTargetsState', { params: { room: group.target } }), group);
            const status = S.statusOf(await read('djTargetsStatus', { params: { room: group.target } }), group);
            return Object.freeze({ group, playback, status });
          } catch (error) { return Object.freeze({ group, error: problem(error) }); }
        }, valid);
        const after = resolution(await read('djTargetsResolve', { query: { target: 'everywhere' } }), 'everywhere');
        if (after.signature !== view.signature || after.revision !== view.revision) fail('The house regrouped during observation. Read affected rooms again.');
        state.observation = Object.freeze({ entries, missing });
        if (entries.some(entry => entry.error)) fail('Some affected groups could not be read. Their errors are shown below; read them again before a new review.');
        if (await recovered() === false) fail('The dashboard could not refresh its current playback. Refresh it and read affected rooms again.');
        valid(); state.recovery = null;
        state.result = Object.freeze({ ...recovery, status: 'observed' });
        state.message = 'Current affected rooms observed. This does not prove how the earlier request finished or prevent it finishing later. Review any new action separately.';
        return true;
      } catch (error) { if (version === epoch && live()) state.message = problem(error); return false; }
      finally { if (reader === abort) { reader = null; state.reading = false; publish(); } }
    }
    function invalidate(group = null, reason = 'The affected rooms changed. Review their current state again.') {
      if (disposed || !live() || state.writing && dispatched) return;
      const entries = state.pending?.before.entries;
      if (group && entries && !entries.some(entry => entry.group.household === group.household
          && entry.group.members.some(room => group.members?.includes(room.room)))) return;
      if (state.pending || state.reading || state.writing) { retire(); state.message = reason; publish(); }
    }
    function observePlayback(value) {
      const pending = state.pending;
      if (!pending || !mutablePlayback(pending.action)) return;
      const entry = pending.before.entries.find(row => row.group.household === value?.zone?.household
        && row.group.members.some(room => value.zone.members?.includes(room.room)));
      if (!entry) return;
      try { if (P.playback(value, entry.group).signature === entry.playback.signature) return; } catch (_) { /* Invalidates below. */ }
      invalidate(value.zone, 'Playback changed in an affected group. Review its current source and DJ again.');
    }
    return { state, busy, canReview, canConfirm, edit, editDirection, refresh, review, confirm, observeResult, invalidate, observePlayback,
      open(target = '') { if (disposed) return false; state.open = true; if (!state.input && target) state.input = target; publish(); return true; },
      close() { state.open = false; retire(); publish(); },
      cancel() { if (!state.writing) { retire(); state.message = ''; publish(); } },
      notify: publish,
      dispose() { state.open = false; retire(); disposed = true; },
    };
  }

  function dashboard({ call, controller, demo = false, changed, completed, now }) {
    const namedCall = (name, { params = {}, query, signal } = {}) => {
      const route = routes[name];
      if (!route || route[0] !== 'GET') fail('The dashboard read route is unavailable.');
      const path = route[1].replace(/\{(\w+)\}/g, (_, key) => encodeURIComponent(params[key]));
      return call(path + (query ? '?' + new URLSearchParams(query) : ''), undefined, { signal });
    };
    const control = model({ call: namedCall, changed, now, completed,
      available: () => !demo && controller.canControl() && controller.state.topologyFresh,
      readable: () => controller.state.active && !controller.state.busy,
      recovered: () => controller.refresh(true),
      execute: async (_, work) => {
        const ok = await controller.mutate(({ check, send }) => work({ check,
          send: (name, body) => send(routes[name][1], body) }), { wholeHouse: true });
        if (!ok) fail(controller.state.message || 'Refresh playback before reviewing a DJ command.');
      },
    });
    return { ...control,
      observe() {
        const pending = control.state.pending;
        if (pending && (!controller.state.active || !controller.state.topologyFresh || controller.state.busy)) control.invalidate();
        else if (pending) for (const entry of pending.before.entries) {
          const item = controller.state.items.find(item => item.zone.household === entry.group.household
            && item.zone.coordinator_room === entry.group.room);
          if (!item?.fresh || JSON.stringify([item.zone.household, item.zone.coordinator_room, [...item.zone.members].sort()]) !== entry.group.shape) {
            control.invalidate(); break;
          }
          control.observePlayback(item.state);
        }
        control.notify();
      },
    };
  }
  return { routes, actions, MAX_AGE, request, inspect, resolution, groupsOf, moodsOf, effect, directionText, model, dashboard };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = DjTargetsRemote;
