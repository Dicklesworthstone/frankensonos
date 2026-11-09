// The dashboard is a read model of the public API, never a second speaker controller.
'use strict';
const DashboardCore = (() => {
  const STATES = ['playing', 'paused', 'stopped', 'transitioning', 'unknown'];
  const EVENTS = ['zone.state', 'topology.changed', 'player.health', 'action.logged', 'dj.status', 'events.reset', 'reset'];
  const text = (v) => typeof v === 'string' && v.trim() !== '' && v.length <= 4096;
  const level = (v) => Number.isInteger(v) && v >= 0 && v <= 100;
  const seconds = (v) => Number.isInteger(v) && v >= 0 && v <= 0xffffffff;
  const key = (z) => `${z.coordinator_room}@${z.household}`;
  const shape = (z) => JSON.stringify([z.household, z.coordinator_room, [...z.members].sort()]);
  const problem = (e) => [e?.message || 'The request failed.', e?.hint].filter(Boolean).join(' ');
  function zones(value) {
    if (!Array.isArray(value) || value.length > 256) throw new Error('Unreadable household inventory.');
    const seen = new Set(), targets = new Set();
    return value.map((z) => {
      if (!z || !text(z.household) || !text(z.coordinator_room) || !STATES.includes(z.transport_state)
          || !Array.isArray(z.members) || !z.members.length || z.members.length > 256
          || !z.members.every(text) || !z.members.includes(z.coordinator_room)
          || (z.degraded != null && (!Array.isArray(z.degraded) || !z.degraded.every((r) => z.members.includes(r))))) {
        throw new Error('Incomplete room inventory. Refresh before controlling playback.');
      }
      if (targets.has(key(z))) throw new Error('Ambiguous room addresses. Use the full remote.');
      targets.add(key(z));
      for (const r of z.members) {
        const id = JSON.stringify([z.household, r]);
        if (seen.has(id)) throw new Error('A room belongs to more than one group.');
        seen.add(id);
      }
      return Object.freeze({ ...z, members: Object.freeze([...z.members]), degraded: Object.freeze([...(z.degraded || [])]) });
    });
  }
  function playback(value, zone) {
    if (!value || shape(zones([value.zone])[0]) !== shape(zone) || !STATES.includes(value.transport_state)
        || [value.volume, value.group_volume].some((v) => v != null && !level(v))
        || (value.mute != null && typeof value.mute !== 'boolean')
        || (value.transport_status != null && !text(value.transport_status))) throw new Error('Playback or group membership changed.');
    const t = value.track;
    if (t != null && (!text(t.uri) || [t.duration_secs, t.position_secs, t.queue_position].some((v) => v != null && !seconds(v))
        || t.queue_position === 0 || (t.duration_secs > 0 && t.position_secs > t.duration_secs))) {
      throw new Error('Unreadable playback position.');
    }
    return Object.freeze({ ...value, zone, track: t == null ? null : Object.freeze({ ...t }) });
  }
  function playbackLabel(state) {
    return state?.transport_status === 'ERROR_OCCURRED' ? 'Playback error' : state?.transport_state || 'unknown';
  }
  function playbackNotice(state) {
    const status = state?.transport_status;
    if (status == null || status === 'OK') return '';
    return status === 'ERROR_OCCURRED' ? 'The speaker reported a playback error.'
      : `Renderer status: ${status}.`;
  }
  // GENA can report playing/paused before it supplies a track URI. Those
  // states still authorize play/pause; track navigation needs a known track.
  function transportAvailable(state, verb) {
    if (!state || !['playing', 'paused', 'stopped'].includes(state.transport_state)) return false;
    if (verb === 'pause') return true;
    if (verb === 'resume') return state.transport_state === 'paused' || !!state.track;
    return ['next', 'previous'].includes(verb) && !!state.track;
  }
  function elapsed(item, now) {
    const t = item?.state?.track;
    if (!t || !seconds(t.position_secs)) return null;
    const extra = item.fresh && item.state.transport_state === 'playing' ? Math.max(0, now - item.at) / 1000 : 0;
    return Math.min(t.duration_secs > 0 ? t.duration_secs : 0xffffffff, Math.floor(t.position_secs + extra));
  }
  function clock(value) {
    if (!seconds(value)) return '—:—';
    const h = Math.floor(value / 3600), m = Math.floor(value / 60) % 60;
    return `${h ? `${h}:${String(m).padStart(2, '0')}` : m}:${String(value % 60).padStart(2, '0')}`;
  }
  function hash(value) {
    let n = 2166136261;
    for (const ch of String(value)) n = Math.imul(n ^ ch.codePointAt(0), 16777619) >>> 0;
    return n;
  }
  async function each(items, work, limit = 4) {
    let next = 0;
    await Promise.all(Array.from({ length: Math.min(limit, items.length) }, async () => {
      while (next < items.length) await work(items[next++]);
    }));
  }
  function request({ fetcher = (...a) => fetch(...a), schedule = setTimeout, cancel = clearTimeout } = {}) {
    const readers = new Set();
    const send = async (url, body, { signal, method = body === undefined ? 'GET' : 'POST' } = {}) => {
      if (!['GET', 'POST', 'PUT'].includes(method) || (method === 'GET') !== (body === undefined)) {
        throw new Error('The dashboard request method does not match its body.');
      }
      const write = method !== 'GET', abort = new AbortController();
      const budget = method === 'POST' && url === '/announce' ? 1200000
        : write && (url === '/queue' || url === '/sonos/playlists/play' || url.startsWith('/dj/') || /^\/scenes\/[^/]+\/apply$/.test(url)
        || (method === 'PUT' && /^\/scenes\/[^/]+$/.test(url) && body?.backup_queues === true)) ? 600000
        : write ? 120000 : /^\/zones\/[^/]+\/queue\/search(?:\?|$)/.test(url) ? 60000 : 10000;
      let timer, finished = false, stopped = null, rejectStopped;
      const timeout = new Promise((_, reject) => { rejectStopped = reject; });
      const stop = (error) => {
        if (finished) return;
        finished = true; stopped = error;
        rejectStopped(error); abort.abort();
      };
      const cancelRead = () => stop(Object.assign(new Error('The obsolete state request was cancelled.'), { code: 'DASHBOARD_READ_CANCELLED' }));
      if (!write) {
        readers.add(cancelRead);
        signal?.addEventListener('abort', cancelRead, { once: true });
        if (signal?.aborted) cancelRead();
      }
      timer = schedule(() => stop(new Error(write
        ? 'No command confirmation arrived. It may still complete; refresh before another action.'
        : 'The daemon did not answer in time.')), budget);
      try {
        return await Promise.race([timeout, (async () => {
          if (finished) throw stopped;
          const response = await fetcher(url, { method,
            headers: { Accept: 'application/json', ...(write ? { 'Content-Type': 'application/json' } : {}) },
            ...(write ? { body: JSON.stringify(body) } : {}), signal: abort.signal,
            credentials: 'same-origin', redirect: 'error', cache: 'no-store' });
          if (finished) throw stopped;
          const raw = await response.text();
          if (finished) throw stopped;
          let data;
          try { data = JSON.parse(raw); } catch (_) { throw new Error('The daemon returned an unreadable response.'); }
          if (!response.ok) {
            const e = new Error(data?.detail || `Request failed (${response.status}).`);
            if (typeof data?.code === 'string') e.code = data.code;
            e.hint = data?.hint; e.status = response.status;
            const retry = response.headers?.get('retry-after');
            e.retryAfter = retry && /^\d+$/.test(retry) ? Number(retry) * 1000
              : retry && !/^[+-]?[0-9.]+$/.test(retry) && Number.isFinite(Date.parse(retry)) ? Math.max(0, Date.parse(retry) - Date.now()) : 0;
            throw e;
          }
          return data;
        })()]);
      } finally {
        finished = true; cancel(timer); readers.delete(cancelRead);
        if (!write) signal?.removeEventListener('abort', cancelRead);
      }
    };
    // Stream/lifecycle recovery cancels observations, never submitted commands.
    send.cancelReads = () => { for (const cancelRead of [...readers]) cancelRead(); };
    return send;
  }
  function model({ call, changed = () => {}, now = () => performance.now(), demo = false }) {
    const state = { items: [], selected: '', loading: false, busy: false, error: '', message: '',
      uncertain: false, connected: false, active: true, initialized: false, retryAfter: 0,
      topologyFresh: false, failedTargets: [] };
    let epoch = 0, lifetime = 0, doubt = 0, uncertainStore = null, disposed = false, reading = null;
    const revisions = new Map();
    const revision = (target) => revisions.get(target) || 0;
    const emit = () => { if (!disposed) { try { changed(state); } catch (_) { /* Presentation only. */ } } };
    const selected = () => state.items.find((i) => key(i.zone) === state.selected);
    const canControl = () => !!(!demo && !disposed && state.active && !state.busy
      && (!reading || reading.retained.has(state.selected))
      && !state.uncertain && selected()?.fresh && (state.connected || now() - selected().at < 30000));
    const canManage = () => !demo && !disposed && state.active && !state.busy && !state.uncertain;
    const stores = {
      schedules: { path: '/schedules', label: 'routines', observed: 'stored list', route: /^\/schedules(?:\/(?:pause|resume|remove))?$/ },
      preferences: { path: '/dj/preferences', label: 'preferences', observed: 'standing taste', route: /^\/dj\/preferences$/ },
      playlists: { path: target => `/sonos/playlists?zone=${encodeURIComponent(target)}`, scoped: true,
        label: 'saved playlists', observed: 'original household catalog', route: /^\/sonos\/playlists\/save$/ },
    };
    const qualified = target => text(target) && !/[\u0000-\u001f\u007f]/.test(target)
      && target.lastIndexOf('@') > 0 && target.lastIndexOf('@') < target.length - 1;
    const storeIdentity = (store, target) => stores[store].scoped ? JSON.stringify([store, target]) : store;
    // House settings remain readable without online speakers. Only a current,
    // explicit reread of the same store and target acknowledges its uncertain
    // write. A saved Sonos catalog belongs to its original household even if
    // the owner has selected another group since the command was submitted.
    async function refreshStored(store, decode, { explicit = false, signal, storeTarget } = {}) {
      const config = Object.hasOwn(stores, store) ? stores[store] : null;
      if (!config || typeof decode !== 'function' || disposed || !state.active || state.busy
          || (config.scoped ? !qualified(storeTarget) : storeTarget !== undefined)) return null;
      const version = lifetime, before = doubt, identity = storeIdentity(store, storeTarget);
      const path = config.scoped ? config.path(storeTarget) : config.path;
      const value = await call(path, undefined, { signal });
      if (disposed || !state.active || version !== lifetime || state.busy || signal?.aborted) return null;
      const result = decode(value);
      if (disposed || !state.active || version !== lifetime || state.busy || signal?.aborted) return null;
      if (explicit && state.uncertain && uncertainStore === identity && before === doubt) {
        state.uncertain = false; uncertainStore = null;
        state.message = `${config.label[0].toUpperCase() + config.label.slice(1)}${config.scoped ? ` for ${storeTarget}` : ''} refreshed. Review the ${config.observed} before another command.`; emit();
      }
      return result;
    }
    // Event names are not household identities. Resolve all matching groups,
    // then issue only qualified reads; unknown names require fresh topology.
    function eventTargets(room) {
      if (!state.topologyFresh || !text(room)) return null;
      const found = state.items.filter((item) => item.zone.members.includes(room)).map((item) => key(item.zone));
      return found.length ? found : null;
    }
    async function refresh(explicit = false, scope = null) {
      if (disposed || !state.active || reading || state.busy) return false;
      if (scope !== null && (typeof scope !== 'object' || Array.isArray(scope)
          || (scope.recovery !== true && (!Array.isArray(scope.targets) || !scope.targets.length
            || scope.targets.length > 256 || !scope.targets.every(text))))) return false;
      let targets = scope?.targets ? new Set(scope.targets) : null;
      // A partial read cannot establish a missing or changed topology baseline.
      if (!state.topologyFresh || (targets && [...targets].some((t) => !state.items.some((i) => key(i.zone) === t)))) {
        targets = null; scope = null;
      }
      const version = ++epoch, observation = { abort: new AbortController(), retained: new Set() }, stamps = new Map(revisions);
      reading = observation;
      state.loading = true; state.retryAfter = 0; emit();
      const current = () => !disposed && state.active && version === epoch;
      try {
        const options = { signal: observation.abort.signal };
        const list = targets ? state.items.map((i) => i.zone) : zones(await call('/zones', undefined, options));
        if (!current()) return false;
        const old = new Map(state.items.map((i) => [shape(i.zone), i]));
        const jobs = [], baselines = new Map(), cooldowns = new Map();
        const items = list.map((zone) => {
          const prior = old.get(shape(zone));
          const wanted = targets ? targets.has(key(zone)) : !scope?.recovery || !prior?.fresh;
          if (!wanted && prior) { observation.retained.add(key(zone)); return prior; }
          const item = { zone, state: prior?.state || null, fresh: false, at: prior?.at ?? now(), error: prior?.error || '' };
          baselines.set(item, { ...item });
          jobs.push(item); return item;
        });
        if (observation.retained.size) emit();
        await each(jobs, async (item) => {
          const target = key(item.zone), stamp = stamps.get(target) || 0;
          const stillCurrent = () => current() && stamp === revision(target);
          if (!stillCurrent()) return;
          try {
            const next = playback(await call(`/zones/${encodeURIComponent(target)}/state`, undefined, options), item.zone);
            if (!stillCurrent()) return;
            item.state = next; item.at = now(); item.fresh = true; item.error = '';
          } catch (e) {
            if (!stillCurrent()) return;
            item.error = problem(e);
            cooldowns.set(item, e.retryAfter || 0);
          }
        });
        if (!current()) return false;
        // One room can finish before another stalls. An intervening event
        // still retires that finished observation before the batch publishes.
        for (const item of jobs) {
          const target = key(item.zone);
          if ((stamps.get(target) || 0) !== revision(target)) Object.assign(item, baselines.get(item));
          else state.retryAfter = Math.max(state.retryAfter, cooldowns.get(item) || 0);
        }
        state.items = items;
        if (!targets) {
          state.topologyFresh = true;
          const retained = new Set(items.map((i) => key(i.zone)));
          for (const target of revisions.keys()) if (!retained.has(target)) revisions.delete(target);
        }
        state.failedTargets = items.filter((i) => i.error).map((i) => key(i.zone));
        state.error = state.failedTargets.length ? 'Some rooms could not be read. Last observations are marked stale.' : '';
        if (!state.initialized) state.selected = key(items.find((i) => i.state?.transport_state === 'playing')?.zone || list[0] || { coordinator_room: '', household: '' });
        if (!items.some((i) => key(i.zone) === state.selected)) state.selected = '';
        state.initialized = true;
        // Scoped/event recovery is never the explicit acknowledgement of a
        // previously uncertain command, even when it observes its effect.
        if (explicit && scope === null && selected()?.fresh && uncertainStore === null) {
          state.uncertain = false; state.message = 'Playback refreshed. Review the current state before acting.';
        }
        return jobs.every((i) => i.fresh);
      } catch (e) {
        if (current()) {
          state.error = problem(e); state.retryAfter = e.retryAfter || 0;
          state.topologyFresh = false;
          for (const item of state.items) item.fresh = false;
        }
        return false;
      } finally {
        if (reading === observation) { reading = null; state.loading = false; emit(); }
      }
    }
    function select(target) {
      if (disposed || state.busy || !state.items.some((i) => key(i.zone) === target)) return false;
      state.selected = target; emit(); return true;
    }
    // One mutation lane for transport, the DJ, search and house scenes. A
    // retained callback cannot dispatch after its operation/lifetime ended.
    async function mutate(operation, { wholeHouse = true, stored, storeTarget } = {}) {
      const config = Object.hasOwn(stores, stored) ? stores[stored] : null, storage = !!config;
      const scoped = storage && config.scoped;
      if ((stored !== undefined && !storage) || (scoped ? !qualified(storeTarget) || storeTarget !== state.selected
        : storeTarget !== undefined) || !(storage && !scoped ? canManage() : canControl())) return false;
      if (reading) {
        // Controls for an unchanged group take priority over background reads
        // of other rooms. Retire that batch before claiming the mutation lane;
        // late bodies cannot publish or release a newer observation's lane.
        const obsolete = reading;
        ++epoch; reading = null; state.loading = false;
        for (const item of state.items) if (!obsolete.retained.has(key(item.zone))) item.fresh = false;
        obsolete.abort.abort();
      }
      const item = selected(), version = epoch, life = lifetime;
      const target = scoped ? storeTarget : storage ? null : key(item.zone), stamp = revision(target);
      let sent = false, accepted = false, open = true;
      const valid = () => open && !disposed && state.active && life === lifetime
        && ((storage && !scoped) || (version === epoch && stamp === revision(target) && state.selected === target));
      const check = () => { if (!valid()) throw new Error('The view changed. Review this action again.'); };
      state.busy = true; state.message = ''; emit();
      try {
        const result = await operation({ item, target, check,
          async send(url, body, options) {
            check();
            if (sent) throw new Error('This action already dispatched its one command.');
            if (storage && (!config.route.test(url)
                || body == null || (options?.method != null && options.method !== 'POST')
                || (scoped && (typeof body !== 'object' || Array.isArray(body) || body.zone !== target)))) {
              throw new Error(`Stored ${config.label} actions must use their native POST endpoints.`);
            }
            sent = true;
            return call(url, body, options);
          } });
        if (!result || typeof result.done !== 'string' || typeof result.changed !== 'boolean'
            || (result.notes != null && (!Array.isArray(result.notes) || !result.notes.every((n) => n && typeof n.detail === 'string')))) {
          throw new Error('The command response was incomplete.');
        }
        accepted = true;
        state.message = [result.done, ...(result.notes || []).map((n) => n.detail)].join(' ');
        return true;
      } catch (e) {
        if (sent) { state.uncertain = true; uncertainStore = storage ? storeIdentity(stored, target) : null; ++doubt; }
        if (!storage) item.fresh = false;
        state.message = `${problem(e)} ${sent ? storage
          ? `Outcome uncertain; no retry was sent. Use Refresh ${config.label} and inspect the ${config.observed} before another command.`
          : 'Outcome uncertain; no retry was sent. Use Refresh before another command.' : 'No command was sent.'}`;
        return false;
      } finally {
        const canRefresh = valid(); open = false;
        state.busy = false; emit();
        if (accepted && canRefresh && !storage) await refresh(false, wholeHouse ? null : { targets: [target] });
      }
    }
    async function command(verb, change = {}) {
      if (!['pause', 'resume', 'next', 'previous', 'volume'].includes(verb)) return false;
      if (verb === 'volume' && (!Number.isInteger(change.delta) || ![-5, 5].includes(change.delta))) return false;
      if (verb !== 'volume' && !transportAvailable(selected()?.state, verb)) return false;
      return mutate(async ({ item, target, check, send }) => {
        const fresh = playback(await call(`/zones/${encodeURIComponent(target)}/state`), item.zone);
        check();
        if (verb !== 'volume' && (fresh.transport_state !== item.state.transport_state
            || fresh.track?.uri !== item.state.track?.uri || fresh.track?.queue_position !== item.state.track?.queue_position)) {
          throw new Error('Playback changed. Refresh before sending this command.');
        }
        return send(`/${verb}`, { zone: target, ...(verb === 'volume' ? { delta: change.delta, group: true } : {}) });
      }, { wholeHouse: false });
    }
    function invalidate(targets = null) {
      if (targets === null) {
        ++epoch; state.topologyFresh = false;
        for (const item of state.items) item.fresh = false;
      } else {
        const wanted = new Set(targets);
        for (const item of state.items) if (wanted.has(key(item.zone))) {
          const target = key(item.zone);
          revisions.set(target, revision(target) + 1); item.fresh = false;
        }
      }
      emit();
    }
    function suspend() {
      ++lifetime;
      state.active = false; state.connected = false;
      invalidate();
    }
    return { state, refresh, refreshStored, eventTargets, select, selected, canControl, canManage, command, mutate, suspend, invalidate,
      connection(value) {
        const lost = state.connected && !value;
        state.connected = value;
        if (lost) invalidate(); else emit();
      },
      resume() { if (!disposed) { state.active = true; emit(); } },
      dispose() { disposed = true; suspend(); } };
  }
  return { STATES, EVENTS, zones, playback, playbackLabel, playbackNotice, transportAvailable, key, shape, elapsed, clock, hash, each, request, model, problem };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = DashboardCore;
