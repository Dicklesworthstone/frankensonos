// Stored alias/default changes use the daemon's revision and never play music.
'use strict';
const RoomConfiguration = (() => {
  const path = '/rooms/configuration';
  const routes = { roomConfigurationRead: ['GET', path], roomConfigurationWrite: ['POST', path], roomConfigurationRooms: ['GET', '/rooms'] };
  const object = value => value !== null && typeof value === 'object' && !Array.isArray(value);
  const fail = message => { throw new Error(message); };
  const controls = /[\u0000-\u001f\u007f-\u009f]/u;
  // Match Rust str::trim / char::to_lowercase, including non-contextual sigma.
  // JS String#trim additionally removes U+FEFF, which is part of a native name.
  const trim = value => value.replace(/^\p{White_Space}+|\p{White_Space}+$/gu, '');
  const text = (value, max = 128) => typeof value === 'string' && trim(value) !== '' && [...value].length <= max && !controls.test(value);
  const clientName = value => text(value, 512) && new TextEncoder().encode(value).length <= 512;
  const normalized = value => [...trim(value)].map(char => /[\u2018\u2019\u201b]/u.test(char) ? "'" : char.toLowerCase()).join('');
  const reserved = value => ['here', 'all', 'everywhere'].includes(normalized(value.includes('@') ? value.slice(0, value.lastIndexOf('@')) : value));
  const problem = error => `${error?.message || 'The room configuration request failed.'}${error?.hint ? ' ' + error.hint : ''}`;
  const freeze = value => {
    if (value && typeof value === 'object' && !Object.isFrozen(value)) {
      for (const item of Object.values(value)) freeze(item);
      Object.freeze(value);
    }
    return value;
  };
  function known(value, fields, label) {
    if (!object(value) || Object.keys(value).some(key => !fields.includes(key))) fail(`${label} has unreadable or unknown fields. No settings were discarded.`);
  }
  function roomList(value) {
    if (!Array.isArray(value) || !value.length || value.length > 512 || value.some(room => !text(room))) {
      fail('An alias needs 1–512 room names, each 1–128 characters without control characters.');
    }
    return [...value];
  }
  function configurationOf(value) {
    known(value, ['revision', 'aliases', 'default_room', 'client_defaults', 'warnings'], 'The room configuration');
    if (!text(value.revision, 4096) || !Array.isArray(value.aliases) || value.aliases.length > 10000
        || !object(value.client_defaults) || Object.keys(value.client_defaults).length > 10000
        || !Array.isArray(value.warnings) || value.warnings.length > 100000) fail('The room configuration response is unreadable. Refresh before editing.');
    const names = new Set();
    const aliases = value.aliases.map(alias => {
      known(alias, ['name', 'rooms'], 'An alias');
      if (!text(alias.name) || names.has(normalized(alias.name))) fail('The alias list contains an unreadable or ambiguous name.');
      names.add(normalized(alias.name)); return { name: alias.name, rooms: roomList(alias.rooms) };
    });
    if (value.default_room !== null && !text(value.default_room)) fail('The global default room is unreadable.');
    const client_defaults = Object.fromEntries(Object.entries(value.client_defaults).map(([client, room]) => {
      if (!clientName(client) || !text(room)) fail('A client default is unreadable. No client settings were discarded.');
      return [client, room];
    }));
    const warnings = value.warnings.map(warning => {
      known(warning, ['line', 'message'], 'A room configuration warning');
      if (!Number.isSafeInteger(warning.line) || warning.line < 0 || !text(warning.message, 16384)) fail('A room configuration warning is unreadable.');
      return { line: warning.line, message: warning.message };
    });
    return freeze({ revision: value.revision, aliases, default_room: value.default_room, client_defaults, warnings });
  }
  function roomsOf(value) {
    if (!Array.isArray(value) || value.length > 10000) fail('Room suggestions are unreadable. Saved names are still available.');
    const seen = new Set();
    return freeze(value.map(room => {
      if (!object(room) || !text(room.name, 512) || !text(room.household, 512) || !text(room.zone, 512)
          || room.aliases != null && (!Array.isArray(room.aliases) || room.aliases.some(alias => !text(alias)))) {
        fail('Room suggestions are incomplete. Saved names are still available.');
      }
      const target = `${room.name}@${room.household}`;
      if (seen.has(target)) fail('Room suggestions are ambiguous. Saved names are still available.');
      seen.add(target); return { target, label: `${room.name} (${room.household})`, zone: room.zone };
    }));
  }
  const signature = config => JSON.stringify([
    [...config.aliases].sort((a, b) => a.name < b.name ? -1 : a.name > b.name ? 1 : 0).map(alias => [alias.name, alias.rooms]),
    config.default_room, Object.entries(config.client_defaults).sort(([a], [b]) => a < b ? -1 : a > b ? 1 : 0),
  ]);
  const aliasOf = (config, name) => config.aliases.find(alias => normalized(alias.name) === normalized(name));
  function changeOf(draft, base) {
    if (!draft || !base) fail('Read the saved room configuration and choose a change first.');
    if (['set_alias', 'remove_alias'].includes(draft.kind)) {
      if (!text(draft.name) || reserved(draft.name)) fail('Use an alias name of 1–128 characters; here, all and everywhere are reserved.');
      const existing = aliasOf(base, draft.name);
      if (draft.existing_name !== null && existing?.name !== draft.existing_name) fail('This draft no longer identifies the original alias. Open its current saved entry.');
      if (draft.existing_name === null && existing) fail('That alias already exists, including case and apostrophe variants. Open its saved entry to edit it.');
      if (draft.kind === 'set_alias' && draft.room_input !== '') fail('Add the typed room to the alias or clear that field before reviewing.');
      if (draft.kind === 'set_alias' && roomList(draft.rooms).some(reserved)) fail('Alias members must be physical rooms; here, all and everywhere are reserved, including household-qualified forms.');
      return freeze({ kind: draft.kind, name: existing?.name ?? draft.name,
        ...(draft.kind === 'set_alias' ? { rooms: roomList(draft.rooms) } : {}) });
    }
    if (!['set_default', 'clear_default'].includes(draft.kind)) fail('Choose an alias or default-room change.');
    if (draft.client !== null && !clientName(draft.client)) fail('Enter the exact client key (1–512 UTF-8 bytes), or choose the global default.');
    if (draft.kind === 'set_default' && (!text(draft.room) || reserved(draft.room))) {
      fail('Choose a physical default room of 1–128 characters; here, all and everywhere cannot be defaults.');
    }
    return freeze({ kind: draft.kind, ...(draft.client === null ? {} : { client: draft.client }),
      ...(draft.kind === 'set_default' ? { room: draft.room } : {}) });
  }
  function expectedOf(base, change) {
    let aliases = base.aliases, default_room = base.default_room, client_defaults = base.client_defaults;
    if (change.kind === 'set_alias') {
      const existing = aliasOf(base, change.name), replacement = { name: existing?.name ?? change.name, rooms: [...change.rooms] };
      aliases = existing ? aliases.map(alias => alias === existing ? replacement : alias) : [...aliases, replacement];
    } else if (change.kind === 'remove_alias') aliases = aliases.filter(alias => normalized(alias.name) !== normalized(change.name));
    else if (Object.hasOwn(change, 'client')) {
      client_defaults = change.kind === 'set_default' ? { ...client_defaults, [change.client]: change.room }
        : Object.fromEntries(Object.entries(client_defaults).filter(([client]) => client !== change.client));
    } else default_room = change.kind === 'set_default' ? change.room : null;
    return freeze({ ...base, aliases, default_room, client_defaults });
  }
  function description(change, config) {
    if (change.kind === 'set_alias') return `Save alias “${change.name}” with ${change.rooms.length} room${change.rooms.length === 1 ? '' : 's'}: ${change.rooms.join(', ')}.`;
    if (change.kind === 'remove_alias') return `Remove alias “${change.name}”. Future commands and routines that use it may no longer resolve their target.`;
    const who = Object.hasOwn(change, 'client') ? `client “${change.client}”` : 'the house';
    if (change.kind === 'set_default') return `Set the default room for ${who} to “${change.room}”.`;
    return `Clear the default room for ${who}.${Object.hasOwn(change, 'client')
      ? config.default_room === null ? ' This client will have no saved fallback room.' : ` This client will fall back to the house default “${config.default_room}”.`
      : ' Clients with their own defaults keep them; other callers must supply a target.'}`;
  }
  function receiptOf(value, planned) {
    if (!object(value) || !text(value.done, 16384) || typeof value.changed !== 'boolean') fail('The save acknowledgement is unreadable.');
    const configuration = configurationOf(value.configuration);
    const changed = signature(planned.expected) !== signature(planned.base);
    if (signature(configuration) !== signature(planned.expected) || value.changed !== changed
        || (changed ? configuration.revision === planned.base.revision : configuration.revision !== planned.base.revision)) {
      fail('The acknowledgement did not confirm exactly the reviewed alias/default change.');
    }
    return freeze({ done: value.done, changed: value.changed, configuration });
  }
  function observation(configuration, change) {
    if (['set_alias', 'remove_alias'].includes(change.kind)) {
      const alias = aliasOf(configuration, change.name);
      return `Current alias “${change.name}”: ${alias ? alias.rooms.join(', ') : 'not present'}.`;
    }
    const client = Object.hasOwn(change, 'client') ? change.client : null;
    const room = client === null ? configuration.default_room : Object.hasOwn(configuration.client_defaults, client) ? configuration.client_defaults[client] : null;
    return `Current default for ${client === null ? 'the house' : `client “${client}”`}: ${room === null ? 'not set' : room}.`;
  }

  function model({ call, execute, readConfiguration, canEdit = () => true, canRead = () => true, blockedReason = () => '', changed = () => {}, saved = () => {} }) {
    let epoch = 0, disposed = false, active = true, reading = null;
    const state = { open: false, loaded: false, loading: false, writing: false, configuration: null, base: null,
      rooms: [], roomMessage: '', draft: null, pending: null, result: null, recovery: null, stale: false, message: '' };
    const publish = () => { if (!disposed) { try { changed(state); } catch (_) { /* A view failure cannot lose a receipt. */ } } };
    const live = () => !disposed && active && state.open;
    const readable = () => live() && !state.loading && !state.writing && canRead();
    const editable = () => live() && state.loaded && !state.loading && !state.writing && !state.recovery && canEdit();
    const current = version => live() && version === epoch;
    function retire() { ++epoch; reading?.abort(); reading = null; state.loading = false; state.pending = null; }
    async function load({ explicit = true } = {}) {
      if (!readable()) return false;
      retire(); const version = epoch, abort = new AbortController(); reading = abort;
      state.loading = true; state.loaded = false; state.message = 'Reading aliases and default rooms…'; publish();
      const decode = value => { if (!current(version)) fail('This configuration read is no longer current.'); return configurationOf(value); };
      try {
        const [stored, rooms] = await Promise.allSettled([
          readConfiguration ? readConfiguration(decode, { explicit, signal: abort.signal }) : call(path, undefined, { signal: abort.signal }).then(decode),
          call('/rooms', undefined, { signal: abort.signal }).then(roomsOf),
        ]);
        if (!current(version)) return false;
        if (stored.status === 'rejected') throw stored.reason;
        if (stored.value === null) fail('The configuration read was superseded. Refresh it again.');
        state.configuration = stored.value; state.loaded = true;
        state.rooms = rooms.status === 'fulfilled' ? rooms.value : [];
        state.roomMessage = rooms.status === 'fulfilled' ? '' : `${problem(rooms.reason)} You can retain or enter exact physical room names.`;
        state.stale = !!state.draft && (state.base.revision !== stored.value.revision || signature(state.base) !== signature(stored.value));
        if (!state.draft) state.base = stored.value;
        if (explicit && state.recovery) {
          state.result = freeze({ ...state.recovery, status: 'observed',
            detail: `${observation(stored.value, state.recovery.change)} This observation does not prove how the earlier request finished. Review again before any new submission.` });
          state.recovery = null;
        }
        state.message = state.stale ? 'Saved settings changed. Your original draft is retained; discard it or open a new edit from the current list.'
          : state.recovery ? 'Use Refresh room configuration to inspect the earlier request before another change.'
            : !canEdit() ? blockedReason() || 'This view cannot save room configuration right now.' : '';
        return true;
      } catch (error) { if (current(version)) { state.stale = true; state.message = problem(error); } return false; }
      finally { if (reading === abort) { reading = null; state.loading = false; publish(); } }
    }
    function begin(draft) {
      if (!editable()) return false;
      retire(); state.base = state.configuration; state.draft = freeze(draft); state.stale = false; state.message = ''; publish(); return true;
    }
    function beginAlias(name = null, remove = false) {
      if (!editable()) return false;
      const alias = name === null ? null : state.configuration.aliases.find(item => item.name === name);
      if (name !== null && !alias) return false;
      return begin({ kind: remove ? 'remove_alias' : 'set_alias', name: alias?.name ?? '', existing_name: alias?.name ?? null,
        rooms: alias ? [...alias.rooms] : [], room_input: '' });
    }
    function beginDefault(client = null, clear = false) {
      if (!editable() || client !== null && typeof client !== 'string') return false;
      const existing = client === null ? state.configuration.default_room
        : Object.hasOwn(state.configuration.client_defaults, client) ? state.configuration.client_defaults[client] : null;
      if (clear && existing === null) return false;
      return begin({ kind: clear ? 'clear_default' : 'set_default', client, client_locked: client === null || existing !== null, room: existing ?? '' });
    }
    function edit(field, value) {
      if (!editable() || !state.draft || typeof value !== 'string' || value.length > 4096
          || !['name', 'room_input', 'client', 'room'].includes(field) || !Object.hasOwn(state.draft, field)
          || field === 'name' && state.draft.existing_name !== null || field === 'client' && state.draft.client_locked) return false;
      ++epoch; state.pending = null; state.draft = freeze({ ...state.draft, [field]: value }); state.message = ''; publish(); return true;
    }
    function addRoom() {
      if (!editable() || state.draft?.kind !== 'set_alias') return false;
      try {
        if (reserved(state.draft.room_input)) fail('Add a physical room; here, all and everywhere are reserved, including household-qualified forms.');
        const rooms = roomList([...state.draft.rooms, state.draft.room_input]);
        ++epoch; state.pending = null; state.draft = freeze({ ...state.draft, rooms, room_input: '' }); state.message = ''; publish(); return true;
      } catch (error) { state.message = problem(error); publish(); return false; }
    }
    function removeRoom(index) {
      if (!editable() || state.draft?.kind !== 'set_alias' || !Number.isInteger(index) || index < 0 || index >= state.draft.rooms.length) return false;
      ++epoch; state.pending = null; state.draft = freeze({ ...state.draft, rooms: state.draft.rooms.filter((_, at) => at !== index) }); publish(); return true;
    }
    function review() {
      if (!editable() || state.stale || !state.draft) return null;
      state.pending = null;
      try {
        const change = changeOf(state.draft, state.base);
        const expected = expectedOf(state.base, change);
        state.pending = freeze({ base: state.base, expected, body: { expected_revision: state.base.revision, change },
          summary: description(change, expected), prior: observation(state.base, change) }); state.message = '';
      } catch (error) { state.message = problem(error); }
      publish(); return state.pending;
    }
    async function save(planned = state.pending) {
      if (!editable() || state.stale || !planned || planned !== state.pending) return false;
      const version = ++epoch, abort = new AbortController(); reading = abort;
      state.pending = null; state.writing = true; state.message = 'Checking and saving the reviewed room configuration…'; publish();
      let started = false, sent = false, receipt = null, failure = null;
      const operation = async ({ check = () => {}, send = call } = {}) => {
        if (started) fail('This reviewed room configuration change already claimed its one command.');
        started = true;
        try {
          const valid = () => { check(); if (!current(version)) fail('The view changed before saving. Your draft is retained.'); };
          valid();
          const fresh = configurationOf(await call(path, undefined, { signal: abort.signal })); valid();
          if (fresh.revision !== planned.base.revision || signature(fresh) !== signature(planned.base)) {
            fail('The saved configuration changed after review. Refresh room configuration and open a new edit.');
          }
          valid(); sent = true;
          receipt = receiptOf(await send(path, planned.body), planned);
          return { done: receipt.done, changed: receipt.changed };
        } catch (error) { failure = error; throw error; }
      };
      try {
        const accepted = execute ? await execute(operation) : !!await operation();
        if (!accepted || !receipt) throw failure || new Error('The command lane is busy. No room configuration was changed.');
        state.configuration = receipt.configuration; state.base = receipt.configuration;
        state.result = freeze({ status: 'accepted', change: planned.body.change, detail: `${receipt.done} ${receipt.changed ? 'Future commands and routines use these saved targets.' : 'The saved configuration was already identical.'} No music starts now.`, receipt });
        state.draft = null; state.stale = false; state.recovery = null; state.message = receipt.done;
        try { saved(receipt.configuration); } catch (_) { /* The verified receipt stays authoritative. */ }
        return true;
      } catch (error) {
        const conflict = error?.code === 'CONFIG_CHANGED' || error?.status === 409;
        state.stale = true;
        state.result = freeze({ status: sent ? conflict ? 'conflict' : 'uncertain' : 'not_sent', change: planned.body.change,
          detail: `${problem(error)} ${sent ? conflict ? 'The house rejected this stale review. Refresh room configuration before another edit.'
            : 'The change may have been saved. Use Refresh room configuration to inspect it; this request was not repeated.'
            : 'No configuration write was sent.'} Your draft is retained.` });
        if (sent) state.recovery = state.result;
        state.message = state.result.detail; return false;
      } finally { if (reading === abort) reading = null; state.writing = false; state.loaded = false; publish(); }
    }
    function close() { retire(); state.open = false; state.loaded = false; publish(); }
    return { state, readable, editable, load, beginAlias, beginDefault, edit, addRoom, removeRoom, review, save,
      open() { if (disposed || !active || state.writing) return false; state.open = true; publish(); void load({ explicit: false }); return true; },
      cancelReview() { if (!state.writing) { ++epoch; state.pending = null; publish(); } },
      discard() { if (!live() || state.writing) return false; retire(); state.draft = null; state.base = state.configuration; state.stale = false; state.message = ''; publish(); return true; },
      observe() { if (!state.writing && (!live() || !canEdit())) state.pending = null; publish(); },
      close, suspend() { active = false; close(); }, resume() { if (!disposed) { active = true; publish(); } },
      dispose() { active = false; close(); disposed = true; },
    };
  }
  return Object.freeze({ path, routes, normalized, configurationOf, roomsOf, signature, changeOf, expectedOf, description, receiptOf, model });
})();
if (typeof module !== 'undefined' && module.exports) module.exports = RoomConfiguration;
