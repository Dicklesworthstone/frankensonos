// Reviewed, revision-bound house policy changes. Authority stays on the daemon.
'use strict';
const PolicyConfiguration = (() => {
  const path = '/policy/configuration';
  const routes = Object.freeze({ policyConfigurationRead: ['GET', path], policyConfigurationWrite: ['POST', path] });
  const object = value => value !== null && typeof value === 'object' && !Array.isArray(value);
  const fail = message => { throw new Error(message); };
  const controls = /[\u0000-\u001f\u007f-\u009f]/u;
  const trim = value => value.replace(/^\p{White_Space}+|\p{White_Space}+$/gu, '');
  // Client keys follow Rust str::to_lowercase (including final sigma).
  const lower = value => value.toLowerCase();
  // Room matching lowercases each character independently.
  const normalizedRoom = value => [...trim(value)].map(char => char.toLowerCase()).join('').replace(/[\u2018\u2019\u201b]/gu, "'");
  const text = (value, max = 512) => typeof value === 'string' && trim(value) !== ''
    && new TextEncoder().encode(value).length <= max && !controls.test(value);
  const number = (value, max) => Number.isSafeInteger(value) && value >= 0 && value <= max;
  const optional = (value, validate) => value == null || validate(value);
  const problem = error => [error?.message || 'The house policy request failed.', error?.hint].filter(v => typeof v === 'string').join(' ');
  const freeze = value => {
    if (value && typeof value === 'object' && !Object.isFrozen(value)) { Object.values(value).forEach(freeze); Object.freeze(value); }
    return value;
  };
  function known(value, fields, label) {
    if (!object(value) || Object.keys(value).some(key => !fields.includes(key))) fail(`${label} contains unreadable or unknown fields. Read the current policy before editing.`);
  }
  function rules(value) {
    if (!Array.isArray(value) || value.length > 512 || value.some(item => !text(item, 128))) fail('Tool rules must be a bounded list of nonblank tool or class names.');
    return [...value];
  }
  function defaultsOf(value) {
    known(value, ['max_volume', 'max_step', 'fade_secs'], 'The default limits');
    if (!number(value.max_volume, 100) || !number(value.max_step, 100) || !number(value.fade_secs, 4294967295)) fail('Default limits are unreadable.');
    return { max_volume: value.max_volume, max_step: value.max_step, fade_secs: value.fade_secs };
  }
  function quietOf(value) {
    if (value == null) return null;
    known(value, ['start', 'end', 'max_volume'], 'Quiet hours');
    if (![value.start, value.end].every(time => typeof time === 'string' && /^(?:[01]\d|2[0-3]):[0-5]\d$/u.test(time))
        || !number(value.max_volume, 100)) fail('Quiet hours need two house-clock times and a maximum volume of 0–100.');
    return { start: value.start, end: value.end, max_volume: value.max_volume };
  }
  function roomOf(value) {
    known(value, ['room', 'max_volume', 'max_step'], 'A room limit');
    if (!text(value.room) || [...value.room].length > 128 || !optional(value.max_volume, v => number(v, 100))
        || !optional(value.max_step, v => number(v, 100))) fail('A room limit is unreadable.');
    return { room: value.room, max_volume: value.max_volume ?? null, max_step: value.max_step ?? null };
  }
  function clientOf(value) {
    known(value, ['client', 'allow', 'deny', 'grant', 'capped', 'manage_others', 'mcp_profile'], 'A client rule');
    if (!text(value.client) || !optional(value.capped, v => typeof v === 'boolean')
        || !optional(value.manage_others, v => typeof v === 'boolean')
        || !optional(value.mcp_profile, v => ['essentials', 'full'].includes(v))) fail('A client rule is unreadable.');
    return { client: value.client, allow: value.allow == null ? null : rules(value.allow), deny: rules(value.deny),
      grant: rules(value.grant ?? []), capped: value.capped ?? null, manage_others: value.manage_others ?? false,
      mcp_profile: value.mcp_profile ?? null };
  }
  function configurationOf(value) {
    known(value, ['revision', 'defaults', 'quiet_hours', 'rooms', 'clients'], 'The house policy');
    if (!text(value.revision, 4096) || !Array.isArray(value.rooms) || value.rooms.length > 10000
        || !Array.isArray(value.clients) || value.clients.length > 10000) fail('The policy configuration response is incomplete.');
    const rooms = value.rooms.map(roomOf), clients = value.clients.map(clientOf);
    if (new Set(rooms.map(row => normalizedRoom(row.room))).size !== rooms.length
        || new Set(clients.map(row => lower(row.client))).size !== clients.length) fail('The policy contains ambiguous room or client identities.');
    return freeze({ revision: value.revision, defaults: defaultsOf(value.defaults), quiet_hours: quietOf(value.quiet_hours), rooms, clients });
  }
  const sorted = (rows, key) => [...rows].sort((a, b) => a[key] < b[key] ? -1 : a[key] > b[key] ? 1 : 0);
  const signature = config => JSON.stringify([config.defaults, config.quiet_hours, sorted(config.rooms, 'room'), sorted(config.clients, 'client')]);
  const roomIn = (base, name) => base.rooms.find(row => normalizedRoom(row.room) === normalizedRoom(name));
  const clientIn = (base, name) => base.clients.find(row => lower(row.client) === lower(name));
  function uintInput(value, label, max = 100, inherit = false) {
    if (inherit && value === '') return null;
    if (typeof value !== 'string' || !/^\d+$/u.test(value) || !number(Number(value), max)) fail(`${label} must be a whole number from 0 to ${max}${inherit ? ', or blank to inherit' : ''}.`);
    return Number(value);
  }
  function ruleInput(value) {
    if (typeof value !== 'string') fail('Enter tool or class names, one per line.');
    const entries = rules(value === '' ? [] : value.split(/\r?\n/u).filter(line => line !== ''));
    if (entries.length > 256 || entries.some(entry => !/^[a-zA-Z0-9_-]{1,128}$/u.test(entry))) {
      fail('Use at most 256 tool or class names per list, with letters, digits, underscores, or hyphens.');
    }
    return entries;
  }
  function changeOf(draft, base) {
    if (!draft || !base) fail('Read the house policy and choose a rule first.');
    switch (draft.kind) {
      case 'set_defaults': return freeze({ kind: draft.kind, max_volume: uintInput(draft.max_volume, 'Maximum volume'),
        max_step: uintInput(draft.max_step, 'Largest volume increase'), fade_secs: uintInput(draft.fade_secs, 'Scheduled ramp seconds', 4294967295) });
      case 'set_quiet_hours': {
        const quiet = quietOf({ start: draft.start, end: draft.end, max_volume: uintInput(draft.max_volume, 'Quiet-hours maximum volume') });
        if (quiet.start === quiet.end) fail('Choose different quiet-hours start and end times, or remove the quiet-hours rule.');
        return freeze({ kind: draft.kind, ...quiet });
      }
      case 'clear_quiet_hours': return freeze({ kind: draft.kind });
      case 'set_room': case 'remove_room': {
        if (!text(draft.room) || [...draft.room].length > 128) fail('Enter a physical room name of 1–128 characters.');
        const existing = roomIn(base, draft.room);
        if (draft.existing !== null && existing?.room !== draft.existing || draft.existing === null && existing) fail('That room rule already exists or its identity changed. Open the saved entry.');
        return freeze({ kind: draft.kind, room: existing?.room ?? draft.room, ...(draft.kind === 'set_room' ? {
          max_volume: uintInput(draft.max_volume, 'Room maximum volume', 100, true), max_step: uintInput(draft.max_step, 'Room largest increase', 100, true),
        } : {}) });
      }
      case 'set_client': case 'remove_client': {
        if (!text(draft.client)) fail('Enter the exact client key, up to 512 UTF-8 bytes.');
        const existing = clientIn(base, draft.client);
        if (draft.existing !== null && existing?.client !== draft.existing || draft.existing === null && existing) fail('That client rule already exists or its identity changed. Open the saved entry.');
        const client = existing?.client ?? draft.client;
        if (draft.kind === 'remove_client') return freeze({ kind: draft.kind, client });
        if (!['inherit', 'only'].includes(draft.allow_mode) || !['inherit', 'capped', 'uncapped'].includes(draft.capped)
            || !['inherit', 'essentials', 'full'].includes(draft.mcp_profile) || typeof draft.manage_others !== 'boolean') fail('Choose valid client rule options.');
        return freeze({ kind: draft.kind, client, allow: draft.allow_mode === 'inherit' ? null : ruleInput(draft.allow),
          deny: ruleInput(draft.deny), grant: ruleInput(draft.grant), capped: draft.capped === 'inherit' ? null : draft.capped === 'capped',
          manage_others: draft.manage_others, mcp_profile: draft.mcp_profile === 'inherit' ? null : draft.mcp_profile });
      }
      default: fail('Choose a house policy rule to edit.');
    }
  }
  function expectedOf(base, change) {
    const { kind, ...value } = change;
    let defaults = base.defaults, quiet_hours = base.quiet_hours, rooms = base.rooms, clients = base.clients;
    if (kind === 'set_defaults') defaults = defaultsOf(value);
    else if (kind === 'set_quiet_hours') quiet_hours = quietOf(value);
    else if (kind === 'clear_quiet_hours') quiet_hours = null;
    else if (kind === 'set_room' || kind === 'remove_room') {
      const existing = roomIn(base, value.room); rooms = rooms.filter(row => row !== existing);
      if (kind === 'set_room') rooms = [...rooms, roomOf({ ...value, room: existing?.room ?? value.room })];
    } else {
      const existing = clientIn(base, value.client); clients = clients.filter(row => row !== existing);
      if (kind === 'set_client') clients = [...clients, clientOf({ ...value, client: lower(existing?.client ?? value.client) })];
    }
    return freeze({ ...base, defaults, quiet_hours, rooms, clients });
  }
  function observation(config, change) {
    switch (change.kind) {
      case 'set_defaults': return `Defaults: maximum ${config.defaults.max_volume}, largest increase ${config.defaults.max_step}, scheduled ramp ${config.defaults.fade_secs} seconds.`;
      case 'set_quiet_hours': case 'clear_quiet_hours': return config.quiet_hours
        ? `Quiet hours: ${config.quiet_hours.start}–${config.quiet_hours.end}, maximum ${config.quiet_hours.max_volume} on the house clock.` : 'Quiet hours: not configured.';
      case 'set_room': case 'remove_room': {
        const row = roomIn(config, change.room); return `Room “${change.room}”: ${row ? `maximum ${row.max_volume ?? 'inherited'}, largest increase ${row.max_step ?? 'inherited'}` : 'no individual limits'}.`;
      }
      default: {
        const row = clientIn(config, change.client);
        if (!row) return `Client “${change.client}”: no individual rules; base policy applies.`;
        return `Client “${row.client}”: ${row.allow === null ? 'base tool access' : row.allow.length ? `allow only ${row.allow.join(', ')}` : 'explicit empty allow list'}; deny ${row.deny.join(', ') || 'none'}; grant ${row.grant.join(', ') || 'none'}; caps ${row.capped === null ? 'inherited' : row.capped ? 'enabled' : 'disabled'}; administration ${row.manage_others ? 'enabled' : 'disabled'}; MCP tool list ${row.mcp_profile ?? 'inherited'}.`;
      }
    }
  }
  function receiptOf(value, planned) {
    if (!object(value) || !text(value.done, 16384) || typeof value.changed !== 'boolean') fail('The save acknowledgement is incomplete.');
    const configuration = configurationOf(value.configuration);
    if (signature(configuration) !== signature(planned.expected)
        || value.changed !== (configuration.revision !== planned.base.revision)) fail('The acknowledgement did not confirm the complete reviewed policy change.');
    return freeze({ done: value.done, changed: value.changed, configuration });
  }

  function model({ call, execute, readConfiguration, canEdit = () => true, canRead = () => true, blockedReason = () => '', changed = () => {} }) {
    let epoch = 0, disposed = false, active = true, reading = null;
    const state = { open: false, loaded: false, loading: false, writing: false, configuration: null, base: null,
      draft: null, pending: null, result: null, recovery: null, stale: false, message: '' };
    const publish = () => { if (!disposed) { try { changed(state); } catch (_) { /* The receipt survives view failures. */ } } };
    const live = () => !disposed && active && state.open;
    const readable = () => live() && !state.loading && !state.writing && canRead();
    const editable = () => live() && state.loaded && !state.loading && !state.writing && !state.recovery && canEdit();
    const current = version => live() && version === epoch;
    function retire() { ++epoch; reading?.abort(); reading = null; state.loading = false; state.pending = null; }
    async function load({ explicit = true } = {}) {
      if (!readable()) return false;
      retire(); const version = epoch, abort = new AbortController(); reading = abort;
      state.loading = true; state.loaded = false; state.message = 'Reading saved house policy…'; publish();
      const decode = value => { if (!current(version)) fail('This policy read is no longer current.'); return configurationOf(value); };
      try {
        const stored = readConfiguration ? await readConfiguration(decode, { explicit, signal: abort.signal })
          : decode(await call(path, undefined, { signal: abort.signal }));
        if (!current(version)) return false;
        if (stored === null) fail('The policy read was superseded. Refresh it again.');
        state.configuration = stored; state.loaded = true;
        state.stale = !!state.draft && (state.base.revision !== stored.revision || signature(state.base) !== signature(stored));
        if (!state.draft) state.base = stored;
        if (explicit && state.recovery) {
          state.result = freeze({ ...state.recovery, status: 'observed', detail: `${observation(stored, state.recovery.change)} This read does not prove how the earlier request finished. Review again before another submission.` });
          state.recovery = null;
        }
        state.message = state.stale ? 'Saved policy changed. Your original draft is retained; discard it or open an edit from the current rules.'
          : state.recovery ? 'Use Refresh house policy to inspect the earlier request before another change.' : !canEdit() ? blockedReason() || 'Policy edits are unavailable right now.' : '';
        return true;
      } catch (error) { if (current(version)) { state.stale = true; state.message = problem(error); } return false; }
      finally { if (reading === abort) { reading = null; state.loading = false; publish(); } }
    }
    function begin(kind, key = null) {
      if (!editable()) return false;
      const config = state.configuration; let draft;
      if (kind === 'set_defaults') draft = Object.fromEntries(Object.entries(config.defaults).map(([name, value]) => [name, String(value)]));
      else if (kind === 'set_quiet_hours') draft = { ...(config.quiet_hours ?? { start: '22:00', end: '07:00', max_volume: 25 }), max_volume: String(config.quiet_hours?.max_volume ?? 25) };
      else if (kind === 'clear_quiet_hours') { if (!config.quiet_hours) return false; draft = {}; }
      else if (['set_room', 'remove_room'].includes(kind)) {
        const row = key === null ? null : config.rooms.find(each => each.room === key);
        if (key !== null && !row || kind === 'remove_room' && !row) return false;
        draft = { existing: row?.room ?? null, room: row?.room ?? '', max_volume: row?.max_volume == null ? '' : String(row.max_volume), max_step: row?.max_step == null ? '' : String(row.max_step) };
      } else if (['set_client', 'remove_client'].includes(kind)) {
        const row = key === null ? null : config.clients.find(each => each.client === key);
        if (key !== null && !row || kind === 'remove_client' && !row) return false;
        draft = { existing: row?.client ?? null, client: row?.client ?? '', allow_mode: row?.allow == null ? 'inherit' : 'only',
          allow: row?.allow?.join('\n') ?? '', deny: row?.deny.join('\n') ?? '', grant: row?.grant.join('\n') ?? '',
          capped: row?.capped == null ? 'inherit' : row.capped ? 'capped' : 'uncapped', manage_others: row?.manage_others ?? false, mcp_profile: row?.mcp_profile ?? 'inherit' };
      } else return false;
      retire(); state.base = config; state.draft = freeze({ kind, ...draft }); state.stale = false; state.message = ''; publish(); return true;
    }
    function edit(field, value) {
      if (!editable() || !state.draft || !Object.hasOwn(state.draft, field) || ['kind', 'existing'].includes(field)
          || ['room', 'client'].includes(field) && state.draft.existing !== null
          || (field === 'manage_others' ? typeof value !== 'boolean' : typeof value !== 'string' || value.length > 65536)) return false;
      ++epoch; state.pending = null; state.draft = freeze({ ...state.draft, [field]: value }); state.message = ''; publish(); return true;
    }
    function review() {
      if (!editable() || state.stale || !state.draft) return null;
      state.pending = null;
      try {
        const change = changeOf(state.draft, state.base), expected = expectedOf(state.base, change);
        state.pending = freeze({ base: state.base, expected, body: { expected_revision: state.base.revision, change },
          prior: observation(state.base, change), summary: observation(expected, change) }); state.message = '';
      } catch (error) { state.message = problem(error); }
      publish(); return state.pending;
    }
    async function save(planned = state.pending) {
      if (!editable() || state.stale || !planned || planned !== state.pending) return false;
      const version = ++epoch, abort = new AbortController(); reading = abort;
      state.pending = null; state.writing = true; state.message = 'Checking and saving the reviewed policy change…'; publish();
      let started = false, sent = false, receipt = null, failure = null;
      const operation = async ({ check = () => {}, send = call } = {}) => {
        if (started) fail('This reviewed policy change already claimed its one command.'); started = true;
        try {
          const valid = () => { check(); if (!current(version)) fail('The view changed before saving. Your draft is retained.'); };
          valid(); const fresh = configurationOf(await call(path, undefined, { signal: abort.signal })); valid();
          if (fresh.revision !== planned.base.revision || signature(fresh) !== signature(planned.base)) fail('Saved policy changed after review. Refresh house policy and open a new edit.');
          valid(); sent = true; receipt = receiptOf(await send(path, planned.body), planned);
          return { done: receipt.done, changed: receipt.changed };
        } catch (error) { failure = error; throw error; }
      };
      try {
        const accepted = execute ? await execute(operation) : !!await operation();
        if (!accepted || !receipt) throw failure || new Error('The command lane is busy. No policy write was sent.');
        state.configuration = receipt.configuration; state.base = receipt.configuration;
        state.result = freeze({ status: 'accepted', change: planned.body.change, detail: `${receipt.done} ${receipt.changed ? 'Future commands use these limits and permissions.' : 'The saved rule was already identical.'}`, receipt });
        state.draft = null; state.stale = false; state.recovery = null; state.message = receipt.done; return true;
      } catch (error) {
        const conflict = error?.code === 'CONFIG_CHANGED' || error?.status === 409;
        state.stale = true;
        state.result = freeze({ status: sent ? conflict ? 'conflict' : 'uncertain' : 'not_sent', change: planned.body.change,
          detail: `${problem(error)} ${sent ? conflict ? 'The house rejected this stale revision. Refresh house policy before another edit.'
            : 'The rule may have been saved. Refresh house policy to inspect it; the request was not repeated.' : 'No policy write was sent.'} Your draft is retained.` });
        if (sent) state.recovery = state.result;
        state.message = state.result.detail; return false;
      } finally { if (reading === abort) reading = null; state.writing = false; state.loaded = false; publish(); }
    }
    function close() { retire(); state.open = false; state.loaded = false; publish(); }
    return { state, readable, editable, load, begin, edit, review, save,
      open() { if (disposed || !active || state.writing) return false; state.open = true; publish(); void load({ explicit: false }); return true; },
      cancelReview() { if (!state.writing) { ++epoch; state.pending = null; publish(); } },
      discard() { if (!live() || state.writing) return false; retire(); state.draft = null; state.base = state.configuration; state.stale = false; state.message = ''; publish(); return true; },
      observe() { if (!state.writing && (!live() || !canEdit())) state.pending = null; publish(); },
      close, suspend() { active = false; close(); }, resume() { if (!disposed) { active = true; publish(); } },
      dispose() { active = false; close(); disposed = true; },
    };
  }
  return Object.freeze({ path, routes, configurationOf, signature, changeOf, expectedOf, observation, receiptOf, model });
})();
if (typeof module !== 'undefined' && module.exports) module.exports = PolicyConfiguration;
