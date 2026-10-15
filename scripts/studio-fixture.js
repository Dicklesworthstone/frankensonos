// Synthetic browser fixture. It is never embedded in the daemon.
'use strict';
const el = (tag, attrs = {}, ...children) => {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(attrs)) {
    if (value === null || value === undefined || value === false) continue;
    if (key === 'text') node.textContent = value;
    else if (key === 'class') node.className = value;
    else if (key.startsWith('on')) node.addEventListener(key.slice(2), value);
    else node.setAttribute(key, value === true ? '' : String(value));
  }
  for (const child of children) if (child != null) node.append(child);
  return node;
};
const house = { zones: [], cards: new Map() }, factories = [], commands = [];
const registerCardPanel = factory => { factories.push(factory); for (const p of house.cards.values()) { const panel = factory(p); p.panels.push(panel); p.panelHost.append(panel.node); } };
function nativeCard(zone, track) {
  const name = `${zone.coordinator_room}@${zone.household}`;
  const parts = { zone, name, writing: false, disposed: false, panels: [], panelHost: el('div'), sliders: new Map(),
    art: el('img', { hidden: true, alt: '' }), title: el('div', { class: 'title', text: track.title }), by: el('div', { class: 'by', text: `${track.creator} · ${track.album}` }), state: el('div', { class: 'state', text: zone.transport_state }),
    previous: el('button', { text: 'Prev', type: 'button' }), play: el('button', { class: 'primary', text: zone.transport_state === 'playing' ? 'Pause' : 'Play', type: 'button' }), next: el('button', { text: 'Next', type: 'button' }),
    favorite: el('select', { 'aria-label': 'Play a favorite' }, el('option', { text: 'Play a household favorite…', value: '' }), el('option', { text: 'Evening favorites', value: 'FV:example' })),
    mood: el('select', { 'aria-label': 'DJ mood' }, ...['Dinner', 'Focus', 'Calm', 'Bright'].map(x => el('option', { text: x, value: x.toLowerCase() }))),
    dj: el('button', { text: zone.household === 'S1' ? 'Start DJ' : 'Stop DJ', type: 'button' }), skip: el('button', { text: 'Skip', type: 'button' }), like: el('button', { text: 'Like', type: 'button' }), dislike: el('button', { text: 'Dislike', type: 'button' }),
    lastState: { zone, transport_state: zone.transport_state, group_volume: 24, volume: 22, mute: false, track }, djRunning: zone.household !== 'S1',
  };
  const act = kind => () => { commands.push({ kind, zone: name }); };
  for (const name of ['previous','play','next','dj','skip','like','dislike']) parts[name].addEventListener('click', act(name));
  const volumes = el('div', { class: 'volumes' });
  for (const [i, room] of zone.members.entries()) {
    const input = el('input', { type: 'range', min: 0, max: 100, value: 20 + i * 4, 'aria-label': `${room} level` });
    const shown = el('output', { text: 20 + i * 4 });
    input.addEventListener('input', () => { shown.textContent = input.value; commands.push({ kind: 'room-draft', room }); });
    const mute = el('button', { type: 'button', class: 'mute', text: 'Mute' });
    const entry = { input, shown, mute, room: `${room}@${zone.household}`, timer: null, pendingVolume: null, version: 0 };
    parts.sliders.set(room, entry);
    volumes.append(el('div', { class: 'volume' }, el('label', { text: room }), input, shown, mute));
  }
  parts.djRow = el('div', { class: 'row' }, parts.mood, parts.dj);
  parts.djPlaying = el('div', { class: 'row', hidden: !parts.djRunning }, parts.skip, parts.like, parts.dislike);
  const clock = el('section', { class: 'playback-clock' }, el('div', { class: 'playback-time' }, el('span', { text: '≈ 2:18 / 9:12' }), el('span', { text: '6:54 left' })),
    el('input', { type: 'range', value: 40, 'aria-label': 'Choose a playback position' }),
    el('div', { class: 'playback-detail', text: 'Estimated from the latest playback sample.' }), el('div', { class: 'row' }, ...['Restart','−15 seconds','+15 seconds','Refresh position'].map(x => el('button', { text: x, type: 'button' }))));
  const shown = el('output', { text: '24' }), consent = el('div', { class: 'row', hidden: true });
  const groupRange = el('input', { type: 'range', value: 24, min: 0, max: 100, 'aria-label': `Group volume for ${zone.members.join(' + ')}` });
  const preview = value => { consent.replaceChildren(el('button', { text: `Set group to ${value}`, onclick: () => { commands.push({ kind: 'group-volume', volume: value, zone: name }); shown.textContent = String(value); consent.hidden = true; } }), el('button', { text: 'Cancel group change', onclick: () => { consent.hidden = true; } })); consent.hidden = false; };
  groupRange.addEventListener('input', () => preview(Number(groupRange.value)));
  const group = el('section', { class: 'group-volume', 'aria-label': 'Whole-group volume' }, el('strong', { text: 'Group volume' }), el('div', { class: 'group-volume-level' }, groupRange, shown),
    el('div', { class: 'row' }, el('button', { text: 'Quieter −5', onclick: () => preview(19) }), el('button', { text: 'Louder +5', onclick: () => preview(29) })), consent, el('p', { role: 'status' }), el('button', { text: 'Refresh group level' }), el('p', { class: 'group-volume-help', text: 'Changes every room in this group. Room caps and quiet hours still apply; playback and mute are unchanged.' }));
  parts.panelHost.append(clock); if (zone.members.length > 1) parts.panelHost.append(group);
  parts.queue = { node: el('details', { class: 'queue' }, el('summary', { text: 'Queue · 12 tracks' }), el('div', { class: 'queue-body' }, el('p', { text: 'The next chapter in your evening.' }), el('ol', { class: 'queue-items' }, ...['II. Adagio assai','III. Presto','Peace Piece'].map(t => el('li', {}, el('div', { class: 'track' }, el('div', { class: 'title', text: t })), el('button', { text: 'Play' })))))), dispose() {} };
  parts.node = el('article', { class: 'zone', 'aria-label': zone.members.join(' + ') }, el('h3', { text: `${zone.members.join(' + ')} · ${zone.household}` }),
    el('div', { class: 'now' }, parts.art, el('div', { class: 'track' }, parts.title, parts.by, parts.state)), el('div', { class: 'row' }, parts.previous, parts.play, parts.next), parts.panelHost, parts.favorite, volumes, parts.djRow, parts.djPlaying, parts.queue.node);
  house.cards.set(JSON.stringify([zone.household, zone.coordinator_room]), parts);
  for (const factory of factories) { const panel = factory(parts); parts.panels.push(panel); parts.panelHost.append(panel.node); }
  return parts;
}
function makeHouse(zones = [
  { household: 'S2', coordinator_room: 'Living room', members: ['Living room','Kitchen'], transport_state: 'playing' },
  { household: 'S2', coordinator_room: 'Study', members: ['Study'], transport_state: 'paused' },
  { household: 'S1', coordinator_room: 'Living room', members: ['Living room','Terrace'], transport_state: 'stopped' },
]) {
  for (const parts of house.cards.values()) { parts.disposed = true; for (const p of parts.panels) p.dispose?.(); }
  house.zones = zones; house.cards = new Map(); const main = document.getElementById('households'); main.replaceChildren();
  for (const [i, z] of zones.entries()) {
    const track = [{ title: 'II. Adagio assai', creator: 'Maurice Ravel', album: 'Piano Concerto in G major', uri: 'renderer:fixture:ravel', queue_position: 2 }, { title: 'Clair de lune', creator: 'Claude Debussy', album: 'Suite bergamasque' }, { title: 'A quiet moment', creator: 'No music playing', album: 'Ready when you are' }][i % 3];
    main.append(el('section', {}, el('h2', { text: z.household }), el('div', { class: 'zones' }, nativeCard(z, track).node)));
  }
}
makeHouse();
const tools = [
  ['Music library','Search your library', 'Find albums, tracks, and household favorites.'],
  ['DJ direction and explanations','Shape the next pick', 'Moods, genres, eras, artists, and complete classical works.'],
  ['DJ session','Behind the selection', 'See the DJ’s reasoning and the next works in its set.'],
  ['DJ preferences','Your standing preferences', 'Your choices come before learned preferences.'],
  ['Room grouping','Move music & group rooms', 'Keep a session with you. Households stay separate.'],
  ['Speaker sound','Speaker sound', 'Bass, treble, and loudness for a selected speaker.'],
  ['Copy speaker sound','Copy speaker sound', 'Review each destination before copying tone.'],
  ['Scenes','Scenes', 'One command for your whole atmosphere.'],
  ['Sleep timers','Sleep timers', 'Fade out gently and pause without keeping a browser open.'],
  ['Schedules','Scheduled routines', 'The daemon remembers the time, even after a restart.'],
  ['Announcements','Announcements', 'Say something, then give the music back.'],
  ['Action history','Action history & undo', 'Review the observed outcomes and supported recovery.'],
];
for (const [label, title, hint] of tools) {
  const target = label === 'Music library' ? document.getElementById('music-library') : label === 'Room grouping' ? document.getElementById('room-groups') : el('section', { class: 'scenes', 'aria-label': label });
  const search = el('input', { 'aria-label': title, placeholder: 'Choose a room or search…' });
  const details = el('details', { class: 'zone' }, el('summary', { text: title }), el('p', { text: hint }), search, el('button', { text: 'Read current state', onclick: () => commands.push({ kind: 'tool-read', title }) }));
  details.addEventListener('toggle', () => { if (details.open) commands.push({ kind: 'tool-open', title }); }); target.append(details);
  if (!target.isConnected) document.getElementById('households').before(target);
}
document.getElementById('live').textContent = 'Live';
window.fixture = { house, commands, makeHouse, el };

// Minimal fixture schema: the runtime uses DjSessionRemote's production decoder.
// This fixture makes no network requests and is not served by the daemon.
const DjSessionRemote = {
  zonesOf(zones) {
    return zones.map(zone => ({ target: `${zone.coordinator_room}@${zone.household}`, room: zone.coordinator_room,
      label: `${zone.members.join(' + ')} (${zone.household})`,
      signature: JSON.stringify([zone.household, zone.coordinator_room, [...zone.members].sort()]) }));
  },
  statusOf(value, zone) {
    if (!value || value.zone !== zone.room || typeof value.running !== 'boolean' || !value.steering) throw new Error('Incomplete fixture observation');
    return structuredClone(value);
  },
};
const storyReads = [];
function storyFor(target) {
  const zone = target.split('@').slice(0, -1).join('@');
  if (target.endsWith('@S1')) return { zone, running: false, steering: { source: 'none' } };
  return { zone, running: true, steering: { source: 'session', mood: 'dinner' },
    now: { title: 'Piano Concerto in G major', composer: 'Maurice Ravel', album: 'Example recording',
      movements: 3, movement: 2, movement_title: 'Adagio assai', minutes: 23,
      reason: { summary: 'A complete work for an unhurried evening. Ravel hasn’t appeared in your recent set; all three movements stay together.', factors: [], relaxed: [] } },
    next: [
      { title: 'Peace Piece', composer: 'Bill Evans', movements: 1, minutes: 7, reason: { summary: 'A change in texture.' } },
      { title: 'Suite bergamasque', composer: 'Claude Debussy', movements: 4, minutes: 18, reason: { summary: 'A complete work, in order.' } },
      { title: 'In a Silent Way', composer: 'Miles Davis', movements: 1, minutes: 20, reason: { summary: 'Variety across the set.' } },
    ] };
}
const call = async (route, options) => {
  if (route !== 'sessionStatus') throw new Error(`Unexpected fixture route: ${route}`);
  storyReads.push({ route, options });
  if (fixture.storyRead) return fixture.storyRead(options.params.room);
  return storyFor(options.params.room);
};
Object.assign(fixture, { storyReads, storyFor, notify(method, dto) {
  const parts = [...house.cards.values()].find(parts => !parts.node.hidden);
  for (const panel of parts.panels) panel[method]?.(dto || parts.lastState);
} });
// Clear in exported screenshots: these are design fixtures, never live site data.
document.querySelector('.studio-footer > span').textContent = 'DESIGN FIXTURE · SYNTHETIC ROOMS AND MUSIC';

// Mirrors the native editor's public navigation event, not its command path.
document.addEventListener('fsonos:dj-edit', event => {
  event.preventDefault();
  fixture.editorTarget = structuredClone(event.detail);
});
