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
  queueMove: ['POST', '/queue/move'],
  queueAdd: ['POST', '/queue'],
  pause: ['POST', '/pause'],
  resume: ['POST', '/resume'],
  next: ['POST', '/next'],
  previous: ['POST', '/previous'],
  seek: ['POST', '/seek'],
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

// One deadline covers connection, response body and read-only backoff. A
// browser abort is not proof that a command stopped executing on the daemon.
const RequestRemote = (() => {
  const READ_TIMEOUT = 15000;
  const WRITE_TIMEOUT = 120000;
  const LONG_WRITE_TIMEOUT = 600000;
  const ANNOUNCE_TIMEOUT = 1200000;
  const RETRIES = 2;
  const TRANSIENT = new Set([408, 429, 502, 503, 504]);
  const UNCERTAIN_HINT = 'It may have taken effect or may still finish. Check the current state before sending another command. This request was not repeated.';

  function timeoutFor(method, where) {
    if (method === 'GET') return READ_TIMEOUT;
    const pathname = where.split('?')[0];
    // Queue expansion, scene fades and spoken announcements can legitimately
    // outlast ordinary controls. Do not interrupt them at a read's deadline.
    if (pathname === '/announce') return ANNOUNCE_TIMEOUT;
    if (pathname === '/queue' || pathname.startsWith('/dj/')
        || /^\/scenes\/[^/]+\/apply$/.test(pathname)) return LONG_WRITE_TIMEOUT;
    return WRITE_TIMEOUT;
  }

  function retryAfter(value, now) {
    if (typeof value !== 'string' || !value.trim()) return null;
    const text = value.trim();
    if (/^[0-9]+$/.test(text)) return Number(text) * 1000;
    // Negative and fractional numbers are not HTTP dates or delta-seconds.
    if (/^[+-]?[0-9.]+$/.test(text)) return null;
    const date = Date.parse(text);
    return Number.isFinite(date) ? Math.max(0, date - now) : null;
  }

  function create({ fetcher = (where, init) => fetch(where, init),
    schedule = setTimeout, cancel = clearTimeout, now = Date.now, random = Math.random } = {}) {
    return async function request(where, init) {
      const read = init.method === 'GET';
      const timeout = timeoutFor(init.method, where);
      const ends = now() + timeout;
      const controller = new AbortController();
      let finished = false, backoff = null, deadline = null;
      const failure = (code, detail, uncertain = false) => {
        const error = new Error(detail);
        error.code = code;
        error.uncertain = uncertain;
        if (uncertain) error.hint = UNCERTAIN_HINT;
        return error;
      };
      const timedOut = failure(read ? 'REMOTE_READ_TIMEOUT' : 'REMOTE_OUTCOME_UNKNOWN',
        read ? 'The state request timed out. Refresh to read it again.'
          : 'The action did not return a confirmation in time.', !read);
      const expired = new Promise((_, reject) => {
        deadline = schedule(() => {
          finished = true;
          // Settle independently of abort: even a stuck body/transport that
          // ignores cancellation must release its caller's busy state.
          reject(timedOut);
          controller.abort();
        }, timeout);
      });
      const run = async () => {
        for (let attempt = 0; ; attempt++) {
          if (finished) throw timedOut;
          try {
            let answer, text;
            try {
              answer = await fetcher(where, { ...init, signal: controller.signal,
                credentials: 'same-origin', redirect: 'error' });
              if (finished) throw timedOut;
              text = await answer.text();
            } catch (cause) {
              if (finished) throw timedOut;
              const error = failure(read ? 'REMOTE_NETWORK_ERROR' : 'REMOTE_OUTCOME_UNKNOWN',
                read ? 'Could not read the current state. Check the connection to FrankenSonos.'
                  : 'The connection ended before this action was confirmed.', !read);
              error.cause = cause;
              throw error;
            }
            if (finished) throw timedOut;
            let data = null, decoded = false;
            try { data = JSON.parse(text); decoded = true; } catch (_) { /* Checked below. */ }
            if (!answer.ok) {
              const detail = data && typeof data.detail === 'string' && data.detail
                ? data.detail : `${init.method} ${where}: ${answer.status}`;
              const error = failure(data && typeof data.code === 'string' ? data.code : 'HTTP_ERROR',
                detail, !read && (answer.status >= 500 || answer.status === 408));
              error.status = answer.status;
              const hint = data && typeof data.hint === 'string' ? data.hint : '';
              error.hint = [hint, error.hint].filter(Boolean).join(' ');
              error.retryAfter = retryAfter(answer.headers?.get('retry-after'), now());
              throw error;
            }
            // Empty/malformed successes cannot acknowledge a write. JSON null
            // and a genuine 204 remain valid for endpoints with no result.
            if (!decoded && answer.status !== 204) {
              throw failure(read ? 'REMOTE_INVALID_RESPONSE' : 'REMOTE_OUTCOME_UNKNOWN',
                'The daemon returned an unreadable response.', !read);
            }
            return data;
          } catch (error) {
            if (finished) throw timedOut;
            // Only reads can repeat. Never retry a POST, including 429/503,
            // lost replies, malformed acknowledgements or timed-out bodies.
            if (!read || attempt >= RETRIES
                || !((error.status === undefined && error.code === 'REMOTE_NETWORK_ERROR') || TRANSIENT.has(error.status))) throw error;
            const base = 250 * (2 ** attempt);
            const delay = Math.max(base + Math.floor(random() * base), error.retryAfter ?? 0);
            // Honor Retry-After in full or stop; never cap it into an early retry.
            if (delay >= ends - now()) throw error;
            await new Promise((resolve) => { backoff = schedule(resolve, delay); });
            backoff = null;
          }
        }
      };
      try {
        return await Promise.race([run(), expired]);
      } finally {
        finished = true;
        cancel(deadline);
        if (backoff !== null) cancel(backoff);
      }
    };
  }

  return { create, timeoutFor, retryAfter, READ_TIMEOUT, WRITE_TIMEOUT,
    LONG_WRITE_TIMEOUT, ANNOUNCE_TIMEOUT, RETRIES };
})();

const requestJson = RequestRemote.create();

/** Call route `name`; its JSON answer, or an Error carrying the API's. */
async function call(name, { params, query, body } = {}) {
  const [method] = ROUTES[name];
  const init = { method, headers: { Accept: 'application/json' }, cache: 'no-store' };
  if (method !== 'GET') {
    init.headers['Content-Type'] = 'application/json';
    init.body = JSON.stringify(body || {});
  }
  return requestJson(path(name, params, query), init);
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

// A room keeps its write lane even when topology replaces its card.
const volumeFlights = new Map();
// A regrouped card must not overtake a transport command still addressing its rooms.
const transportFlights = new Map();
const validVolume = (value) => Number.isInteger(value) && value >= 0 && value <= 100;

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
  catalogStatus: new Map(),
};

// Embedded card panels share the card's lifetime and ordered state reads.
const cardPanels = [];
function addCardPanel(parts, factory) {
  const panel = factory(parts);
  parts.panels.push(panel);
  parts.panelHost.append(panel.node);
  if (parts.lastState) panel.observe(parts.lastState);
}
function registerCardPanel(factory) {
  cardPanels.push(factory);
  for (const parts of house.cards.values()) addCardPanel(parts, factory);
}
function notifyCard(parts, method, ...args) {
  if (method === 'stale') parts.lastState = null;
  parts.queue?.[method]?.(...args);
  for (const panel of parts.panels || []) panel[method]?.(...args);
}

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
  let zones;
  try { zones = checkedZones(await call('zones')); }
  catch (error) {
    if (version === house.readVersion) for (const parts of house.cards.values()) notifyCard(parts, 'stale');
    throw error;
  }
  if (version !== house.readVersion) return;
  house.zones = zones;
  render();
  catalogs.reconcile(zones);
  await readEach(zones, refreshZone, () => version === house.readVersion);
}

function ownsCard(parts) {
  return !parts.disposed && house.cards.get(zoneKey(parts.zone)) === parts;
}

function disposeCard(parts) {
  parts.disposed = true;
  ++parts.readVersion;
  notifyCard(parts, 'dispose');
  for (const entry of parts.sliders.values()) {
    clearTimeout(entry.timer);
    entry.timer = entry.pendingVolume = null;
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
    panels: [],
    panelHost: el('div'),
    lastState: null,
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
    djMood: null,
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
    zone: name, call, el, current: () => ownsCard(parts),
    blocked: () => parts.writing || transportBusy(parts),
    execute: async (work) => {
      if (!beginAction(parts)) return;
      try { return await work(); } finally { endAction(parts); }
    },
  });
  house.cards.set(zoneKey(zone), parts);
  parts.node = el('article', { class: 'zone', 'aria-label': label },
    el('h3', { text: label }),
    el('div', { class: 'now' }, parts.art, el('div', { class: 'track' }, parts.title, parts.by, parts.state)),
    el('div', { class: 'row' }, parts.previous, parts.play, parts.next),
    parts.panelHost,
    parts.favorite,
    volumes,
    parts.djRow,
    parts.djPlaying,
    parts.queue.node);
  for (const factory of cardPanels) addCardPanel(parts, factory);
  actionControls(parts, transportBusy(parts));
  return parts.node;
}

/** A room's volume slider, sent once the hand rests (debounced). */
function slider(parts, room, household) {
  const id = `vol-${tag(`${household}/${room}`)}`;
  const input = el('input', { id, type: 'range', min: 0, max: 100, step: 1, value: 0, disabled: true });
  const shown = el('output', { for: id, text: '–' });
  const mute = el('button', { type: 'button', class: 'mute', text: 'Mute', 'aria-pressed': 'false', 'aria-label': `Mute ${room}` });
  const entry = { input, shown, mute, muted: false, room: roomName(room, household), timer: null, pendingVolume: null, volumeWriting: false, version: 0, readVersion: 0, volumeVersion: 0, mutedPending: false };
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
    const volume = Number(input.value);
    if (!ownsCard(parts) || input.disabled || !validVolume(volume)) return;
    ++entry.version;
    ++entry.volumeVersion;
    entry.pendingVolume = volume;
    shown.textContent = `${volume}…`;
    input.setAttribute('aria-busy', 'true');
    input.title = 'Applying volume; further adjustments replace the queued level.';
    clearTimeout(entry.timer);
    entry.timer = setTimeout(() => {
      entry.timer = null;
      void writeVolume(entry, parts);
    }, 250);
  });
  parts.sliders.set(room, entry);
  return el('div', { class: 'volume' }, el('label', { for: id, text: room }), input, shown, mute);
}

/** One request per room at a time, with at most one unsent absolute level. */
async function writeVolume(entry, parts) {
  if (!ownsCard(parts) || volumeFlights.has(entry.room)
      || entry.timer !== null || entry.pendingVolume === null) return;
  const volume = entry.pendingVolume;
  entry.pendingVolume = null;
  entry.volumeWriting = true;
  volumeFlights.set(entry.room, entry);
  let outcome, error;
  try {
    outcome = await call('volume', { body: { zone: entry.room, volume } });
    if (!outcome || !validVolume(outcome.volume)) throw new Error('The volume reply is incomplete.');
    if (ownsCard(parts)) noted(outcome);
  } catch (failure) { error = failure; }
  volumeFlights.delete(entry.room);
  entry.volumeWriting = false;
  // Fence reads that began while the old command could still change the level.
  ++entry.version;
  if (!ownsCard(parts)) {
    for (const card of house.cards.values()) {
      for (const replacement of card.sliders.values()) {
        if (replacement.room === entry.room) void refreshVolume(replacement, card).catch(() => {});
      }
    }
    return;
  }
  if (error) {
    // A lost reply is not proof of no effect. Never replay it or drain drafts
    // after it; require a new user adjustment following an authoritative read.
    clearTimeout(entry.timer);
    entry.timer = entry.pendingVolume = null;
    ++entry.volumeVersion;
    entry.input.disabled = true;
    entry.input.setAttribute('aria-busy', 'false');
    entry.input.title = 'Volume result uncertain. Reading the current level; queued changes were discarded.';
    entry.shown.textContent = '?';
    failed(error);
    void refreshVolume(entry, parts).catch(() => {});
    return;
  }
  // Repeating the same draft must not repeatedly step around a policy clamp.
  if (entry.pendingVolume === volume) {
    clearTimeout(entry.timer);
    entry.timer = entry.pendingVolume = null;
  }
  if (entry.pendingVolume === null) setVolume(entry, outcome.volume);
  else if (entry.timer === null) void writeVolume(entry, parts);
}

/** Show whether a room is muted. */
function setMute(entry, muted) {
  entry.muted = muted;
  entry.mute.textContent = muted ? 'Unmute' : 'Mute';
  entry.mute.setAttribute('aria-pressed', String(muted));
}

/** Show an observed level only when no local adjustment can still change it. */
function setVolume(entry, volume) {
  if (!validVolume(volume) || volumeFlights.has(entry.room)
      || entry.pendingVolume !== null || entry.timer !== null) return;
  entry.input.disabled = false;
  entry.input.setAttribute('aria-busy', 'false');
  entry.input.title = '';
  entry.input.value = String(volume);
  entry.shown.textContent = String(volume);
}

function transportRooms(parts) {
  return parts.zone.members.map((room) => roomName(room, parts.zone.household));
}

function transportBusy(parts) {
  return transportRooms(parts).some((room) => transportFlights.has(room));
}

function actionControls(parts, busy) {
  for (const control of [parts.play, parts.next, parts.previous, parts.favorite,
    parts.dj, parts.skip, parts.mood, parts.like, parts.dislike]) control.disabled = busy;
  parts.favorite.disabled = busy || !(house.favorites.get(parts.zone.household) || []).length;
  notifyCard(parts, 'block', busy);
}

function beginAction(parts) {
  if (!ownsCard(parts) || parts.writing || transportBusy(parts)) return false;
  parts.writing = true;
  for (const room of transportRooms(parts)) transportFlights.set(room, parts);
  ++parts.readVersion;
  actionControls(parts, true);
  return true;
}

function endAction(parts) {
  parts.writing = false;
  const affected = new Set();
  for (const [room, owner] of transportFlights) {
    if (owner === parts) { affected.add(room); transportFlights.delete(room); }
  }
  ++parts.readVersion;
  // A split/merged group inherits the pending command's exclusion until it
  // settles. Then invalidate old reads and reconcile each affected live card.
  for (const current of house.cards.values()) {
    if (!ownsCard(current) || !transportRooms(current).some((room) => affected.has(room))) continue;
    ++current.readVersion;
    const busy = current.writing || transportBusy(current);
    actionControls(current, busy);
    if (!busy) void refreshZone(current.zone).catch(() => {});
  }
}

async function act(parts, request, announce = false) {
  if (!beginAction(parts)) return;
  try {
    const outcome = noted(await request());
    if (announce && outcome && typeof outcome.done === 'string') toast(outcome.done);
  } catch (error) { failed(error); }
  finally { endAction(parts); }
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
    notifyCard(parts, 'stale');
    throw error;
  }
  parts.lastState = state;
  notifyCard(parts, 'observe', state);
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
  try { await parts.queue?.reconcile?.(); } catch (error) { failure = error; }
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
    parts.djMood = (status.steering && status.steering.mood) || '';
    if (parts.djRunning) parts.mood.value = parts.djMood;
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

/** A zone's favorites picker, retaining last-known choices during an outage. */
function fillFavorites(parts) {
  const favorites = house.favorites.get(parts.zone.household) || [];
  const phase = house.catalogStatus.get(CatalogRemote.keyOf('favorites', parts.zone.household)) || 'loading';
  const selected = parts.favorite.value;
  const caption = phase === 'ready' ? 'Play a favorite…'
    : phase === 'loading' ? 'Loading favorites…'
      : phase === 'retry' ? 'Favorites unavailable; retrying…' : 'Favorites unavailable';
  parts.favorite.replaceChildren(el('option', { value: '', text: caption }));
  for (const favorite of favorites) {
    parts.favorite.append(el('option', { value: favorite.id, text: favorite.title }));
  }
  if (favorites.some((favorite) => favorite.id === selected)) parts.favorite.value = selected;
  parts.favorite.hidden = favorites.length === 0 && phase === 'ready';
  parts.favorite.disabled = parts.writing || transportBusy(parts) || favorites.length === 0;
  parts.favorite.title = phase !== 'ready' && favorites.length
    ? 'Last-known favorites are shown while their list is refreshed.' : '';
}

/** Read one household; a failure is not an empty favorites collection. */
async function loadFavorites(zone) {
  const favorites = await call('favorites', { query: { zone } });
  const text = (value) => typeof value === 'string' && value.trim() !== '';
  if (!Array.isArray(favorites) || favorites.some((f) => !f || !text(f.id)
      || typeof f.title !== 'string' || !text(f.kind))
      || new Set(favorites.map((f) => f.id)).size !== favorites.length) {
    throw new Error('The favorites list is unreadable.');
  }
  return favorites.filter((f) => f.kind !== 'unplayable');
}

function fillMoods(select, selected = select.value) {
  const phase = house.catalogStatus.get(CatalogRemote.keyOf('moods')) || 'loading';
  const caption = phase === 'ready' ? 'Any mood'
    : phase === 'loading' ? 'Any mood — loading choices…'
      : phase === 'retry' ? 'Any mood — other moods unavailable; retrying…' : 'Any mood — other moods unavailable';
  select.replaceChildren(el('option', { value: '', text: caption }));
  for (const mood of house.moods || []) select.append(el('option', { value: mood, text: mood }));
  if ((house.moods || []).includes(selected)) select.value = selected;
}

async function loadMoods(zone) {
  const moods = await call('moods', { query: { zone } });
  if (!moods || !Array.isArray(moods.moods) || moods.moods.some((m) => !m
      || typeof m.name !== 'string' || !m.name.trim())) throw new Error('The DJ mood list is unreadable.');
  const names = moods.moods.map((m) => m.name);
  if (new Set(names).size !== names.length) throw new Error('The DJ mood list is ambiguous.');
  return names;
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

  function model({ connect, reload, refresh, status = () => {}, invalidate = () => {},
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
      invalidate(room);
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
        else {
          let failure = null;
          for (const room of batch) {
            if (!active || version !== generation) break;
            try { await refresh(room); } catch (error) { failure = error; }
          }
          if (failure && active && version === generation) throw failure;
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
        invalidate();
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
      invalidate();
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

// Catalog reads have their own recovery lane: a failed favorites/moods read
// must not hide controls until a page reload or force whole-house polling.
const CatalogRemote = (() => {
  const FIRST = 5000, MAX = 60000, CONCURRENCY = 4;
  const keyOf = (kind, household = '') => JSON.stringify([kind, household]);

  function sources(zones) {
    const firsts = new Map();
    for (const zone of zones) {
      const source = { kind: 'favorites', household: zone.household,
        zone: roomName(zone.coordinator_room, zone.household) };
      const prior = firsts.get(source.household);
      if (!prior || source.zone < prior.zone) firsts.set(source.household, source);
    }
    const found = [...firsts.values()].sort((a, b) => a.zone < b.zone ? -1 : a.zone > b.zone ? 1 : 0);
    return [...found.map((source) => ({ ...source, key: keyOf('favorites', source.household) })),
      ...(found.length ? [{ ...found[0], kind: 'moods', key: keyOf('moods') }] : [])];
  }

  function model({ read, publish, status = () => {}, removed = () => {},
    schedule = setTimeout, cancel = clearTimeout, now = Date.now }) {
    const slots = new Map();
    let active = false, disposed = false, running = 0, timer = null;
    const clear = () => { if (timer !== null) cancel(timer); timer = null; };
    const reset = (slot) => { ++slot.epoch; slot.done = false; slot.due = Math.max(now(), slot.notBefore); slot.delay = FIRST; };

    async function run(slot) {
      const source = slot.source, epoch = slot.epoch;
      const current = () => active && slots.get(source.key) === slot && slot.epoch === epoch;
      slot.busy = true;
      ++running;
      try {
        // Do not reenter the dispatcher if an injected read throws synchronously.
        await Promise.resolve();
        if (!current()) return;
        status(source, 'loading');
        const value = await read(source);
        if (!current()) return;
        publish(source, value);
        slot.done = true;
        slot.notBefore = 0;
        slot.delay = FIRST;
        status(source, 'ready');
      } catch (error) {
        if (!current()) return;
        // Missing optional capabilities and explicit permission denials wait
        // for a new source, reconnect or user refresh, not automatic hammering.
        slot.done = [401, 403].includes(error?.status)
          || ['NOT_IMPLEMENTED', 'POLICY_DENIED'].includes(error?.code);
        // Preserve the server's minimum across page visibility and reconnects.
        slot.notBefore = now() + Math.max(0, error?.retryAfter ?? 0);
        slot.done ||= !Number.isFinite(slot.notBefore);
        slot.due = Math.max(now() + slot.delay, slot.notBefore);
        slot.delay = Math.min(MAX, slot.delay * 2);
        status(source, slot.done ? 'unavailable' : 'retry');
      } finally {
        slot.busy = false;
        --running;
        pump();
      }
    }

    function pump() {
      clear();
      if (!active) return;
      for (const slot of slots.values()) {
        if (running >= CONCURRENCY) break;
        if (!slot.busy && !slot.done && slot.due <= now()) void run(slot);
      }
      if (running >= CONCURRENCY) return;
      const due = [...slots.values()].filter((slot) => !slot.busy && !slot.done && Number.isFinite(slot.due)).map((slot) => slot.due);
      // Chunk a long wait without ever dispatching before its actual due time.
      if (due.length) timer = schedule(() => { timer = null; pump(); }, Math.min(MAX, Math.max(0, Math.min(...due) - now())));
    }

    function reconcile(zones) {
      if (disposed) return;
      const wanted = new Map(sources(zones).map((source) => [source.key, source]));
      for (const [key, slot] of slots) {
        if (!wanted.has(key)) { slots.delete(key); removed(slot.source); }
      }
      for (const [key, source] of wanted) {
        let slot = slots.get(key);
        if (!slot) {
          slot = { source, epoch: 0, busy: false, notBefore: 0 };
          reset(slot);
          slots.set(key, slot);
        } else if (slot.source.zone !== source.zone) {
          slot.source = source;
          reset(slot);
        }
      }
      pump();
    }

    function suspend() {
      active = false;
      clear();
      for (const slot of slots.values()) reset(slot);
    }

    function resume() {
      if (disposed || active) return;
      active = true;
      pump();
    }

    return { reconcile, resume, suspend,
      refresh() { if (active) { for (const slot of slots.values()) reset(slot); pump(); } },
      dispose() { disposed = true; suspend(); slots.clear(); },
    };
  }

  return { model, sources, keyOf, FIRST, MAX, CONCURRENCY };
})();

const catalogs = CatalogRemote.model({
  read: (source) => source.kind === 'favorites' ? loadFavorites(source.zone) : loadMoods(source.zone),
  publish(source, value) {
    if (source.kind === 'favorites') house.favorites.set(source.household, value);
    else house.moods = value;
  },
  status(source, phase) {
    house.catalogStatus.set(source.key, phase);
    for (const parts of house.cards.values()) {
      if (source.kind === 'favorites' && parts.zone.household === source.household) fillFavorites(parts);
      if (source.kind === 'moods') fillMoods(parts.mood,
        !parts.writing && parts.djRunning ? (parts.djMood ?? parts.mood.value) : parts.mood.value);
    }
  },
  removed(source) {
    house.catalogStatus.delete(source.key);
    if (source.kind === 'favorites') house.favorites.delete(source.household);
    else house.moods = null;
  },
});

function listen() {
  const controller = LiveRemote.model({
    connect: 'EventSource' in window ? () => new EventSource(path('events')) : null,
    reload: loadZones,
    refresh: async (room) => {
      const zones = LiveRemote.affected(house.zones, room);
      if (!zones.length) await loadZones();
      else for (const zone of zones) await refreshZone(zone);
    },
    status: (text) => { $('live').textContent = text; },
    invalidate: (room) => {
      const matches = typeof room === 'string' ? LiveRemote.affected(house.zones, room) : [];
      for (const parts of house.cards.values()) {
        if (!matches.length || parts.zone.members.includes(room)) notifyCard(parts, 'stale');
      }
    },
  });
  const suspend = () => { controller.suspend(); catalogs.suspend(); };
  const resume = () => { if (!document.hidden) { catalogs.resume(); controller.resume(); } };
  document.addEventListener('visibilitychange', () => {
    if (document.hidden) suspend(); else resume();
  });
  window.addEventListener('online', () => { controller.refresh(); catalogs.refresh(); });
  window.addEventListener('pagehide', suspend);
  window.addEventListener('pageshow', resume);
  resume();
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
