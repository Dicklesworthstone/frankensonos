// FrankenSonos web remote: a small reference client of the public HTTP API.
//
// Every call goes through ROUTES (a test checks that each one exists), and
// every value from the house is shown with textContent, never as markup.
'use strict';

const ROUTES = {
  zones: ['GET', '/zones'],
  state: ['GET', '/zones/{room}/state'],
  djStatus: ['GET', '/zones/{room}/dj'],
  moods: ['GET', '/dj/moods'],
  scenes: ['GET', '/scenes'],
  doctor: ['GET', '/doctor'],
  events: ['GET', '/events'],
  art: ['GET', '/art'],
  favorites: ['GET', '/favorites'],
  queue: ['GET', '/zones/{room}/queue'],
  queuePlay: ['POST', '/queue/play'],
  queueRemove: ['POST', '/queue/remove'],
  queueAdd: ['POST', '/queue'],
  pause: ['POST', '/pause'],
  resume: ['POST', '/resume'],
  next: ['POST', '/next'],
  previous: ['POST', '/previous'],
  volume: ['POST', '/volume'],
  mute: ['POST', '/mute'],
  playFavorite: ['POST', '/play/favorite'],
  djStart: ['POST', '/dj/start'],
  djStop: ['POST', '/dj/stop'],
  djSkip: ['POST', '/dj/skip'],
  djSteer: ['POST', '/dj/steer'],
  djFeedback: ['POST', '/dj/feedback'],
  applyScene: ['POST', '/scenes/{name}/apply'],
};

const $ = (id) => document.getElementById(id);

/** The path of route `name` with `params` filled in and `query` added. */
function path(name, params = {}, query = null) {
  const [, template] = ROUTES[name];
  const filled = template.replace(/\{(\w+)\}/g, (_, key) => encodeURIComponent(params[key]));
  const search = query ? new URLSearchParams(query).toString() : '';
  return search ? `${filled}?${search}` : filled;
}

/** Call route `name`; its JSON answer, or an Error carrying the API's. */
async function call(name, { params, query, body } = {}) {
  const [method] = ROUTES[name];
  const init = { method, headers: { Accept: 'application/json' }, cache: 'no-store' };
  if (method !== 'GET') {
    init.headers['Content-Type'] = 'application/json';
    init.body = JSON.stringify(body || {});
  }
  const where = path(name, params, query);
  // A stuck read must not hold event recovery forever. Writes are never
  // timed out into an automatic retry or dispatched through another path.
  const controller = method === 'GET' ? new AbortController() : null;
  const deadline = controller ? setTimeout(() => controller.abort(), 15000) : null;
  if (controller) init.signal = controller.signal;
  try {
    const answer = await fetch(where, init);
    const text = await answer.text();
    let data = null;
    try {
      data = text ? JSON.parse(text) : null;
    } catch (_) {
      data = null;
    }
    if (!answer.ok) {
      const error = new Error((data && data.detail) || `${method} ${where}: ${answer.status}`);
      error.code = data && data.code;
      error.hint = data && data.hint;
      throw error;
    }
    return data;
  } finally {
    if (deadline !== null) clearTimeout(deadline);
  }
}

/** An element: `attrs` (text, class, on<event>, attributes) and children. */
function el(tag, attrs = {}, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(attrs)) {
    if (value === null || value === undefined || value === false) continue;
    if (key === 'text') node.textContent = value;
    else if (key === 'class') node.className = value;
    else if (key.startsWith('on')) node.addEventListener(key.slice(2), value);
    else node.setAttribute(key, value === true ? '' : String(value));
  }
  for (const child of children) if (child !== null && child !== undefined) node.append(child);
  return node;
}

let toastTimer = 0;
function toast(message) {
  const box = $('toast');
  box.textContent = message;
  box.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { box.hidden = true; }, 4000);
}

function failed(error) {
  toast(error.hint ? `${error.message} ${error.hint}` : error.message);
}

/** Tell the outcome's notes (a clamped volume, say), if any. */
function noted(outcome) {
  const notes = (outcome && outcome.notes) || [];
  if (notes.length) toast(notes.map((n) => n.detail).join(' '));
  return outcome;
}

/** A short tag that changes with the track, for the art's URL. */
function tag(text) {
  let h = 0;
  for (let i = 0; i < text.length; i += 1) h = (h * 31 + text.charCodeAt(i)) >>> 0;
  return h.toString(36);
}

// The house as last read.
const house = {
  zones: [],
  cards: new Map(),
  readVersion: 0,
  layout: null,
  moods: null,
  dj: true,
  // Household -> its playable Sonos favorites, as last read.
  favorites: new Map(),
};

/** How to name `room` of `household` to the API. */
function roomName(room, household) {
  return `${room}@${household}`;
}

function zoneKey(zone) {
  return JSON.stringify([zone.household, zone.coordinator_room]);
}

/** Membership, not transport state, determines a card's lifetime. */
function groupShape(zone) {
  return JSON.stringify([zone.household, zone.coordinator_room, [...zone.members].sort()]);
}

function checkedZones(value) {
  const text = (v) => typeof v === 'string' && v.trim() !== '';
  if (!Array.isArray(value)) throw new Error('The room list is unreadable.');
  const seen = new Set();
  for (const zone of value) {
    if (!zone || !text(zone.household) || !text(zone.coordinator_room)
        || !Array.isArray(zone.members) || !zone.members.length
        || !zone.members.every(text) || !zone.members.includes(zone.coordinator_room)) {
      throw new Error('The room list is incomplete.');
    }
    for (const room of zone.members) {
      const key = JSON.stringify([zone.household, room]);
      if (seen.has(key)) throw new Error('The room list assigns a room more than once.');
      seen.add(key);
    }
  }
  return value;
}

/** Settle all reads with bounded fan-out, including when one room fails. */
async function readEach(items, read, current = () => true) {
  let index = 0, failure = null;
  await Promise.all(Array.from({ length: Math.min(4, items.length) }, async () => {
    while (current() && index < items.length) {
      const item = items[index++];
      try { await read(item); } catch (error) { failure = error; }
    }
  }));
  if (failure && current()) throw failure;
}

async function loadZones() {
  const version = ++house.readVersion;
  const zones = checkedZones(await call('zones'));
  if (version !== house.readVersion) return;
  house.zones = zones;
  render();
  await readEach(zones, refreshZone, () => version === house.readVersion);
}

function ownsCard(parts) {
  return !parts.disposed && house.cards.get(zoneKey(parts.zone)) === parts;
}

function disposeCard(parts) {
  parts.disposed = true;
  ++parts.readVersion;
  parts.queue.dispose();
  for (const entry of parts.sliders.values()) {
    clearTimeout(entry.timer);
    ++entry.version;
  }
}

function render() {
  const layout = JSON.stringify(house.zones.map(groupShape));
  // Do not detach nodes when only playback changed: focus, drafts, queue
  // selections, reviews and scroll belong to the person using this card.
  if (layout === house.layout) {
    for (const zone of house.zones) house.cards.get(zoneKey(zone)).zone = zone;
    return;
  }
  const old = house.cards;
  const keep = new Map();
  for (const zone of house.zones) {
    const parts = old.get(zoneKey(zone));
    if (parts && groupShape(parts.zone) === groupShape(zone)) {
      parts.zone = zone;
      keep.set(zoneKey(zone), parts);
    }
  }
  for (const [key, parts] of old) if (!keep.has(key)) disposeCard(parts);
  house.cards = keep;
  house.layout = layout;
  const main = $('households');
  const focused = document.activeElement;
  main.replaceChildren();
  if (!house.zones.length) {
    main.append(el('p', { class: 'empty', text: 'No speakers found yet.' }));
    return;
  }
  const byHome = new Map();
  for (const zone of house.zones) {
    if (!byHome.has(zone.household)) byHome.set(zone.household, []);
    byHome.get(zone.household).push(zone);
  }
  for (const [household, zones] of byHome) {
    const id = `home-${tag(household)}`;
    const grid = el('div', { class: 'zones' });
    for (const zone of zones) grid.append(keep.get(zoneKey(zone))?.node || card(zone));
    main.append(el('section', { 'aria-labelledby': id },
      el('h2', { id, text: byHome.size > 1 ? `Household ${household}` : 'Rooms' }),
      grid));
  }
  if (focused && main.contains(focused)) focused.focus({ preventScroll: true });
}

/** One zone's card; its parts are kept to update in place. */
function card(zone) {
  const name = roomName(zone.coordinator_room, zone.household);
  const label = zone.members.join(' + ');
  const parts = {
    zone,
    name,
    disposed: false,
    readVersion: 0,
    writing: false,
    art: el('img', { alt: '', hidden: true, width: 64, height: 64 }),
    title: el('div', { class: 'title', text: 'Nothing playing' }),
    by: el('div', { class: 'by' }),
    state: el('div', { class: 'state', text: zone.transport_state }),
    previous: el('button', { type: 'button', text: 'Prev', 'aria-label': `Previous track in ${label}` }),
    play: el('button', { class: 'primary', type: 'button', text: 'Play', 'aria-label': `Play ${label}` }),
    next: el('button', { type: 'button', text: 'Next', 'aria-label': `Next track in ${label}` }),
    favorite: el('select', { 'aria-label': `Play a favorite in ${label}`, hidden: true }),
    sliders: new Map(),
    mood: el('select', { 'aria-label': `DJ mood for ${label}` }),
    dj: el('button', { type: 'button', text: 'Start DJ', 'aria-label': `Start the DJ in ${label}` }),
    skip: el('button', { type: 'button', text: 'Skip', 'aria-label': `Skip to the DJ's next pick in ${label}` }),
    like: el('button', { type: 'button', text: 'Like', 'aria-label': `Like what the DJ plays in ${label}` }),
    dislike: el('button', { type: 'button', text: 'Dislike', 'aria-label': `Dislike what the DJ plays in ${label}` }),
    djRow: null,
    djPlaying: null,
    djRunning: false,
    tag: '',
  };
  parts.art.addEventListener('load', () => { parts.art.hidden = false; });
  parts.art.addEventListener('error', () => { parts.art.hidden = true; });
  parts.play.addEventListener('click', () => {
    const verb = parts.zone.transport_state === 'playing' ? 'pause' : 'resume';
    act(parts, () => call(verb, { body: { zone: name } }));
  });
  parts.next.addEventListener('click', () => act(parts, () => call('next', { body: { zone: name } })));
  parts.previous.addEventListener('click', () => act(parts, () => call('previous', { body: { zone: name } })));
  parts.favorite.addEventListener('change', () => {
    const favorite = parts.favorite.value;
    parts.favorite.value = '';
    if (favorite) act(parts, () => call('playFavorite', { body: { zone: name, favorite } }));
  });
  parts.skip.addEventListener('click', () => act(parts, () => call('djSkip', { body: { zone: name } })));
  // A running DJ takes a new mood from its next pick; a stopped one starts in it.
  parts.mood.addEventListener('change', () => {
    if (!parts.djRunning) return;
    const body = parts.mood.value ? { zone: name, mood: parts.mood.value } : { zone: name, clear: true };
    act(parts, () => call('djSteer', { body }), true);
  });
  for (const [button, signal] of [[parts.like, 'like'], [parts.dislike, 'dislike']]) {
    button.addEventListener('click', () => {
      act(parts, () => call('djFeedback', { body: { zone: name, signal } }), true);
    });
  }
  parts.dj.addEventListener('click', () => {
    if (parts.djRunning) {
      act(parts, () => call('djStop', { body: { zone: name } }));
    } else {
      const body = { zone: name };
      if (parts.mood.value) body.mood = parts.mood.value;
      act(parts, () => call('djStart', { body }));
    }
  });
  const volumes = el('div', { class: 'volumes' });
  for (const room of zone.members) volumes.append(slider(parts, room, zone.household));
  parts.djRow = el('div', { class: 'row', hidden: !house.dj }, parts.mood, parts.dj);
  parts.djPlaying = el('div', { class: 'row', hidden: true }, parts.skip, parts.like, parts.dislike);
  fillMoods(parts.mood);
  fillFavorites(parts);
  parts.queue = QueueRemote.panel({
    zone: name, call, el, played: () => { refreshZone(zone).catch(() => {}); },
  });
  house.cards.set(zoneKey(zone), parts);
  parts.node = el('article', { class: 'zone', 'aria-label': label },
    el('h3', { text: label }),
    el('div', { class: 'now' }, parts.art, el('div', { class: 'track' }, parts.title, parts.by, parts.state)),
    el('div', { class: 'row' }, parts.previous, parts.play, parts.next),
    parts.favorite,
    volumes,
    parts.djRow,
    parts.djPlaying,
    parts.queue.node);
  return parts.node;
}

/** A room's volume slider, sent once the hand rests (debounced). */
function slider(parts, room, household) {
  const id = `vol-${tag(`${household}/${room}`)}`;
  const input = el('input', { id, type: 'range', min: 0, max: 100, step: 1, value: 0, disabled: true });
  const shown = el('output', { for: id, text: '–' });
  const mute = el('button', { type: 'button', class: 'mute', text: 'Mute', 'aria-pressed': 'false', 'aria-label': `Mute ${room}` });
  const entry = { input, shown, mute, muted: false, room: roomName(room, household), until: 0, timer: 0, version: 0, readVersion: 0, volumeVersion: 0, mutedPending: false };
  mute.addEventListener('click', () => {
    if (!ownsCard(parts) || entry.mutedPending) return;
    entry.mutedPending = mute.disabled = true;
    ++entry.version;
    const want = !entry.muted;
    setMute(entry, want);
    call('mute', { body: { zone: entry.room, mute: want } })
      .then(noted)
      .catch((error) => { if (ownsCard(parts)) { setMute(entry, !want); failed(error); } })
      .finally(() => {
        entry.mutedPending = false;
        if (ownsCard(parts)) { mute.disabled = false; void refreshVolume(entry, parts).catch(() => {}); }
      });
  });
  input.addEventListener('input', () => {
    if (!ownsCard(parts)) return;
    ++entry.version;
    const version = ++entry.volumeVersion;
    shown.textContent = input.value;
    entry.until = Date.now() + 1500;
    clearTimeout(entry.timer);
    entry.timer = setTimeout(() => {
      if (!ownsCard(parts) || version !== entry.volumeVersion) return;
      call('volume', { body: { zone: entry.room, volume: Number(input.value) } })
        .then(noted)
        .then((outcome) => {
          if (!ownsCard(parts) || version !== entry.volumeVersion) return;
          if (outcome && typeof outcome.volume === 'number') setVolume(entry, outcome.volume, true);
        })
        .catch(failed);
    }, 250);
  });
  parts.sliders.set(room, entry);
  return el('div', { class: 'volume' }, el('label', { for: id, text: room }), input, shown, mute);
}

/** Show whether a room is muted. */
function setMute(entry, muted) {
  entry.muted = muted;
  entry.mute.textContent = muted ? 'Unmute' : 'Mute';
  entry.mute.setAttribute('aria-pressed', String(muted));
}

/** Show a room's volume, unless its slider is in hand. */
function setVolume(entry, volume, force = false) {
  if (!force && Date.now() < entry.until) return;
  entry.input.disabled = false;
  entry.input.value = String(volume);
  entry.shown.textContent = String(volume);
}

async function act(parts, request, announce = false) {
  if (!ownsCard(parts) || parts.writing) return;
  parts.writing = true;
  ++parts.readVersion;
  const controls = [parts.play, parts.next, parts.previous, parts.favorite,
    parts.dj, parts.skip, parts.mood, parts.like, parts.dislike];
  for (const control of controls) control.disabled = true;
  try {
    const outcome = noted(await request());
    if (announce && outcome && typeof outcome.done === 'string') toast(outcome.done);
  } catch (error) { failed(error); }
  finally {
    parts.writing = false;
    if (ownsCard(parts)) {
      for (const control of controls) control.disabled = false;
      void refreshZone(parts.zone).catch(() => {});
    }
  }
}

async function refreshZone(zone) {
  const parts = house.cards.get(zoneKey(zone));
  if (!parts || !ownsCard(parts)) return;
  const version = ++parts.readVersion;
  const current = () => ownsCard(parts) && version === parts.readVersion;
  const primary = parts.sliders.get(zone.coordinator_room);
  const levelVersion = primary?.version;
  let state;
  try {
    state = await call('state', { params: { room: parts.name } });
    if (!current()) return;
    if (!state || !['playing', 'paused', 'stopped', 'transitioning', 'unknown'].includes(state.transport_state)
        || (state.track != null && (typeof state.track !== 'object' || typeof state.track.uri !== 'string'))) {
      throw new Error('The playback state is unreadable.');
    }
    checkedZones([state.zone]);
    if (groupShape(state.zone) !== groupShape(parts.zone)) throw new Error('The group changed; refreshing topology.');
  } catch (error) {
    if (!current()) return;
    parts.state.textContent = 'State unavailable; last observation shown';
    throw error;
  }
  parts.zone.transport_state = state.transport_state;
  const playing = state.transport_state === 'playing';
  parts.play.textContent = playing ? 'Pause' : 'Play';
  parts.play.setAttribute('aria-label', `${playing ? 'Pause' : 'Play'} ${zone.members.join(' + ')}`);
  parts.state.textContent = state.transport_state;
  const track = state.track;
  parts.title.textContent = track ? track.title || track.uri : 'Nothing playing';
  parts.by.textContent = track ? [track.creator, track.album].filter(Boolean).join(' · ') : '';
  const next = track ? tag(`${track.uri}|${track.title || ''}`) : '';
  if (next !== parts.tag) {
    parts.tag = next;
    if (track) {
      parts.art.src = path('art', {}, { zone: parts.name, v: next });
    } else {
      parts.art.hidden = true;
      parts.art.removeAttribute('src');
    }
  }
  const coordinator = primary && primary.version === levelVersion ? primary : null;
  if (coordinator && typeof state.volume === 'number') setVolume(coordinator, state.volume);
  if (coordinator && !coordinator.mutedPending && typeof state.mute === 'boolean') setMute(coordinator, state.mute);
  // Member reads are sequential inside each of at most four group workers.
  let failure = null;
  for (const room of zone.members.filter((room) => room !== zone.coordinator_room)) {
    if (!current()) return;
    try { await refreshVolume(parts.sliders.get(room), parts); } catch (error) { failure = error; }
  }
  if (!current()) return;
  try { await refreshDj(parts, current); } catch (error) { failure = error; }
  if (failure && current()) throw failure;
}

async function refreshVolume(entry, parts) {
  const version = entry.version;
  const readVersion = ++entry.readVersion;
  const state = await call('state', { params: { room: entry.room } });
  if (!ownsCard(parts) || version !== entry.version || readVersion !== entry.readVersion) return;
  if (!state || typeof state !== 'object') throw new Error('The room level is unreadable.');
  if (typeof state.volume === 'number') setVolume(entry, state.volume);
  if (!entry.mutedPending && typeof state.mute === 'boolean') setMute(entry, state.mute);
}

async function refreshDj(parts, current = () => ownsCard(parts)) {
  if (!house.dj) return;
  try {
    const status = await call('djStatus', { params: { room: parts.name } });
    if (!current()) return;
    if (!status || typeof status.running !== 'boolean') throw new Error('The DJ state is unreadable.');
    parts.djRunning = status.running;
    // A rebuilt card shows the mood the running DJ plays, not "Any mood".
    if (parts.djRunning) parts.mood.value = (status.steering && status.steering.mood) || '';
  } catch (error) {
    if (!current()) return;
    if (error.code === 'NOT_IMPLEMENTED') {
      house.dj = false;
      for (const each of house.cards.values()) {
        each.djRow.hidden = true;
        each.djPlaying.hidden = true;
      }
      return;
    }
    throw error;
  }
  const label = parts.zone.members.join(' + ');
  parts.dj.textContent = parts.djRunning ? 'Stop DJ' : 'Start DJ';
  parts.dj.setAttribute('aria-label', `${parts.djRunning ? 'Stop' : 'Start'} the DJ in ${label}`);
  parts.djPlaying.hidden = !parts.djRunning;
}

/** A zone's favorites picker, from its household's favorites. */
function fillFavorites(parts) {
  const favorites = house.favorites.get(parts.zone.household) || [];
  parts.favorite.replaceChildren(el('option', { value: '', text: 'Play a favorite…' }));
  for (const favorite of favorites) {
    parts.favorite.append(el('option', { value: favorite.id, text: favorite.title }));
  }
  parts.favorite.hidden = favorites.length === 0;
}

/** Each household's playable favorites, read through one of its rooms. */
async function loadFavorites() {
  const firsts = new Map();
  for (const zone of house.zones) {
    if (!firsts.has(zone.household)) firsts.set(zone.household, zone);
  }
  await Promise.all([...firsts].map(async ([household, zone]) => {
    try {
      const favorites = await call('favorites', {
        query: { zone: roomName(zone.coordinator_room, household) },
      });
      house.favorites.set(household, favorites.filter((f) => f.kind !== 'unplayable'));
    } catch (_) {
      house.favorites.set(household, []);
    }
  }));
  for (const parts of house.cards.values()) fillFavorites(parts);
}

function fillMoods(select) {
  select.replaceChildren(el('option', { value: '', text: 'Any mood' }));
  for (const mood of house.moods || []) select.append(el('option', { value: mood, text: mood }));
}

async function loadMoods() {
  const zone = house.zones[0];
  if (!zone) return;
  try {
    const moods = await call('moods', { query: { zone: roomName(zone.coordinator_room, zone.household) } });
    house.moods = moods.moods.map((m) => m.name);
  } catch (_) {
    house.moods = [];
  }
  for (const parts of house.cards.values()) fillMoods(parts.mood);
}

async function loadScenes() {
  let scenes = [];
  try {
    scenes = await call('scenes');
  } catch (_) {
    scenes = [];
  }
  const box = $('scenes');
  box.replaceChildren();
  for (const scene of scenes) {
    box.append(el('button', {
      type: 'button',
      text: scene.name,
      'aria-label': `Apply the scene ${scene.name}`,
      onclick: () => call('applyScene', { params: { name: scene.name }, body: {} })
        .then((applied) => {
          const failures = applied.failed || [];
          toast(failures.length ? `${scene.name}: ${failures.length} step(s) failed.` : applied.done);
          return loadZones();
        })
        .catch(failed),
    }));
  }
  $('scenes-section').hidden = scenes.length === 0;
}

async function loadDoctor() {
  let report;
  try {
    report = await call('doctor');
  } catch (_) {
    return;
  }
  const failing = (report.checks || []).filter((c) => c.status === 'fail');
  const banner = $('doctor');
  banner.replaceChildren();
  if (!failing.length) {
    banner.hidden = true;
    return;
  }
  const list = el('ul');
  for (const check of failing) {
    list.append(el('li', { text: check.remedy ? `${check.title}: ${check.summary}. ${check.remedy}` : `${check.title}: ${check.summary}` }));
  }
  banner.append(el('strong', { text: `${failing.length} setup check(s) failed` }), list);
  banner.hidden = false;
}

// Read-only event reconciliation. A burst has one fixed flush deadline, so
// continuous notifications cannot postpone refresh forever. Only one batch
// runs at a time; changes during it cause a later read, never another write.
const LiveRemote = (() => {
  const COALESCE = 300;
  const FALLBACK = 10000;
  const MAX_BACKOFF = 60000;
  const MAX_PENDING = 64;

  function model({ connect, reload, refresh, status = () => {},
    schedule = setTimeout, cancel = clearTimeout }) {
    let active = false, disposed = false, connected = false, busy = false;
    let full = false, synced = false, failedRead = false, generation = 0;
    let stream = null, flush = null, retry = null, backoff = FALLBACK;
    const rooms = new Set();
    const clear = () => {
      if (flush !== null) cancel(flush);
      if (retry !== null) cancel(retry);
      flush = retry = null;
    };
    const report = () => status(!active ? 'Live updates paused'
      : failedRead ? 'State refresh failed; retrying…'
        : connected ? (synced ? 'Live' : 'Synchronizing…')
          : 'Refreshing without live events');

    function armRetry() {
      if (!active || busy || retry !== null || (connected && !failedRead)) return;
      retry = schedule(() => {
        retry = null;
        openFeed();
        request();
      }, backoff);
    }

    function queueFlush() {
      if (!active || busy || flush !== null || (failedRead && retry !== null) || (!full && !rooms.size)) return;
      flush = schedule(() => { flush = null; void run(); }, COALESCE);
    }

    function request(room = null) {
      if (!active) return;
      if (typeof room !== 'string' || !room.trim() || room.length > 512 || rooms.size >= MAX_PENDING) {
        full = true;
        rooms.clear();
      } else if (!full) rooms.add(room);
      queueFlush();
    }

    async function run() {
      if (!active || busy) return;
      const version = generation;
      const all = full;
      const batch = [...rooms];
      full = false;
      rooms.clear();
      busy = true;
      if (retry !== null) cancel(retry);
      retry = null;
      try {
        if (all) await reload();
        else for (const room of batch) {
          if (!active || version !== generation) break;
          await refresh(room);
        }
        if (active && version === generation) {
          if (all) { synced = true; failedRead = false; }
          backoff = FALLBACK;
        }
      } catch (_) {
        if (active && version === generation) {
          failedRead = true;
          backoff = Math.min(MAX_BACKOFF, backoff * 2);
        }
      } finally {
        busy = false;
        if (active) {
          report();
          armRetry();
          queueFlush();
        }
      }
    }

    function openFeed() {
      if (!active || stream || !connect) return;
      let feed;
      try { feed = connect(); } catch (_) { return; }
      if (!feed) return;
      stream = feed;
      const current = () => active && stream === feed;
      feed.addEventListener('open', () => {
        if (!current()) return;
        connected = true;
        synced = false;
        ++generation;
        backoff = FALLBACK;
        if (retry !== null) cancel(retry);
        retry = null;
        // Includes initial subscription and reconnect: close the snapshot gap.
        request();
        report();
      });
      feed.addEventListener('error', () => {
        if (!current()) return;
        connected = false;
        // Let a CONNECTING EventSource retain its own resume cursor. A CLOSED
        // one will never reconnect itself; replace it only at the retry pace.
        if (feed.readyState === 2) { feed.close(); stream = null; }
        report();
        armRetry();
      });
      for (const [kind, field] of [['zone.state', 'room'], ['dj.status', 'zone']]) {
        feed.addEventListener(kind, (event) => {
          if (!current()) return;
          let change;
          try { change = JSON.parse(event.data); } catch (_) { request(); return; }
          request(change && !Array.isArray(change) ? change[field] : null);
        });
      }
      for (const kind of ['topology.changed', 'player.health', 'events.reset']) {
        feed.addEventListener(kind, () => { if (current()) request(); });
      }
    }

    function suspend() {
      active = false;
      ++generation;
      connected = synced = false;
      full = false;
      rooms.clear();
      clear();
      if (stream) stream.close();
      stream = null;
      report();
    }

    function resume() {
      if (disposed || active) return;
      active = true;
      ++generation;
      failedRead = false;
      backoff = FALLBACK;
      openFeed();
      request();
      report();
    }

    return { resume, suspend,
      refresh() { if (active) { openFeed(); request(); } },
      dispose() { disposed = true; suspend(); },
    };
  }

  // Event room names are not household identities. Refresh every matching
  // group using its qualified address; never paint an unqualified delta.
  function affected(zones, room) {
    return zones.filter((zone) => zone.members.includes(room));
  }

  return { model, affected, COALESCE, FALLBACK, MAX_BACKOFF, MAX_PENDING };
})();

function listen() {
  let catalogsLoaded = false;
  const controller = LiveRemote.model({
    connect: 'EventSource' in window ? () => new EventSource(path('events')) : null,
    reload: async () => {
      try { await loadZones(); }
      finally {
        if (!catalogsLoaded && house.zones.length) {
          catalogsLoaded = true;
          void loadMoods();
          void loadFavorites();
        }
      }
    },
    refresh: async (room) => {
      const zones = LiveRemote.affected(house.zones, room);
      if (!zones.length) await loadZones();
      else for (const zone of zones) await refreshZone(zone);
    },
    status: (text) => { $('live').textContent = text; },
  });
  document.addEventListener('visibilitychange', () => {
    if (document.hidden) controller.suspend(); else controller.resume();
  });
  window.addEventListener('online', () => controller.refresh());
  window.addEventListener('pagehide', () => controller.suspend());
  window.addEventListener('pageshow', () => { if (!document.hidden) controller.resume(); });
  if (!document.hidden) controller.resume();
  return controller;
}

/** The header's "Pause everywhere": every group of every household at once. */
function pauseEverywhere() {
  const button = $('pause-all');
  button.disabled = true;
  call('pause', { body: { zone: 'everywhere' } })
    .then(noted)
    .then((outcome) => { if (outcome && !(outcome.notes || []).length) toast(outcome.done); })
    .catch(failed)
    .finally(() => {
      button.disabled = false;
      loadZones().catch(() => {});
    });
}

function start() {
  $('pause-all').addEventListener('click', pauseEverywhere);
  listen();
  void loadScenes();
  void loadDoctor();
}

start();
