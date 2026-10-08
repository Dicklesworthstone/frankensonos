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
  pause: ['POST', '/pause'],
  resume: ['POST', '/resume'],
  next: ['POST', '/next'],
  volume: ['POST', '/volume'],
  djStart: ['POST', '/dj/start'],
  djStop: ['POST', '/dj/stop'],
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
  // Room name -> household, to name a room unambiguously when two
  // households both have it.
  homes: new Map(),
  cards: new Map(),
  moods: null,
  dj: true,
};

/** How to name `room` of `household` to the API. */
function roomName(room, household) {
  const homes = house.homes.get(room);
  return homes && homes.size > 1 ? `${room}@${household}` : room;
}

function zoneKey(zone) {
  return `${zone.household}/${zone.coordinator_room}`;
}

async function loadZones() {
  const zones = await call('zones');
  house.zones = zones;
  house.homes = new Map();
  for (const zone of zones) {
    for (const room of zone.members) {
      if (!house.homes.has(room)) house.homes.set(room, new Set());
      house.homes.get(room).add(zone.household);
    }
  }
  render();
  await Promise.all(zones.map((zone) => refreshZone(zone).catch(() => {})));
}

function render() {
  const main = $('households');
  main.replaceChildren();
  house.cards = new Map();
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
    for (const zone of zones) grid.append(card(zone));
    main.append(el('section', { 'aria-labelledby': id },
      el('h2', { id, text: byHome.size > 1 ? `Household ${household}` : 'Rooms' }),
      grid));
  }
}

/** One zone's card; its parts are kept to update in place. */
function card(zone) {
  const name = roomName(zone.coordinator_room, zone.household);
  const label = zone.members.join(' + ');
  const parts = {
    zone,
    name,
    art: el('img', { alt: '', hidden: true, width: 64, height: 64 }),
    title: el('div', { class: 'title', text: 'Nothing playing' }),
    by: el('div', { class: 'by' }),
    state: el('div', { class: 'state', text: zone.transport_state }),
    play: el('button', { class: 'primary', type: 'button', text: 'Play', 'aria-label': `Play ${label}` }),
    next: el('button', { type: 'button', text: 'Next', 'aria-label': `Next track in ${label}` }),
    sliders: new Map(),
    mood: el('select', { 'aria-label': `DJ mood for ${label}` }),
    dj: el('button', { type: 'button', text: 'Start DJ', 'aria-label': `Start the DJ in ${label}` }),
    djRow: null,
    djRunning: false,
    tag: '',
  };
  parts.art.addEventListener('load', () => { parts.art.hidden = false; });
  parts.art.addEventListener('error', () => { parts.art.hidden = true; });
  parts.play.addEventListener('click', () => {
    const verb = parts.zone.transport_state === 'playing' ? 'pause' : 'resume';
    act(parts, call(verb, { body: { zone: name } }));
  });
  parts.next.addEventListener('click', () => act(parts, call('next', { body: { zone: name } })));
  parts.dj.addEventListener('click', () => {
    if (parts.djRunning) {
      act(parts, call('djStop', { body: { zone: name } }));
    } else {
      const body = { zone: name };
      if (parts.mood.value) body.mood = parts.mood.value;
      act(parts, call('djStart', { body }));
    }
  });
  const volumes = el('div', { class: 'volumes' });
  for (const room of zone.members) volumes.append(slider(parts, room, zone.household));
  parts.djRow = el('div', { class: 'row', hidden: !house.dj }, parts.mood, parts.dj);
  fillMoods(parts.mood);
  house.cards.set(zoneKey(zone), parts);
  return el('article', { class: 'zone', 'aria-label': label },
    el('h3', { text: label }),
    el('div', { class: 'now' }, parts.art, el('div', { class: 'track' }, parts.title, parts.by, parts.state)),
    el('div', { class: 'row' }, parts.play, parts.next),
    volumes,
    parts.djRow);
}

/** A room's volume slider, sent once the hand rests (debounced). */
function slider(parts, room, household) {
  const id = `vol-${tag(`${household}/${room}`)}`;
  const input = el('input', { id, type: 'range', min: 0, max: 100, step: 1, value: 0, disabled: true });
  const shown = el('output', { for: id, text: '–' });
  const entry = { input, shown, room: roomName(room, household), until: 0, timer: 0 };
  input.addEventListener('input', () => {
    shown.textContent = input.value;
    entry.until = Date.now() + 1500;
    clearTimeout(entry.timer);
    entry.timer = setTimeout(() => {
      call('volume', { body: { zone: entry.room, volume: Number(input.value) } })
        .then(noted)
        .then((outcome) => {
          if (outcome && typeof outcome.volume === 'number') setVolume(entry, outcome.volume, true);
        })
        .catch(failed);
    }, 250);
  });
  parts.sliders.set(room, entry);
  return el('div', { class: 'volume' }, el('label', { for: id, text: room }), input, shown);
}

/** Show a room's volume, unless its slider is in hand. */
function setVolume(entry, volume, force = false) {
  if (!force && Date.now() < entry.until) return;
  entry.input.disabled = false;
  entry.input.value = String(volume);
  entry.shown.textContent = String(volume);
}

function act(parts, request) {
  parts.play.disabled = true;
  request
    .then(noted)
    .catch(failed)
    .finally(() => {
      parts.play.disabled = false;
      refreshZone(parts.zone).catch(() => {});
    });
}

async function refreshZone(zone) {
  const parts = house.cards.get(zoneKey(zone));
  if (!parts) return;
  const state = await call('state', { params: { room: parts.name } });
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
  const coordinator = parts.sliders.get(zone.coordinator_room);
  if (coordinator && typeof state.volume === 'number') setVolume(coordinator, state.volume);
  await Promise.all(zone.members
    .filter((room) => room !== zone.coordinator_room)
    .map((room) => refreshVolume(parts.sliders.get(room)).catch(() => {})));
  await refreshDj(parts).catch(() => {});
}

async function refreshVolume(entry) {
  const state = await call('state', { params: { room: entry.room } });
  if (typeof state.volume === 'number') setVolume(entry, state.volume);
}

async function refreshDj(parts) {
  if (!house.dj) return;
  try {
    const status = await call('djStatus', { params: { room: parts.name } });
    parts.djRunning = Boolean(status.running);
  } catch (error) {
    if (error.code === 'NOT_IMPLEMENTED') {
      house.dj = false;
      for (const each of house.cards.values()) each.djRow.hidden = true;
      return;
    }
    parts.djRunning = false;
  }
  const label = parts.zone.members.join(' + ');
  parts.dj.textContent = parts.djRunning ? 'Stop DJ' : 'Start DJ';
  parts.dj.setAttribute('aria-label', `${parts.djRunning ? 'Stop' : 'Start'} the DJ in ${label}`);
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

// Live updates: refresh what an event touches, a moment after it lands.
const pending = new Map();
function soon(key, work) {
  clearTimeout(pending.get(key));
  pending.set(key, setTimeout(() => { pending.delete(key); work().catch(() => {}); }, 300));
}

function zoneOf(room) {
  return house.zones.find((zone) => zone.members.includes(room));
}

function listen() {
  const live = $('live');
  if (!('EventSource' in window)) {
    live.textContent = 'Refreshing every 10 s';
    setInterval(() => loadZones().catch(() => {}), 10000);
    return;
  }
  const stream = new EventSource(path('events'));
  stream.addEventListener('open', () => { live.textContent = 'Live'; });
  stream.addEventListener('error', () => { live.textContent = 'Reconnecting…'; });
  stream.addEventListener('zone.state', (event) => {
    let change = {};
    try {
      change = JSON.parse(event.data);
    } catch (_) {
      return;
    }
    const zone = zoneOf(change.room);
    if (!zone) {
      soon('all', loadZones);
      return;
    }
    const onlyLevel = change.transport === undefined && change.track === undefined;
    const parts = house.cards.get(zoneKey(zone));
    const entry = parts && parts.sliders.get(change.room);
    if (onlyLevel && entry && typeof change.volume === 'number') {
      setVolume(entry, change.volume);
      return;
    }
    soon(zoneKey(zone), () => refreshZone(zone));
  });
  for (const kind of ['topology.changed', 'player.health', 'events.reset']) {
    stream.addEventListener(kind, () => soon('all', loadZones));
  }
}

async function start() {
  try {
    await loadZones();
  } catch (error) {
    $('households').replaceChildren(el('p', { class: 'empty', text: error.message }));
  }
  listen();
  loadMoods();
  loadScenes();
  loadDoctor();
}

start();
